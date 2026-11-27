//! The screens' processes: `idn-display --commands --screen NAME`, next to this program,
//! spoken to by JSON lines on their standard input and output; the screens listed by
//! `idn-display --list-screens`.

use super::{ChildLink, Driver};
use crate::proto::{FromChild, ScreenInfo, ToChild};
use idn_common::cmd::RunWithin as _;
use idn_common::sync::LockOk as _;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

/// `idn-display` itself.
pub struct Programs {
    exe: PathBuf,
    /// No desktop (Linux, §D.8.9): the cards, held by the service, each screen's process
    /// handed a lease of its connector.
    #[cfg(target_os = "linux")]
    kms: Mutex<Option<crate::kms::Lessor>>,
}

impl Programs {
    /// This program (the service is `idn-display --service`).
    pub fn next_to_me() -> Programs {
        Programs {
            exe: std::env::current_exe().unwrap_or_else(|_| "idn-display".into()),
            #[cfg(target_os = "linux")]
            kms: Mutex::new(None),
        }
    }
}

/// The screens of `idn-display --list-screens`'s JSON.
pub fn parse_list(v: &serde_json::Value) -> Vec<ScreenInfo> {
    v["screens"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| ScreenInfo {
            name: s["name"].as_str().unwrap_or_default().to_owned(),
            width: s["width"].as_u64().unwrap_or(0) as u32,
            height: s["height"].as_u64().unwrap_or(0) as u32,
            refresh_hz: s["refresh_mhz"]
                .as_f64()
                .map_or(0.0, |m| (m / 10.0).round() / 100.0),
            x: s["x"].as_i64().unwrap_or(0) as i32,
            y: s["y"].as_i64().unwrap_or(0) as i32,
            modes: serde_json::from_value(s["modes"].clone()).unwrap_or_default(),
        })
        // A screen without a name is found by its number only: named by it.
        .enumerate()
        .map(|(k, mut s)| {
            if s.name.is_empty() {
                s.name = format!("{}", k + 1);
            }
            s
        })
        .collect()
}

impl Driver for Programs {
    fn list(&self) -> Result<Vec<ScreenInfo>, String> {
        #[cfg(target_os = "linux")]
        if let Some(k) = self.kms.lock_ok().as_mut() {
            return Ok(k.list());
        }
        let out = Command::new(&self.exe)
            .arg("--list-screens")
            .stderr(Stdio::null())
            .output_within(std::time::Duration::from_secs(10))
            .map_err(|e| format!("{}: {e}", self.exe.display()))?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|_| "idn-display listed no screens".to_owned())?;
        let l = parse_list(&v);
        if l.is_empty() && v.get("kms").is_some() {
            // No desktop: the screens driven directly, the cards held from now on.
            #[cfg(target_os = "linux")]
            {
                let mut k = crate::kms::Lessor::open()
                    .map_err(|e| format!("no desktop on this computer, and {e}"))?;
                eprintln!("idn-display: no desktop: the screens are driven directly (KMS)");
                let l = k.list();
                *self.kms.lock_ok() = Some(k);
                return Ok(l);
            }
            #[cfg(not(target_os = "linux"))]
            return Err("no desktop on this computer".into());
        }
        Ok(l)
    }

    fn sound_cards(&self) -> Option<Vec<serde_json::Value>> {
        let l = idn_audio::devices_known().ok()?;
        Some(
            l.into_iter()
                .filter_map(|d| {
                    let o = d.output.as_ref()?;
                    Some(serde_json::json!({
                        "name": d.name,
                        "id": d.id,
                        "default": d.default_output,
                        "channels": o.channels,
                    }))
                })
                .collect(),
        )
    }

    fn start(&self, screen: &ScreenInfo) -> Result<Box<dyn ChildLink>, String> {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--commands")
            .arg("--screen")
            .arg(&screen.name)
            .arg("--name")
            .arg(format!("Display {}", screen.name))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // No desktop: its connector's lease, open in the process at the same number (only
        // that process gets it; the service's copy is closed once it has started).
        #[cfg(target_os = "linux")]
        let _lease = match self.kms.lock_ok().as_mut() {
            Some(k) => {
                use std::os::fd::AsRawFd as _;
                use std::os::unix::process::CommandExt as _;
                let fd = k.lease(&screen.name)?;
                let raw = fd.as_raw_fd();
                cmd.arg("--drm-fd").arg(raw.to_string());
                // SAFETY: only fcntl (async-signal-safe) between fork and exec.
                unsafe {
                    cmd.pre_exec(move || {
                        let fl = libc::fcntl(raw, libc::F_GETFD);
                        if fl < 0 || libc::fcntl(raw, libc::F_SETFD, fl & !libc::FD_CLOEXEC) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                Some(fd)
            }
            None => None,
        };
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", self.exe.display()))?;
        // Ends with this program however it ends (Windows: in its job).
        #[cfg(windows)]
        idn_common::supervisor::in_job(&child);
        let stdin = child.stdin.take();
        let (tx, rx) = channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Ok(m) = serde_json::from_str::<FromChild>(&line)
                        && tx.send(m).is_err()
                    {
                        break;
                    }
                }
            });
        }
        let said = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let said = said.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if let Some(l) = line.strip_prefix("idn-display: ") {
                        *said.lock_ok() = l.to_owned();
                    }
                }
            });
        }
        Ok(Box::new(Proc {
            child: Some(child),
            stdin,
            rx,
            said,
        }))
    }
}

struct Proc {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: Receiver<FromChild>,
    said: Arc<Mutex<String>>,
}

impl ChildLink for Proc {
    fn send(&mut self, m: &ToChild) -> Result<(), String> {
        let line = serde_json::to_string(m).map_err(|e| e.to_string())?;
        let w = self
            .stdin
            .as_mut()
            .ok_or("the screen's process has no input")?;
        writeln!(w, "{line}")
            .and_then(|_| w.flush())
            .map_err(|e| e.to_string())
    }

    fn recv(&mut self) -> Vec<FromChild> {
        self.rx.try_iter().collect()
    }

    fn ended(&mut self) -> Option<String> {
        let status = self.child.as_mut()?.try_wait().ok().flatten()?;
        let said = self.said.lock_ok().clone();
        Some(if said.is_empty() {
            format!("the screen's process ended ({status})")
        } else {
            format!("the screen's process ended ({status}); it last said: {said}")
        })
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Its input closed: it ends the normal way; killed if it hasn't within a second
        // (waited for on a thread of its own: the service doesn't wait).
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                for _ in 0..10 {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_screen_list_is_read() {
        let v = serde_json::json!({"screens": [
            {"screen": 1, "name": "DP-2", "width": 1920, "height": 1080, "refresh_mhz": 59940, "x": 0, "y": 0},
            {"screen": 2, "name": "", "width": 3840, "height": 2160, "refresh_mhz": 30000, "x": 1920, "y": 0}
        ]});
        let l = super::parse_list(&v);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].name.as_str(), l[0].refresh_hz), ("DP-2", 59.94));
        assert_eq!((l[1].name.as_str(), l[1].x, l[1].width), ("2", 1920, 3840));
    }
}
