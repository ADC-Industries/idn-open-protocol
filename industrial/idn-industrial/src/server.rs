//! IDN Site Manager | Industrial (§I.12): the company's site authority, the links from
//! its machines (QUIC and WebSocket), enrolment by token, and what the machines send,
//! stored before it is acknowledged and handed to the application server's side (the
//! MQTT broker, §I.7).

use crate::mqtt::{Broker, Hook, Msg};
use crate::store;
use crate::token::{self, Token};
use crate::transport::{self, Link};
use crate::wire::{self, Item};
use idn_common::sync::LockOk as _;
use idn_open_protocol::trust::{Authority, AuthorityConfig};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

use idn_common::err::other as err;

use idn_common::text::base64 as b64;

fn unb64(s: &str) -> Option<Vec<u8>> {
    idn_common::text::unbase64(s.as_bytes())
}

/// Tokens by id (hex).
const TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("tokens");
/// Machines by fingerprint.
const MACHINES: TableDefinition<&str, &[u8]> = TableDefinition::new("machines");
/// Item ids seen (machine fingerprint + id), with when: repeats are answered as such.
const SEEN: TableDefinition<&[u8], i64> = TableDefinition::new("seen");
/// What machines sent, by arrival: machine fingerprint (64 bytes), arrival time (8),
/// then the item (store::encode), until the application's side has it.
const INBOX: TableDefinition<u64, &[u8]> = TableDefinition::new("inbox");
/// Items waiting for a machine (down the link), in order: fingerprint (64 bytes), then
/// the sequence (u64 BE); the item (store::encode).
const OUTBOUND: TableDefinition<&[u8], &[u8]> = TableDefinition::new("outbound");
/// Module settings pushed to machines (§I.8): `machine:<fingerprint>:<name>` or
/// `tag:<key>=<value>:<name>` → the file (TOML).
const MODULES: TableDefinition<&str, &str> = TableDefinition::new("modules");

/// Web-UI users and their roles (§I.11.1): name → JSON `{hash, role}`.
const USERS: TableDefinition<&str, &str> = TableDefinition::new("users");

/// Commands from applications held until answered or expired (§I.7.3), by the command
/// item's id (hex) → JSON ([`broker_side::Held`]); taken up again after a restart.
const COMMANDS: TableDefinition<&str, &[u8]> = TableDefinition::new("commands");
/// Items applications refused (§I.7.5), by a sequence → JSON; never dropped.
const DEAD: TableDefinition<u64, &[u8]> = TableDefinition::new("dead_letters");
/// Messages the application's side has taken, kept a while so a later refusal still has
/// the item (§I.7.5): a sequence → the inbox record; `RECENT_IDS`: item id → sequence.
const RECENT: TableDefinition<u64, &[u8]> = TableDefinition::new("recent");
const RECENT_IDS: TableDefinition<&[u8], u64> = TableDefinition::new("recent_ids");
/// The machine's tags when an inbox item arrived (`plant=PD,role=batch`), by its sequence:
/// stamped on its MQTT messages (§I.7.2), so a re-tag doesn't change items already queued.
const INBOX_TAGS: TableDefinition<u64, &str> = TableDefinition::new("inbox_tags");

/// A machine's tags as one property: `key=value` pairs, comma separated, in key order.
pub fn tags_text(tags: &BTreeMap<String, String>) -> String {
    tags.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The append-only audit (§I.11.6): a sequence → one JSON record.
const AUDIT: TableDefinition<u64, &[u8]> = TableDefinition::new("audit");
/// Counters: `inbox`, `outbound` (the last sequence numbers given).
const COUNTS: TableDefinition<&str, u64> = TableDefinition::new("counts");

/// How long item ids are remembered for repeats (§I.6.3).
const SEEN_FOR_MS: i64 = 24 * 3600 * 1000;

/// Settings for [`SiteManager::start`].
#[derive(Debug, Clone)]
pub struct Config {
    /// The volume: CA, devices, tokens, machines, the inbox.
    pub data_dir: PathBuf,
    /// UDP for QUIC links (`None`: WebSocket only).
    pub quic: Option<SocketAddr>,
    /// The addresses machines are given in tokens: `idn://host:29206`,
    /// `wss://host/idn/link`.
    pub public_urls: Vec<String>,
    /// TCP for MQTT clients (`None`: MQTT over the WebSocket at `/mqtt` only).
    pub mqtt: Option<SocketAddr>,
}

/// A token as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub id: String,
    secret: String,
    /// The name the machine gets (else its own).
    pub name: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// The machine that used it.
    pub used_by: Option<String>,
}

