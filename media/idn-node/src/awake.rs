//! Keeping the computer awake while services run:
//! a computer going to sleep in the middle of a service stops its stream, its sound and
//! its screens. The screen may still turn off.
//!
//! Windows: `SetThreadExecutionState` on the thread that holds it (the Node's supervise
//! loop, which lives as long as the Node). Linux: a `systemd-inhibit` blocking sleep
//! while held. macOS: `caffeinate -i`.

/// Held or not; `hold` is called each round with whether it should be.
#[derive(Default)]
pub struct Awake {
    held: bool,
    #[cfg(not(windows))]
    child: Option<std::process::Child>,
    /// When it was last started: one that keeps ending at once (no logind in a
    /// container, a polkit refusal) is tried again once a minute, said once.
    #[cfg(not(windows))]
    tried: Option<std::time::Instant>,
    #[cfg(not(windows))]
    said_gone: bool,
}

impl Awake {
    /// Holds the computer awake (`true`) or lets it sleep again; logs the change once.
    pub fn hold(&mut self, on: bool, log: &dyn Fn(&str)) {
        if on == self.held {
            #[cfg(not(windows))]
            if on {
                // The inhibitor gone (killed, logind restarted): taken again.
                let gone = self
                    .child
                    .as_mut()
                    .is_none_or(|c| c.try_wait().ok().flatten().is_some());
                let due = self
                    .tried
                    .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60));
                if gone && due {
                    if !self.said_gone && self.tried.is_some() {
                        self.said_gone = true;
                        log(
                            "keeping the computer awake: the inhibitor keeps ending; tried again each minute",
                        );
                    }
                    self.tried = Some(std::time::Instant::now());
                    self.child = spawn();
                }
            }
            return;
        }
        self.held = on;
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Power::{
                ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
            };
            // SAFETY: plain flags; it affects only this thread's request.
            unsafe {
                SetThreadExecutionState(if on {
                    ES_CONTINUOUS | ES_SYSTEM_REQUIRED
                } else {
                    ES_CONTINUOUS
                });
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            if on {
                self.tried = Some(std::time::Instant::now());
                self.child = spawn();
                if self.child.is_none() {
                    log(
                        "keep awake: couldn't hold the computer awake (no systemd-inhibit or caffeinate)",
                    );
                }
            }
        }
        log(if on {
            "keep awake: services running, the computer won't sleep"
        } else {
            "keep awake: the computer may sleep again"
        });
    }
}

#[cfg(not(windows))]
fn spawn() -> Option<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut c = if cfg!(target_os = "macos") {
        let mut c = Command::new("caffeinate");
        c.arg("-i");
        c
    } else {
        let mut c = Command::new("systemd-inhibit");
        c.args([
            "--what=sleep:idle",
            "--who=IDN Node",
            "--why=IDN services are running",
            "--mode=block",
            "sleep",
            "infinity",
        ]);
        c
    };
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

impl Drop for Awake {
    fn drop(&mut self) {
        self.hold(false, &|_| {});
    }
}
