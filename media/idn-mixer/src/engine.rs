//! The running desk (spec/extensions/idn-mixer.md §M.1, §M.3–M.5): inputs from the
//! routing matrix through the machine's Node, the desk on its 2 ms clock, the buses out
//! as an IDN source, commands, snapshots and meters.

use crate::card::{CardIn, CardOut, Cards};
use crate::desk::{
    BUS_STRIPS, BUTTONS, Bus, Button, Channel, ChannelPreset, DCA, Dca, Desk, Insert, MUTE_GROUPS,
    MuteGroup, Settings, Talkback, bus_strip_name, named_bus, rate, rate_hz,
};
use crate::dsp::to_db;
use crate::plugins::{self, Plugins};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::routing::{
    AudioReceiver, ReceiverKind, Router, RouterConfig, RouterEvent,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod workers;
use workers::*;

mod commands;
pub mod controls;
pub mod describe;
mod network_routing;
mod problems;
pub mod scoped;
mod state;
pub use controls::controls;
pub use state::{DeskState, InputOrigin, InputSource};

/// The site's transmitters' origins: `HOST (name)` → (machine, origin) (§M.28.3).
type SiteOrigins =
    std::collections::HashMap<String, (String, idn_open_protocol::node::routing::AudioOrigin)>;
/// A network-audio device's receive channels' subscriptions, `(device, channel)` each.
type RingSubs = Vec<Option<(String, String)>>;

mod console_plugins;
mod endpoints;
mod front;
mod iem_api;
mod iem_link;
mod recording;
mod snapshots;
mod switcher_link;
pub mod typed;
mod x32_link;

/// How to start.
#[derive(Clone)]
pub struct ConsoleConfig {
    /// The routable receiver's and the output's name. Default "Mixer".
    pub name: String,
    /// Processing channels (1–64). Default 32.
    pub channels: usize,
    /// Desk inputs: the routable receiver's channels, which the input patch gives the
    /// channels (§M.11). Default 64.
    pub inputs: usize,
    /// Desk outputs: the output's channels, which the output patch fills. Default 64.
    pub outputs: usize,
    /// The machine's IDN Node: inputs come from the routing matrix through it.
    pub node: Option<Arc<Node>>,
    pub secure: Option<idn_open_protocol::secure::SecureConfig>,
    /// Announce on mDNS (default on; off in tests).
    pub mdns: Option<bool>,
    pub discovery_server: Option<String>,
    /// Each input lane's buffer, ms (10: the low-latency mode, §A.10). With `adaptive`,
    /// only where this machine's programs' lanes are held at most.
    pub latency_ms: u32,
    /// Each source's lane adapts to its path (roadmap R.30), all of them in line (the
    /// desk's channels stay aligned): a network source from its path's start, this
    /// machine's programs from their block and a little room. On by default;
    /// `--latency MS` holds one latency instead.
    pub adaptive: bool,
    /// Samples a block (96 or 64, `--block`); `None`: as the desk was saved (its Setup's
    /// choice), else 96.
    pub block: Option<usize>,
    /// The desk's sample rate, asked for (`--rate`): wins over its saved one (§N.40).
    pub rate: Option<u32>,
    /// The machine's rate as its Node gave it in the command (`--machine-rate`: a Node
    /// service starts again with a new one); `None`: asked of the Node at the start.
    pub machine_rate: Option<u32>,
    /// Where snapshots are kept (`snapshots.json`); `None`: in memory only.
    pub state_dir: Option<PathBuf>,
    /// The plugin host program; `None`: `idn-plugin-host` next to this program (or
    /// `IDN_PLUGIN_HOST`).
    pub plugin_host: Option<PathBuf>,
    /// A sound card's inputs on the desk's channels, and the desk's outputs on a card,
    /// directly (§M.6).
    pub card_in: Option<CardIn>,
    pub card_out: Option<CardOut>,
    /// A network-audio device on this machine as the desk's sound card (roadmap R.33,
    /// [`crate::netaudio`]): the desk works in its rings directly, on site time.
    pub network_audio: Option<crate::netaudio::NetAudioConfig>,
    /// Where multitrack recordings go (§M.13), a folder each; `None`: `recordings` in the
    /// state directory (without one, in the desk program's folder,
    /// `idn_common::paths::program_dir("mixer")`).
    pub recordings: Option<PathBuf>,
    /// The X32 whose preamps the desk controls (`--x32`, §M.23.1): put in the desk's
    /// `x32` setting at the start (which the `desk` command changes later). `None`: as
    /// saved.
    pub x32: Option<String>,
    /// The IEM flow for phones on the network (§M.26, idn-iem.md).
    pub iem: crate::iem::IemConfig,
    /// What checks site-signed tokens (an embedder's own, or tests'); `None`: the Node's
    /// site membership, with a Node.
    pub token_check: Option<Arc<idn_open_protocol::web::TokenCheck>>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        ConsoleConfig {
            name: "Mixer".into(),
            channels: 32,
            inputs: 64,
            outputs: 64,
            node: None,
            secure: None,
            mdns: None,
            discovery_server: None,
            latency_ms: 10,
            adaptive: true,
            block: None,
            rate: None,
            machine_rate: None,
            state_dir: None,
            plugin_host: None,
            card_in: None,
            card_out: None,
            network_audio: None,
            recordings: None,
            x32: None,
            iem: Default::default(),
            token_check: None,
        }
    }
}

