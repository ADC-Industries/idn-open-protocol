//! Stream inputs (spec/extensions/idn-switcher.md §S.22): each stream is taken in by
//! `idn-codec ingest`, its own program next to the switcher, as an ordinary source of the
//! site ("HOST (NAME)"), which any receiver can use; the switcher takes it from its fixed
//! local port like any input. The program is started again if it ends, and stops when the
//! switcher does (its standard input closes, `--parent-pipe`).

use idn_common::sync::LockOk as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `idn-codec` in IDN's programs folder (this program's, or the one a program hosting
/// IDN set).
pub fn ingest_exe() -> PathBuf {
    let name = if cfg!(windows) {
        "idn-codec.exe"
    } else {
        "idn-codec"
    };
    idn_common::supervisor::programs_dir()
        .map(|d| d.join(name))
        .unwrap_or_else(|| name.into())
}

/// How a stream input is doing, from its program's `status` lines.
#[derive(Clone, Debug, Default)]
pub struct StreamStatus {
    /// "connecting", "receiving", "waiting: …", or "stopped: …" when the program ended.
    pub state: String,
    /// The video, e.g. "h264 1920x1080".
    pub video: Option<String>,
    /// Pictures taken in.
    pub pictures: u64,
    /// Times the stream was opened again.
    pub reconnects: u64,
    /// Receivers of its source (the switcher among them).
    pub receivers: u64,
}

struct Running {
    child: Child,
    _pipe: Option<ChildStdin>,
}

struct Inner {
    exe: PathBuf,
    url: String,
    name: String,
    port: u16,
    status: Mutex<StreamStatus>,
    running: Mutex<Option<Running>>,
    stop: AtomicBool,
}

/// One stream input's program, kept running; stopped when dropped.
pub struct StreamIn {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StreamIn {
    /// Starts taking `url` in as the source `name` with the program `exe`.
    pub fn start(exe: &Path, url: &str, name: &str) -> std::io::Result<StreamIn> {
        // A fixed port, so the input finds it again after a restart: one the system never
        // hands out by itself, so no other socket takes it between restarts.
        let port = idn_common::net::kept_port();
        let inner = Arc::new(Inner {
            exe: exe.to_owned(),
            url: url.to_owned(),
            name: name.to_owned(),
            port,
            status: Mutex::new(StreamStatus {
                state: "connecting".into(),
                ..Default::default()
            }),
            running: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        spawn(&inner)?;
        let thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("idn-switcher-stream".into())
                .spawn(move || {
                    while !inner.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(500));
                        let ended = {
                            let mut r = inner.running.lock_ok();
                            match r.as_mut().map(|r| r.child.try_wait()) {
                                Some(Ok(Some(_))) | Some(Err(_)) => {
                                    *r = None;
                                    true
                                }
                                Some(Ok(None)) => false,
                                None => true,
                            }
                        };
                        if ended && !inner.stop.load(Ordering::Relaxed) {
                            // Again after 3 s (a stream that keeps failing waits between),
                            // the wait ended by a stop (a removal doesn't wait it out).
                            idn_open_protocol::audio::pace::sleep_unless_stopped(
                                Duration::from_secs(3),
                                &inner.stop,
                            );
                            if !inner.stop.load(Ordering::Relaxed)
                                && let Err(e) = spawn(&inner)
                            {
                                inner.status.lock_ok().state = format!("stopped: {e}");
                            }
                        }
                    }
                })
                .expect("a thread")
        };
        Ok(StreamIn {
            inner,
            thread: Some(thread),
        })
    }

    /// Where the switcher takes it from.
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.inner.port)
    }

    /// How it is doing.
    pub fn status(&self) -> StreamStatus {
        self.inner.status.lock_ok().clone()
    }
}

fn spawn(inner: &Arc<Inner>) -> std::io::Result<()> {
    let mut child = Command::new(&inner.exe)
        .args(["ingest", &inner.url, "--name", &inner.name, "--port"])
        .arg(inner.port.to_string())
        .args(["--status", "--parent-pipe"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", inner.exe.display())))?;
    if let Some(err) = child.stderr.take() {
        let inner = inner.clone();
        std::thread::spawn(move || {
            let mut last = String::new();
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                match line
                    .strip_prefix("status ")
                    .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                {
                    Some(v) => {
                        let mut s = inner.status.lock_ok();
                        s.state = v["state"].as_str().unwrap_or_default().to_owned();
                        s.video = v["video"].as_str().map(str::to_owned);
                        s.pictures = v["pictures"].as_u64().unwrap_or(0);
                        s.reconnects = v["reconnects"].as_u64().unwrap_or(0);
                        s.receivers = v["receivers"].as_u64().unwrap_or(0);
                    }
                    None if !line.trim().is_empty() => last = line,
                    None => {}
                }
            }
            // The program ended: say why (its last line).
            if !inner.stop.load(Ordering::Relaxed) {
                let why = last.trim_start_matches("idn-codec: ").trim();
                inner.status.lock_ok().state = if why.is_empty() {
                    "stopped".into()
                } else {
                    format!("stopped: {why}")
                };
            }
        });
    }
    let pipe = child.stdin.take();
    *inner.running.lock_ok() = Some(Running { child, _pipe: pipe });
    Ok(())
}

impl Drop for StreamIn {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(mut r) = self.inner.running.lock_ok().take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A stand-in for idn-codec: prints two status lines, then ends with a reason.
    #[test]
    fn status_lines_and_restart() {
        let dir = idn_common::testing::TempDir::new("idn-sw-streams");
        let exe = dir.join("fake-codec");
        let count = dir.join("starts");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\necho x >> {c}\n\
                 echo 'status {{\"state\":\"receiving\",\"video\":\"h264 1280x720\",\"pictures\":25,\"reconnects\":1,\"receivers\":1}}' >&2\n\
                 sleep 1\necho 'idn-codec: the stream ended' >&2\n",
                c = count.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = StreamIn::start(&exe, "srt://:9000?mode=listener", "Stream 1").unwrap();
        assert!(s.address().starts_with("127.0.0.1:"));
        std::thread::sleep(Duration::from_millis(500));
        let st = s.status();
        assert_eq!(st.state, "receiving");
        assert_eq!(st.video.as_deref(), Some("h264 1280x720"));
        assert_eq!((st.pictures, st.reconnects, st.receivers), (25, 1, 1));
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(s.status().state, "stopped: the stream ended");
        // Started again 3 s after it ended.
        std::thread::sleep(Duration::from_secs(4));
        let starts = std::fs::read_to_string(&count).unwrap().lines().count();
        assert!(starts >= 2, "started {starts} times");
        drop(s);
    }
}
