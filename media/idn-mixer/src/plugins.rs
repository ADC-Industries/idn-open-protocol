//! The desk's plugins (spec/extensions/idn-mixer.md §M.6): the helper process
//! (`idn-plugin-host`), the chains built in it from the settings' inserts, and starting
//! it again when it stops. The host is started when the first plugin is wanted (or the
//! plugin list is asked for), not before.

use crate::desk::{
    BLOCK, BUS_STRIPS, Insert, Inserts, MAIN_CHAIN, Settings, bus_strip, bus_strip_name,
    chain_strip, channel_chain, named_bus, rate,
};
use idn_common::sync::LockOk as _;
use idn_plugin_host::Host;
use idn_plugin_host::protocol::ChainAudio;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long building a chain may take (a big plugin loads its samples).
const BUILD: Duration = Duration::from_secs(30);
/// Other requests.
const QUICK: Duration = Duration::from_secs(5);

/// The host as the desk sees it.
struct HostIo(Arc<Host>);

impl Inserts for HostIo {
    fn submit(&self, seq: u64, chains: &[ChainAudio]) {
        self.0.submit_ref(seq, chains);
    }

    fn take(&self, seq: u64, wait: Duration) -> Option<Vec<ChainAudio>> {
        self.0.take_wait(seq, wait)
    }
}

/// A chain as built in the host.
#[derive(Clone)]
struct Built {
    channels: u16,
    inserts: Vec<Insert>,
    latency: u32,
    /// Plugins whose state was read from them (a knob turned) and isn't in the settings
    /// yet: an edit meanwhile doesn't load the settings' older state into them (the knob
    /// went back; bug round E, E2-7).
    fresh: BTreeSet<usize>,
}

/// The chains the settings ask for: chain number → (channels, inserts).
fn wanted(s: &Settings) -> BTreeMap<u32, (u16, Vec<Insert>)> {
    let n = s.channels.len();
    let mut w = BTreeMap::new();
    for (i, c) in s.channels.iter().enumerate() {
        if i % 2 == 1 && s.channels[i - 1].link || c.inserts.is_empty() {
            continue;
        }
        let pair = c.link && i % 2 == 0 && i + 1 < n;
        w.insert(
            channel_chain(i),
            (if pair { 2 } else { 1 }, c.inserts.clone()),
        );
    }
    for k in 0..BUS_STRIPS {
        if let Some(b) = s.bus(k).filter(|b| !b.inserts.is_empty()) {
            w.insert(bus_strip(k).0, (2, b.inserts.clone()));
        }
    }
    w
}

/// A chain's strip, for messages.
pub fn strip_name(chain: u32) -> String {
    match chain_strip(chain) {
        _ if chain < MAIN_CHAIN => format!("channel {chain}"),
        Some(k) => bus_strip_name(k),
        None => format!("chain {chain}"),
    }
}

/// Why the plugins are off after repeated crashes.
const GIVEN_UP: &str = "the plugin host crashed 3 times within a minute: the plugins are off (the sound passes without them) until the desk's plugins change — remove or replace the one added last";

/// The plugins of one desk.
pub struct Plugins {
    program: PathBuf,
    host: Mutex<Option<Arc<Host>>>,
    /// What is built in the host now; held while it is changed, so changes go in turn.
    built: Mutex<BTreeMap<u32, Built>>,
    /// Chains that couldn't be built or are stuck, and why.
    errors: Arc<Mutex<BTreeMap<u32, String>>>,
    /// The last scan.
    list: Mutex<Option<Value>>,
    /// Plugins whose parameters changed since their state was last read.
    dirty: Arc<Mutex<BTreeSet<(u32, usize)>>>,
    /// Plugins whose own window is open (§M.10).
    windows: Arc<Mutex<BTreeSet<(u32, usize)>>>,
    /// Samples a block, as the desk runs (its chains are built for it).
    block: usize,
    pub(crate) restarts: Mutex<u64>,
    /// Chains that failed to build, with what they were (their signature) and why: not
    /// tried again until they change. Every desk edit loaded a refused or crashing plugin
    /// again (bug round E, E2-1/2).
    failed: Mutex<BTreeMap<u32, (u64, String)>>,
    /// The host's crashes in the last minute; three, and the plugins stay off until the
    /// desk's plugins change (`given_up`: what they were). A crashing plugin restarted the
    /// host for ever, every strip flipping between dry and processed (E2-1).
    crashes: Mutex<std::collections::VecDeque<std::time::Instant>>,
    given_up: Mutex<Option<u64>>,
    /// Chains stuck (a plugin not answering), since when: ten seconds and the chain is
    /// set aside and the host started again without it (a hung plugin kept a real-time
    /// thread spinning for good; bug round E, E2-3).
    stuck_since: Arc<Mutex<BTreeMap<u32, std::time::Instant>>>,
}