struct Shared {
    cfg: ConsoleConfig,
    desk: Mutex<Desk>,
    /// Changes of the settings take turns here, the desk itself held only to swap them in.
    /// Always taken before `desk`, never while it is held.
    editing: Mutex<()>,
    version: Mutex<u64>,
    changed: Condvar,
    snapshots: Mutex<BTreeMap<String, Settings>>,
    /// The snapshots' order as set (§M.28, `snapshot_order`); those not in it come after,
    /// by name.
    snapshot_order: Mutex<Vec<String>>,
    out: Output,
    stop: AtomicBool,
    /// The input lanes, one per transmitter routed to the desk.
    lanes: Arc<Mutex<BTreeMap<String, Lane>>>,
    /// The input mixer the lanes feed; kept so in-process feeds can add lanes of their own.
    input: Arc<Mixer>,
    /// Host-fed input lanes by name ([`Console::feed`]), apart from the routed ones.
    feeds: Mutex<BTreeMap<String, Lane>>,
    /// When each transmitter's sound last arrived: a backup input's failover goes by it
    /// (a stopped transmitter keeps its route and lane).
    heard: Arc<Mutex<BTreeMap<String, Instant>>>,
    /// Blocks skipped because the clock fell behind; the last block's processing time.
    late: AtomicU64,
    block_us: AtomicU64,
    /// How late blocks started, the worst over the last 10 s (µs): the one just ended and
    /// the one running, as (worst, since).
    start_late: Mutex<(u64, u64, Instant)>,
    /// The clock's thread runs real-time (the system allowing it, and the desk keeping up).
    realtime: AtomicBool,
    /// The clock's blocks (marked each one) and its restarts after a panic: the status's
    /// `clock_alive` and `clock_restarts`.
    clock_beat: idn_common::heartbeat::Heartbeat,
    plugins: Plugins,
    /// The sound card the outputs play on, directly.
    card_out: Mutex<Option<Arc<idn_audio::Lane>>>,
    /// The card the desk's clock follows (roadmap R.26), when it has one.
    card_time: Mutex<Option<crate::card::CardTime>>,
    /// What the desk's clock follows now, and a card's rate against its label (ppm).
    clock_source: Mutex<(&'static str, f64)>,
    /// The network-audio device's state, as the clock thread last saw it (R.33).
    net_audio: Mutex<Option<crate::netaudio::NetSnapshot>>,
    /// Blocks the output thread was too far behind to take (eight queued).
    out_dropped: AtomicU64,
    /// Held while the plugins are brought in line with the settings.
    syncing: Mutex<()>,
    /// The routable receiver: its routes say which desk inputs are in use.
    receiver: Mutex<Option<Arc<AudioReceiver>>>,
    /// Switchers' outputs brought in as desk inputs (§M.17), and those on the network now.
    switchers: Mutex<crate::switchers::Saved>,
    switchers_here: Mutex<std::collections::BTreeSet<String>>,
    /// The snapshot last saved or recalled.
    snapshot: Mutex<Option<String>>,
    /// The panel and API refuse changes (§M.11); the control surface still works.
    locked: AtomicBool,
    /// Channel presets by name (§M.11), kept beside the snapshots.
    presets: Mutex<BTreeMap<String, ChannelPreset>>,
    /// Each desk input's source in words ("Stage box 13"), from the routes.
    input_names: Mutex<Vec<String>>,
    /// Each desk input's source, typed (§M.28.3).
    input_sources: Mutex<Vec<state::InputSource>>,
    /// The site's transmitters' origins (`HOST (name)` → machine, origin), and when last
    /// asked of the Node (every 5 s).
    site_origins: Mutex<(Option<std::time::Instant>, SiteOrigins)>,
    /// The network-audio device's receive channels' subscriptions (device, channel) from its
    /// `device.json`, and that file's time (read again when it changes).
    ring_subs: Mutex<(Option<std::time::SystemTime>, RingSubs)>,
    /// Audio follows video (§M.12): what the switcher last said, and whether it is heard.
    afv: Mutex<AfvStatus>,
    /// A multitrack recording under way, and a recording played in place of the inputs
    /// (§M.13), with the desk inputs kept live while it plays.
    recorder: Mutex<Option<crate::record::Recorder>>,
    /// Recordings starting or finishing now (a rate restart waits for them).
    recording_busy: std::sync::atomic::AtomicUsize,
    /// `restart` asked for (§M.28): the saver starts the desk again.
    restart_wanted: AtomicBool,
    /// The last round-trip test's result (§M.28.2), for the state.
    round_trip_last: Mutex<Option<Value>>,
    soundcheck: Mutex<Option<crate::record::Soundcheck>>,
    live: Mutex<Vec<bool>>,
    /// The X32 the desk controls over Ethernet (§M.23.1), while its setting names one.
    x32: Mutex<x32_link::Slot>,
    /// The IEM flow on the network, its phones and their talkback (§M.26).
    iem: iem_link::Hub,
    /// Every change, who made it and from where (§M.25).
    audit: Arc<idn_open_protocol::web::audit::Audit>,
    /// Typed failure events (§M.27): the push channels', the Site Manager's log through
    /// the Node; and the watch that finds them.
    events: Arc<idn_open_protocol::web::problems::Events>,
    watcher: Mutex<problems::Watcher>,
    /// The sources on the network as last listed (with a Node): a lost source's cause.
    listed: Mutex<Option<std::collections::BTreeSet<String>>>,
    /// Blocks whose processing took longer than their time (the DSP over its budget).
    slow_blocks: AtomicU64,
    /// The sound cards' state, while the desk has one (§M.6).
    cards_status: Mutex<Option<Arc<Mutex<Value>>>>,
    /// The outputs' channel names last told as their origin (idn-routing.md §A.16).
    origin: Mutex<Vec<String>>,
    /// The named output endpoints' IDN outputs.
    endpoints: endpoints::Endpoints,
}

/// The switcher as audio follows video hears it (§M.12).
#[derive(Default, Clone)]
struct AfvStatus {
    connected: bool,
    program: String,
    on_air: Vec<String>,
    inputs: Vec<String>,
}

/// A switcher's tally message (`<idn_tally program preview on_air inputs/>`, idn-switcher
/// §S.23): (program, on air, every input), labels one a line.
fn parse_tally(xml: &str) -> Option<(String, Vec<String>, Vec<String>)> {
    let e = idn_open_protocol::control::parse_element(xml)?;
    if e.name != "idn_tally" {
        return None;
    }
    let list = |k: &str| -> Vec<String> {
        e.attr(k)
            .unwrap_or("")
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    };
    Some((
        e.attr("program").unwrap_or("").to_owned(),
        list("on_air"),
        list("inputs"),
    ))
}

/// The running desk; stops when dropped.
pub struct Console {
    shared: Arc<Shared>,
    _publisher: Publisher,
    _router: Option<Router>,
    clock: Option<std::thread::JoinHandle<()>>,
    watch: Option<std::thread::JoinHandle<()>>,
    /// Audio follows video's switcher watcher (§M.12).
    follow: Option<std::thread::JoinHandle<()>>,
    /// Brings switchers' outputs in (§M.17), with a Node.
    switcher_outputs: Option<std::thread::JoinHandle<()>>,
    /// Connects the X32 the settings name and turns its changes into new versions
    /// (§M.23.1).
    x32_watch: Option<std::thread::JoinHandle<()>>,
    cards: Option<Cards>,
    /// Saves the desk on SIGINT/SIGTERM (a stop within a second of a
    /// change lost it).
    _on_stop: idn_open_protocol::shutdown::Registration,
    /// Every change, who made it and from where (`audit.jsonl` in its folder; §M.25).
    audit: Arc<idn_open_protocol::web::audit::Audit>,
    /// Site-signed tokens for the desk on this machine, checked with its Node's site
    /// membership (idn-node.md §N.31.5); none without a Node.
    tokens: Option<Arc<idn_open_protocol::web::TokenCheck>>,
    /// The IEM flow's threads (§M.26).
    iem_threads: Vec<std::thread::JoinHandle<()>>,
    /// HTTP's per-control sequence numbers (the API front's, `engine/front.rs`).
    http_seq: Mutex<idn_open_protocol::web::events::Sequencer>,
}

/// A merge patch (RFC 7386: `patch`'s members replace `target`'s, objects recursively,
/// `null` removes), except that arrays merge element by element, `null` keeping an
/// element: `"eq": [null, {"gain": 3}]` changes band 2 alone.
fn merge(target: &mut Value, patch: &Value) {
    if let (Value::Array(t), Value::Array(p)) = (&mut *target, patch) {
        for (i, v) in p.iter().enumerate() {
            match t.get_mut(i) {
                Some(_) if v.is_null() => {}
                Some(x) => merge(x, v),
                None => t.push(v.clone()),
            }
        }
        return;
    }
    if let Value::Object(p) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        let t = target.as_object_mut().expect("made an object");
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else {
        *target = patch.clone();
    }
}

/// The desk inputs' labels on the routing matrix: the names of the channels that take
/// each, else `In N`.
fn input_labels(s: &Settings, inputs: usize) -> Vec<String> {
    (1..=inputs)
        .map(|k| {
            let names: Vec<&str> = s
                .channels
                .iter()
                .enumerate()
                .filter(|(i, c)| c.source.unwrap_or(i + 1) == k)
                .map(|(_, c)| c.name.as_str())
                .collect();
            if names.is_empty() {
                format!("In {k}")
            } else {
                names.join(" / ")
            }
        })
        .collect()
}

/// Parking, manual or automatic, stays as the desk has it when a snapshot is recalled
/// (with automatic parking a recalled snapshot showed as changed at once).
fn keep_parking(s: &mut Settings, desk: &Settings) {
    s.auto_park = desk.auto_park;
    s.block = desk.block;
    s.rate = desk.rate;
    s.x32 = desk.x32.clone();
    s.iem_control = desk.iem_control;
    // The IEM mixes and marks: the desk's, unless the snapshot holds them (§M.26).
    if !s.includes_iem {
        s.iem_mixes = desk.iem_mixes.clone();
        for (c, d) in s.channels.iter_mut().zip(&desk.channels) {
            c.iem = d.iem;
        }
    }
    s.includes_iem = desk.includes_iem;
    // The recall safes too: the desk's, not the snapshot's (a safe set after the snapshot was
    // saved showed it changed after a recall).
    for (c, d) in s.channels.iter_mut().zip(&desk.channels) {
        c.shown = d.shown;
        c.safe = d.safe;
    }
    for k in 0..BUS_STRIPS {
        if let (Some(b), Some(d)) = (s.bus_mut(k), desk.bus(k)) {
            b.safe = d.safe;
        }
    }
}

/// What a snapshot's recall takes (§M.15); outside it, the desk stays as it is.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRecallScope {
    /// The channels (from 1); `None`: all.
    pub channels: Option<Vec<usize>>,
    /// The buses (main, aux, groups, matrix, FX, headphones), DCAs, mute groups, talkback,
    /// the user keys, the automix, duckers and audio follows video.
    pub buses: bool,
    /// The output patch and its delays.
    pub outputs: bool,
    /// What of each channel it takes (§M.28); `None`: all of it.
    pub parts: Option<Vec<RecallPart>>,
    /// Faders, sends and pans moved to the snapshot's over this many seconds (0–10);
    /// `None`: 0.2 s.
    pub fade_s: Option<f32>,
}

