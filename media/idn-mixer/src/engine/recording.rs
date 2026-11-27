//! Recording the desk and soundcheck (play recordings back through the channels).

use super::*;

/// Recording starts take turns on it; a rate restart holds it from its check to its end, so
/// no recording starts in between (bug run N2-1: one started then was cut by the exit).
pub(super) static STARTING: Mutex<()> = Mutex::new(());

impl Console {
    /// Starts a multitrack recording (§M.13) of `channels` (from 1; `None`: those on the
    /// desk), each channel's input as it comes, into a new folder (`name`, or the date and
    /// time): the folder's name.
    pub fn record_start(
        &self,
        channels: Option<Vec<usize>>,
        name: Option<&str>,
    ) -> Result<String, String> {
        let sh = &self.shared;
        // The files are made with the recorder's lock not held: the clock thread takes it
        // every block, and creating 64 WAV files on a slow disk was a dropout at the start of
        // every recording. Starts take turns on a lock of their own.
        let _one = STARTING.lock_ok();
        if sh.recorder.lock_ok().is_some() {
            return Err("already recording".into());
        }
        let (s, active, inputs) = {
            let d = sh.desk.lock_ok();
            let s = d.settings_arc();
            let active: Vec<bool> = (0..s.channels.len()).map(|i| d.active(i)).collect();
            (s, active, d.io().0)
        };
        let chosen: Vec<usize> = match channels {
            // Each channel once, in order: one given twice wrote two tracks into one file.
            Some(c) => c
                .into_iter()
                .filter(|&c| c >= 1 && c <= s.channels.len())
                .map(|c| c - 1)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
            None => (0..s.channels.len()).filter(|&i| active[i]).collect(),
        };
        let tracks: Vec<crate::record::Track> = chosen
            .into_iter()
            .filter_map(|i| {
                let c = &s.channels[i];
                let input = c.source.unwrap_or(i + 1).checked_sub(1)?;
                (input < inputs).then(|| crate::record::Track {
                    input,
                    channel: i + 1,
                    name: c.name.clone(),
                })
            })
            .collect();
        let root = self.recordings_dir();
        std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        let block = self.shared.desk.lock_ok().block();
        // Busy from its files' making to the recorder held (a rate restart waits: bug run
        // M6-1).
        let _busy = Busy::new(&self.shared.recording_busy);
        let r = crate::record::Recorder::start(&root, name, tracks, rate_hz(), block)
            .map_err(|e| e.to_string())?;
        let folder = r
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        *sh.recorder.lock_ok() = Some(r);
        self.bump();
        Ok(folder)
    }

    /// Stops the recording, its files finished: how it went.
    pub fn record_stop(&self) -> Result<Value, String> {
        // Busy until its files are finished (M6-1: taken out first, the desk could end
        // while its writer still wrote the last seconds and the headers).
        let _busy = Busy::new(&self.shared.recording_busy);
        let r = self
            .shared
            .recorder
            .lock_ok()
            .take()
            .ok_or("not recording")?;
        let st = r.stop();
        self.bump();
        Ok(st)
    }

    /// Where recordings go (§M.13): the configured folder, else `recordings` in the desk's
    /// state folder, else in the desk program's own folder (never the working directory: a
    /// desk embedded without folders recorded wherever its host was started).
    pub fn recordings_dir(&self) -> PathBuf {
        let c = &self.shared.cfg;
        c.recordings.clone().unwrap_or_else(|| {
            c.state_dir
                .clone()
                .unwrap_or_else(|| idn_common::paths::program_dir("mixer"))
                .join("recordings")
        })
    }

    /// The recordings, newest first (§M.13).
    pub fn recordings(&self) -> Vec<Value> {
        crate::record::recordings(&self.recordings_dir())
    }

    /// Plays recording `name` in place of the live inputs (§M.13), paused at its start;
    /// `None` goes back to the live inputs.
    pub fn soundcheck(&self, name: Option<&str>) -> Result<(), String> {
        let sh = &self.shared;
        let sc = match name {
            None => None,
            Some(n) => {
                // (Nor `:`: on Windows "C:" named a drive's folder; bug run 4.)
                if n.is_empty() || n.contains(['/', '\\', ':']) || n.starts_with('.') {
                    return Err("a recording's folder name".into());
                }
                let inputs = sh.desk.lock_ok().io().0;
                Some(
                    crate::record::Soundcheck::open(
                        &self.recordings_dir().join(n),
                        sh.desk.lock_ok().block(),
                        inputs,
                        rate_hz(),
                    )
                    .map_err(|e| e.to_string())?,
                )
            }
        };
        // The one before let go outside the lock: its drop joins its reader thread, and
        // the clock takes this lock every block.
        let old = std::mem::replace(&mut *sh.soundcheck.lock_ok(), sc);
        drop(old);
        self.bump();
        Ok(())
    }

    /// Keeps channel `n`'s input live during the soundcheck (or not).
    pub fn soundcheck_live(&self, n: usize, live: bool) -> Result<(), String> {
        let sh = &self.shared;
        let (input, inputs) = {
            let d = sh.desk.lock_ok();
            let c = d
                .settings()
                .channels
                .get(n.wrapping_sub(1))
                .ok_or("no such channel")?;
            (
                c.source
                    .unwrap_or(n)
                    .checked_sub(1)
                    .ok_or("the channel has no input")?,
                d.io().0,
            )
        };
        let mut l = sh.live.lock_ok();
        l.resize(inputs, false);
        if let Some(x) = l.get_mut(input) {
            *x = live;
        }
        drop(l);
        self.bump();
        Ok(())
    }

    /// The soundcheck's transport: play, pause, locate, loop.
    pub fn soundcheck_transport(&self, t: crate::record::Transport) -> Result<(), String> {
        let sh = &self.shared;
        sh.soundcheck
            .lock_ok()
            .as_ref()
            .ok_or("no soundcheck loaded")?
            .transport(t);
        self.bump();
        Ok(())
    }
}

/// A count held while a recording starts or finishes.
struct Busy<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> Busy<'a> {
    fn new(n: &'a std::sync::atomic::AtomicUsize) -> Busy<'a> {
        n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Busy(n)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
