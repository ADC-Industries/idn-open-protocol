//! The IDN Node's core (spec/extensions/idn-node.md): the machine's settings, keys and site
//! membership, and the requests of the programs connected to it. The `idn-node` service
//! adds the operating-system parts (the listener with its access control, the service
//! registration) and the panel.

use crate::supervisor::BridgeState;
use idn_common::sync::LockOk as _;
use idn_common::sync::RwLockOk as _;
use idn_open_protocol::link::{Identity, LinkConfig};
use idn_open_protocol::node::bridges::{
    Aes67Bridges, AppsSetting, AudioBridges, CaptionsSetting, CaptureBridges, DataSetting,
    ListenSetting, NetAudioSetting, OmtBridges, RoutersSetting, Runs, ServicesNow, ServicesOff,
    SiteLinkSetting, SiteManagerSetting,
};
use idn_open_protocol::node::routing::{
    RouteChange, RouteTable, labels_attr, parse_labels, routes_element,
};
use idn_open_protocol::node::video::VideoSettings;
use idn_open_protocol::node::{MAJOR, is_certificate_verify_input, wire};
use idn_open_protocol::secure::SiteKey;
use idn_open_protocol::trust::pki::{KeyFile, write_private};
use idn_open_protocol::trust::proto::{b64, unb64};
use idn_open_protocol::trust::{DeviceRole, Enrollment, EnrollmentConfig, TrustState};
use idn_open_protocol::xml::Element;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncRead, AsyncWrite};

mod background;
mod settings_file;
use background::*;

mod log_files;
mod netaudio_do;
mod netaudio_pool;
mod netaudio_settings;
mod serve;
mod session_lists;
mod trial;

mod links;

mod crash_reports;
mod pairing;
pub mod problems;
mod requests;
mod routes_of;
mod setting;
mod site_clock;
mod site_here;
mod status_of;

mod config;
mod elements;
mod node_core;
pub(crate) use config::same_path;
pub use config::{Approval, Change, ClockSettings, LAN_PANEL_PORT, ProgramRule, Settings};
use config::{installed_with_node, parse_rules, rules_text};
use elements::*;

/// The default panel port (§N.6).
pub const PANEL_PORT: u16 = idn_common::ports::NODE_PANEL;

/// The machine's rate as last worked out ([`NodeCore::audio_rate`]: every identity, every
/// status): the services' commands carry it (§N.40).
pub(crate) static MACHINE_RATE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(idn_open_protocol::node::rate::DEFAULT);

/// A program's outgoing messages, waiting for its connection's writer, by size: past
/// [`OUTBOX_MAX`] unsent, nothing more is queued and the connection is ended (a program
/// that never read its replies grew the Node by hundreds of MB a second, deep bug run
/// 2026-11-09).
#[derive(Debug, Clone)]
pub struct Outbox {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    waiting: Arc<std::sync::atomic::AtomicUsize>,
    over: Arc<AtomicBool>,
}

/// Bytes a program's outbox may hold unsent.
pub const OUTBOX_MAX: usize = 32 << 20;

impl Outbox {
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Outbox {
        Outbox {
            tx,
            waiting: Arc::default(),
            over: Arc::default(),
        }
    }

    /// Queues `m`; false (and the connection to end) when too much waits already.
    pub fn send(&self, m: String) -> bool {
        let n = m.len();
        if self.over.load(Ordering::Relaxed)
            || self.waiting.fetch_add(n, Ordering::Relaxed) + n > OUTBOX_MAX
        {
            self.over.store(true, Ordering::Relaxed);
            return false;
        }
        self.tx.send(m).is_ok()
    }

    /// The writer took `n` bytes off.
    pub(crate) fn sent(&self, n: usize) {
        self.waiting.fetch_sub(n, Ordering::Relaxed);
    }

    /// Whether it overflowed (the connection is to end).
    pub(crate) fn over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }
}

/// A connected program (§N.4.2), as the panel shows it.
#[derive(Debug, Clone)]
pub struct Client {
    /// The name it gave in `node_hello`.
    pub app: String,
    /// Its process id, as it said and as the operating system reported it.
    pub pid: u32,
    /// The operating system's account of who it runs as.
    pub user: String,
    /// Run as root or elevated ([`Peer::admin`]).
    pub admin: bool,
    /// Its account's number, where the system says (Linux, macOS).
    pub uid: Option<u32>,
    /// Its account (Unix: its uid; Windows: its SIDs, from the connection).
    pub owner: Option<idn_open_protocol::diag::Owner>,
    /// Its executable's full path ("" when the system didn't say).
    pub exe: String,
    /// When it connected.
    pub since: SystemTime,
    /// Its outputs, inputs and routable receivers, by handle.
    pub regs: BTreeMap<String, Reg>,
    /// Messages for this program alone (its receivers' routes).
    #[doc(hidden)]
    pub out: Option<Outbox>,
    /// The services it runs for the session, as it last reported them (`idn session`,
    /// §N.14.4), by id.
    pub services: Option<BTreeMap<String, BridgeState>>,
}

