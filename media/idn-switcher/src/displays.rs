//! Screens (spec/extensions/idn-display.md §D.3): an output of the switcher full screen on
//! a display of its machine — a projector, a confidence monitor — each through its own
//! `idn-display` process (next to this program), started again if it stops.

use idn_common::backoff::Backoff;
use idn_common::cmd::RunWithin as _;
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One screen and what it shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    /// The screen: its number from 1 or name (`--list-screens`), `kms:CONNECTOR` for a
    /// display with no desktop on Linux, or `vcam`: the virtual camera (a webcam).
    pub screen: String,
    /// The output's full name (`HOST (Switcher)`), as [`crate::Switcher::outputs`] gives it.
    pub output: String,
    /// Its sound on this sound card (`Some("")`: the default), or none.
    pub audio: Option<String>,
}

impl Screen {
    pub fn to_json(&self) -> Value {
        json!({"screen": self.screen, "output": self.output, "audio": self.audio})
    }

    pub fn from_json(v: &Value) -> Option<Screen> {
        Some(Screen {
            screen: v["screen"].as_str().filter(|s| !s.is_empty())?.to_owned(),
            output: v["output"].as_str().filter(|s| !s.is_empty())?.to_owned(),
            audio: v["audio"].as_str().map(str::to_owned),
        })
    }
}

/// Where an output is on this machine, by its full name (`None`: not here now).
pub type Resolve = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct Running {
    child: Child,
    /// Its standard input, held open: when this process ends, however it ends, the pipe
    /// closes and the display stops (`--parent-pipe`).
    _pipe: Option<std::process::ChildStdin>,
    /// What it was started with (the screen, the output's address, the sound card).
    with: (Screen, String),
    /// Its last line on standard error.
    said: Arc<Mutex<String>>,
    /// When it was started.
    started: Instant,
}

struct Inner {
    exe: PathBuf,
    resolve: Resolve,
    want: Mutex<Vec<Screen>>,
    running: Mutex<BTreeMap<String, Running>>,
    /// When each screen's last start failed, how long it waits before trying again, and
    /// why.
    failed: Mutex<BTreeMap<String, (Instant, Duration, String)>>,
    /// Each screen's waits before a start again: 3 s, growing while its display keeps
    /// stopping; back to 3 s after a display that ran a minute (locked after `failed`).
    waits: Mutex<BTreeMap<String, Backoff>>,
    stop: AtomicBool,
}

