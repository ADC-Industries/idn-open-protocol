//! The switcher as a service (IDN Media as a server and libraries; the UI in the client):
//! the engine ([`Switcher`]) with what makes it a switcher to run day to day: its
//! commands by name (`POST /api/v1/<command>`'s), its setup kept between runs, undo and
//! setups, the show and its cues, files, lists and stream inputs, streams and recordings,
//! MQTT, the starters and title templates, and the state that reports them all.
//! `idn-switcher` is this behind HTTP; a program embeds it with [`Service::start`].

// Without FFmpeg there are no files, streams or recordings: their parts go unused.
#![cfg_attr(not(feature = "ffmpeg"), allow(dead_code, unused_imports))]

use crate::frame::Frame;
use crate::{
    AudioSource, Kind, SlideAction, Slideshow, Source, Switcher, SwitcherConfig, TimerAction,
    TimerMode, Ycc,
};
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod commands;
mod destinations;
mod format;
mod http;
mod media;
mod mqtt_link;
pub mod problems;
mod rules;
mod saved;
mod setups;
mod shows;
pub mod state_schema;
pub use format::saved_format;
mod streams;
pub mod typed;

pub use typed::{Command, InputRef};

mod files;
mod lifecycle;
mod parsing;
mod report;
use files::*;
pub use parsing::parse_format;
use parsing::*;

/// The mixes as `mixes.json` keeps them: buses by their inputs' labels.
fn saved_mixes(st: &Value) -> Value {
    let label = |id: &Value| {
        st["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| !id.is_null() && i["id"] == *id)
            .map(|i| i["label"].clone())
            .unwrap_or(Value::Null)
    };
    st["mixes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            // (A wipe's soft edge and a luma wipe, its pattern by label: as the program's.)
            let t = setups::saved_transition(&m["transition"], &label);
            json!({"name": m["name"], "width": m["width"], "height": m["height"],
                "follow": m["follows_program"],
                "program": label(&m["program"]), "preview": label(&m["preview"]),
                "kind": t["kind"], "duration": t["duration"], "soft": t["soft"], "luma": t["luma"]})
        })
        .collect()
}

/// The streams and recordings of the program, and the inputs recorded on their own (ISO).
struct Streams {
    #[cfg(feature = "ffmpeg")]
    gateway: Arc<idn_ffmpeg::gateway::Gateway>,
    #[cfg(feature = "ffmpeg")]
    ff: Arc<idn_ffmpeg::Ffmpeg>,
    /// ISO recordings by id (their file destination's id): each input's own encoder.
    #[cfg(feature = "ffmpeg")]
    isos: Mutex<std::collections::BTreeMap<u64, Iso>>,
    /// Changes to the streams, for the long poll.
    version: Mutex<u64>,
    /// Backup ingests (§S.30), by id: the stream they back up.
    pairs: Mutex<std::collections::BTreeMap<u64, u64>>,
    /// Recordings of what a stream sends (§S.30), by id: the stream.
    records: Mutex<std::collections::BTreeMap<u64, u64>>,
    /// Each destination live now: since when (Unix seconds), for the ON AIR / REC timers.
    live_since: Mutex<std::collections::BTreeMap<u64, u64>>,
    /// Streams' names for people (`name`, else the server's host), by id.
    names: Mutex<std::collections::BTreeMap<u64, String>>,
    /// The health watch (§S.30): each stream's and recording's last health.
    watch: Mutex<std::collections::BTreeMap<u64, Watched>>,
    /// The last alerts (§S.30), newest last: the state lists them.
    alerts: Mutex<std::collections::VecDeque<Value>>,
    /// Each stream's and recording's sent rate (unix seconds, kbit/s) over the last two
    /// minutes, sampled by the health watch, by id.
    history: Mutex<std::collections::BTreeMap<u64, std::collections::VecDeque<(u64, u32)>>>,
    /// The machine's Node: alerts go on to the Site Manager.
    node: Option<Arc<idn_open_protocol::node::Node>>,
    /// The switcher's typed events (§S.37), shared with the service.
    events: Arc<idn_open_protocol::web::problems::Events>,
    /// Each destination's counters and typed reason at the last look (§S.37): dropped
    /// packets, skipped pictures, the reason said, failed.
    seen: Mutex<std::collections::BTreeMap<u64, StreamSeen>>,
}