/// A part of a channel a snapshot recall takes (§M.28).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RecallPart {
    /// Fader, mute, pan, solo.
    Levels,
    /// Aux, FX and stream sends; main, groups, DCA and mute-group membership.
    Sends,
    /// Trim, polarity, delay, filters, gate, EQ, compressor, de-esser, limiter, noise
    /// suppression, inserts.
    Processing,
    /// Its inputs (source, backup input, failover).
    Inputs,
    /// Its name and colour.
    Names,
}

impl Default for SnapshotRecallScope {
    fn default() -> Self {
        SnapshotRecallScope {
            channels: None,
            buses: true,
            outputs: true,
            parts: None,
            fade_s: None,
        }
    }
}

/// `sc`'s `parts` put on `o` (§M.28).
fn take_parts(o: &mut Channel, sc: &Channel, parts: &[RecallPart]) {
    for p in parts {
        match p {
            RecallPart::Levels => {
                (o.fader, o.mute, o.pan, o.solo) = (sc.fader, sc.mute, sc.pan, sc.solo);
            }
            RecallPart::Sends => {
                o.sends = sc.sends.clone();
                o.fx_sends = sc.fx_sends.clone();
                o.stream = sc.stream.clone();
                (o.main, o.groups) = (sc.main, sc.groups.clone());
                (o.dca, o.mute_groups) = (sc.dca.clone(), sc.mute_groups.clone());
            }
            RecallPart::Processing => {
                (o.trim, o.polarity, o.delay) = (sc.trim, sc.polarity, sc.delay);
                (o.hpf, o.lpf, o.hpf_last, o.lpf_last) = (sc.hpf, sc.lpf, sc.hpf_last, sc.lpf_last);
                (o.hpf_slope, o.lpf_slope) = (sc.hpf_slope, sc.lpf_slope);
                o.gate = sc.gate.clone();
                (o.eq_on, o.eq) = (sc.eq_on, sc.eq.clone());
                o.comp = sc.comp.clone();
                o.deesser = sc.deesser.clone();
                o.limiter = sc.limiter.clone();
                (o.denoise, o.denoise_amount) = (sc.denoise, sc.denoise_amount);
                (o.inserts, o.insert_point) = (sc.inserts.clone(), sc.insert_point);
            }
            RecallPart::Inputs => {
                (o.source, o.input_b) = (sc.source, sc.input_b);
                (o.use_b, o.failover) = (sc.use_b, sc.failover);
            }
            RecallPart::Names => {
                o.name = sc.name.clone();
                o.color = sc.color.clone();
            }
        }
    }
}

