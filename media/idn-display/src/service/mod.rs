//! The display service (idn-display.md §D.8): one per machine, run by the IDN Node in the
//! desktop session (`apps.display`), a screen wall's foundation (the UI is the client's).
//! It keeps each screen's feed and settings (by the screen's name, in `screens.json`),
//! runs an `idn-display --commands` for each screen given a feed (started again if it
//! stops), tells it what to show, and serves the API: the state, typed commands,
//! thumbnails, the push channel with typed events, the audit. On this machine the API is
//! open; on the network (HTTPS) it takes the site's tokens for the engine `display` only.

mod api;
mod child;
mod run;
mod state;
mod typed;

use state::event_words;
#[cfg(test)]
use state::size_and_drop_problems;
use state::transitions;

pub use typed::{Command, ScreenRef};

use crate::proto::{FromChild, Idle, ScreenInfo, Settings, Show, State, Status, ToChild};
use idn_common::sync::LockOk as _;
use idn_open_protocol::web::audit::Audit;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A thumbnail as it came back: when, and the JPEG or why not.
type Thumb = (Instant, Result<Vec<u8>, String>);

/// The screens' saved feeds and settings.
pub const FILE: &str = "screens.json";
/// How often the screens are listed again (a screen plugged in or out).
const LIST_EVERY: Duration = Duration::from_secs(10);
/// A screen's process that stopped is started again after this.
const RESTART_AFTER: Duration = Duration::from_secs(3);
/// Typed events kept for the push channels.
const EVENTS_KEPT: usize = 256;
/// The service's own settings (the network API on or off).
pub const SERVICE_FILE: &str = "service.json";
/// How often the sound cards are listed again.
const CARDS_EVERY: Duration = Duration::from_secs(60);
/// The window dropped pictures are counted over, and how many in it make a problem.
const DROP_WINDOW: Duration = Duration::from_secs(60);
const DROPS_A_PROBLEM: u64 = 60;

/// Starts and lists the screens' processes (the real one runs `idn-display`; the tests'
/// is in memory).
pub trait Driver: Send + Sync {
    /// The machine's screens, in the desktop's order.
    fn list(&self) -> Result<Vec<ScreenInfo>, String>;
    /// Starts the process for `screen`.
    fn start(&self, screen: &ScreenInfo) -> Result<Box<dyn ChildLink>, String>;
    /// The machine's sound cards that play (`{name, id, default, channels}`); `None`: not
    /// listed here.
    fn sound_cards(&self) -> Option<Vec<Value>> {
        None
    }
}

/// One screen's process.
pub trait ChildLink: Send {
    fn send(&mut self, m: &ToChild) -> Result<(), String>;
    /// What it said since last asked.
    fn recv(&mut self) -> Vec<FromChild>;
    /// Whether it has ended, and why (its last words).
    fn ended(&mut self) -> Option<String>;
}

/// A screen's saved feed and settings.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub feed: String,
    #[serde(default)]
    pub settings: Settings,
    /// The screen's name for people (`Stage left`); empty: its own name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Who set the feed (as the audit says them), and when (seconds since 1970).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub feed_by: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub feed_at: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// A screen's running process.
struct Running {
    link: Box<dyn ChildLink>,
    /// What it was last sent: the feed, the settings (files resolved).
    sent_feed: Option<String>,
    sent_settings: Option<Settings>,
    /// Its screen and GPU once its window is up.
    ready: Option<(ScreenInfo, String)>,
    status: Option<Status>,
    /// The test pattern as last sent.
    sent_test: bool,
    /// Its dropped-picture counter over the last minute: (when, the count).
    drops: VecDeque<(Instant, u64)>,
}

#[derive(Default)]
struct Inner {
    /// Bumped at every change worth sending (not the figures).
    version: u64,
    screens: Vec<ScreenInfo>,
    listed_at: Option<Instant>,
    list_error: Option<String>,
    saved: BTreeMap<String, Saved>,
    running: BTreeMap<String, Running>,
    /// A screen's last status kept after its process ended (and why it ended).
    last: BTreeMap<String, Status>,
    failed: BTreeMap<String, (Instant, String)>,
    identify: BTreeMap<String, (Instant, f64)>,
    /// Screens showing the test pattern, until when (`None`: until turned off).
    test: BTreeMap<String, Option<Instant>>,
    events: VecDeque<(u64, Value)>,
    event_seq: u64,
    /// The sound cards, and when they were listed.
    cards: Option<Vec<Value>>,
    cards_at: Option<Instant>,
    /// The network API turned off (`lan`), kept in `service.json`.
    lan_off: bool,
}