/// A destination at the last look (§S.37): dropped packets, skipped pictures, the typed
/// reason said, failed.
type StreamSeen = (u64, u64, Option<&'static str>, bool);

/// What the health watch knows of one destination (§S.30).
#[derive(Debug, Clone, Default)]
struct Watched {
    /// `"ok"`, `"warning"` or `"error"`, and since when.
    level: &'static str,
    since: Option<Instant>,
    /// The level last alerted (`""`: none, or back to normal).
    alerted: &'static str,
    /// A recording's disk: `"warning"` or `"error"` alerted, else `""`.
    disk: &'static str,
}

/// A stream's video rate never goes under this while it adapts, by default (Mbit/s).
const STREAM_FLOOR_MBPS: f64 = 1.5;
/// How long a problem lasts before it is an alert (a reconnect of a second or two isn't).
const ALERT_AFTER: Duration = Duration::from_secs(4);

/// A recording with an encoder of its own: one input on its own (ISO), or the program with
/// extra sound tracks. Its gateway, the file's destination, and the taps that feed it.
#[cfg(feature = "ffmpeg")]
struct Iso {
    /// The input recorded on its own (ISO), else `None` (the program with tracks, or a mix).
    input: Option<u32>,
    /// The mix streamed or recorded (§S.15), if it is one, and its name: a mix removed
    /// and made again under that name (a setup loaded) is taken up again.
    mix: Option<u32>,
    mix_name: Option<String>,
    gateway: Arc<idn_ffmpeg::gateway::Gateway>,
    dest: u64,
    tap: IsoTaps,
}

/// What feeds an own-encoder recording.
#[cfg(feature = "ffmpeg")]
enum IsoTaps {
    /// Held for their `Drop`: dropping stops what feeds the recording.
    Input { _tap: crate::InputTap },
    Tracks {
        _pictures: idn_open_protocol::api::Tap,
        _sound: crate::TracksTap,
    },
    /// A mix's output (§S.15), streamed or recorded on its own encoder.
    Mix { _tap: idn_open_protocol::api::Tap },
}

pub struct Service {
    sw: Arc<Switcher>,
    /// Saved preset inputs and virtual shots whose camera isn't here yet (§S.21): made
    /// when an input of its label appears, kept in `sources.json` meanwhile.
    pending_shots: Mutex<Vec<Value>>,
    /// Saved inputs that couldn't be made at the start (a video on a drive not there yet):
    /// kept in the saved list, not dropped by the next save (bug round 2026-11-09).
    unrestored: Mutex<Vec<Value>>,
    /// Saved scene items whose input isn't here, by the scene's label: kept the same way.
    unplaced_items: Mutex<std::collections::BTreeMap<String, Vec<Value>>>,
    /// Saved global layers whose input isn't here, by id, as saved (§S.5).
    unplaced_globals: Mutex<std::collections::BTreeMap<u64, Value>>,
    streams: Streams,
    media: Media,
    /// Outputs full screen on this machine's displays (idn-display §D.3).
    displays: crate::displays::Displays,
    /// The show: the cue list and where it is (§S.27), and its last error.
    show: Mutex<crate::show::Runner>,
    /// Errors from cues' actions, by cue (kept until that cue fires again), and a count
    /// of the show's changes (the clients' long poll wakes on it).
    show_errors: Arc<Mutex<Vec<(usize, String)>>>,
    show_version: Arc<std::sync::atomic::AtomicU64>,
    /// The CPU time last looked at: when, how much, and the % worked out then.
    cpu_seen: Mutex<Option<(Instant, Duration, f64)>>,
    /// Its GPU use, looked at most every 500 ms: when, the share, the counters.
    gpu_seen: Mutex<(
        Option<Instant>,
        Option<f64>,
        idn_common::gpu_load::ProcessGpu,
    )>,
    /// Undo and redo of the setup's edits (§S.28).
    undo: Mutex<UndoLog>,
    replaying: AtomicBool,
    /// Counts setups loaded (§S.28): what is kept by label is read again.
    setup_gen: std::sync::atomic::AtomicU64,
    /// The last change applied per client and control over HTTP (per-control sequence
    /// numbers, `X-IDN-Seq`).
    seq: Mutex<idn_open_protocol::web::events::Sequencer>,
    /// The MQTT link (§S.33), started once the API is.
    mqtt: std::sync::OnceLock<Arc<mqtt_link::MqttLink>>,
    /// Every change, who made it and from where (`audit.jsonl` in its folder; §S.35).
    audit: Arc<idn_open_protocol::web::audit::Audit>,
    /// Site-signed tokens for the switcher on this machine, checked with its Node's site
    /// membership (idn-node.md §N.31.5); none without a Node.
    tokens: Option<Arc<idn_open_protocol::web::TokenCheck>>,
    /// Typed failure events (§S.37): the push channel's, the Site Manager's log through
    /// the Node (rate-limited).
    events: Arc<idn_open_protocol::web::problems::Events>,
    /// The failure watch's memory between rounds (§S.37).
    watcher: Mutex<problems::Watcher>,
}

/// One step back or forward (§S.28): the command that does it, and what it is.
#[derive(Debug, Clone)]
struct Step {
    what: String,
    cmd: String,
    body: Value,
}

/// The setup's edits that can be undone (not the live show: cuts, a layer shown or hidden,
/// transitions).
#[derive(Debug, Default)]
struct UndoLog {
    back: Vec<Step>,
    forward: Vec<Step>,
    /// Inputs made again by an undo: their old id → the new one.
    ids: std::collections::BTreeMap<u64, u64>,
}

/// Steps kept each way.
const UNDO_STEPS: usize = 100;
/// The commands undo covers.
const UNDOABLE: &[&str] = &[
    "add_input",
    "remove_input",
    "rename_input",
    "title",
    "scene_items",
    "global_layer",
    "filters",
    "show_set",
    "cue_add",
    "cue_update",
    "cue_delete",
    "cue_move",
];

/// The scenes, kept in the state directory.
const SCENES_FILE: &str = "scenes.json";
/// The global layers (§S.5), their inputs by label.
const GLOBALS_FILE: &str = "global-layers.json";
/// The multiview's layout and whether it is sent (§S.7).
const MULTIVIEW_FILE: &str = "multiview.json";
/// The inputs kept receiving while not in use (§S.3), by label.
const KEEP_ACTIVE_FILE: &str = "keep-active.json";
/// The mixes, kept in the state directory.
const MIXES_FILE: &str = "mixes.json";
/// Titles and slideshows, by label (§S.16).
const SOURCES_FILE: &str = "sources.json";
/// Inputs' filters, by label (§S.17).
const FILTERS_FILE: &str = "filters.json";
/// Cameras' control, by label (§S.21).
const CAMERAS_FILE: &str = "cameras.json";
/// Inputs' sound sync offsets (`audio_offset`, ms), by label (§S.28).
const AUDIO_OFFSETS_FILE: &str = "audio-offsets.json";
/// Scenes' desk snapshots recalled as they go live, by label.
const SCENE_SNAPSHOTS_FILE: &str = "scene-snapshots.json";
/// Title templates, by name; timers (§S.19).
const TEMPLATES_FILE: &str = "templates.json";
const TIMERS_FILE: &str = "timers.json";
/// The monitor's volume, mute, preview and sound card (§S.24).
const MONITOR_FILE: &str = "monitor.json";
/// What each screen shows (idn-display §D.3).
const SCREENS_FILE: &str = "screens.json";
/// The show loaded, and the shows saved by name (§S.27).
const SHOW_FILE: &str = "show.json";
const SHOWS_FILE: &str = "shows.json";
/// Setups saved by name (§S.28): `setups/<name>.json`, the files a setup is made of.
const SETUPS_DIR: &str = "setups";
const SETUP_FILES: &[&str] = &[
    SOURCES_FILE,
    GLOBALS_FILE,
    SCENES_FILE,
    MIXES_FILE,
    MULTIVIEW_FILE,
    KEEP_ACTIVE_FILE,
    FILTERS_FILE,
    CAMERAS_FILE,
    AUDIO_OFFSETS_FILE,
    SCENE_SNAPSHOTS_FILE,
    TIMERS_FILE,
    SHOW_FILE,
];
/// The setup loaded last.
const SETUP_CURRENT: &str = "setup-current.txt";
/// The Planning Center personal access token (this machine only; never in the state).
const PCO_FILE: &str = "planning-center.json";

/// The service's folder (its saved setup, shows, templates; [`ServiceConfig::dir`]), set
/// at its start: one switcher service a process (the engine's rate is the process's too).
static STATE_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

fn state_dir() -> std::path::PathBuf {
    STATE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| idn_common::paths::program_dir("switcher"))
}

