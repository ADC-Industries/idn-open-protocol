//! A sound card's health while it plays or captures ([`DeviceStatus`]: running, failing,
//! lost, with why), told on change to a callback; and [`probe_device`]: whether a device
//! opens now, why not, and the most channels it offers.

use cpal::traits::DeviceTrait;
use idn_common::sync::LockOk as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A device's state ([`DeviceStatus::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Taking or giving sound, no recent errors.
    Running,
    /// Still running, with errors in the last [`ERROR_RECENT`] (overruns, underruns, a
    /// driver's complaint): sound may break up.
    Failing,
    /// No sound taken or given for [`LOST_AFTER`]: unplugged, taken by another program in
    /// exclusive mode, the driver stopped. Reopen it (or another).
    Lost,
}

/// Errors this recent make a running device [`DeviceState::Failing`].
pub const ERROR_RECENT: Duration = Duration::from_secs(2);
/// No progress for this long makes it [`DeviceState::Lost`].
pub const LOST_AFTER: Duration = Duration::from_secs(1);

/// A device's health, from [`crate::Playback::status`] / [`crate::Capture::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus {
    pub state: DeviceState,
    /// For people: `running`, the last error, or why it counts as lost.
    pub text: String,
    /// Errors the audio system reported since it opened, and the last one's words.
    pub errors: u64,
    pub last_error: Option<String>,
}

/// What a stream keeps for its health: its last error, and when its sound last moved.
#[derive(Default)]
pub(crate) struct Health {
    last_error: Mutex<Option<(Instant, String)>>,
    seen: Mutex<Option<(u64, Instant)>>,
    /// The stream can't go on (its device changed under it): why.
    ended: Mutex<Option<String>>,
}

impl Health {
    /// An error the audio system reported (its words). One that ends the stream (its
    /// device's rate changed, or it was set up again and failed: an ASIO driver's
    /// `StreamInvalidated`) makes it lost for good, not "works again" once errors stop:
    /// its owner opens the device again.
    pub fn error_of(&self, e: &cpal::Error) {
        self.error(e.to_string());
        if e.kind() == cpal::ErrorKind::StreamInvalidated {
            *self.ended.lock_ok() = Some(e.to_string());
        }
    }

    /// Not ended (a stream built after a failed attempt, whose late error said so).
    pub fn clear_ended(&self) {
        *self.ended.lock_ok() = None;
    }

    /// An error the audio system reported (its words).
    pub fn error(&self, text: String) {
        *self.last_error.lock_ok() = Some((Instant::now(), text));
    }

    /// The status from the stream's progress (frames moved so far), its error count, and
    /// whether it ended for good (`ended`: why).
    pub fn status(&self, progress: u64, errors: u64, ended: Option<&str>) -> DeviceStatus {
        let mine = self.ended.lock_ok().clone();
        let now = Instant::now();
        let moved_at = {
            let mut s = self.seen.lock_ok();
            match *s {
                Some((p, at)) if p == progress => at,
                _ => {
                    *s = Some((progress, now));
                    now
                }
            }
        };
        let last = self.last_error.lock_ok().clone();
        let last_error = last.as_ref().map(|(_, t)| t.clone());
        let recent = last
            .as_ref()
            .is_some_and(|(at, _)| now.duration_since(*at) < ERROR_RECENT);
        let stalled = now.duration_since(moved_at);
        // The driver's "ended" counts once the sound has also stopped (an ASIO driver's
        // other stream set up again plays on: no reopen for it; bug run 2026-11-25).
        let ended = ended.or(mine.as_deref().filter(|_| stalled >= LOST_AFTER));
        let (state, text) = if let Some(why) = ended {
            (DeviceState::Lost, format!("ended: {why}"))
        } else if stalled >= LOST_AFTER {
            (
                DeviceState::Lost,
                format!(
                    "no sound for {:.1} s (unplugged, taken by another program, or its driver stopped){}",
                    stalled.as_secs_f64(),
                    last_error
                        .as_deref()
                        .map_or(String::new(), |e| format!("; last error: {e}"))
                ),
            )
        } else if recent {
            (
                DeviceState::Failing,
                last_error.clone().unwrap_or_else(|| "errors".into()),
            )
        } else {
            (DeviceState::Running, "running".into())
        };
        DeviceStatus {
            state,
            text,
            errors,
            last_error,
        }
    }
}