/// `snapshot` recalled onto `desk` within `scope`, safes kept (§M.15): a safe channel or bus,
/// and anything outside the scope, as the desk has it; parking and the safes themselves
/// are the desk's.
fn recalled(snapshot: &Settings, desk: &Settings, scope: &SnapshotRecallScope) -> Settings {
    let mut out = desk.clone();
    let named = |i: usize| scope.channels.as_ref().is_none_or(|c| c.contains(&(i + 1)));
    // A stereo pair (on the desk or in the snapshot) recalls, or stays safe, as one: either
    // side named or safe stands for both (one side alone would be put back to the other).
    let pair = |s: &Settings, i: usize| {
        let first = i & !1;
        first + 1 < s.channels.len() && s.channels[first].link
    };
    let other = |i: usize| (pair(desk, i) || pair(snapshot, i)).then_some(i ^ 1);
    for (i, (o, sc)) in out.channels.iter_mut().zip(&snapshot.channels).enumerate() {
        let wanted = named(i) || other(i).is_some_and(named);
        let safe = o.safe || other(i).is_some_and(|j| desk.channels[j].safe);
        if wanted && !safe {
            match &scope.parts {
                None => {
                    *o = Channel {
                        shown: o.shown,
                        safe: o.safe,
                        ..sc.clone()
                    }
                }
                Some(parts) => take_parts(o, sc, parts),
            }
        }
    }
    if scope.buses {
        let take = |o: &mut Bus, sc: &Bus| {
            if !o.safe {
                *o = Bus {
                    safe: o.safe,
                    ..sc.clone()
                };
            }
        };
        take(&mut out.main, &snapshot.main);
        take(&mut out.monitor, &snapshot.monitor);
        take(&mut out.stream, &snapshot.stream);
        for (list, from) in [
            (&mut out.aux, &snapshot.aux),
            (&mut out.groups, &snapshot.groups),
            (&mut out.matrix, &snapshot.matrix),
            (&mut out.fx, &snapshot.fx),
        ] {
            for (o, sc) in list.iter_mut().zip(from) {
                take(o, sc);
            }
        }
        out.dca = snapshot.dca.clone();
        out.mute_groups = snapshot.mute_groups.clone();
        out.talkback = snapshot.talkback.clone();
        out.buttons = snapshot.buttons.clone();
        out.solo_afl = snapshot.solo_afl;
        out.automix = snapshot.automix;
        out.duckers = snapshot.duckers.clone();
        out.afv = snapshot.afv.clone();
    }
    if scope.outputs {
        out.outputs = snapshot.outputs.clone();
        out.output_delay = snapshot.output_delay.clone();
    }
    // The IEM mixes and every channel's mark, as one, from a snapshot that holds them
    // (§M.26, whatever the scope and the safes: the phones' setup); else the desk's.
    for (o, (sc, d)) in out
        .channels
        .iter_mut()
        .zip(snapshot.channels.iter().zip(&desk.channels))
    {
        o.iem = if snapshot.includes_iem { sc.iem } else { d.iem };
    }
    if snapshot.includes_iem {
        out.iem_mixes = snapshot.iem_mixes.clone();
    }
    out.includes_iem = false;
    out.auto_park = desk.auto_park;
    out
}

/// Whether command `cmd` runs while the desk is locked (§M.11): the lock itself, and a
/// musician's own IEM mix (§M.26: the phones keep working through a locked service).
pub fn works_locked(cmd: &str) -> bool {
    matches!(cmd, "lock" | "iem_mix")
}

/// Settings without the plugins' saved states (they change by themselves), to compare.
fn without_states(s: &Settings) -> Settings {
    let mut s = s.clone();
    for c in &mut s.channels {
        c.inserts.iter_mut().for_each(|i| i.state.clear());
    }
    for k in 0..BUS_STRIPS {
        if let Some(b) = s.bus_mut(k) {
            b.inserts.iter_mut().for_each(|i| i.state.clear());
        }
    }
    s
}

fn patched<T: serde::Serialize + serde::de::DeserializeOwned>(
    v: &T,
    patch: &Value,
) -> Result<T, String> {
    let mut j = serde_json::to_value(v).map_err(|e| e.to_string())?;
    merge(&mut j, patch);
    serde_json::from_value(j).map_err(|e| format!("bad settings: {e}"))
}