/// Media files and stills as inputs: each plays into an output of its own on this machine
/// (not announced), which the switcher takes like any source.
struct Media {
    #[cfg(feature = "ffmpeg")]
    ff: Arc<idn_ffmpeg::Ffmpeg>,
    /// The program's rate: files' pictures above it aren't downloaded or converted.
    #[cfg(feature = "ffmpeg")]
    fps: (u32, u32),
    #[cfg(feature = "ffmpeg")]
    publisher: idn_open_protocol::api::Publisher,
    #[cfg(feature = "ffmpeg")]
    players: Mutex<std::collections::BTreeMap<u32, (String, idn_ffmpeg::player::FilePlayer)>>,
    /// The compositor's GPU: files' pictures decoded there stay there (§E.27).
    #[cfg(all(feature = "ffmpeg", feature = "gpu"))]
    gpu: Option<idn_open_protocol::gpu::GpuHandIn>,
    /// Lists (§S.18), by input: how each was made (for `sources.json`) and its player.
    #[cfg(feature = "ffmpeg")]
    lists: Mutex<std::collections::BTreeMap<u32, (Value, idn_ffmpeg::player::ListPlayer)>>,
    /// Stream inputs (§S.22), by input: the address and its `idn-codec ingest`.
    streams_in: Mutex<std::collections::BTreeMap<u32, (String, crate::streams::StreamIn)>>,
    /// The file inputs that loop (kept with the setup, §S.28).
    #[cfg(feature = "ffmpeg")]
    looping: Mutex<std::collections::BTreeSet<u32>>,
    /// The machine's Node: its media folder (relative names) and the site's library.
    node: Option<Arc<idn_open_protocol::node::Node>>,
    /// The site's library and its catalog, kept 30 s: a list of library items found the
    /// library and fetched its whole catalog for each item.
    library: Mutex<
        Option<(
            std::time::Instant,
            Arc<idn_open_protocol::library::Library>,
            idn_open_protocol::library::Catalog,
        )>,
    >,
}

/// The starters built in (G6, §S.32).
const STARTERS: &str = include_str!("../starters.json");

/// How a program starts the switcher service ([`Service::start`]).
#[derive(Clone)]
pub struct ServiceConfig {
    /// The engine's: name, format, HDR, outputs, GPU.
    pub switcher: SwitcherConfig,
    /// Its folder (saved setup, shows, templates, setups); default the switcher's state
    /// folder (`idn_common::paths::program_dir("switcher")`).
    pub dir: Option<std::path::PathBuf>,
    /// Network sources taken as inputs at the start (before the saved setup is restored).
    pub inputs: Vec<String>,
    /// The machine's IDN Node: site encryption, the desks' feeds, the machine's rate.
    pub node: Option<Arc<idn_open_protocol::node::Node>>,
    /// Its own sound rate, else the machine's given, else asked of the Node, else 48 kHz.
    pub rate: Option<u32>,
    pub machine_rate: Option<u32>,
}

impl Service {
    /// The service's folder (its settings, shows, templates).
    pub fn folder(&self) -> std::path::PathBuf {
        state_dir()
    }
}

#[cfg(test)]
mod tests;