/// One registration of a program (§N.4.2, idn-routing.md §A.3).
#[derive(Debug, Clone, Default)]
pub struct Reg {
    /// `output`, `input` or `audio_rx` (a routable receiver).
    pub kind: String,
    pub name: String,
    pub port: Option<u16>,
    /// An output's sound channels, once known; a receiver's channels.
    pub channels: Option<u16>,
    pub labels: Vec<String>,
    /// A receiver's routed channels as the program reports them: channel → state.
    pub states: BTreeMap<u16, String>,
    /// What a receiver's adapting lanes need, ms, as the program reports it (R.30,
    /// idn-routing.md §A.14): for the site's sync groups.
    pub need_ms: Option<u32>,
    /// A receiver's routed channels with a signal arriving (idn-routing.md §A.15).
    pub signal: std::collections::BTreeSet<u16>,
    /// An output's latest levels for the matrix's meters (dBFS, `-12,-90,…`), and when
    /// they came (§A.15).
    pub levels: Option<(Instant, String)>,
    /// A status the program keeps up to date (a site link's, JSON: idn-remote.md
    /// X.12.5), carried in the report to the Site Manager.
    pub status: Option<String>,
    /// What a receiver is, as its program said (idn-routing.md §A.16; "" not said).
    pub rx_kind: String,
    /// Where an output's sound comes from, as its program said (§A.16).
    pub origin: Option<idn_open_protocol::node::routing::AudioOrigin>,
    /// A receiver's routed channels: the transport their sound comes by (§A.16).
    pub via: BTreeMap<u16, String>,
}

/// An alert for the Site Manager (§N.18), with the typed event it carries (§N.41).
#[derive(Debug, Clone, Default)]
pub struct Alert {
    /// When (Unix seconds).
    pub at: u64,
    pub app: String,
    pub level: String,
    pub text: String,
    /// The event's name ("" for a plain alert) and its fields as JSON ("" for none).
    pub event: String,
    pub detail: String,
}

/// Keeps an alert for the Site Manager (the newest 200).
fn queue_alert(q: &Mutex<VecDeque<Alert>>, a: Alert) {
    let mut q = q.lock_ok();
    if q.len() >= 200 {
        q.pop_front();
    }
    q.push_back(a);
}

/// Who is on the other end of a connection, from the operating system (§N.3).
#[derive(Debug, Clone, Default)]
pub struct Peer {
    pub pid: Option<u32>,
    pub user: String,
    /// Its executable's full path, when the system says (§N.8.1).
    pub exe: Option<String>,
    /// Run as root (Linux) or elevated (Windows: an administrator's, or SYSTEM): may
    /// change who may use the Node (§N.8.1).
    pub admin: bool,
    /// Its account's number (Linux, macOS).
    pub uid: Option<u32>,
    /// Windows: the pipe's handle (its value), to ask who is on the other end once the
    /// program has written (impersonation: bound to the connection, not to a process id
    /// that may be reused; bug run 2026-11-25).
    pub pipe: Option<usize>,
}

/// The site membership: the enrollment when a server is set, and the passphrase key.
#[derive(Default)]
struct Member {
    enrollment: Option<Arc<Enrollment>>,
    /// The site's file channel with this membership (media files, §N.25).
    channel: Option<Arc<idn_open_protocol::channel::Channel>>,
    site: Option<Arc<SiteKey>>,
    /// Why there is no enrollment despite a server being set.
    error: Option<String>,
}

/// The site clock while it runs, its ports, and why it doesn't when it can't.
type ClockSlot = (
    Option<idn_open_protocol::ptp::Clock>,
    (u16, u16),
    Option<String>,
);