impl Console {
    /// Starts: the output is on the network at once; inputs arrive as routes are made.
    pub fn start(cfg: ConsoleConfig) -> io::Result<Console> {
        let n = cfg.channels.clamp(1, 64);
        let inputs = cfg.inputs.clamp(1, 256);
        let outputs = cfg.outputs.clamp(1, 256);
        // The desk as it was when it last ran (a restart lost everything
        // but the snapshots), and its block size from it unless asked for one.
        // (Unreadable or not this build's: set aside and said, not saved over by the
        // fresh desk a second later, bug round 2026-11-09.)
        let saved = cfg.state_dir.as_ref().and_then(|d| {
            idn_common::fsutil::load_saved(&d.join(DESK_FILE), |b| {
                serde_json::from_slice::<Settings>(b).ok()
            })
            .unwrap_or_else(|why| {
                idn_open_protocol::log::log(&format!("mixer: the desk: {why}"));
                None
            })
        });
        // (Each checked on its own: an embedder's bad one doesn't throw the saved one away.)
        let valid = |b: &usize| crate::desk::BLOCKS.contains(b);
        let block = cfg
            .block
            .filter(valid)
            .or(saved.as_ref().map(|s| s.block).filter(valid))
            .unwrap_or_else(crate::desk::default_block);
        // The rate, before anything is made for it: asked for, else the desk's own, else
        // the machine's (from the command, else the Node), else 48 kHz (§N.40).
        let valid_rate = |r: &u32| idn_open_protocol::node::rate::valid(*r);
        let rate = cfg
            .rate
            .filter(valid_rate)
            .or(saved.as_ref().and_then(|s| s.rate).filter(valid_rate))
            .or(cfg.machine_rate.filter(valid_rate))
            .or(cfg
                .node
                .as_ref()
                .map(|n| idn_open_protocol::node::rate::ask(n)))
            .unwrap_or(idn_open_protocol::node::rate::DEFAULT);
        crate::desk::set_rate(rate);
        // The suppression's delay at this rate measured now, not on the clock thread the
        // first time a channel turns it on (bug run J2-4).
        let _ = idn_dsp::denoise::latency(rate);
        // The routed inputs, then each IEM mix's talkback input (§M.26).
        let mut desk = Desk::with_block(n, inputs + crate::iem::TALKBACK_INPUTS, outputs, block);
        if let Some(mut saved) = saved {
            // The block it runs, also when the start asked for another.
            saved.block = block;
            desk.set(saved, false);
        } else {
            let mut s = desk.settings().clone();
            s.block = block;
            desk.set(s, false);
        }
        // The X32 asked for at the start (`--x32`), kept as the desk's setting.
        if let Some(a) = &cfg.x32 {
            let mut s = desk.settings().clone();
            s.x32.address = a.trim().to_owned();
            desk.set(s, false);
        }
        let publisher = Publisher::new(PublisherConfig {
            mdns: cfg.mdns,
            secure: cfg.secure.clone(),
            discovery_server: cfg.discovery_server.clone(),
            ..Default::default()
        })?;
        let out = Output::new(
            &publisher,
            OutputConfig {
                audio_labels: (0..outputs)
                    .map(|o| desk.settings().output_label(o))
                    .collect(),
                // Switchers find the desk by it, for its feeds (§M.18).
                roles: vec![crate::switchers::DESK_ROLE.into()],
                ..OutputConfig::new(&cfg.name)
            },
        )?;
        // Made what the desk's settings are made on loading (one that wasn't showed
        // "changed" right after its recall).
        let snapshots: BTreeMap<String, Settings> = cfg
            .state_dir
            .as_ref()
            .map(|d| load_snapshots(&d.join(SNAPSHOTS_FILE)))
            .unwrap_or_default()
            .into_iter()
            .map(|(k, s)| (k, s.normalised(n, outputs)))
            .collect();
        let input = Mixer::new(rate_hz(), inputs as u16, &cfg.name);
        let audit = idn_open_protocol::web::audit::Audit::for_engine(cfg.state_dir.as_deref());
        let tokens = cfg.token_check.clone().or_else(|| {
            cfg.node.as_ref().map(|n| {
                idn_open_protocol::web::TokenCheck::from_enrollment("desk", n.enrollment().clone())
            })
        });
        // The IEM multicast copy's interface (idn-iem.md §I.5): asked for, else the
        // Node's, else towards the site, else the default route's.
        let iem_if = {
            let st = cfg
                .iem
                .interface
                .is_none()
                .then(|| cfg.node.as_ref()?.status().ok())
                .flatten();
            let attr = |k: &str| st.as_ref().and_then(|e| e.attr(k)).map(str::to_owned);
            crate::iem::multicast_interface(
                cfg.iem.interface,
                attr("interface").as_deref(),
                attr("server").as_deref(),
            )
        };
        let (iem, iem_ends) = iem_link::Hub::new(
            cfg.iem.clone(),
            iem_if,
            rate_hz(),
            inputs,
            tokens.clone(),
            publisher.clone(),
        );
        // Typed failure events (§M.27): to the Site Manager's log through the Node.
        let events = idn_open_protocol::web::problems::Events::new();
        if let Some(n) = &cfg.node {
            events.log.to_node(n.clone());
        }
        let shared = Arc::new(Shared {
            desk: Mutex::new(desk),
            editing: Mutex::new(()),
            version: Mutex::new(1),
            changed: Condvar::new(),
            snapshots: Mutex::new(snapshots),
            snapshot_order: Mutex::new(load_state(
                cfg.state_dir.as_deref(),
                SNAPSHOT_ORDER_FILE,
                |b| serde_json::from_slice(b).ok(),
            )),
            out,
            stop: AtomicBool::new(false),
            late: AtomicU64::new(0),
            start_late: Mutex::new((0, 0, Instant::now())),
            realtime: AtomicBool::new(false),
            clock_beat: idn_common::heartbeat::Heartbeat::new(),
            block_us: AtomicU64::new(0),
            lanes: Arc::default(),
            input: input.clone(),
            feeds: Mutex::new(BTreeMap::new()),
            heard: Arc::default(),
            plugins: Plugins::new(
                cfg.plugin_host
                    .clone()
                    .unwrap_or_else(idn_plugin_host::default_host_program),
            )
            .with_block(block),
            syncing: Mutex::new(()),
            card_out: Mutex::new(None),
            card_time: Mutex::new(None),
            clock_source: Mutex::new(("own", 0.0)),
            net_audio: Mutex::new(None),
            out_dropped: AtomicU64::new(0),
            receiver: Mutex::new(None),
            switchers: Mutex::new(load_state(cfg.state_dir.as_deref(), SWITCHERS_FILE, |b| {
                serde_json::from_slice(b).ok()
            })),
            switchers_here: Mutex::new(Default::default()),
            // The snapshot loaded when it last ran.
            snapshot: Mutex::new(
                cfg.state_dir
                    .as_ref()
                    .and_then(|d| std::fs::read_to_string(d.join(SNAPSHOT_FILE)).ok())
                    .map(|n| n.trim().to_owned())
                    .filter(|n| !n.is_empty()),
            ),
            locked: AtomicBool::new(false),
            presets: Mutex::new(load_state(cfg.state_dir.as_deref(), PRESETS_FILE, |b| {
                serde_json::from_slice(b).ok()
            })),
            input_names: Mutex::new(Vec::new()),
            input_sources: Mutex::new(Vec::new()),
            site_origins: Mutex::new((None, std::collections::HashMap::new())),
            ring_subs: Mutex::new((None, Vec::new())),
            afv: Mutex::default(),
            recorder: Mutex::new(None),
            recording_busy: std::sync::atomic::AtomicUsize::new(0),
            restart_wanted: AtomicBool::new(false),
            round_trip_last: Mutex::new(None),
            soundcheck: Mutex::new(None),
            live: Mutex::new(Vec::new()),
            x32: Mutex::default(),
            iem,
            audit: audit.clone(),
            events,
            watcher: Mutex::default(),
            listed: Mutex::new(None),
            slow_blocks: AtomicU64::new(0),
            cards_status: Mutex::new(None),
            origin: Mutex::new(Vec::new()),
            endpoints: endpoints::Endpoints::new(publisher.clone()),
            cfg,
        });
        let mut cards =
            (shared.cfg.card_in.is_some() || shared.cfg.card_out.is_some()).then(|| {
                let block = shared.desk.lock_ok().block();
                Cards::start(
                    shared.cfg.card_in.clone(),
                    shared.cfg.card_out.clone(),
                    input.clone(),
                    shared.cfg.latency_ms,
                    block,
                )
            });
        *shared.card_out.lock_ok() = cards.as_mut().and_then(|c| c.out.take());
        *shared.card_time.lock_ok() = cards.as_ref().and_then(|c| c.time.clone());
        *shared.cards_status.lock_ok() = cards.as_ref().map(Cards::status_cell);
        // Where its outputs' sound comes from: this desk's buses (§A.16).
        problems::tell_origin(&shared);
        // Inputs: the routable receiver, a lane per transmitter.
        let router = match &shared.cfg.node {
            Some(node) => {
                let labels = input_labels(shared.desk.lock_ok().settings(), inputs);
                let rx = Arc::new(AudioReceiver::register_as(
                    node,
                    &shared.cfg.name,
                    inputs as u16,
                    &labels,
                    ReceiverKind::Desk,
                )?);
                *shared.receiver.lock_ok() = Some(rx.clone());
                // Its need to the site and the site's floor for it (§A.14): the desk in line
                // with the other receivers of its sources.
                if shared.cfg.adaptive {
                    rx.attach_mixer(&input);
                }
                let (lanes, heard) = (shared.lanes.clone(), shared.heard.clone());
                let (input, latency) = (input.clone(), shared.cfg.latency_ms);
                let adaptive = shared.cfg.adaptive;
                // Each same-machine transmitter's target: (ms, last needed).
                let targets: Mutex<BTreeMap<String, (u32, Instant)>> = Mutex::default();
                Some(Router::start(
                    node.clone(),
                    rx,
                    RouterConfig { lossless: true },
                    move |ev| match ev {
                        RouterEvent::Block(b) => {
                            heard.lock_ok().insert(b.tx.to_owned(), Instant::now());
                            let mut l = lanes.lock_ok();
                            // The desk's own, unless the site sets the receiver's (§A.12).
                            let want = b.latency_ms.unwrap_or(latency);
                            // A program on this machine (shared memory, the ASIO driver)
                            // runs on the desk's clock (R.26): its sound needs only its own
                            // block and a little room (the lane grows on an underrun).
                            // From its largest block of the last 10 s: up at once, down
                            // only after 10 s and by 2 ms or more. Each block's own made a
                            // source whose blocks vary (128 or 256 frames) refill its lane
                            // over and over: continuous crackle.
                            let want = if b.audio.via == "machine" && b.latency_ms.is_none() {
                                let rate = b.audio.header.sample_rate.max(1) as u64;
                                let frames = b.audio.channels.first().map_or(0, Vec::len) as u64;
                                let need = (frames * 1000).div_ceil(rate) as u32 + 3;
                                let now = Instant::now();
                                let mut t = targets.lock_ok();
                                let e = t.entry(b.tx.to_owned()).or_insert((need, now));
                                if need > e.0 {
                                    *e = (need, now);
                                } else if need + 2 > e.0 {
                                    e.1 = now;
                                } else if now.duration_since(e.1) > Duration::from_secs(10) {
                                    *e = (need, now);
                                }
                                want.min(e.0)
                            } else {
                                want
                            };
                            // Adapting (R.30): a network source from its path's start, a
                            // program here from its own need (never under it); in line with
                            // the others either way. The site's latency still wins.
                            let here = b.audio.via == "machine";
                            let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                                input.lane(&LaneConfig {
                                    latency_ms: if adaptive && !here && b.latency_ms.is_none() {
                                        b.path.start_ms()
                                    } else {
                                        want
                                    },
                                    max_correction_ppm: 1000.0,
                                    adaptive,
                                    min_ms: if here { want } else { 2 },
                                    keep_start_level: false,
                                })
                            });
                            if adaptive {
                                lane.follow_site(b.latency_ms);
                            } else {
                                lane.follow_latency(want);
                            }
                            let at = idn_open_protocol::audio::site_time(
                                b.audio.header.timestamp,
                                b.audio.header.metadata.as_deref(),
                            );
                            lane.push_timed(
                                b.audio.header.sample_rate,
                                &b.audio.channels,
                                Some(b.map),
                                at,
                            );
                        }
                        RouterEvent::Gone(tx) => {
                            lanes.lock_ok().remove(tx);
                            targets.lock_ok().remove(tx);
                            heard.lock_ok().remove(tx);
                        }
                    },
                )?)
            }
            None => None,
        };
        // Its plugins, as saved.
        sync_plugins(&shared, false);
        let clock = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-mixer-clock".into())
                // Real-time where the system allows (as the plugin host's threads): at
                // normal priority blocks started up to 19 ms late with 12 of 16 cores busy,
                // 27 ms with 20 busy threads (2.4 ms idle), past the
                // inputs' 10 ms. A desk that falls behind goes back to normal priority
                // (`run`): real-time, it starved the threads feeding it.
                .spawn(move || {
                    let rt = idn_common::rt::audio_this_thread();
                    shared.realtime.store(rt, Ordering::Relaxed);
                    // Started again after a panic (a bug in a block's processing): the desk
                    // goes on, and the status says how often.
                    idn_common::heartbeat::keep_running(
                        "the desk's clock",
                        &shared.stop,
                        &shared.clock_beat,
                        || run(&shared, &input),
                    )
                })?
        };
        let watch = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-mixer-plugins".into())
                .spawn(move || watch_plugins(&shared))?
        };
        let follow = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-mixer-afv".into())
                .spawn(move || watch_switcher(&shared))?
        };
        let switcher_outputs = match &shared.cfg.node {
            Some(_) => {
                let shared = shared.clone();
                Some(
                    std::thread::Builder::new()
                        .name("idn-mixer-switchers".into())
                        .spawn(move || watch_switcher_outputs(&shared))?,
                )
            }
            None => None,
        };
        let x32_watch = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-mixer-x32".into())
                .spawn(move || x32_link::watch(&shared))?
        };
        // (Weak: the desk's output must go before the publisher when the console drops.)
        let on_stop = {
            let shared = Arc::downgrade(&shared);
            idn_open_protocol::shutdown::register(move || {
                if let Some(shared) = shared.upgrade() {
                    // A recording finished, its headers written (bug run N2-7, N3-10: the
                    // Node's restart of its network desk, on a rate change, cut its last
                    // seconds).
                    let recording = shared.recorder.lock_ok().take();
                    if let Some(r) = recording {
                        let _ = r.stop();
                    }
                    store_states(&shared, false);
                    save_desk(&shared, &mut Vec::new());
                }
            })
        };
        let iem_threads = iem_link::start(&shared, iem_ends)?;
        Ok(Console {
            iem_threads,
            http_seq: Mutex::default(),
            audit,
            tokens,
            shared,
            _publisher: publisher,
            _router: router,
            clock: Some(clock),
            watch: Some(watch),
            follow: Some(follow),
            switcher_outputs,
            x32_watch: Some(x32_watch),
            cards,
            _on_stop: on_stop,
        })
    }

    /// Its audit: every change, who made it and from where (§M.25).
    pub fn audit(&self) -> &Arc<idn_open_protocol::web::audit::Audit> {
        &self.audit
    }

    /// What checks site-signed tokens for this desk (with a Node), for a network front
    /// ([`idn_open_protocol::web::PasswordGate::with_tokens`]).
    pub fn token_check(&self) -> Option<Arc<idn_open_protocol::web::TokenCheck>> {
        self.tokens.clone()
    }

    /// A command run for `who`, written in the audit as theirs (HTTP, the push channel,
    /// OSC, a snapshot cue; §M.25). Locked (§M.11), only `lock`. Refused outside their
    /// scope ([`Console::permits`]: read-only, one bus); their front checks that first
    /// for its own answer (HTTP's 403).
    pub fn command_as(
        &self,
        who: &idn_open_protocol::web::Who,
        cmd: &str,
        v: &Value,
    ) -> Result<(), String> {
        let done = if let Err(e) = self.permits(who, cmd, v) {
            Err(e)
        } else if self.locked() && !works_locked(cmd) {
            Err("the desk is locked".to_owned())
        } else {
            self.command(cmd, v)
        };
        self.audit
            .record(who, cmd, v, done.as_ref().err().map(String::as_str));
        done
    }

    /// [`Console::command_as`] with the command's answer: `{"id"}` for `iem_mix_add` (the
    /// new mix's), else `{}`.
    pub fn command_answer_as(
        &self,
        who: &idn_open_protocol::web::Who,
        cmd: &str,
        v: &Value,
    ) -> Result<Value, String> {
        if cmd != "iem_mix_add" && cmd != "endpoint_add" && cmd != "latency_test" {
            return self.command_as(who, cmd, v).map(|()| json!({}));
        }
        if cmd == "latency_test" {
            // (Its answer is the measurement; a locked desk may still be measured.)
            let done = self
                .permits(who, cmd, v)
                .and_then(|()| self.latency_test(v));
            self.audit
                .record(who, cmd, v, done.as_ref().err().map(String::as_str));
            return done;
        }
        let done = if let Err(e) = self.permits(who, cmd, v) {
            Err(e)
        } else if self.locked() {
            Err("the desk is locked".to_owned())
        } else if cmd == "endpoint_add" {
            self.endpoint_add(v)
        } else {
            self.iem_mix_add(v["name"].as_str().unwrap_or(""), v["id"].as_str())
        };
        self.audit
            .record(who, cmd, v, done.as_ref().err().map(String::as_str));
        done.map(|id| json!({ "id": id }))
    }

    /// The output (its port, its receivers).
    pub fn output(&self) -> &Output {
        &self.shared.out
    }

    fn changed(&self) {
        *self.shared.version.lock_ok() += 1;
        self.shared.changed.notify_all();
    }

    fn edit(
        &self,
        slow: bool,
        f: impl FnOnce(&mut Settings) -> Result<(), String>,
    ) -> Result<(), String> {
        change_settings(&self.shared, slow, f)?;
        sync_plugins(&self.shared, slow);
        self.changed();
        Ok(())
    }

    /// As [`Self::edit`] (not ramped), for a load of a channel's processing (a preset, a
    /// copy): its plugin states loaded over ones just read from a plugin's window (F7-2).
    fn edit_loading(
        &self,
        f: impl FnOnce(&mut Settings) -> Result<(), String>,
    ) -> Result<(), String> {
        change_settings(&self.shared, false, f)?;
        sync_plugins(&self.shared, true);
        self.changed();
        Ok(())
    }

    /// The settings now.
    pub fn settings(&self) -> Settings {
        // Cloned with the desk let go (the clock thread takes it every block).
        let s = self.shared.desk.lock_ok().settings_arc();
        (*s).clone()
    }

    fn bump(&self) {
        *self.shared.version.lock_ok() += 1;
        self.shared.changed.notify_all();
    }

    /// Starts the main mix's integrated loudness again (§M.15): a new service.
    pub fn reset_loudness(&self) {
        self.shared.desk.lock_ok().reset_loudness();
    }

    /// Locks or unlocks the desk against changes from its API and controls (§M.11).
    /// `channel_count {n}` (§M.28): the desk's channels from its next start (1–64; a
    /// desk started with `--channels` keeps that). The state's `channels_next` says it
    /// until then; `restart` starts it again now.
    pub fn channel_count(&self, n: usize) -> Result<(), String> {
        if !(1..=64).contains(&n) {
            return Err("n: 1 to 64 channels".into());
        }
        let dir = self
            .shared
            .cfg
            .state_dir
            .as_ref()
            .ok_or("this desk keeps nothing (no state folder): start it with --channels")?;
        let b = serde_json::to_vec(&json!({ "channels": n })).map_err(|e| e.to_string())?;
        idn_common::fsutil::write_atomic(&dir.join(SHAPE_FILE), &b).map_err(|e| e.to_string())?;
        self.changed();
        Ok(())
    }

    /// The channel count kept for the next start, when it isn't the one running.
    pub(super) fn channels_next(&self) -> Option<usize> {
        let now = self.shared.desk.lock_ok().settings_arc().channels.len();
        self.shared
            .cfg
            .state_dir
            .as_deref()
            .and_then(saved_channel_count)
            .filter(|&n| n != now)
    }

    /// `restart` (§M.28): the desk saved and started again at once by the IDN Node (about
    /// a second of silence; its settings, snapshots and plugins kept). Refused when it
    /// wasn't started by the Node (run by hand: start it again yourself) or while it
    /// records.
    pub fn restart(&self) -> Result<(), String> {
        if !idn_common::switches::on("IDN_SUPERVISED") {
            return Err("this desk wasn't started by the IDN Node: start it again yourself".into());
        }
        if self.shared.recorder.lock_ok().is_some()
            || self.shared.recording_busy.load(Ordering::SeqCst) > 0
        {
            return Err("not while it records: stop the recording first".into());
        }
        self.shared.restart_wanted.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// `latency_test {output, input}` (§M.28.2): a click out of desk output `output`
    /// (silent otherwise while it runs) heard back on desk input `input` (both from 1),
    /// looped back by a cable, a card or a route; answers `{ms, samples}` (kept in the
    /// state's `latency.measured`), or why not within about 2 s.
    pub fn latency_test(&self, v: &Value) -> Result<Value, String> {
        let n = |k: &str| v[k].as_u64().map(|x| x as usize);
        let (output, input) = (
            n("output").ok_or("output: a desk output, from 1")?,
            n("input").ok_or("input: a desk input, from 1")?,
        );
        self.shared.desk.lock_ok().start_round_trip(output, input)?;
        let end = std::time::Instant::now()
            + Duration::from_secs_f32(2.0 * crate::desk::roundtrip_give_up_s() + 0.5);
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let rt = self.shared.desk.lock_ok().round_trip();
            match rt {
                crate::desk::RoundTrip::Done { samples, .. } => {
                    let ms = samples as f64 * 1000.0 / f64::from(crate::desk::rate_hz());
                    let r = json!({"ms": (ms * 100.0).round() / 100.0, "samples": samples,
                        "output": output, "input": input, "at": idn_common::epoch::now().as_secs()});
                    *self.shared.round_trip_last.lock_ok() = Some(r.clone());
                    self.changed();
                    return Ok(r);
                }
                crate::desk::RoundTrip::Failed(why) => return Err(why),
                _ if std::time::Instant::now() >= end => {
                    return Err("the desk's clock isn't running".into());
                }
                _ => {}
            }
        }
    }

    pub fn lock(&self, on: bool) {
        self.shared.locked.store(on, Ordering::Relaxed);
        self.changed();
    }

    pub fn locked(&self) -> bool {
        self.shared.locked.load(Ordering::Relaxed)
    }

    /// Something a client shows changed outside the desk's settings: wakes the long polls
    /// and push channels.
    pub fn touch(&self) {
        self.changed();
    }

    /// The state's version: it grows with every change.
    pub fn version(&self) -> u64 {
        *self.shared.version.lock_ok()
    }

    /// Waits until the state changes past `after` (or `timeout`); the version then.
    pub fn wait_change(&self, after: u64, timeout: Duration) -> u64 {
        let v = self.shared.version.lock_ok();
        let (v, _) = self
            .shared
            .changed
            .wait_timeout_while(v, timeout, |v| *v <= after)
            .unwrap();
        *v
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.clock.take() {
            let _ = t.join();
        }
        if let Some(t) = self.watch.take() {
            let _ = t.join();
        }
        if let Some(t) = self.follow.take() {
            let _ = t.join();
        }
        if let Some(t) = self.switcher_outputs.take() {
            let _ = t.join();
        }
        if let Some(t) = self.x32_watch.take() {
            let _ = t.join();
        }
        for t in self.iem_threads.drain(..) {
            let _ = t.join();
        }
        // (Its client stops with it: the X32 forgets the subscription 10 s later.)
        self.shared.x32.lock_ok().client.take();
    }
}