/// Calls `f` with each change of state (and the first status), every 250 ms, while
/// `status` gives one (`None`: the stream is gone, the watch ends).
pub(crate) fn watch(
    status: impl Fn() -> Option<DeviceStatus> + Send + 'static,
    f: impl Fn(&DeviceStatus) + Send + 'static,
) {
    let _ = std::thread::Builder::new()
        .name("idn-audio-health".into())
        .spawn(move || {
            let mut last: Option<DeviceState> = None;
            while let Some(s) = status() {
                if last != Some(s.state) {
                    last = Some(s.state);
                    f(&s);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
}

/// Which way a device is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

/// Why a device doesn't open ([`DeviceProbe::problem`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceProblem {
    /// No such device (unplugged, or the name matches none).
    Missing,
    /// Another program holds it (exclusive mode, an ALSA hardware device in use).
    Busy,
    /// This user may not open it (permissions, the system's privacy settings).
    NoPermission,
    /// Anything else (its words in [`DeviceProbe::text`]).
    Other,
}

/// What [`probe_device`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProbe {
    /// The device found (its name), or the one asked for.
    pub name: String,
    /// It opened now (and was closed again).
    pub opens: bool,
    /// Why not, and the audio system's words.
    pub problem: Option<DeviceProblem>,
    pub text: String,
    /// The most channels it offers that way (0: not known), and how it opens by default.
    pub max_channels: u16,
    pub default: Option<crate::StreamInfo>,
}

/// An error opening a device as a [`DeviceProblem`]: by its kind (busy, no permission,
/// not found), else by its words.
pub(crate) fn problem_of(e: &std::io::Error) -> DeviceProblem {
    match e.kind() {
        std::io::ErrorKind::ResourceBusy => DeviceProblem::Busy,
        std::io::ErrorKind::PermissionDenied => DeviceProblem::NoPermission,
        std::io::ErrorKind::NotFound => DeviceProblem::Missing,
        _ => classify(&e.to_string()),
    }
}

/// The audio system's words as a [`DeviceProblem`].
pub(crate) fn classify(text: &str) -> DeviceProblem {
    let t = text.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if any(&["busy", "in use", "exclusive", "ebusy"]) {
        DeviceProblem::Busy
    } else if any(&["permission", "denied", "eacces", "not permitted", "privacy"]) {
        DeviceProblem::NoPermission
    } else if any(&[
        "not found",
        "no such",
        "not available",
        "no default",
        "enodev",
        "unplugged",
    ]) {
        DeviceProblem::Missing
    } else {
        DeviceProblem::Other
    }
}

/// Whether the device `name` (`None`: the default one) opens now `dir`: it is opened in
/// its default format as [`crate::Playback`] or [`crate::Capture`] would open it, and
/// closed again without starting (nothing played or kept). With why not, and its most
/// channels. Probes run one at a time (two at once on one card, through different
/// names, saw each other as busy).
pub fn probe_device(name: Option<&str>, dir: Direction) -> DeviceProbe {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _one = ONE_AT_A_TIME.lock_ok();
    let input = dir == Direction::Input;
    let failed = |name: String, e: &std::io::Error| DeviceProbe {
        name,
        opens: false,
        problem: Some(problem_of(e)),
        text: e.to_string(),
        max_channels: 0,
        default: None,
    };
    let device = match crate::find_device(name, input) {
        Ok(d) => d,
        Err(e) => return failed(name.unwrap_or("").to_owned(), &e),
    };
    let device_name = crate::name_of(&device);
    let config = if input {
        device.default_input_config()
    } else {
        device.default_output_config()
    };
    let config = match config {
        Ok(c) => c,
        Err(e) => return failed(device_name, &crate::cpal_err(e)),
    };
    let max_channels = if input {
        device
            .supported_input_configs()
            .map(|c| c.map(|r| r.channels()).max().unwrap_or(0))
    } else {
        device
            .supported_output_configs()
            .map(|c| c.map(|r| r.channels()).max().unwrap_or(0))
    }
    .unwrap_or(0);
    let default = crate::info(Ok(config));
    let (fmt, sc) = (config.sample_format(), config.config());
    #[cfg(all(any(windows, target_os = "linux"), feature = "asio"))]
    let running = crate::asio_host::is_running(&device_name);
    #[cfg(not(all(any(windows, target_os = "linux"), feature = "asio")))]
    let running = false;
    let open = if running {
        Ok(())
    } else if input {
        device
            .build_input_stream_raw(sc, fmt, |_: &cpal::Data, _: &_| {}, |_| {}, None)
            .map(drop)
    } else {
        device
            .build_output_stream_raw(sc, fmt, |_: &mut cpal::Data, _: &_| {}, |_| {}, None)
            .map(drop)
    };
    let (opens, problem, text) = match open.map_err(crate::cpal_err) {
        // (Not opened: an ASIO driver running here answers from what it offers.)
        Ok(()) if running => (true, None, "running here".to_owned()),
        Ok(()) => (true, None, "opens".to_owned()),
        Err(e) => (false, Some(problem_of(&e)), e.to_string()),
    };
    DeviceProbe {
        name: device_name,
        opens,
        problem,
        text,
        max_channels: max_channels.max(default.as_ref().map_or(0, |d| d.channels)),
        default,
    }
}

/// Shared between a stream's error callback and its owner.
pub(crate) type SharedHealth = Arc<Health>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_failing_lost() {
        let h = Health::default();
        assert_eq!(h.status(0, 0, None).state, DeviceState::Running);
        assert_eq!(h.status(100, 0, None).state, DeviceState::Running);
        h.error("ALSA: underrun".into());
        let s = h.status(200, 1, None);
        assert_eq!(
            (s.state, s.text.as_str()),
            (DeviceState::Failing, "ALSA: underrun")
        );
        // No progress for longer than LOST_AFTER: lost.
        std::thread::sleep(LOST_AFTER + Duration::from_millis(50));
        let s = h.status(200, 1, None);
        assert_eq!(s.state, DeviceState::Lost, "{s:?}");
        assert!(s.text.contains("last error: ALSA: underrun"), "{}", s.text);
        assert_eq!(
            h.status(300, 1, Some("the program exited")).state,
            DeviceState::Lost
        );
        assert_eq!(classify("Device or resource busy"), DeviceProblem::Busy);
        assert_eq!(classify("Permission denied"), DeviceProblem::NoPermission);
        assert_eq!(classify("no default audio device"), DeviceProblem::Missing);
        assert_eq!(classify("something else"), DeviceProblem::Other);
    }

    /// A device that isn't there: missing, said so.
    #[test]
    fn a_device_that_isnt_there() {
        let p = probe_device(Some("no such sound card at all 7f3a"), Direction::Output);
        assert!(!p.opens);
        assert_eq!(p.problem, Some(DeviceProblem::Missing), "{p:?}");
    }
}
