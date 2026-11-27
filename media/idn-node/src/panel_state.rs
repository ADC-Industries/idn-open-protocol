//! The Node panel's status (`GET /api/v1/status`, `GET /api/v1/state`, the push channel's
//! `state`), typed: the panel makes it as [`NodeStatus`], so every key is declared, and its
//! schema is `GET /api/v1/state_schema`. Parts that are another module's own (the routing
//! matrix, the network's sources, drivers' states, the problems) stay JSON.
//!
//! A client reads it with `serde_json::from_value::<NodeStatus>`; a key added later is
//! skipped.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Node's status as its panel and API give it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeStatus {
    /// Moves when anything else in the status changes (`?after=VERSION`).
    pub version: u64,
    /// The Node's own version (it was `version` until 2026-11-19) and build.
    pub node_version: String,
    pub build: String,
    /// The machine's name and fingerprint.
    pub name: String,
    pub fingerprint: String,
    /// The site's server (empty: none) and the network interface used ("" any).
    pub server: String,
    pub interface: String,
    /// Encryption required, a passphrase set, its key's id.
    pub require: bool,
    pub passphrase_set: bool,
    pub key_id: String,
    /// The site membership: `none`, `pending`, `enrolled`, …; connected to the site now.
    pub state: String,
    pub connected: bool,
    /// The comparison code while joining; a typed join code's tries left; how the site
    /// takes machines (`approval`, `code_or_approval`, `code_and_approval`).
    pub code: String,
    pub code_left: Option<u32>,
    pub join_mode: String,
    /// The site's devices, from its directory (§N.42); null until known.
    pub site_devices: Option<u32>,
    /// The first-run wizard was answered.
    pub welcomed: bool,
    /// The simple-mode PIN and the site's lock.
    pub panel: PanelLock,
    pub error: String,
    /// The caller is on the network (the LAN panel or a token), not this machine.
    #[serde(default)]
    pub lan: bool,
    /// The LAN panel: on, its port.
    pub lan_panel: bool,
    pub lan_port: u16,
    /// Joins a site found on the network by itself.
    pub auto_site: bool,
    /// This computer's sound bridges (§N.14), run by the session agent.
    pub bridges: Bridges,
    /// Screens and webcams as IDN sources (R.19).
    pub capture: Capture,
    /// The link to another location (R.18).
    pub sitelink: SiteLink,
    /// Listening on phones (§N.19).
    pub listen: Listen,
    /// IDN Data (§N.24).
    pub data: DataService,
    /// Live captions (§N.28).
    pub captions: CaptionsService,
    /// A network-audio device and the desk in its rings (R.33); "" or false: the default.
    pub netaudio: NetAudio,
    /// The switcher, the mixer and the display service started at log-on (§N.14.4).
    pub apps: Apps,
    /// Routers (§N.29): `Name=SOURCE; …`.
    pub routers: String,
    /// Open Media Transport bridges (§N.27).
    pub omt: Omt,
    /// The media folder and files between machines (§N.25); null: none.
    pub media: Value,
    /// AES67 streams (§N.14.5) and the last search's.
    pub aes67: Aes67,
    /// Everything that runs, with its switches and state (§N.14.4).
    pub services: Vec<ServiceState>,
    /// `linux`, `windows`, `macos`.
    pub os: String,
    /// What is wrong now (§N.41.3): `{problem, level, text, …}`.
    pub problems: Vec<Value>,
    /// The machine's video settings (§N.13); "" where unset.
    pub video: VideoSettings,
    /// The site clock (PTP) now; null: not running.
    pub clock: Option<ClockState>,
    /// The sample rate: the machine's own (null: the site's), in use, from where (§N.40).
    pub audio_rate: AudioRate,
    pub clock_settings: ClockSettings,
    /// Sites found on the network.
    pub sites: Vec<SiteFound>,
    /// The routing matrix's data (idn-routing.md §A.6).
    pub audio: Value,
    pub uptime_s: u64,
    /// The programs connected to the Node.
    pub programs: Vec<Program>,
    /// Per-program approval (§N.8.1): on, its rules.
    pub approve_programs: bool,
    pub program_rules: Vec<ProgramRule>,
    /// Link requests from other computers taken (§N.32).
    pub link_requests: bool,
    /// The network's sources (the panel's lists).
    pub network: Value,
    /// Linked computers (§N.32): on, the links, requests.
    pub pairs: Value,
    /// The site's options for this machine.
    pub options: NodeOptions,
    /// The panel's log, newest last.
    pub log: Vec<String>,
    /// Kept awake (no sleep while it runs).
    pub keep_awake: bool,
    /// A site found and asked to join, not kept yet; its name.
    pub trial_server: String,
    pub trial_name: String,
    /// An IDN Appliance.
    pub appliance: bool,
    /// The site's name, where known.
    pub site_name: Option<String>,
    /// This computer's network adapters.
    pub interfaces: Vec<Interface>,
    /// Behringer's X-USB driver installed (Windows); null where none is needed.
    pub x32_driver: Option<bool>,
    /// VB-CABLE (Windows): installed, packaged, asked for; null elsewhere.
    pub vbcable: Value,
    /// IDN's virtual sound card (Windows 11); null elsewhere.
    pub vsound: Value,
    /// Virtual screens (§N.36).
    pub vscreens: Value,
    /// The encoders that work in the session, the displays, the programs with sound open
    /// (the session agent's lists); null until it says.
    pub encoders: Value,
    pub displays: Value,
    pub audio_apps: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PanelLock {
    /// A simple-mode PIN is set; it is the site's.
    pub pin: bool,
    pub site_only: bool,
    /// Locked by the site (§N.31): changes from the Site Manager only.
    pub locked: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Bridges {
    pub mic: bool,
    pub mic_name: String,
    pub mic_device: String,
    pub apps: String,
    pub speaker: bool,
    pub speaker_name: String,
    pub x32: bool,
    pub x32_name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Capture {
    pub screens: String,
    pub webcams: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SiteLink {
    pub mode: String,
    pub address: String,
    pub port: String,
    pub limit: String,
    pub groups: String,
    pub sound: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Listen {
    pub channels: String,
    pub port: String,
    pub title: String,
    pub kbps: String,
    /// The listening page's address as phones reach it.
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DataService {
    pub on: bool,
    pub library: bool,
    pub port: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CaptionsService {
    pub on: bool,
    pub source: String,
    pub model: String,
    pub words: String,
    pub name: String,
    /// The language spoken (whisper's code; empty: English).
    #[serde(default)]
    pub lang: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NetAudio {
    pub on: bool,
    pub device: String,
    pub channels: String,
    pub latency: String,
    pub receivers_latency: String,
    pub interface: String,
    pub desk: bool,
    pub desk_name: String,
    pub desk_block: String,
    pub desk_outputs: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Apps {
    pub switcher: bool,
    pub mixer: bool,
    pub display: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Omt {
    pub sends: String,
    pub recvs: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Aes67 {
    pub sends: String,
    pub recvs: String,
    /// The last search: running, when, the streams heard, its error.
    pub found: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServiceState {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub runs: String,
    /// Running now (until the Node starts again), and starting by itself.
    pub on: bool,
    pub auto: bool,
    pub state: String,
    pub restarts: Option<u32>,
    pub for_s: Option<u64>,
    /// Its last line, and who switched it.
    pub line: String,
    pub by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VideoSettings {
    pub threads: String,
    pub parallel_decode: String,
    pub encoding: String,
    pub decoding: String,
    pub ndi: String,
    pub precision: String,
    pub processor: String,
    pub format: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClockState {
    pub state: String,
    pub locked: bool,
    /// The grandmaster.
    pub gm: String,
    pub offset_ns: Option<f64>,
    pub delay_ns: Option<f64>,
    pub domain: String,
    pub ports: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AudioRate {
    /// The machine's own (null: the site's).
    pub setting: Option<u32>,
    pub hz: u32,
    /// Where the rate in use came from.
    pub from: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClockSettings {
    pub on: bool,
    pub domain: u8,
    pub protocol: String,
    pub priority: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SiteFound {
    pub name: Option<String>,
    pub server: Option<String>,
    pub ca: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Program {
    pub app: Option<String>,
    pub pid: Option<String>,
    pub user: Option<String>,
    pub exe: Option<String>,
    /// Per-program approval: `allowed`, `waiting`, `blocked`.
    pub approval: Option<String>,
    pub since: Option<u64>,
    /// Its outputs and inputs.
    pub registrations: Vec<Registration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Registration {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProgramRule {
    pub path: Option<String>,
    pub allow: bool,
    pub app: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeOptions {
    pub version: u64,
    /// `{key, value}` each.
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Interface {
    pub name: String,
    pub addr: String,
    pub up: bool,
    pub physical: bool,
    pub speed_mbps: Option<u64>,
}

/// [`NodeStatus`]'s JSON schema.
pub fn state_schema() -> Value {
    schemars::schema_for!(NodeStatus).to_value()
}