/// Where the desk's current settings are kept, beside the snapshots.
const DESK_FILE: &str = "desk.json";
/// Switchers' outputs the desk knows (§M.17).
const SWITCHERS_FILE: &str = "switcher-inputs.json";

fn unix_now() -> u64 {
    idn_common::epoch::now().as_secs()
}

/// The saved snapshots.
const SNAPSHOTS_FILE: &str = "snapshots.json";
/// The name of the snapshot last saved or recalled.
const SNAPSHOT_FILE: &str = "snapshot.txt";
/// The channel count for the next start (§M.28, `channel_count`).
const SHAPE_FILE: &str = "desk-shape.json";

/// The channel count `channel_count` kept in `dir` for the next start, if any (1–64).
/// A damaged file is set aside and said (the start then takes the channels asked for).
pub fn saved_channel_count(dir: &std::path::Path) -> Option<usize> {
    let v: Value = load_state(Some(dir), SHAPE_FILE, |b| serde_json::from_slice(b).ok());
    v["channels"]
        .as_u64()
        .map(|n| n as usize)
        .filter(|n| (1..=64).contains(n))
}

/// The snapshots' order (§M.28).
const SNAPSHOT_ORDER_FILE: &str = "snapshot-order.json";

/// Channel presets.
const PRESETS_FILE: &str = "presets.json";