/// The service's own settings, as kept.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ServiceFile {
    #[serde(default)]
    lan_off: bool,
}

/// The display service.
pub struct Service {
    dir: PathBuf,
    /// The machine's media folder (the site's library): logos and slides may be there.
    media: Option<PathBuf>,
    driver: Box<dyn Driver>,
    inner: Mutex<Inner>,
    changed: Condvar,
    pub audit: Arc<Audit>,
    /// Site-signed tokens (engine `display`), with a Node.
    pub tokens: Option<Arc<idn_open_protocol::web::TokenCheck>>,
    pub lan: Option<idn_open_protocol::web::PasswordGate>,
    /// Thumbnails come back by id, with when (one answered after its asker gave up
    /// went at the next; bug run 2026-11-24: it stayed for good).
    thumbs: Mutex<HashMap<u64, Thumb>>,
    thumbs_ready: Condvar,
    next_id: AtomicU64,
    seq: Mutex<idn_open_protocol::web::events::Sequencer>,
    stop: AtomicBool,
    /// The failure events to the Site Manager's log through the Node (idn-node.md §N.41),
    /// rate-limited; its sink set by whoever runs the service.
    pub log: idn_open_protocol::web::problems::LogForward,
}

/// `logo.png`, `logo.jpg` or `logo.jpeg` in `dir`, if there.
fn logo_in(dir: &Path) -> Option<PathBuf> {
    ["logo.png", "logo.jpg", "logo.jpeg"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

/// A saved file of the service's: its default when there is none; one that can't be read
/// is set aside (`NAME.bad-SECS-PID-N`) and said.
fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    idn_common::fsutil::load_json(path)
        .unwrap_or_else(|e| {
            eprintln!("idn-display: {e}");
            None
        })
        .unwrap_or_default()
}

impl Service {
    /// The service with its state in `dir` (made if missing), its screens' feeds and
    /// settings read back from it.
    pub fn new(
        dir: &Path,
        media: Option<PathBuf>,
        driver: Box<dyn Driver>,
        tokens: Option<Arc<idn_open_protocol::web::TokenCheck>>,
    ) -> Arc<Service> {
        let _ = std::fs::create_dir_all(dir);
        // (A damaged file is set aside and said, never read as none and saved over.)
        let saved: BTreeMap<String, Saved> = load(&dir.join(FILE));
        let own: ServiceFile = load(&dir.join(SERVICE_FILE));
        Arc::new(Service {
            dir: dir.to_owned(),
            media,
            driver,
            inner: Mutex::new(Inner {
                saved,
                version: 1,
                lan_off: own.lan_off,
                ..Default::default()
            }),
            changed: Condvar::new(),
            audit: Audit::for_engine(Some(dir)),
            lan: tokens.clone().map(|t| {
                idn_open_protocol::web::PasswordGate::tokens_only(t, "idn_display", "X-IDN-Display")
            }),
            tokens,
            thumbs: Mutex::default(),
            thumbs_ready: Condvar::new(),
            next_id: AtomicU64::new(1),
            seq: Mutex::default(),
            stop: AtomicBool::new(false),
            log: Default::default(),
        })
    }

    /// Looks after the screens until [`Service::stop`]: ten times a second.
    pub fn start(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("idn-display-service".into())
            .spawn(move || {
                while !me.stop.load(Ordering::Relaxed) {
                    me.tend();
                    std::thread::sleep(Duration::from_millis(100));
                }
                me.inner.lock_ok().running.clear();
            })
            .expect("a thread")
    }

    /// Stops [`Service::start`]'s thread (the tests'; the program ends with its process).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// The state folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn bump(&self, g: &mut Inner) {
        g.version += 1;
        self.changed.notify_all();
    }

    /// A typed event for the push channels (`{"type": "event", "event", "screen", "level",
    /// "text", …}`); the failures (`lost`, `fallback`, `other_site`, `recovered`) also to
    /// the Site Manager's log, rate-limited (idn-node.md §N.41).
    fn event(&self, g: &mut Inner, event: &str, screen: &str, mut fields: Value) {
        g.event_seq += 1;
        fields["type"] = json!("event");
        fields["event"] = json!(event);
        fields["screen"] = json!(screen);
        fields["at"] = json!(idn_common::epoch::secs());
        let (level, text) = event_words(event, screen, &fields);
        fields["level"] = json!(level);
        fields["text"] = json!(text);
        if matches!(event, "lost" | "fallback" | "other_site" | "recovered") {
            self.log.offer(&fields);
        }
        g.events.push_back((g.event_seq, fields));
        while g.events.len() > EVENTS_KEPT {
            g.events.pop_front();
        }
        self.changed.notify_all();
    }

    /// The typed events after `seq`, and the newest's place.
    pub fn events_since(&self, seq: u64) -> (Vec<Value>, u64) {
        let g = self.inner.lock_ok();
        (
            g.events
                .iter()
                .filter(|(s, _)| *s > seq)
                .map(|(_, v)| v.clone())
                .collect(),
            g.event_seq,
        )
    }

    pub fn event_seq(&self) -> u64 {
        self.inner.lock_ok().event_seq
    }

    pub fn version(&self) -> u64 {
        self.inner.lock_ok().version
    }

    /// Waits up to `timeout` for the version to pass `after`.
    pub fn wait_change(&self, after: u64, timeout: Duration) -> u64 {
        let g = self.inner.lock_ok();
        let (g, _) = self
            .changed
            .wait_timeout_while(g, timeout, |g| g.version <= after)
            .unwrap_or_else(|e| e.into_inner());
        g.version
    }

    fn save(&self, g: &Inner) {
        let body = serde_json::to_vec_pretty(&g.saved).unwrap_or_default();
        if let Err(e) = idn_common::fsutil::write(self.dir.join(FILE), body) {
            eprintln!("idn-display: {}: {e}", self.dir.join(FILE).display());
        }
    }

    /// A screen named by `r` among those listed (or saved): its name.
    fn resolve(&self, g: &Inner, r: &ScreenRef) -> Result<String, String> {
        match r {
            ScreenRef::Number(n) => g
                .screens
                .get((*n as usize).wrapping_sub(1))
                .map(|s| s.name.clone())
                .ok_or_else(|| format!("no screen {n} (they are numbered from 1)")),
            ScreenRef::Name(s) => {
                if let Some(x) = g.screens.iter().find(|x| x.name == *s) {
                    return Ok(x.name.clone());
                }
                if let Ok(n) = s.parse::<u64>() {
                    return self.resolve(g, &ScreenRef::Number(n));
                }
                if g.saved.contains_key(s) {
                    return Ok(s.clone());
                }
                Err(format!("no screen {s:?}"))
            }
        }
    }

    /// `path` if it is inside the state folder or the media folder (a logo or a slide:
    /// no other file of this machine may be shown, or read back as a thumbnail).
    fn allowed(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path.trim());
        let p = if p.is_relative() {
            self.media.as_deref().unwrap_or(&self.dir).join(p)
        } else {
            p.to_owned()
        };
        let real = p
            .canonicalize()
            .map_err(|e| format!("{}: {e}", p.display()))?;
        let inside = [Some(self.dir.as_path()), self.media.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(|d| d.canonicalize().ok())
            .any(|d| real.starts_with(d));
        if !inside {
            return Err(format!(
                "{}: a file in the media folder{} or the service's folder ({})",
                p.display(),
                self.media
                    .as_deref()
                    .map(|m| format!(" ({})", m.display()))
                    .unwrap_or_default(),
                self.dir.display()
            ));
        }
        Ok(real)
    }

    /// The settings as the screen's process gets them: the logo (or the site's) and the
    /// slideshow's folder as full paths.
    fn resolved(&self, s: &Settings) -> Settings {
        let mut s = s.clone();
        let f = &mut s.fallback;
        f.logo = if f.logo.trim().is_empty() {
            self.media
                .as_deref()
                .and_then(logo_in)
                .or_else(|| logo_in(&self.dir))
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        } else {
            self.allowed(&f.logo)
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        if !f.slideshow.trim().is_empty() {
            f.slideshow = self
                .allowed(&f.slideshow)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
        }
        s
    }

    /// Runs a command (the API's, the push channel's) for `by` (as the audit says them:
    /// kept as who set a feed): its answer, or why not.
    pub fn command_by(&self, cmd: &str, body: &Value, by: &str) -> Result<Value, String> {
        let c: Command = serde_json::from_value(json!({"cmd": cmd, "body": body}))
            .map_err(|e| format!("{cmd}: {e}"))?;
        self.run_by(c, by)
    }

    /// Whether the network API is turned off (`lan`).
    pub fn lan_off(&self) -> bool {
        self.inner.lock_ok().lan_off
    }

    /// The screens listed again, outside the lock (listing starts a program).
    fn relist(&self) {
        let due = {
            let g = self.inner.lock_ok();
            g.listed_at.is_none_or(|t| t.elapsed() >= LIST_EVERY)
        };
        if !due {
            return;
        }
        let listed = self.driver.list();
        let cards_due = self
            .inner
            .lock_ok()
            .cards_at
            .is_none_or(|t| t.elapsed() >= CARDS_EVERY);
        let cards = if cards_due {
            self.driver.sound_cards()
        } else {
            None
        };
        let mut g = self.inner.lock_ok();
        g.listed_at = Some(Instant::now());
        if cards_due {
            g.cards_at = Some(Instant::now());
            if cards.is_some() && cards != g.cards {
                g.cards = cards;
                self.bump(&mut g);
            }
        }
        match listed {
            Ok(l) => {
                let before: Vec<String> = g.screens.iter().map(|s| s.name.clone()).collect();
                let now: Vec<String> = l.iter().map(|s| s.name.clone()).collect();
                if g.screens != l || g.list_error.is_some() {
                    g.screens = l;
                    g.list_error = None;
                    self.bump(&mut g);
                }
                let added: Vec<&String> = now.iter().filter(|n| !before.contains(n)).collect();
                let removed: Vec<&String> = before.iter().filter(|n| !now.contains(n)).collect();
                if !added.is_empty() || !removed.is_empty() {
                    let f = json!({"added": added, "removed": removed});
                    self.event(&mut g, "screens", "", f);
                }
            }
            Err(e) => {
                if g.list_error.as_deref() != Some(e.as_str()) {
                    g.list_error = Some(e);
                    self.bump(&mut g);
                }
            }
        }
    }

    /// Brings the screens' processes in line with what is wanted, and takes in what they
    /// said.
    pub fn tend(&self) {
        self.relist();
        let mut g = self.inner.lock_ok();
        let g = &mut *g;
        let now = Instant::now();
        g.identify.retain(|_, (t, _)| *t > now);
        let before = g.test.len();
        g.test.retain(|_, t| t.is_none_or(|t| t > now));
        if g.test.len() != before {
            self.bump(g);
        }
        // Ended by themselves: their last words are why.
        let ended: Vec<(String, String)> = g
            .running
            .iter_mut()
            .filter_map(|(k, r)| r.link.ended().map(|e| (k.clone(), e)))
            .collect();
        for (k, why) in ended {
            if let Some(r) = g.running.remove(&k)
                && let Some(st) = r.status
            {
                g.last.insert(k.clone(), st);
            }
            let why = if why.is_empty() {
                "the screen's process stopped".to_owned()
            } else {
                why
            };
            g.failed.insert(k.clone(), (now, why.clone()));
            self.event(g, "stopped", &k, json!({"error": why}));
            self.bump(g);
        }
        // Wanted: a feed, or identify, on a screen that is there.
        let screens = g.screens.clone();
        for s in &screens {
            let saved = g.saved.get(&s.name).cloned().unwrap_or_default();
            // A feed, identify, the test pattern, or the fallback when idle.
            let want = !saved.feed.is_empty()
                || g.identify.contains_key(&s.name)
                || g.test.contains_key(&s.name)
                || (g.saved.contains_key(&s.name) && saved.settings.idle == Idle::Fallback);
            if !want {
                if g.running.remove(&s.name).is_some() {
                    g.last.remove(&s.name);
                    self.bump(g);
                }
                continue;
            }
            if !g.running.contains_key(&s.name) {
                if g.failed
                    .get(&s.name)
                    .is_some_and(|(t, _)| now.duration_since(*t) < RESTART_AFTER)
                {
                    continue;
                }
                match self.driver.start(s) {
                    Ok(link) => {
                        g.running.insert(
                            s.name.clone(),
                            Running {
                                link,
                                sent_feed: None,
                                sent_settings: None,
                                ready: None,
                                status: None,
                                sent_test: false,
                                drops: VecDeque::new(),
                            },
                        );
                        self.bump(g);
                    }
                    Err(e) => {
                        g.failed.insert(s.name.clone(), (now, e));
                        self.bump(g);
                        continue;
                    }
                }
            }
            let resolved = self.resolved(&saved.settings);
            let Some(r) = g.running.get_mut(&s.name) else {
                continue;
            };
            // Settings first (the sound card, the wall's sync group), then the feed.
            if r.sent_settings.as_ref() != Some(&resolved)
                && r.link
                    .send(&ToChild::Settings {
                        settings: resolved.clone(),
                    })
                    .is_ok()
            {
                r.sent_settings = Some(resolved);
            }
            if r.sent_feed.as_deref() != Some(saved.feed.as_str())
                && r.link
                    .send(&ToChild::Feed {
                        source: saved.feed.clone(),
                    })
                    .is_ok()
            {
                r.sent_feed = Some(saved.feed.clone());
            }
            let test = g.test.contains_key(&s.name);
            if let Some(r) = g.running.get_mut(&s.name)
                && r.sent_test != test
                && r.link.send(&ToChild::TestPattern { on: test }).is_ok()
            {
                r.sent_test = test;
            }
        }
        // Screens gone (unplugged): their processes stop; their settings stay.
        let gone: Vec<String> = g
            .running
            .keys()
            .filter(|k| !screens.iter().any(|s| &s.name == *k))
            .cloned()
            .collect();
        for k in gone {
            g.running.remove(&k);
            self.bump(g);
        }
        // What the processes said.
        let names: Vec<String> = g.running.keys().cloned().collect();
        for name in names {
            let msgs = match g.running.get_mut(&name) {
                Some(r) => r.link.recv(),
                None => continue,
            };
            for m in msgs {
                match m {
                    FromChild::Ready { screen, adapter } => {
                        let lines = self.identify_lines(g, &name);
                        let ident = g.identify.get(&name).map(|(t, _)| *t);
                        if let Some(r) = g.running.get_mut(&name) {
                            r.ready = Some((screen, adapter));
                            // Identify asked for before it was up.
                            if let Some(t) = ident {
                                let _ = r.link.send(&ToChild::Identify {
                                    lines,
                                    seconds: t.saturating_duration_since(now).as_secs_f64(),
                                });
                            }
                        }
                        g.failed.remove(&name);
                        self.bump(g);
                    }
                    FromChild::Status { status } => self.took_status(g, &name, status),
                    FromChild::Thumbnail { id, jpeg, error } => {
                        let r = match error {
                            Some(e) => Err(e),
                            None => idn_common::text::unbase64(&jpeg)
                                .filter(|j| j.starts_with(&[0xff, 0xd8]))
                                .ok_or_else(|| "the screen sent no picture".to_owned()),
                        };
                        let mut t = self.thumbs.lock_ok();
                        t.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(10));
                        t.insert(id, (Instant::now(), r));
                        drop(t);
                        self.thumbs_ready.notify_all();
                    }
                }
            }
        }
    }

    /// A screen's new status: the events its change makes.
    fn took_status(&self, g: &mut Inner, name: &str, st: Status) {
        let feed = g
            .saved
            .get(name)
            .map(|s| s.feed.clone())
            .unwrap_or_default();
        let before = g
            .running
            .get(name)
            .and_then(|r| r.status.clone())
            .unwrap_or_default();
        let ev = transitions(&before, &st, &feed);
        let key = |s: &Status| {
            (
                s.state,
                s.showing,
                s.reason.clone(),
                s.attempts,
                s.gave_up,
                s.routed_to.clone(),
                s.stats.source_size,
            )
        };
        let moved = key(&before) != key(&st);
        if let Some(r) = g.running.get_mut(name) {
            let now = Instant::now();
            r.drops.push_back((now, st.stats.dropped));
            while r
                .drops
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) > DROP_WINDOW)
            {
                r.drops.pop_front();
            }
            r.status = Some(st);
        }
        for (e, f) in ev {
            self.event(g, e, name, f);
        }
        if moved {
            self.bump(g);
        }
    }

    /// What the screen shows, `width` pixels wide, as a JPEG (from its process: what is
    /// on the screen, overlays and fallback included).
    pub fn thumbnail(&self, screen: &ScreenRef, width: u32) -> Result<Vec<u8>, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut g = self.inner.lock_ok();
            let name = self.resolve(&g, screen)?;
            let r = g
                .running
                .get_mut(&name)
                .filter(|r| r.ready.is_some())
                .ok_or_else(|| format!("{name} shows nothing now (no feed)"))?;
            r.link.send(&ToChild::Thumbnail { id, width })?;
        }
        let t = self.thumbs.lock_ok();
        let (mut t, _) = self
            .thumbs_ready
            .wait_timeout_while(t, Duration::from_secs(3), |t| !t.contains_key(&id))
            .unwrap_or_else(|e| e.into_inner());
        t.remove(&id)
            .map(|(_, r)| r)
            .unwrap_or_else(|| Err("the screen didn't answer in time".into()))
    }
}