/// A machine as stored and listed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    pub fingerprint: String,
    pub name: String,
    /// Its name in MQTT topics (`idn/<topic>/…`): the name, made safe and unique.
    #[serde(default)]
    pub topic: String,
    pub edition: String,
    pub tags: BTreeMap<String, String>,
    pub enrolled_ms: i64,
    pub last_seen_ms: i64,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub revoked: bool,
    /// Now connected, by which transport, from where (not stored).
    #[serde(skip_deserializing)]
    pub online: Option<Online>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Online {
    pub transport: String,
    pub address: String,
    pub since_ms: i64,
    /// The machine's clock against the Site Manager's when it connected (ms; positive:
    /// the machine is behind), within the hello's travel time (§I.6.3).
    pub clock_offset_ms: i64,
}

/// Something that happened, for the application's side and the web UI.
#[derive(Debug, Clone)]
pub enum Event {
    /// An item stored in the inbox (its sequence number).
    Stored(u64),
    Online(String),
    Offline(String),
    Enrolled(Box<Machine>),
    Revoked(String),
}

struct Shared {
    cfg: Config,
    authority: Authority,
    tls: Arc<rustls::ServerConfig>,
    db: Database,
    /// Connected machines: their link, its details and a number telling links apart.
    online: Mutex<HashMap<String, (Link, Online, u64)>>,
    events: broadcast::Sender<Event>,
    broker: Broker,
    /// Machines' requests waiting for the application's answer, by item id (hex).
    answers: Mutex<HashMap<String, Answer>>,
    /// Kept inbox items were delivered up to here; a new subscriber has them sent again
    /// to persistent sessions only.
    rescan: tokio::sync::Notify,
    /// Machines' batches waiting to be stored, each with where its answer goes: those that
    /// arrive while a commit runs are stored together in the next (one disk sync for
    /// many machines' batches).
    #[allow(clippy::type_complexity)]
    store_waiting: Mutex<
        Vec<(
            String,
            Vec<Item>,
            std::sync::mpsc::SyncSender<io::Result<Vec<Option<u64>>>>,
        )>,
    >,
    /// Held by whoever stores.
    store_committing: Mutex<()>,
    /// Wakes a machine's down-link delivery, by fingerprint.
    downlink: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    /// Running remote-support screen sessions (§I.11.3), by session id.
    screens: Mutex<HashMap<String, Arc<ScreenSession>>>,
    /// Days a message no application has taken is held (§I.7.3); 0: until one takes it.
    hold_days: std::sync::atomic::AtomicU32,
    /// IDN Voice's hub (§I.18), for its MQTT commands; weak: Voice holds this Site Manager.
    voice: Mutex<Option<std::sync::Weak<idn_voice::hub::Hub>>>,
    /// Voice over IDN's link (idn-voice.md §V.14), held while the Site Manager runs.
    voice_link: Mutex<Option<idn_voice::VoiceLink>>,
}

/// The hold when nothing is set: 7 days.
pub const HOLD_DAYS: u32 = 7;

/// The site's own settings on the volume (`settings.json`), set in the web UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SiteSettings {
    #[serde(default = "default_hold")]
    hold_days: u32,
}

fn default_hold() -> u32 {
    HOLD_DAYS
}

/// The saved settings; the defaults when there are none. A damaged file is set aside
/// (`settings.json.bad-SECS-PID-N`) and logged, not read as none and then saved over.
fn load_settings(dir: &std::path::Path) -> SiteSettings {
    let defaults = || SiteSettings {
        hold_days: HOLD_DAYS,
    };
    match idn_common::fsutil::load_json(&dir.join("settings.json")) {
        Ok(s) => s.unwrap_or_else(defaults),
        Err(e) => {
            idn_open_protocol::log::log(&format!("site manager: {e}"));
            defaults()
        }
    }
}

/// Who may do what in the web UI (§I.11.1): shared with the media Site Manager.
pub use idn_open_protocol::web::Role;

mod broker_side;
mod dead_letters;
mod downlink;
mod enrol;
mod machine_links;
mod machine_modules;
mod screens;
mod updates;
mod users_audit;

/// What a browser needs to watch a session (§I.11.3): a receiver for the frames, the
/// latest keyframe to start on, whether control is allowed, and the machine.
pub type ScreenWatch = (
    broadcast::Receiver<Arc<Vec<u8>>>,
    Option<Arc<Vec<u8>>>,
    bool,
    String,
);