#[cfg(test)]
mod iem_tests;
#[cfg(test)]
mod tests;

/// A saved file of the desk's (`file` in `dir`): empty when there is none; one that can't
/// be read is set aside and said (the next save would have written over every preset with
/// the one new; bug review 3).
fn load_state<T: Default>(
    dir: Option<&std::path::Path>,
    file: &str,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> T {
    let Some(dir) = dir else {
        return T::default();
    };
    match idn_common::fsutil::load_saved(&dir.join(file), parse) {
        Ok(v) => v.unwrap_or_default(),
        Err(why) => {
            idn_open_protocol::log::log(&format!("mixer: {why}"));
            T::default()
        }
    }
}

/// The saved snapshots, each on its own: one this build can't read is left out (said, and
/// the file copied aside first so it isn't lost at the next save); a file that can't be
/// read at all is set aside whole (one bad snapshot, or a cut-off file, lost every snapshot
/// before, then the next save wrote the empty list; bug round 2026-11-09).
fn load_snapshots(path: &std::path::Path) -> BTreeMap<String, Settings> {
    let all = match idn_common::fsutil::load_saved(path, |b| {
        serde_json::from_slice::<BTreeMap<String, serde_json::Value>>(b).ok()
    }) {
        Ok(m) => m.unwrap_or_default(),
        Err(why) => {
            idn_open_protocol::log::log(&format!("mixer: snapshots: {why}"));
            return BTreeMap::new();
        }
    };
    let mut out = BTreeMap::new();
    let mut bad = Vec::new();
    for (name, v) in all {
        match serde_json::from_value::<Settings>(v) {
            Ok(s) => {
                out.insert(name, s);
            }
            Err(e) => bad.push(format!("{name} ({e})")),
        }
    }
    if !bad.is_empty() {
        // A copy of its own (one per second before: a second load in the same second
        // replaced the first copy).
        let kept = match idn_common::fsutil::copy_aside(path) {
            Ok(copy) => format!("the file kept as {}", copy.display()),
            Err(e) => format!("couldn't copy the file aside: {e}"),
        };
        idn_open_protocol::log::log(&format!(
            "mixer: snapshots this build can't read, left out ({kept}): {}",
            bad.join(", ")
        ));
    }
    out
}