/// A signature of what is to be built (to tell when it changed).
fn signature<T: std::hash::Hash>(t: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

fn chain_signature(channels: u16, inserts: &[Insert]) -> u64 {
    let parts: Vec<(&str, &str, bool)> = inserts
        .iter()
        .map(|p| (p.key.as_str(), p.state.as_str(), p.bypass))
        .collect();
    signature(&(channels, parts))
}

/// Host crashes in a minute after which the plugins stay off.
const CRASHES_TO_GIVE_UP: usize = 3;

impl Plugins {
    pub fn new(program: PathBuf) -> Plugins {
        Plugins {
            program,
            host: Mutex::new(None),
            built: Mutex::new(BTreeMap::new()),
            errors: Arc::default(),
            list: Mutex::new(None),
            dirty: Arc::default(),
            windows: Arc::default(),
            restarts: Mutex::new(0),
            block: BLOCK,
            failed: Mutex::new(BTreeMap::new()),
            crashes: Mutex::new(std::collections::VecDeque::new()),
            given_up: Mutex::new(None),
            stuck_since: Arc::default(),
        }
    }

    /// For a desk running `block` samples a block.
    pub fn with_block(mut self, block: usize) -> Plugins {
        self.block = block;
        self
    }

    /// The host, started if it isn't running (what was built in a stopped one is gone).
    fn host(&self, built: &mut BTreeMap<u32, Built>) -> Result<Arc<Host>, String> {
        let mut h = self.host.lock_ok();
        if let Some(host) = h.as_ref().filter(|h| h.alive()) {
            return Ok(host.clone());
        }
        if h.take().is_some() {
            *self.restarts.lock_ok() += 1;
            let mut c = self.crashes.lock_ok();
            let now = std::time::Instant::now();
            c.push_back(now);
            while c
                .front()
                .is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(60))
            {
                c.pop_front();
            }
            if c.len() >= CRASHES_TO_GIVE_UP {
                built.clear();
                self.windows.lock_ok().clear();
                return Err(GIVEN_UP.into());
            }
        }
        built.clear();
        self.windows.lock_ok().clear();
        let host = Arc::new(
            Host::spawn(&self.program).map_err(|e| format!("the plugin host didn't start: {e}"))?,
        );
        // The host's events: a knob turned in a plugin's window (its state is read again,
        // as after a change from the panel), a window closed by the user.
        let events = host.events();
        let (dirty, windows, errors, stuck_since) = (
            self.dirty.clone(),
            self.windows.clone(),
            self.errors.clone(),
            self.stuck_since.clone(),
        );
        std::thread::Builder::new()
            .name("idn-plugins-events".into())
            .spawn(move || {
                while let Ok(e) = events.recv() {
                    let at = (
                        e["chain"].as_u64().unwrap_or(0) as u32,
                        e["index"].as_u64().unwrap_or(0) as usize,
                    );
                    match e["event"].as_str() {
                        Some("param") => {
                            dirty.lock_ok().insert(at);
                        }
                        Some("editor_closed") => {
                            windows.lock_ok().remove(&at);
                        }
                        // A chain whose plugin stopped answering plays dry until it
                        // answers again: shown with the strip's plugins.
                        Some("stuck") => {
                            stuck_since
                                .lock_ok()
                                .entry(at.0)
                                .or_insert_with(std::time::Instant::now);
                            errors.lock_ok().insert(
                                at.0,
                                "a plugin stopped answering: this strip plays without its plugins"
                                    .into(),
                            );
                        }
                        Some("running") => {
                            stuck_since.lock_ok().remove(&at.0);
                            errors.lock_ok().remove(&at.0);
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        *h = Some(host.clone());
        Ok(host)
    }

    /// A chain stuck for over ten seconds, if any.
    pub fn stuck_too_long(&self) -> Option<u32> {
        self.stuck_since
            .lock_ok()
            .iter()
            .find(|(_, t)| t.elapsed() > std::time::Duration::from_secs(10))
            .map(|(c, _)| *c)
    }

    /// Sets chain `chain` aside (not built again until it changes) and lets the host go,
    /// to be started again without it. The desk must have let go of the host first.
    pub fn set_aside(&self, chain: u32) {
        let mut built = self.built.lock_ok();
        if let Some(b) = built.get(&chain) {
            self.failed.lock_ok().insert(
                chain,
                (
                    chain_signature(b.channels, &b.inserts),
                    "a plugin stopped answering for 10 s: this strip plays without its plugins until they change".into(),
                ),
            );
        }
        built.clear();
        self.windows.lock_ok().clear();
        self.stuck_since.lock_ok().clear();
        // (Dropped here and by the desk: the process is ended.)
        self.host.lock_ok().take();
    }

    /// Whether the host was running and has stopped (crashed) with chains in it.
    pub fn stopped(&self) -> bool {
        let built = self.built.lock_ok();
        !built.is_empty() && self.host.lock_ok().as_ref().is_some_and(|h| !h.alive())
    }

    /// The host's process id, while it runs.
    pub fn host_pid(&self) -> Option<u32> {
        self.host
            .lock_ok()
            .as_ref()
            .filter(|h| h.alive())
            .map(|h| h.pid())
    }

    pub fn running(&self) -> bool {
        self.host.lock_ok().as_ref().is_some_and(|h| h.alive())
    }

    /// Builds, changes and removes chains in the host to match `s`; what the desk is to
    /// send to (`None` when there are no chains) and the chains built, with their latency.
    /// `recall`: a whole-desk load (snapshot, preset): its plugin states are loaded even over
    /// one just read from a plugin's window (bug round F, F7-2).
    pub fn apply(
        &self,
        s: &Settings,
        recall: bool,
    ) -> (Option<Arc<dyn Inserts>>, BTreeMap<u32, u32>) {
        let want = wanted(s);
        let mut built = self.built.lock_ok();
        let mut errors = BTreeMap::new();
        // Given up after repeated crashes: off until the plugins change.
        let want_sig = signature(
            &want
                .iter()
                .map(|(id, (ch, ins))| (*id, chain_signature(*ch, ins)))
                .collect::<Vec<_>>(),
        );
        {
            let mut g = self.given_up.lock_ok();
            match *g {
                Some(sig) if sig == want_sig => {
                    for id in want.keys() {
                        errors.insert(*id, GIVEN_UP.into());
                    }
                    *self.errors.lock_ok() = errors;
                    return (None, BTreeMap::new());
                }
                Some(_) => {
                    *g = None;
                    self.crashes.lock_ok().clear();
                }
                None => {}
            }
        }
        let host = if want.is_empty() {
            // Nothing wanted: whatever is built goes (the host stays, idle).
            if let Some(h) = self.host.lock_ok().as_ref().filter(|h| h.alive()) {
                for id in built.keys() {
                    let _ = h.request(json!({"cmd": "remove", "chain": id}), QUICK);
                }
            }
            built.clear();
            None
        } else {
            match self.host(&mut built) {
                Ok(h) => Some(h),
                Err(e) if e == GIVEN_UP => {
                    *self.given_up.lock_ok() = Some(want_sig);
                    for id in want.keys() {
                        errors.insert(*id, e.clone());
                    }
                    None
                }
                Err(e) => {
                    errors.insert(0, e);
                    None
                }
            }
        };
        if let Some(h) = &host {
            built.retain(|id, _| {
                let keep = want.contains_key(id);
                if !keep {
                    let _ = h.request(json!({"cmd": "remove", "chain": id}), QUICK);
                }
                keep
            });
            let mut failed = self.failed.lock_ok();
            failed.retain(|id, _| want.contains_key(id));
            for (&id, (channels, inserts)) in &want {
                let sig = chain_signature(*channels, inserts);
                // Failed as it is now: not tried again (said again).
                if let Some((f, why)) = failed.get(&id)
                    && *f == sig
                    && !built.contains_key(&id)
                {
                    errors.insert(id, why.clone());
                    continue;
                }
                match self.build(h, &mut built, id, *channels, inserts, recall) {
                    Ok(()) => {
                        failed.remove(&id);
                    }
                    Err(e) => {
                        let e = if h.alive() {
                            e
                        } else {
                            format!(
                                "{e} (the plugin host stopped while it was being loaded: one of its plugins crashed it)"
                            )
                        };
                        failed.insert(id, (sig, e.clone()));
                        errors.insert(id, e);
                    }
                }
            }
            drop(failed);
        }
        // (A chain still stuck stays said so: the next apply erased it.)
        for c in self.stuck_since.lock_ok().keys() {
            errors.entry(*c).or_insert_with(|| {
                "a plugin stopped answering: this strip plays without its plugins".into()
            });
        }
        *self.errors.lock_ok() = errors;
        let chains: BTreeMap<u32, u32> = built.iter().map(|(&id, b)| (id, b.latency)).collect();
        let io = host
            .filter(|_| !chains.is_empty())
            .map(|h| Arc::new(HostIo(h)) as Arc<dyn Inserts>);
        (io, chains)
    }

    /// One chain: changed in place where the same plugins are in the same order (bypass,
    /// a recalled state), built afresh otherwise.
    fn build(
        &self,
        h: &Host,
        built: &mut BTreeMap<u32, Built>,
        id: u32,
        channels: u16,
        inserts: &[Insert],
        recall: bool,
    ) -> Result<(), String> {
        if let Some(b) = built.get_mut(&id)
            && b.channels == channels
            && b.inserts.len() == inserts.len()
            && b.inserts.iter().zip(inserts).all(|(a, b)| a.key == b.key)
        {
            for (i, (have, want)) in b.inserts.iter_mut().zip(inserts).enumerate() {
                if want.state == have.state {
                    b.fresh.remove(&i);
                }
                let keep = !recall && b.fresh.contains(&i) && want.state != have.state;
                if recall {
                    b.fresh.remove(&i);
                }
                if !keep && want.state != have.state && !want.state.is_empty() {
                    let r = h.request(
                        json!({"cmd": "load", "chain": id, "index": i, "state": want.state}),
                        QUICK,
                    )?;
                    b.latency = r["latency"].as_u64().unwrap_or(b.latency as u64) as u32;
                }
                if want.bypass != have.bypass {
                    let r = h.request(
                        json!({"cmd": "bypass", "chain": id, "index": i, "on": want.bypass}),
                        QUICK,
                    )?;
                    b.latency = r["latency"].as_u64().unwrap_or(b.latency as u64) as u32;
                }
                let state = keep.then(|| have.state.clone());
                *have = want.clone();
                if let Some(st) = state {
                    have.state = st;
                }
            }
            return Ok(());
        }
        built.remove(&id);
        if let Some(p) = inserts.iter().find(|p| !self.allowed(&p.key)) {
            return Err(format!(
                "{}: not a plugin of this machine's plugin folders (look again for plugins)",
                if p.name.is_empty() { &p.key } else { &p.name }
            ));
        }
        let plugins: Vec<Value> = inserts
            .iter()
            .map(|p| json!({"key": p.key, "state": p.state, "bypass": p.bypass}))
            .collect();
        let r = h.request(
            json!({"cmd": "chain", "chain": id, "channels": channels, "rate": rate(), "block": self.block, "plugins": plugins}),
            BUILD,
        )?;
        built.insert(
            id,
            Built {
                channels,
                inserts: inserts.to_vec(),
                fresh: BTreeSet::new(),
                latency: r["latency"].as_u64().unwrap_or(0) as u32,
            },
        );
        Ok(())
    }

    /// Why chains couldn't be built (chain 0: the host itself).
    pub fn errors(&self) -> Vec<Value> {
        self.errors
            .lock_ok()
            .iter()
            .map(|(&id, e)| json!({"strip": if id == 0 { "host".into() } else { strip_name(id) }, "error": e}))
            .collect()
    }

    /// The plugins on this machine (the last scan, or a new one). Scanned by a host
    /// process of its own, never the one running the chains.
    pub fn list(&self, rescan: bool) -> Result<Value, String> {
        if !rescan && let Some(l) = self.list.lock_ok().clone() {
            return Ok(l);
        }
        let l = idn_plugin_host::scan(&self.program, &[])?;
        *self.list.lock_ok() = Some(l.clone());
        Ok(l)
    }

    /// A plugin the desk may load: one of the last scan's, or a file in the plugin
    /// folders (an insert's key comes from the panel, and a path
    /// loaded as given could be a library on the network).
    fn allowed(&self, key: &str) -> bool {
        let listed = self
            .list
            .lock_ok()
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|p| p["key"] == key));
        listed
            || key
                .strip_prefix("clap:")
                .or_else(|| key.strip_prefix("vst3:"))
                .and_then(|k| k.rsplit_once('#'))
                .is_some_and(|(path, _)| idn_plugin_host::in_plugin_folders(path))
    }

    fn running_host(&self) -> Result<Arc<Host>, String> {
        self.host
            .lock_ok()
            .clone()
            .filter(|h| h.alive())
            .ok_or_else(|| "the plugin host isn't running".into())
    }

    fn check(&self, chain: u32, index: usize) -> Result<(), String> {
        match self.built.lock_ok().get(&chain) {
            Some(b) if index < b.inserts.len() => Ok(()),
            Some(_) => Err("no such plugin".into()),
            None => Err(format!("{} has no plugins running", strip_name(chain))),
        }
    }

    /// A plugin's parameters, as the panel shows them.
    pub fn params(&self, chain: u32, index: usize) -> Result<Value, String> {
        self.check(chain, index)?;
        let mut r = self.running_host()?.request(
            json!({"cmd": "params", "chain": chain, "index": index}),
            QUICK,
        )?;
        r.as_object_mut().map(|o| o.remove("id"));
        Ok(r)
    }

    /// Sets a parameter; the chain's latency now, if it changed.
    pub fn set(
        &self,
        chain: u32,
        index: usize,
        param: u32,
        value: f64,
    ) -> Result<Option<u32>, String> {
        self.check(chain, index)?;
        let r = self.running_host()?.request(
            json!({"cmd": "set", "chain": chain, "index": index, "param": param, "value": value}),
            QUICK,
        )?;
        self.dirty.lock_ok().insert((chain, index));
        let latency = r["latency"].as_u64().map(|l| l as u32);
        let mut built = self.built.lock_ok();
        Ok(match (built.get_mut(&chain), latency) {
            (Some(b), Some(l)) if b.latency != l => {
                b.latency = l;
                Some(l)
            }
            _ => None,
        })
    }

    /// A chain's plugins' output peaks (dBFS, one per plugin in order) since last asked.
    pub fn levels(&self, chain: u32) -> Result<Value, String> {
        if !self.built.lock_ok().contains_key(&chain) {
            return Ok(json!({ "levels": [] }));
        }
        let mut r = self
            .running_host()?
            .request(json!({"cmd": "levels", "chain": chain}), QUICK)?;
        r.as_object_mut().map(|o| o.remove("id"));
        Ok(r)
    }

    /// Asks each chain for its latency again (a plugin may know it only once it runs);
    /// whether any changed.
    pub fn poll_latency(&self) -> bool {
        let ids: Vec<u32> = self.built.lock_ok().keys().copied().collect();
        if ids.is_empty() {
            return false;
        }
        let Ok(host) = self.running_host() else {
            return false;
        };
        let mut changed = false;
        for id in ids {
            let Ok(r) = host.request(json!({"cmd": "latency", "chain": id}), QUICK) else {
                continue;
            };
            let Some(l) = r["latency"].as_u64().map(|l| l as u32) else {
                continue;
            };
            if let Some(b) = self.built.lock_ok().get_mut(&id)
                && b.latency != l
            {
                b.latency = l;
                changed = true;
            }
        }
        changed
    }

    /// Shows a plugin's own window (§M.10) on this machine's desktop, inside the
    /// application's window `parent` (`x11:ID`, `hwnd:ID`) or as a window of its own; or
    /// closes it (`open` false).
    pub fn editor(
        &self,
        chain: u32,
        index: usize,
        open: bool,
        parent: Option<&str>,
        title: &str,
    ) -> Result<Value, String> {
        self.check(chain, index)?;
        let h = self.running_host()?;
        if open {
            let mut req = json!({"cmd": "editor", "chain": chain, "index": index, "title": title});
            if let Some(p) = parent {
                req["parent"] = json!(p);
            }
            let mut r = h.request(req, QUICK)?;
            r.as_object_mut().map(|o| o.remove("id"));
            self.windows.lock_ok().insert((chain, index));
            Ok(r)
        } else {
            h.request(
                json!({"cmd": "editor_close", "chain": chain, "index": index}),
                QUICK,
            )?;
            self.windows.lock_ok().remove(&(chain, index));
            Ok(json!({"open": false}))
        }
    }

    /// Whether a plugin's own window is open.
    pub fn editor_open(&self, chain: u32, index: usize) -> bool {
        self.windows.lock_ok().contains(&(chain, index))
    }

    /// The plugins whose own window is open: (chain, index).
    pub fn editors(&self) -> Vec<(u32, usize)> {
        self.windows.lock_ok().iter().copied().collect()
    }

    /// The chains and their latency now.
    pub fn chains(&self) -> BTreeMap<u32, u32> {
        self.built
            .lock_ok()
            .iter()
            .map(|(&id, b)| (id, b.latency))
            .collect()
    }

    /// Reads the state of plugins whose parameters changed (all of them with `all`):
    /// those whose state is new.
    pub fn refresh(&self, all: bool) -> Vec<ReadState> {
        let todo: Vec<(u32, usize)> = if all {
            self.built
                .lock_ok()
                .iter()
                .flat_map(|(&id, b)| (0..b.inserts.len()).map(move |i| (id, i)))
                .collect()
        } else {
            std::mem::take(&mut *self.dirty.lock_ok())
                .into_iter()
                .collect()
        };
        let Ok(h) = self.running_host() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (chain, index) in todo {
            let Ok(r) = h.request(
                json!({"cmd": "state", "chain": chain, "index": index}),
                QUICK,
            ) else {
                continue;
            };
            let Some(state) = r["state"].as_str() else {
                continue;
            };
            let mut built = self.built.lock_ok();
            if let Some(c) = built.get_mut(&chain)
                && let Some(b) = c.inserts.get_mut(index)
                && b.state != state
            {
                let before = std::mem::replace(&mut b.state, state.to_owned());
                c.fresh.insert(index);
                out.push(ReadState {
                    chain,
                    index,
                    key: b.key.clone(),
                    state: b.state.clone(),
                    before,
                });
            }
        }
        out
    }
}

/// A plugin's state read by [`Plugins::refresh`].
#[derive(Clone, Debug)]
pub struct ReadState {
    pub chain: u32,
    pub index: usize,
    pub key: String,
    pub state: String,
    /// The state the plugin had (as the settings had it) before.
    pub before: String,
}

impl Plugins {
    /// The settings have these states now: no longer kept over a change (F7-2: left
    /// marked, the next snapshot recall kept the plugin's own state instead of the snapshot's).
    pub fn committed(&self, states: &[ReadState]) {
        let mut built = self.built.lock_ok();
        for r in states {
            if let Some(c) = built.get_mut(&r.chain)
                && c.inserts.get(r.index).is_some_and(|b| b.state == r.state)
            {
                c.fresh.remove(&r.index);
            }
        }
    }
}

/// Writes states read by [`Plugins::refresh`] into the settings (where the same plugin is
/// still in that place, and the settings still have the state it had before: one
/// recalled since then is not written over; F7-2). Those written.
pub fn store_states(s: &mut Settings, states: &[ReadState]) -> Vec<ReadState> {
    let mut stored = Vec::new();
    for r in states {
        let (chain, index, key, state) = (&r.chain, &r.index, &r.key, &r.state);
        let list = match *chain {
            c if c < MAIN_CHAIN => s.channels.get_mut(c as usize - 1).map(|c| &mut c.inserts),
            c => chain_strip(c)
                .and_then(|k| s.bus_mut(k))
                .map(|b| &mut b.inserts),
        };
        if let Some(ins) = list
            .and_then(|l| l.get_mut(*index))
            .filter(|i| &i.key == key && i.state == r.before)
        {
            ins.state = state.clone();
            stored.push(r.clone());
        }
    }
    stored
}

/// The insert list a command names (`{"channel": N}` or `{"bus": "main" | 1–8}`), and its
/// chain number.
pub fn strip<'a>(s: &'a mut Settings, v: &Value) -> Result<(&'a mut Vec<Insert>, u32), String> {
    if let Some(n) = v["channel"].as_u64() {
        let i = (n as usize).wrapping_sub(1);
        let count = s.channels.len();
        if i >= count {
            return Err(format!("channels are 1 to {count}"));
        }
        if i % 2 == 1 && s.channels[i - 1].link {
            return Err("a linked pair's plugins are on its first channel".into());
        }
        return Ok((&mut s.channels[i].inserts, channel_chain(i)));
    }
    let k = named_bus(v)?;
    let b = s.bus_mut(k).ok_or("no such bus")?;
    Ok((&mut b.inserts, bus_strip(k).0))
}