/// The screens, kept showing what they were given; stops their processes when dropped.
pub struct Displays {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// `idn-display` in IDN's programs folder (this program's, or the one a program hosting
/// IDN set).
pub fn display_exe() -> PathBuf {
    let name = if cfg!(windows) {
        "idn-display.exe"
    } else {
        "idn-display"
    };
    idn_common::supervisor::programs_dir()
        .map(|d| d.join(name))
        .unwrap_or_else(|| name.into())
}

impl Displays {
    /// Starts looking after the screens; `resolve` finds an output's address.
    pub fn start(exe: PathBuf, resolve: Resolve) -> Displays {
        let inner = Arc::new(Inner {
            exe,
            resolve,
            want: Mutex::new(Vec::new()),
            running: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(BTreeMap::new()),
            waits: Mutex::new(BTreeMap::new()),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("idn-switcher-screens".into())
                .spawn(move || {
                    while !inner.stop.load(Ordering::Relaxed) {
                        tend(&inner);
                        std::thread::sleep(Duration::from_millis(500));
                    }
                })
                .expect("a thread")
        };
        Displays {
            inner,
            thread: Some(thread),
        }
    }

    /// What the screens show from now on (one entry a screen; screens not named go dark).
    pub fn set(&self, mut screens: Vec<Screen>) {
        let mut seen = std::collections::BTreeSet::new();
        screens.retain(|s| seen.insert(s.screen.clone()));
        *self.inner.want.lock_ok() = screens;
        self.inner.failed.lock_ok().clear();
        self.inner.waits.lock_ok().clear();
        tend(&self.inner);
    }

    pub fn screens(&self) -> Vec<Screen> {
        self.inner.want.lock_ok().clone()
    }

    /// Each screen given an output: whether its display runs, and what went wrong.
    pub fn status(&self) -> Value {
        let running = self.inner.running.lock_ok();
        let failed = self.inner.failed.lock_ok();
        Value::Array(
            self.inner
                .want
                .lock_ok()
                .iter()
                .map(|s| {
                    let r = running.get(&s.screen);
                    let error = failed
                        .get(&s.screen)
                        .map(|(_, _, e)| e.clone())
                        .or_else(|| {
                            (self.inner.resolve)(&s.output)
                                .is_none()
                                .then(|| "the output isn't here now".to_owned())
                        })
                        .unwrap_or_default();
                    json!({
                        "screen": s.screen, "output": s.output, "audio": s.audio,
                        "running": r.is_some(),
                        "error": error,
                    })
                })
                .collect(),
        )
    }

    /// The machine's screens, as `idn-display --list-screens` reports them.
    pub fn list(&self) -> Result<Value, String> {
        let out = Command::new(&self.inner.exe)
            .arg("--list-screens")
            .stderr(Stdio::null())
            .output_within(std::time::Duration::from_secs(10))
            .map_err(|e| format!("{}: {e}", self.inner.exe.display()))?;
        serde_json::from_slice(&out.stdout).map_err(|_| "idn-display listed no screens".into())
    }
}

/// Brings the processes in line with what is wanted: stops those no longer wanted or
/// changed, starts those missing (not within the screen's wait after a failed start: 3 s,
/// growing to a minute).
fn tend(inner: &Inner) {
    let want = inner.want.lock_ok().clone();
    let mut running = inner.running.lock_ok();
    let mut failed = inner.failed.lock_ok();
    let mut waits = inner.waits.lock_ok();
    let mut wait_for = |screen: &str, ran: Option<Duration>| {
        let b = waits
            .entry(screen.to_owned())
            .or_insert_with(screen_backoff);
        if ran.is_some_and(|r| r >= STEADY) {
            b.reset();
        }
        b.next()
    };
    // Ended by themselves: their last words are why.
    let ended: Vec<String> = running
        .iter_mut()
        .filter_map(|(k, r)| r.child.try_wait().ok().flatten().map(|_| k.clone()))
        .collect();
    for k in ended {
        if let Some(r) = running.remove(&k) {
            let said = r.said.lock_ok().clone();
            let wait = wait_for(&k, Some(r.started.elapsed()));
            failed.insert(
                k,
                (
                    Instant::now(),
                    wait,
                    if said.is_empty() {
                        "the display stopped".into()
                    } else {
                        said
                    },
                ),
            );
        }
    }
    for s in &want {
        let address = (inner.resolve)(&s.output);
        let current = running.get(&s.screen).map(|r| &r.with);
        let wanted = address.clone().map(|a| (s.clone(), a));
        if current == wanted.as_ref() {
            continue;
        }
        if let Some(mut r) = running.remove(&s.screen) {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
        let Some((s, address)) = wanted else {
            continue;
        };
        if failed
            .get(&s.screen)
            .is_some_and(|(t, w, _)| t.elapsed() < *w)
        {
            continue;
        }
        let mut cmd = Command::new(&inner.exe);
        cmd.arg(&address)
            .arg("--parent-pipe")
            .arg("--name")
            .arg(format!("Screen {}", s.screen));
        match s.screen.strip_prefix("kms:") {
            Some(c) => cmd.arg("--kms").arg(c),
            // A webcam for Zoom, Teams, browsers (idn-display §D.5).
            None if s.screen == "vcam" => cmd.arg("--virtual-camera"),
            None => cmd.arg("--screen").arg(&s.screen),
        };
        if let Some(a) = &s.audio {
            cmd.arg("--audio")
                .arg(if a.is_empty() { "default" } else { a });
        }
        match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                let pipe = child.stdin.take();
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
                failed.remove(&s.screen);
                running.insert(
                    s.screen.clone(),
                    Running {
                        child,
                        _pipe: pipe,
                        with: (s, address),
                        said,
                        started: Instant::now(),
                    },
                );
            }
            Err(e) => {
                let wait = wait_for(&s.screen, None);
                failed.insert(
                    s.screen.clone(),
                    (
                        Instant::now(),
                        wait,
                        format!("{}: {e}", inner.exe.display()),
                    ),
                );
            }
        }
    }
    // Screens no longer wanted go dark.
    let gone: Vec<String> = running
        .keys()
        .filter(|k| !want.iter().any(|s| &s.screen == *k))
        .cloned()
        .collect();
    for k in gone {
        if let Some(mut r) = running.remove(&k) {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }
    failed.retain(|k, _| want.iter().any(|s| &s.screen == k));
    waits.retain(|k, _| want.iter().any(|s| &s.screen == k));
}

/// A display that ran this long before it stopped was working: its next wait is the
/// first again.
const STEADY: Duration = Duration::from_secs(60);

/// A screen's waits before its display is started again: 3 s, doubling, up to a minute.
fn screen_backoff() -> Backoff {
    Backoff::new(Duration::from_secs(3), Duration::from_secs(60))
}

impl Drop for Displays {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for (_, mut r) in std::mem::take(&mut *self.inner.running.lock_ok()) {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }
}