/// Where the service keeps its state: `display` in IDN's state folder.
fn default_state_dir() -> PathBuf {
    idn_common::paths::state_dir_or_temp().join("display")
}

const USAGE: &str = "\
usage: idn-display --service [--port N] [--lan-port N] [--no-lan] [--state-dir DIR] [--no-node]

  The display service (idn-display.md §D.8): this computer's screens, each showing a feed
  (a source, or a routing source to follow) set through its API on
  http://127.0.0.1:PORT/api/v1/ (default 29222; no UI of its own: the UI is the
  client's). With the machine's IDN Node in a site, the API is also on the network over
  HTTPS (--lan-port, default 29223) for the Site Manager's tokens (engine display) only;
  --no-lan keeps it closed. Each screen given a feed runs an idn-display --commands.
  The screens' feeds and settings are kept in screens.json in the state directory, and
  every change in audit.jsonl (GET /api/v1/audit).";

/// `idn-display --service`.
pub fn main() {
    let (mut port, mut lan_port, mut no_lan, mut use_node) = (
        idn_common::ports::DISPLAY,
        idn_common::ports::DISPLAY_LAN,
        false,
        true,
    );
    let mut dir = None::<PathBuf>;
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    let _ = it.next(); // --service
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.value(&a),
            "--lan-port" => lan_port = it.value(&a),
            "--no-lan" => no_lan = true,
            "--state-dir" => dir = Some(it.value::<String>(&a).into()),
            "--no-node" => use_node = false,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("idn-display: unknown argument {other:?}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    let node = use_node
        .then(|| match idn_open_protocol::node::Node::connect("idn-display-service") {
            Ok(n) => Some(n),
            Err(e) => {
                eprintln!("idn-display: no IDN Node ({e}): no site tokens, the API on this machine only");
                None
            }
        })
        .flatten();
    let media = node.as_ref().and_then(|n| {
        let r = n
            .request(idn_open_protocol::xml::Element::new("media"))
            .ok()?;
        Some(PathBuf::from(r.attr("dir").filter(|d| !d.is_empty())?))
    });
    let tokens = node.as_ref().map(|n| {
        idn_open_protocol::web::TokenCheck::from_enrollment("display", n.enrollment().clone())
    });
    let dir = dir.unwrap_or_else(default_state_dir);
    let svc = Service::new(
        &dir,
        media,
        Box::new(child::Programs::next_to_me()),
        tokens.clone(),
    );
    // Failure events to the Site Manager's log through the Node (§N.41).
    if let Some(n) = node {
        svc.log.to_node(Arc::new(n));
    }
    let _tender = svc.start();
    // The screens' settings copied once a day, 14 days kept (idn-node.md §N.42).
    idn_common::fsutil::keep_daily_settings_copies(dir.clone());
    if let Err(e) = api::serve(&svc, port, (!no_lan).then_some(lan_port)) {
        eprintln!("idn-display: port {port}: {e}");
        std::process::exit(1);
    }
    println!("idn-display: the display service's API on http://127.0.0.1:{port}/api/v1/");
    idn_open_protocol::shutdown::install();
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests;
