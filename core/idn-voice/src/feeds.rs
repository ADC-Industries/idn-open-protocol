//! A channel's programs, `idn-codec` started next to the Site Manager and again if they
//! end, each with a token of its own on the WebSocket (§V.9, §V.10):
//! - an IEM channel's feed (`voice-feed SOURCE`): its source's sound, always;
//! - a Walkie-Talkie channel's talk source (`voice-feed SOURCE --vox ID`): talking while
//!   it has sound (the desk's talkback);
//! - a channel made an IDN source (`voice-out --name "Voice NAME"`): listening, its
//!   talkers mixed as a source's sound.

use crate::VoiceConfig;
use crate::hub::Program;
use crate::model::{Channel, Kind};
use idn_common::backoff::Backoff;
use idn_common::sync::LockOk as _;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One program: its arguments (what it is), token, stop and state.
struct Prog {
    args: Vec<String>,
    token: String,
    name: String,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<String>>,
}

pub struct Feeds {
    cfg: Arc<VoiceConfig>,
    progs: Mutex<HashMap<(u32, Key), Prog>>,
}

/// The program kinds, as map keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Feed,
    Vox,
    Out,
}

impl Key {
    fn program(self) -> Program {
        match self {
            Key::Feed => Program::Feed,
            Key::Vox => Program::Vox,
            Key::Out => Program::Out,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Key::Feed => "feed",
            Key::Vox => "talk source",
            Key::Out => "as a source",
        }
    }
}

/// The programs a channel asks for: (kind, arguments, its name on the WebSocket).
fn wanted(c: &Channel) -> Vec<(Key, Vec<String>, String)> {
    let mut v = Vec::new();
    match &c.kind {
        Kind::Iem { source } => v.push((
            Key::Feed,
            vec!["voice-feed".into(), source.clone()],
            format!("{source} (feed)"),
        )),
        Kind::Walkie { .. } => {
            if let Some(s) = c.talk_source.as_deref().filter(|s| !s.trim().is_empty()) {
                v.push((
                    Key::Vox,
                    vec![
                        "voice-feed".into(),
                        s.to_owned(),
                        "--vox".into(),
                        c.id.to_string(),
                    ],
                    s.to_owned(),
                ));
            }
        }
    }
    if c.as_source {
        let name = format!("Voice {}", c.name);
        v.push((
            Key::Out,
            vec!["voice-out".into(), "--name".into(), name.clone()],
            format!("{name} (source)"),
        ));
    }
    v
}

impl Feeds {
    pub fn new(cfg: Arc<VoiceConfig>) -> Arc<Feeds> {
        Arc::new(Feeds {
            cfg,
            progs: Mutex::new(HashMap::new()),
        })
    }

    /// The channel, name and kind a program's token is for.
    pub fn channel_of(&self, token: &str) -> Option<(u32, String, Program)> {
        self.progs
            .lock_ok()
            .iter()
            .find(|(_, p)| {
                idn_open_protocol::trust::proto::same(p.token.as_bytes(), token.as_bytes())
            })
            .map(|((c, k), p)| (*c, p.name.clone(), k.program()))
    }

    /// How channel `c`'s programs are doing, for the administrators' list.
    pub fn state(&self, c: u32) -> Option<String> {
        let m = self.progs.lock_ok();
        let list: Vec<String> = m
            .iter()
            .filter(|((ch, _), _)| *ch == c)
            .map(|((_, k), p)| format!("{}: {}", k.label(), p.state.lock_ok()))
            .collect();
        (!list.is_empty()).then(|| list.join("; "))
    }

    /// The programs the channels ask for: started, stopped, started again when changed.
    pub fn sync(&self, channels: &[Channel]) {
        let want: HashMap<(u32, Key), (Vec<String>, String)> = channels
            .iter()
            .flat_map(|c| {
                wanted(c)
                    .into_iter()
                    .map(move |(k, a, n)| ((c.id, k), (a, n)))
            })
            .collect();
        let mut m = self.progs.lock_ok();
        m.retain(|key, p| {
            let keep = want.get(key).is_some_and(|(a, _)| *a == p.args);
            if !keep {
                p.stop.store(true, Ordering::Relaxed);
            }
            keep
        });
        for (key, (args, name)) in want {
            if m.contains_key(&key) {
                continue;
            }
            let p = Prog {
                args: args.clone(),
                token: idn_open_protocol::random::token_hex(20),
                name,
                stop: Arc::new(AtomicBool::new(false)),
                state: Arc::new(Mutex::new("starting".into())),
            };
            match (&self.cfg.codec, &self.cfg.feed_url) {
                (Some(exe), Some(url)) => {
                    let mut full = args;
                    full.extend(["--url".to_owned(), url.clone()]);
                    let (exe, token) = (exe.clone(), p.token.clone());
                    let (stop, state) = (p.stop.clone(), p.state.clone());
                    let _ = std::thread::Builder::new()
                        .name("idn-voice-feed".into())
                        .spawn(move || supervise(&exe, &full, &token, &stop, &state));
                }
                _ => *p.state.lock_ok() = "needs idn-codec on this machine".into(),
            }
            m.insert(key, p);
        }
    }
}

impl Drop for Feeds {
    /// Voice let go: its programs end with it.
    fn drop(&mut self) {
        for p in self.progs.lock_ok().values() {
            p.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// Runs a program until stopped, starting it again 3 s after it ends.
fn supervise(
    exe: &std::path::Path,
    args: &[String],
    token: &str,
    stop: &AtomicBool,
    state: &Mutex<String>,
) {
    // (Its last error line kept for the state, and longer waits while it keeps failing, 3 s
    // doubling to a minute, again 3 s after a long run: a refused feed started every 3 s
    // for good, saying only its exit status; bug run B.)
    let mut backoff = Backoff::new(Duration::from_secs(3), Duration::from_secs(60));
    let mut wait = Duration::from_secs(3);
    while !stop.load(Ordering::Relaxed) {
        let child = Command::new(exe)
            .args(args)
            .env("IDN_VOICE_FEED_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child: Child = match child {
            Ok(c) => c,
            Err(e) => {
                *state.lock_ok() = format!("can't start idn-codec: {e}");
                idn_open_protocol::audio::pace::sleep_unless_stopped(Duration::from_secs(10), stop);
                continue;
            }
        };
        *state.lock_ok() = "running".into();
        let started = std::time::Instant::now();
        let last_line = std::sync::Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let l = last_line.clone();
            let _ = std::thread::Builder::new()
                .name("idn-voice-feed-err".into())
                .spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                        if !line.trim().is_empty() {
                            *l.lock_ok() = line.chars().take(300).collect();
                        }
                    }
                });
        }
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            match child.try_wait() {
                Ok(Some(st)) => {
                    std::thread::sleep(Duration::from_millis(100));
                    let why = last_line.lock_ok().clone();
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff.reset();
                    }
                    wait = backoff.next();
                    *state.lock_ok() = if why.is_empty() {
                        format!("ended ({st}); starting again in {} s", wait.as_secs())
                    } else {
                        format!(
                            "ended ({st}): {why}; starting again in {} s",
                            wait.as_secs()
                        )
                    };
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(500)),
                Err(_) => break,
            }
        }
        idn_open_protocol::audio::pace::sleep_unless_stopped(wait, stop);
    }
}