/// The Node.
pub struct NodeCore {
    dir: PathBuf,
    settings: Mutex<Settings>,
    /// Held through each change of the settings: a change starts from a copy and writes
    /// it back, so two at once (the panel's Switch and the Site Manager) would lose one.
    changing: Mutex<()>,
    device_key: KeyFile,
    device_open: Arc<Identity>,
    link: LinkConfig,
    member: RwLock<Member>,
    clients: Mutex<BTreeMap<u64, Client>>,
    next: AtomicU64,
    events: tokio::sync::broadcast::Sender<String>,
    log: Mutex<VecDeque<String>>,
    /// Panel tokens: (token, issued, full: may change who uses the Node; F6-1).
    tokens: Mutex<Vec<(String, Instant, bool)>>,
    /// Opens IDN Data's panel from this one: its key goes to IDN Data in its environment.
    data_panel: idn_open_protocol::web::HandOff,
    stop: AtomicBool,
    started: Instant,
    /// Site Managers found on the LAN while no server is set (§N.11.4).
    sites: Mutex<Vec<idn_open_protocol::discovery::site::SiteInfo>>,
    /// Audio routes of this machine's receivers (`routes.xml`, idn-routing.md §A.2).
    routes: Mutex<RouteTable>,
    /// Receivers' sync-group floors the Site Manager set (§A.14), ms: kept while the Node
    /// runs (not saved: the site sends them again), told with the routes.
    floors: Mutex<BTreeMap<String, u32>>,
    /// When programs were last told a matrix watches the levels (§A.15).
    levels_told: Mutex<Option<Instant>>,
    /// Relayed pairings asked by programs here (§N.42.3): ticket → the program that asked
    /// (its file, and its connection)
    /// (only it is answered; the newest 64 kept).
    key_tickets: Mutex<BTreeMap<u64, (String, u64)>>,
    /// Goes up with every change to whether services run (a restart's "on again" is
    /// skipped when another change came in meanwhile).
    service_changes: std::sync::atomic::AtomicU64,
    /// The site clock while it runs, its ports, and why it doesn't when it can't.
    clock: Mutex<ClockSlot>,
    /// The clock's offset sampled once a second while it is locked, the last minute's:
    /// its jitter (N5, §N.30).
    offsets: Mutex<std::collections::VecDeque<(std::time::Instant, f64)>>,
    /// The services the Node service runs itself (the site link, AES67), by id.
    node_services: Mutex<BTreeMap<String, BridgeState>>,
    /// Wrong simple-mode PINs in a row, and since when they are refused (§N.20).
    pin_lock: idn_open_protocol::web::PinLock,
    /// Look for a Site Manager now, not at the next half minute (the wizard, §N.22).
    search_now: AtomicBool,
    /// An update being fetched (§N.23); after that, the files say where it is.
    update: Mutex<Option<crate::update::Progress>>,
    /// The site's newest release for this machine as last asked (`check_updates`,
    /// idn-node.md §N.42): its version (empty: none) and when the answer came.
    update_available: Mutex<Option<(String, u64)>>,
    /// Runs as the system's service (an update's helper stops and starts it so).
    service: AtomicBool,
    /// Programs' alerts not yet passed to the Site Manager (§N.18): when (Unix seconds),
    /// the program, the level, the text, and a typed event's name and fields (§N.41; ""
    /// for a plain alert). The newest 200.
    alerts: Arc<Mutex<VecDeque<Alert>>>,
    /// Typed failure events (§N.41): this Node's own and its programs', for its push
    /// channel; its own and the programs' go on to the Site Manager rate-limited.
    pub(crate) problem_events: Arc<idn_open_protocol::web::problems::Events>,
    /// What the Node's own watch found wrong last (§N.41), by problem name.
    problems_now: Mutex<BTreeMap<String, serde_json::Value>>,
    /// The media folder and the files to and from other machines (§N.25).
    media: Arc<crate::media::Media>,
    /// Linked computers (§N.32, idn-extensions.md §E.28).
    pairs: Arc<crate::pairs::Pairs>,
    /// This Node, weakly: what runs later (a link made) tells programs through it.
    me: std::sync::OnceLock<std::sync::Weak<NodeCore>>,
    /// The programs with sound open in the session (for "Programs' sound"): asked for by
    /// the panel (until when), and the session runner's last list (when, what).
    audio_apps: Mutex<AudioApps>,
    /// A site found on the network and asked to join, not kept yet (the
    /// address is saved only once the site approves this machine and it
    /// joins): its address, and since when it hasn't answered (to give up on it).
    trial_site: Mutex<Option<TrialSite>>,
    /// VB-CABLE's install asked for from the panel (its number, and when): the session
    /// agent (`idn-session`) runs VB-Audio's setup in the user's session, elevated (Windows asks
    /// to trust the driver there, never in the service's session).
    vbcable_request: Mutex<Option<(u64, Instant)>>,
}

/// See [`NodeCore::trial_site`].
#[derive(Clone)]
pub struct TrialSite {
    pub server: String,
    pub name: String,
    /// Since when it hasn't been reachable (or approving), if so.
    pub failing_since: Option<Instant>,
}

/// See [`NodeCore::want_audio_apps`].
#[derive(Default)]
pub struct AudioApps {
    wanted_until: Option<Instant>,
    at: Option<u64>,
    /// (executable, name, playing).
    list: Vec<(String, String, bool)>,
    /// This session's displays (`<display …/>` as the session runner sent them), and when.
    displays: Vec<Element>,
    displays_at: Option<u64>,
    /// The encoders that work in the session (`<encoder processor codec by/>`), and when.
    encoders: Vec<Element>,
    encoders_at: Option<u64>,
}

impl NodeCore {}

impl std::fmt::Debug for NodeCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeCore({})", self.dir.display())
    }
}

fn state_name(s: &TrustState) -> &'static str {
    match s {
        TrustState::Starting => "starting",
        TrustState::NoAuthority => "no_authority",
        TrustState::WaitingForPairing => "waiting",
        TrustState::Pending { .. } => "pending",
        TrustState::ConfirmCode { .. } => "confirm",
        TrustState::CodeNeeded => "code_needed",
        TrustState::CodeAccepted => "code_accepted",
        TrustState::WrongCode { .. } => "wrong_code",
        TrustState::CodeExpired => "code_expired",
        TrustState::Enrolled { .. } => "enrolled",
        TrustState::Denied => "denied",
        TrustState::Revoked => "revoked",
        TrustState::Error(_) => "error",
    }
}

#[cfg(test)]
mod tests;