/// A running screen session: the pictures fan out to the browsers watching it, and the
/// latest keyframe is kept so a browser that joins can start decoding at once.
struct ScreenSession {
    fp: String,
    /// Browser-ready frame records (`[u8 keyframe][i64 pts_ms LE][H.264]`).
    frames: broadcast::Sender<Arc<Vec<u8>>>,
    last_keyframe: Mutex<Option<Arc<Vec<u8>>>>,
    control: bool,
    user: String,
    started_ms: i64,
    /// Browsers watching now: the session ends (and the machine stops capturing) when the
    /// last one leaves, so a screen is captured only while someone watches (§I.11.3).
    viewers: std::sync::atomic::AtomicUsize,
}

/// Where a machine's request waits for the application's (status, payload).
type Answer = tokio::sync::oneshot::Sender<(u8, Vec<u8>)>;

/// The Site Manager. Cheap to clone.
#[derive(Clone)]
pub struct SiteManager(Arc<Shared>);

fn db_err(e: impl std::fmt::Display) -> io::Error {
    err(format!("database: {e}"))
}

impl SiteManager {
    /// Opens the volume (creating the site CA on first start) and starts listening on
    /// QUIC; the WebSocket side is [`SiteManager::router`], served by the caller's HTTP
    /// server.
    pub async fn start(cfg: Config) -> io::Result<SiteManager> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let authority = Authority::open(AuthorityConfig {
            state_dir: cfg.data_dir.join("authority"),
            passphrase: None,
        })?;
        let tls = Arc::new(crate::tls::server_config(
            &authority.server_creds(),
            &authority.ca_der(),
        )?);
        let db = Database::create(cfg.data_dir.join("site.redb")).map_err(db_err)?;
        {
            let w = db.begin_write().map_err(db_err)?;
            w.open_table(TOKENS).map_err(db_err)?;
            w.open_table(MACHINES).map_err(db_err)?;
            w.open_table(SEEN).map_err(db_err)?;
            w.open_table(INBOX).map_err(db_err)?;
            w.open_table(COUNTS).map_err(db_err)?;
            w.open_table(OUTBOUND).map_err(db_err)?;
            w.open_table(MODULES).map_err(db_err)?;
            w.open_table(USERS).map_err(db_err)?;
            w.open_table(AUDIT).map_err(db_err)?;
            w.open_table(COMMANDS).map_err(db_err)?;
            w.open_table(DEAD).map_err(db_err)?;
            w.open_table(RECENT).map_err(db_err)?;
            w.open_table(RECENT_IDS).map_err(db_err)?;
            w.open_table(INBOX_TAGS).map_err(db_err)?;
            w.commit().map_err(db_err)?;
        }
        let (events, _) = broadcast::channel(1024);
        let (broker, hooks) = Broker::open(&cfg.data_dir.join("mqtt.redb"))?;
        let sm = SiteManager(Arc::new(Shared {
            cfg: cfg.clone(),
            authority,
            tls,
            db,
            online: Mutex::new(HashMap::new()),
            events,
            broker,
            answers: Mutex::new(HashMap::new()),
            rescan: tokio::sync::Notify::new(),
            store_waiting: Mutex::new(Vec::new()),
            store_committing: Mutex::new(()),
            downlink: Mutex::new(HashMap::new()),
            screens: Mutex::new(HashMap::new()),
            hold_days: std::sync::atomic::AtomicU32::new(load_settings(&cfg.data_dir).hold_days),
            voice: Mutex::new(None),
            voice_link: Mutex::new(None),
        }));
        if let Some(addr) = cfg.mqtt {
            let l = tokio::net::TcpListener::bind(addr).await?;
            idn_open_protocol::log::log(&format!("site manager: MQTT on TCP {}", l.local_addr()?));
            let b = sm.0.broker.clone();
            tokio::spawn(async move {
                // One failed accept (a client reset before it was taken, too many open
                // files) isn't the end of the broker: it stopped for good, the panel still
                // saying it listened (deep bug run 2026-11-09).
                loop {
                    let (s, from) = match l.accept().await {
                        Ok(x) => x,
                        Err(e) => {
                            idn_open_protocol::log::log(&format!(
                                "mqtt: a connection not taken: {e}"
                            ));
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            continue;
                        }
                    };
                    let _ = s.set_nodelay(true);
                    let b = b.clone();
                    tokio::spawn(async move { b.serve(s, from.to_string()).await });
                }
            });
        }
        // Every machine is offline until it connects.
        for m in sm.machines().unwrap_or_default() {
            sm.publish_status(&m.fingerprint);
        }
        tokio::spawn(sm.clone().deliver());
        tokio::spawn(sm.clone().hooks(hooks));
        // Commands held when it stopped: on their way again (the machine's claim keeps
        // one from running twice).
        sm.resume_commands();
        tokio::spawn(sm.clone().announce());
        if let Some(addr) = cfg.quic {
            let q = transport::QuicServer::bind(addr, (*sm.0.tls).clone())?;
            idn_open_protocol::log::log(&format!("site manager: QUIC on UDP {}", q.local_addr()?));
            let me = sm.clone();
            tokio::spawn(async move {
                while let Some(link) = q.accept().await {
                    let me = me.clone();
                    tokio::spawn(async move { me.link(link).await });
                }
            });
        }
        // Old item ids forgotten every hour.
        let me = sm.clone();
        tokio::spawn(async move {
            loop {
                let _ = me.prune_seen();
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        Ok(sm)
    }

    /// The volume.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.0.cfg.data_dir
    }

    /// Days a message no application has taken is held before it goes (§I.7.3), on the
    /// Site Manager's own clock; 0: held until an application takes it.
    pub fn hold_days(&self) -> u32 {
        self.0.hold_days.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Sets the hold (at most 10 years), kept on the volume.
    pub fn set_hold_days(&self, days: u32) -> io::Result<()> {
        if days > 3650 {
            return Err(idn_common::err::invalid_input(
                "the hold is 0 (until taken) to 3650 days",
            ));
        }
        let mut s = load_settings(&self.0.cfg.data_dir);
        s.hold_days = days;
        idn_common::fsutil::write_atomic(
            &self.0.cfg.data_dir.join("settings.json"),
            &serde_json::to_vec_pretty(&s).unwrap_or_default(),
        )?;
        self.0
            .hold_days
            .store(days, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// The addresses tokens give machines.
    pub fn public_urls(&self) -> &[String] {
        &self.0.cfg.public_urls
    }

    /// The site CA's fingerprint.
    pub fn ca_fingerprint(&self) -> String {
        self.0.authority.ca_fingerprint()
    }

    pub fn authority(&self) -> &Authority {
        &self.0.authority
    }

    /// Events as they happen.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }

    /// The MQTT broker (§I.7).
    pub fn broker(&self) -> &Broker {
        &self.0.broker
    }

    // ---- tokens (§I.5)

    // ---- machines

    // ---- the inbox

    // ---- commands to machines

    // ---- links

    // ---- module settings (§I.8)

    // ---- down the link

    // ---- MQTT (§I.7)

    // ---- users and roles (§I.11.1)

    // ---- the audit (§I.11.6)

    // ---- remote-support screen sessions (§I.11.3)

    /// The HTTP routes: `/healthz`, the WebSocket link at `/idn/link`, and the API under
    /// `/api` (bearer `api_key`).
    pub fn router(&self, api_key: String) -> axum::Router {
        crate::http::router(self.clone(), api_key)
    }

    /// IDN Voice started on this Site Manager (§I.18): its MQTT commands go to it.
    pub(crate) fn set_voice(&self, hub: &Arc<idn_voice::hub::Hub>) {
        *self.0.voice.lock_ok() = Some(Arc::downgrade(hub));
    }

    /// Voice's link listener, kept for the Site Manager's life (dropped, it stops).
    pub(crate) fn hold_voice_link(&self, link: idn_voice::VoiceLink) {
        *self.0.voice_link.lock_ok() = Some(link);
    }

    pub(crate) fn tls(&self) -> Arc<rustls::ServerConfig> {
        self.0.tls.clone()
    }
}

/// A value's name as topic levels: `/` keeps its meaning; `+`, `#` and empty levels
/// can't be in a topic.
fn point_topic(name: &str) -> Option<String> {
    let levels: Vec<String> = name
        .split('/')
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.chars()
                .map(|c| if c == '+' || c == '#' { '_' } else { c })
                .collect()
        })
        .collect();
    (!levels.is_empty()).then(|| levels.join("/"))
}

fn user_prop<'a>(m: &'a Msg, k: &str) -> Option<&'a str> {
    m.props
        .user
        .iter()
        .find(|(a, _)| a == k)
        .map(|(_, v)| v.as_str())
}

fn storable(op: u8) -> bool {
    matches!(op, wire::OP_MESSAGE | wire::OP_VALUES | wire::OP_FILE)
}
