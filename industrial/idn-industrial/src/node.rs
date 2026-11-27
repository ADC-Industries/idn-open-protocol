//! The IDN Node | Industrial's link to its Site Manager (§I.4–§I.6): joining with a
//! token, the connection (QUIC first, then the WebSocket), the outbox drained as the
//! Site Manager stores each item, and requests from the Site Manager handed to the
//! machine's program.

use crate::store::Outbox;
use crate::transport::{self, Link};
use crate::wire::{self, Item};
use idn_common::backoff::Backoff;
use idn_common::sync::LockOk as _;
use idn_open_protocol::trust::pki::{DeviceCreds, KeyFile};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};

/// The edition this build reports (§I.2).
pub const EDITION: &str = "industrial";
/// Items sent together on one stream, in order.
const BATCH: usize = 64;
const BATCH_BYTES: usize = 4 << 20;
/// How long the Node's program has to answer a request from the Site Manager: under the
/// Site Manager's own 60 s wait, so its answer (not a cut connection) is what arrives; it
/// was 30 s, under an action's own 60 s and SQL's 40 s (deep bug run 2026-11-09).
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(55);
/// Pings while connected (the WebSocket through a tunnel must not look idle).
const PING_EVERY: Duration = Duration::from_secs(20);
/// While on the WebSocket, QUIC is tried again this often (§I.4.3).
const QUIC_RETRY: Duration = Duration::from_secs(300);
/// How long QUIC gets before the WebSocket is tried.
const QUIC_FIRST: Duration = Duration::from_secs(2);

use idn_common::err::other as err;

/// The runtime the Node's link runs on (two threads: it moves messages, not pictures).
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("idn-industrial")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Settings for [`IndustrialNode::start`].
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Where the key, the site's certificates and the outbox are kept.
    pub state_dir: PathBuf,
    /// The machine's name, offered at joining (a token may set another).
    pub name: String,
    /// This software's version, reported to the Site Manager.
    pub version: String,
}

/// The site this machine joined (`site.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Site {
    urls: Vec<String>,
    /// Base64 DER.
    ca: String,
    cert: String,
    crl: String,
    name: String,
    #[serde(default)]
    tags: BTreeMap<String, String>,
}

/// A request from the Site Manager (a command from the application server) for the
/// machine's program; answer it with [`Command::answer`] within [`ANSWER_TIMEOUT`].
pub struct Command {
    pub kind: String,
    pub payload: Vec<u8>,
    /// The item's id (hex), unique per command.
    pub id: String,
    reply: oneshot::Sender<(u8, Vec<u8>)>,
}

impl Command {
    /// `ok`: the answer's payload; otherwise the payload says why it failed.
    pub fn answer(self, ok: bool, payload: Vec<u8>) {
        let _ = self
            .reply
            .send((if ok { wire::ST_OK } else { wire::ST_ERROR }, payload));
    }

    /// Nobody waits for the answer any more (the Node gave up on it, [`ANSWER_TIMEOUT`]):
    /// not to be run — the Site Manager was told it didn't happen in time (deep bug run
    /// 2026-11-09: a queued plant command ran minutes after its timeout).
    pub fn given_up(&self) -> bool {
        self.reply.is_closed()
    }

    /// Not a type this program handles.
    pub fn unknown(self) {
        let _ = self.reply.send((wire::ST_UNKNOWN, Vec::new()));
    }

    /// Answers with a raw status and payload (for handlers that choose the status, like
    /// remote support).
    pub fn respond(self, status: u8, payload: Vec<u8>) {
        let _ = self.reply.send((status, payload));
    }
}

/// A value for [`IndustrialNode::send_values`] (§I.6.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Value {
    pub name: String,
    pub value: serde_json::Value,
    /// Milliseconds since 1970 when read; 0: now.
    #[serde(default)]
    pub time_ms: i64,
    /// `good`, `uncertain`, `bad` or `stale`.
    #[serde(default = "good")]
    pub quality: String,
}

fn good() -> String {
    "good".into()
}

/// What the link is doing, for the panel and the local API.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub enrolled: bool,
    pub name: String,
    /// The site CA's fingerprint.
    pub site: Option<String>,
    pub connected: bool,
    /// `quic` or `websocket`.
    pub transport: Option<&'static str>,
    pub address: Option<String>,
    /// Items waiting in the outbox, and dropped over its limits.
    pub queued: u64,
    pub dropped: u64,
    /// The outbox's size and its limit, in bytes (the only limit: never by age).
    pub queued_bytes: u64,
    pub limit_bytes: u64,
    pub last_error: Option<String>,
}

/// Someone waiting for a running command's answer.
type Answering = oneshot::Sender<(u8, Vec<u8>)>;

struct Shared {
    cfg: NodeConfig,
    key: KeyFile,
    outbox: Outbox,
    /// New items in the outbox.
    wake: Notify,
    /// The site changed (joined or left): reconnect.
    kick: Notify,
    site: Mutex<Option<Site>>,
    status: Mutex<Status>,
    commands: mpsc::Sender<Command>,
    commands_rx: Mutex<Option<mpsc::Receiver<Command>>>,
    link: Mutex<Option<Link>>,
    stop: std::sync::atomic::AtomicBool,
    /// Takes what the Site Manager sends down (local MQTT, §I.7.4); `false`: not taken.
    downlink: Mutex<Option<Downlink>>,
    /// Ids of items taken lately (the Site Manager sends again after a lost answer).
    seen: Mutex<std::collections::VecDeque<[u8; 16]>>,
    /// Commands running now, by id, with whoever waits for the same command's answer.
    running: Mutex<HashMap<[u8; 16], Vec<Answering>>>,
    /// The Site Manager's latest settings for this machine (§I.8), and who wants them.
    config: Mutex<serde_json::Value>,
    on_config: Mutex<Option<OnConfig>>,
}

type Downlink = Arc<dyn Fn(&Item) -> bool + Send + Sync>;
type OnConfig = Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// The Node | Industrial's link. Cheap to clone.
#[derive(Clone)]
pub struct IndustrialNode(Arc<Shared>);

use idn_common::text::base64 as b64;

fn unb64(s: &str) -> io::Result<Vec<u8>> {
    idn_common::text::unbase64(s.as_bytes()).ok_or_else(|| err("bad base64"))
}

impl IndustrialNode {
    /// Opens the machine's state and starts the link (it waits until the machine has
    /// joined a site).
    pub fn start(cfg: NodeConfig) -> io::Result<IndustrialNode> {
        std::fs::create_dir_all(&cfg.state_dir)?;
        let key = KeyFile::load_or_create(&cfg.state_dir.join("device.key"))?;
        let mut outbox = Outbox::open(&cfg.state_dir.join("outbox.redb"))?;
        // The machine's own limit (`[outbox] max_mb` in node.toml), at least 16 MB.
        if let Some(mb) = crate::modules::load(&cfg.state_dir)
            .settings
            .outbox
            .and_then(|o| o.max_mb)
        {
            outbox.max_bytes = mb.max(16) << 20;
        }
        // A damaged file is set aside (`site.json.bad-SECS-PID-N`) and logged: the machine then
        // starts as not joined, its record kept to look at or put back.
        let site = idn_common::fsutil::load_json::<Site>(&cfg.state_dir.join("site.json"))
            .unwrap_or_else(|e| {
                idn_open_protocol::log::log(&format!("node: {e}"));
                None
            });
        let (tx, rx) = mpsc::channel(64);
        let node = IndustrialNode(Arc::new(Shared {
            status: Mutex::new(Status {
                enrolled: site.is_some(),
                name: site.as_ref().map_or(cfg.name.clone(), |s| s.name.clone()),
                site: site.as_ref().and_then(site_id),
                ..Default::default()
            }),
            cfg,
            key,
            outbox,
            wake: Notify::new(),
            kick: Notify::new(),
            site: Mutex::new(site),
            commands: tx,
            commands_rx: Mutex::new(Some(rx)),
            link: Mutex::new(None),
            stop: Default::default(),
            downlink: Mutex::new(None),
            seen: Mutex::new(std::collections::VecDeque::new()),
            running: Mutex::new(HashMap::new()),
            config: Mutex::new(serde_json::Value::Null),
            on_config: Mutex::new(None),
        }));
        let n = node.clone();
        runtime().spawn(async move { n.run().await });
        Ok(node)
    }

    /// Joins the site a token names (§I.5), replacing any site joined before. `name`:
    /// the machine's name if the token doesn't give one.
    pub fn enrol(&self, token_text: &str) -> io::Result<Status> {
        let token = crate::token::Token::decode(token_text)?;
        let me = self.clone();
        let site = runtime().block_on(async move { me.join(&token).await })?;
        let path = self.0.cfg.state_dir.join("site.json");
        idn_common::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(&site).map_err(err)?)?;
        {
            let mut st = self.0.status.lock_ok();
            st.enrolled = true;
            st.name = site.name.clone();
            st.site = site_id(&site);
        }
        *self.0.site.lock_ok() = Some(site);
        self.drop_link("joined another site");
        self.0.kick.notify_one();
        Ok(self.status())
    }

    async fn join(&self, token: &crate::token::Token) -> io::Result<Site> {
        let seen = Arc::new(Mutex::new(None));
        let tls = crate::tls::join_config(token.ca_fingerprint, seen.clone())?;
        let link = connect_any(&token.urls, tls).await?;
        let (mut s, mut r) = link.open().await?;
        let spki = self.0.key.spki();
        let ca_der = seen
            .lock_ok()
            .clone()
            .ok_or_else(|| err("the Site Manager's CA wasn't seen"))?;
        let ca_spki = idn_open_protocol::trust::pki::Anchor::new(ca_der.clone())?.ca_der_spki()?;
        let body = serde_json::json!({
            "token": token.id_hex(),
            "name": self.0.cfg.name,
            "edition": EDITION,
            "key": b64(&spki),
            "mac": b64(token.mac("device", &spki, &ca_spki)),
        });
        let it = Item::new(
            wire::OP_JOIN,
            "idn.join",
            serde_json::to_vec(&body).map_err(err)?,
        );
        wire::write_item(&mut s, &it).await?;
        let (status, answer) = wire::read_answer(&mut r).await?;
        link.close("joined");
        if status != wire::ST_OK {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("joining refused: {}", String::from_utf8_lossy(&answer)),
            ));
        }
        let a: serde_json::Value = serde_json::from_slice(&answer).map_err(err)?;
        let field = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        // The Site Manager proves it knows the token too.
        let theirs = unb64(&field("mac"))?;
        if !crate::token::same(&theirs, &token.mac("authority", &spki, &ca_spki)) {
            return Err(err("the Site Manager's proof didn't match the token"));
        }
        let cert = unb64(&field("cert"))?;
        // The certificate must be for this key, from this CA.
        let info = idn_open_protocol::trust::pki::Anchor::new(ca_der.clone())?
            .verify_with(&cert, false)?;
        if info.fingerprint != self.0.key.fingerprint() {
            return Err(err("the certificate isn't for this machine's key"));
        }
        let tags = a
            .get("tags")
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Ok(Site {
            urls: token.urls.clone(),
            ca: b64(&ca_der),
            cert: b64(&cert),
            crl: field("crl"),
            name: info.name,
            tags,
        })
    }

    /// Leaves the site: forgets its certificates (the outbox stays).
    pub fn leave(&self) -> io::Result<()> {
        let _ = std::fs::remove_file(self.0.cfg.state_dir.join("site.json"));
        *self.0.site.lock_ok() = None;
        self.0.status.lock_ok().enrolled = false;
        self.drop_link("left the site");
        Ok(())
    }

    /// Where items the Site Manager sends down go (the local MQTT bridge).
    pub fn set_downlink(&self, f: impl Fn(&Item) -> bool + Send + Sync + 'static) {
        *self.0.downlink.lock_ok() = Some(Arc::new(f));
    }

    /// Called with the Site Manager's settings for this machine whenever they change.
    pub fn set_on_config(&self, f: impl Fn(&serde_json::Value) + Send + Sync + 'static) {
        let f: OnConfig = Arc::new(f);
        let now = self.0.config.lock_ok().clone();
        if !now.is_null() {
            f(&now);
        }
        *self.0.on_config.lock_ok() = Some(f);
    }

    /// The Site Manager's latest settings for this machine.
    pub fn site_config(&self) -> serde_json::Value {
        self.0.config.lock_ok().clone()
    }

    /// Whether [`IndustrialNode::stop`] was called.
    pub fn stopped(&self) -> bool {
        self.0.stop.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stops the link (the outbox keeps what is waiting).
    pub fn stop(&self) {
        self.0
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.drop_link("stopping");
        self.0.kick.notify_one();
    }

    fn drop_link(&self, why: &str) {
        if let Some(l) = self.0.link.lock_ok().take() {
            l.close(why);
        }
    }

    fn queue(&self, it: Item) -> io::Result<String> {
        if !wire::valid_kind(&it.kind) {
            return Err(idn_common::err::invalid_input(
                "type: lower-case letters, digits, '.', '-', '_'; at most 128",
            ));
        }
        if it.payload.len() > wire::MAX_PAYLOAD {
            return Err(idn_common::err::invalid_input("payload over 16 MiB"));
        }
        let id = it.id_hex();
        self.0.outbox.push(&it)?;
        self.0.wake.notify_one();
        Ok(id)
    }

    /// Queues a message for the Site Manager (§I.6.1); returns its id once it is on disk.
    pub fn send(&self, kind: &str, payload: Vec<u8>) -> io::Result<String> {
        self.queue(Item::new(wire::OP_MESSAGE, kind, payload))
    }

    /// Queues values (§I.6.2).
    pub fn send_values(&self, values: &[Value]) -> io::Result<String> {
        let now = wire::now_ms();
        let v: Vec<Value> = values
            .iter()
            .map(|v| Value {
                time_ms: if v.time_ms == 0 { now } else { v.time_ms },
                ..v.clone()
            })
            .collect();
        self.queue(Item::new(
            wire::OP_VALUES,
            "values",
            serde_json::to_vec(&v).map_err(err)?,
        ))
    }

    /// Queues a file (stage 1: whole, at most 16 MiB).
    pub fn send_file(&self, name: &str, bytes: Vec<u8>) -> io::Result<String> {
        let mut it = Item::new(wire::OP_FILE, "file", bytes);
        it.kind = format!("file.{}", sanitize(name));
        if !wire::valid_kind(&it.kind) {
            it.kind = "file".into();
        }
        self.queue(it)
    }

    /// Opens a screen stream to the Site Manager for a remote-support session
    /// (§I.11.3): the first item carries the session id, then the caller writes encoded
    /// pictures on the returned [`transport::Send`] until it drops. The link must be up.
    pub async fn open_screen(
        &self,
        session: &str,
    ) -> io::Result<(transport::Send, transport::Recv)> {
        let link = self
            .0
            .link
            .lock_ok()
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "not connected"))?;
        let (mut s, r) = link.open().await?;
        let it = Item::new(wire::OP_SCREEN, "screen", session.as_bytes().to_vec());
        wire::write_item(&mut s, &it).await?;
        Ok((s, r))
    }

    /// Asks the Site Manager (the application server behind it) and waits for the
    /// answer: not queued, so the link must be up. Returns (ok, payload).
    pub fn request(
        &self,
        kind: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<(bool, Vec<u8>)> {
        let link = self
            .0
            .link
            .lock_ok()
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "not connected"))?;
        let it = Item::new(wire::OP_REQUEST, kind, payload);
        runtime().block_on(async move {
            let work = async {
                let (mut s, mut r) = link.open().await?;
                wire::write_item(&mut s, &it).await?;
                tokio::io::AsyncWriteExt::shutdown(&mut s).await?;
                wire::read_answer(&mut r).await
            };
            let (st, p) = tokio::time::timeout(timeout, work)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no answer in time"))??;
            Ok((st == wire::ST_OK, p))
        })
    }

    /// The requests from the Site Manager, for the machine's program (taken once).
    pub fn commands(&self) -> Option<mpsc::Receiver<Command>> {
        self.0.commands_rx.lock_ok().take()
    }

    pub fn status(&self) -> Status {
        let mut s = self.0.status.lock_ok().clone();
        s.queued = self.0.outbox.len().unwrap_or(0);
        s.dropped = self.0.outbox.dropped();
        s.queued_bytes = self.0.outbox.bytes();
        s.limit_bytes = self.0.outbox.max_bytes;
        s
    }

    /// The tags the site gave this machine.
    pub fn tags(&self) -> BTreeMap<String, String> {
        self.0
            .site
            .lock_ok()
            .as_ref()
            .map(|s| s.tags.clone())
            .unwrap_or_default()
    }

    fn set_error(&self, e: impl std::fmt::Display) {
        let mut st = self.0.status.lock_ok();
        st.last_error = Some(e.to_string());
        st.connected = false;
        st.transport = None;
    }

    async fn run(self) {
        // 1 s doubling to 30 s while it keeps failing; each wait up to a quarter shorter at
        // random, so the machines a restarted Site Manager dropped don't all come back in
        // the same instant.
        let mut backoff =
            Backoff::new(Duration::from_secs(1), Duration::from_secs(30)).jitter(0.25);
        while !self.0.stop.load(std::sync::atomic::Ordering::Relaxed) {
            let site = self.0.site.lock_ok().clone();
            let Some(site) = site else {
                let _ = tokio::time::timeout(Duration::from_secs(5), self.0.kick.notified()).await;
                continue;
            };
            let creds = match self.creds(&site) {
                Ok(c) => c,
                Err(e) => {
                    self.set_error(format!("the site's certificates: {e}"));
                    let _ =
                        tokio::time::timeout(Duration::from_secs(30), self.0.kick.notified()).await;
                    continue;
                }
            };
            match connect_any(
                &site.urls,
                crate::tls::client_config(&creds).unwrap_or_else(|_| unreachable_cfg()),
            )
            .await
            {
                Ok(link) => {
                    let began = std::time::Instant::now();
                    let why = self.session(link, &site, &creds).await;
                    self.set_error(why);
                    // A session that ended at once (the Site Manager couldn't store what
                    // was sent, a store error here) waits before the next, growing: it
                    // was connected again at once, over and over (deep bug run 2026-11-09).
                    if began.elapsed() < Duration::from_secs(10) {
                        // (Not cut short by new items: each would start it again.)
                        tokio::time::sleep(backoff.next()).await;
                    } else {
                        backoff.reset();
                    }
                }
                Err(e) => {
                    self.set_error(e);
                    let _ = tokio::time::timeout(backoff.next(), self.0.kick.notified()).await;
                }
            }
        }
    }

    fn creds(&self, site: &Site) -> io::Result<Arc<DeviceCreds>> {
        let crl = unb64(&site.crl).unwrap_or_default();
        crate::tls::creds(
            self.0.key.clone(),
            unb64(&site.cert)?,
            unb64(&site.ca)?,
            Some(&crl),
        )
    }

    /// One connection, until it ends; returns why.
    async fn session(&self, link: Link, site: &Site, creds: &Arc<DeviceCreds>) -> String {
        // Hello: the Site Manager checks the edition and says the settings.
        let hello = serde_json::json!({
            "name": site.name,
            "edition": EDITION,
            "version": self.0.cfg.version,
            // For the Site Manager's clock check (§I.6.3).
            "time_ms": wire::now_ms(),
        });
        let r = async {
            let (mut s, mut r) = link.open().await?;
            let it = Item::new(
                wire::OP_HELLO,
                "idn.hello",
                serde_json::to_vec(&hello).map_err(err)?,
            );
            wire::write_item(&mut s, &it).await?;
            tokio::time::timeout(Duration::from_secs(10), wire::read_answer(&mut r))
                .await
                .map_err(|_| err("no answer to hello"))?
        }
        .await;
        match r {
            Ok((wire::ST_OK, cfg)) => self.apply_config(&cfg, creds),
            Ok((_, why)) => {
                link.close("refused");
                return format!("refused: {}", String::from_utf8_lossy(&why));
            }
            Err(e) => {
                link.close("hello failed");
                return format!("hello: {e}");
            }
        }
        {
            let mut st = self.0.status.lock_ok();
            st.connected = true;
            st.transport = Some(link.transport);
            st.address = Some(link.remote.clone());
            st.last_error = None;
        }
        *self.0.link.lock_ok() = Some(link.clone());
        let quic_url = site.urls.iter().find(|u| u.starts_with("idn://")).cloned();
        let on_ws = link.transport == "websocket";
        let creds2 = creds.clone();
        let why = tokio::select! {
            e = self.drain(&link) => format!("sending: {e}"),
            e = self.serve(&link, creds) => format!("receiving: {e}"),
            e = ping(&link) => format!("ping: {e}"),
            _ = async {
                // On the WebSocket, QUIC again now and then: when it answers, this link
                // ends and the next one is QUIC.
                let Some(u) = quic_url.filter(|_| on_ws) else {
                    return std::future::pending().await;
                };
                loop {
                    tokio::time::sleep(QUIC_RETRY).await;
                    let Ok(cfg) = crate::tls::client_config(&creds2) else { continue };
                    if let Ok(Ok(l)) = tokio::time::timeout(QUIC_FIRST, transport::connect_quic(&u[6..], cfg)).await {
                        l.close("probe");
                        return;
                    }
                }
            } => "QUIC reachable again".to_owned(),
            _ = link.closed() => "the link ended".to_owned(),
        };
        link.close(&why);
        *self.0.link.lock_ok() = None;
        why
    }

    fn apply_config(&self, cfg: &[u8], creds: &Arc<DeviceCreds>) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(cfg) else {
            return;
        };
        // The settings (all but the revocation list) to whoever wants them, on a change.
        let mut settings = v.clone();
        if let Some(o) = settings.as_object_mut() {
            o.remove("crl");
        }
        let changed = {
            let mut c = self.0.config.lock_ok();
            let changed = *c != settings;
            *c = settings.clone();
            changed
        };
        if changed {
            let f = self.0.on_config.lock_ok().clone();
            if let Some(f) = f {
                // On a thread of its own, the latest settings, one at a time: a reload
                // restarts modules and waits for the outbox's disk, and ran on one of the
                // link's two runtime threads (bug run 2026-11-25).
                static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
                let me = self.0.clone();
                let _ = std::thread::Builder::new()
                    .name("idn-config".into())
                    .spawn(move || {
                        let _one = ONE.lock_ok();
                        let latest = me.config.lock_ok().clone();
                        f(&latest);
                    });
            }
        }
        // A newer revocation list: kept for the next connections.
        if let Some(crl) = v
            .get("crl")
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            && let Ok(der) = unb64(crl)
            && creds.anchor.accept_crl(&der).ok().flatten().is_some()
        {
            let mut site = self.0.site.lock_ok();
            if let Some(s) = site.as_mut() {
                s.crl = crl.to_owned();
                let path = self.0.cfg.state_dir.join("site.json");
                if let Ok(b) = serde_json::to_vec_pretty(s) {
                    let _ = idn_common::fsutil::write_atomic(&path, &b);
                }
            }
        }
    }

    /// Sends the outbox in order, a batch at a time (up to [`BATCH`] items or
    /// [`BATCH_BYTES`] on one stream; the Site Manager stores them in that order and
    /// answers for each), and forgets each item once stored. Returns only on a failure.
    async fn drain(&self, link: &Link) -> io::Error {
        loop {
            let batch = match self.0.outbox.peek(0, BATCH) {
                Ok(b) => b,
                Err(e) => return e,
            };
            if batch.is_empty() {
                self.0.wake.notified().await;
                continue;
            }
            // Within the byte budget (at least one, however large).
            let mut bytes = 0;
            let take = batch
                .iter()
                .take_while(|(_, it)| {
                    bytes += it.payload.len() + 32;
                    bytes <= BATCH_BYTES
                })
                .count()
                .max(1);
            let batch = &batch[..take];
            let statuses = match send_batch(link, batch).await {
                Ok(s) => s,
                Err(e) => return e,
            };
            // The items the Site Manager has (or refused for good) forgotten in one commit,
            // off the runtime's threads; up to the first it couldn't store now.
            let mut done = Vec::with_capacity(batch.len());
            let mut failed = None;
            for ((seq, it), st) in batch.iter().zip(statuses) {
                match st {
                    wire::ST_OK | wire::ST_DUPLICATE => {}
                    // The Site Manager couldn't store it now: the rest again later.
                    wire::ST_ERROR => {
                        failed = Some(err(format!("the Site Manager couldn't store {}", it.kind)));
                        break;
                    }
                    // Refused for good: logged and dropped, so one bad item can't stop
                    // the queue.
                    _ => idn_open_protocol::log::log(&format!(
                        "industrial: {} {} refused by the Site Manager ({st}); dropped",
                        it.kind,
                        it.id_hex()
                    )),
                }
                done.push(*seq);
            }
            let me = self.clone();
            let removed = tokio::task::spawn_blocking(move || me.0.outbox.remove_all(&done))
                .await
                .unwrap_or_else(|e| Err(idn_common::err::other(e)));
            if let Err(e) = removed {
                return e;
            }
            if let Some(e) = failed {
                return e;
            }
        }
    }

    /// Streams the Site Manager opens: requests for the program, settings, pings.
    async fn serve(&self, link: &Link, creds: &Arc<DeviceCreds>) -> io::Error {
        loop {
            let (mut s, mut r) = match link.accept().await {
                Ok(x) => x,
                Err(e) => return e,
            };
            let me = self.clone();
            let creds = creds.clone();
            tokio::spawn(async move {
                let Ok(Ok(it)) =
                    tokio::time::timeout(Duration::from_secs(30), wire::read_item(&mut r)).await
                else {
                    return;
                };
                let (st, p) = match it.op {
                    wire::OP_PING => (wire::ST_OK, Vec::new()),
                    wire::OP_CONFIG => {
                        me.apply_config(&it.payload, &creds);
                        (wire::ST_OK, Vec::new())
                    }
                    wire::OP_REQUEST => me.command(it).await,
                    wire::OP_MESSAGE => me.downlink(&it),
                    _ => (wire::ST_UNKNOWN, Vec::new()),
                };
                let _ = wire::write_answer(&mut s, st, &p).await;
            });
        }
    }

    /// An item sent down: to the handler, once (by id).
    fn downlink(&self, it: &Item) -> (u8, Vec<u8>) {
        if self.0.seen.lock_ok().contains(&it.id) {
            return (wire::ST_DUPLICATE, Vec::new());
        }
        let f = self.0.downlink.lock_ok().clone();
        let Some(f) = f else {
            return (
                wire::ST_UNKNOWN,
                b"nothing on this machine takes it".to_vec(),
            );
        };
        if !f(it) {
            return (wire::ST_REFUSED, Vec::new());
        }
        let mut seen = self.0.seen.lock_ok();
        seen.push_back(it.id);
        if seen.len() > 10_000 {
            seen.pop_front();
        }
        (wire::ST_OK, Vec::new())
    }

    /// A command, claimed by its id first (§I.7.3): one sent again (a retry, a Site
    /// Manager restart) gets the first one's answer, waits for it while it runs, or is
    /// said to have been cut short by a restart of the Node; never run twice.
    async fn command(&self, it: Item) -> (u8, Vec<u8>) {
        let id = it.id;
        // Running here now: wait for that run's answer.
        let waiting = {
            let mut r = self.0.running.lock_ok();
            r.get_mut(&id).map(|w| {
                let (tx, rx) = oneshot::channel();
                w.push(tx);
                rx
            })
        };
        if let Some(rx) = waiting {
            return rx
                .await
                .unwrap_or((wire::ST_ERROR, b"the first run ended".to_vec()));
        }
        // Remote support (mouse and keys, the screen, chat) isn't claimed on disk: a
        // session's many inputs pushed the plant's commands out of the claims kept, and a
        // held plant command sent again would then run twice (deep bug run 2026-11-09).
        // Sent again, an input is at worst repeated.
        if it.kind.starts_with("remote.") {
            return self.run_command(it).await;
        }
        match self.0.outbox.claim(&id) {
            Ok(crate::store::Claim::Answered(st, p)) => return (st, p),
            Ok(crate::store::Claim::CutShort) => {
                return (
                    wire::ST_ERROR,
                    b"this command started before the Node restarted and may have run; it isn't run again"
                        .to_vec(),
                );
            }
            Ok(crate::store::Claim::New) => {}
            Err(e) => {
                return (
                    wire::ST_ERROR,
                    format!("claiming the command: {e}").into_bytes(),
                );
            }
        }
        self.0.running.lock_ok().insert(id, Vec::new());
        let a = self.run_command(it).await;
        let _ = self.0.outbox.answered(&id, a.0, &a.1);
        for w in self.0.running.lock_ok().remove(&id).unwrap_or_default() {
            let _ = w.send(a.clone());
        }
        a
    }

    async fn run_command(&self, it: Item) -> (u8, Vec<u8>) {
        let (tx, rx) = oneshot::channel();
        let c = Command {
            kind: it.kind,
            payload: it.payload,
            id: idn_common::hex::hex(&it.id),
            reply: tx,
        };
        if self.0.commands.try_send(c).is_err() {
            return (
                wire::ST_UNKNOWN,
                b"no program takes commands on this machine".to_vec(),
            );
        }
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => (wire::ST_UNKNOWN, Vec::new()),
            Err(_) => (
                wire::ST_TIMEOUT,
                b"the program didn't answer in time".to_vec(),
            ),
        }
    }
}

/// The site's CA fingerprint.
fn site_id(site: &Site) -> Option<String> {
    let der = unb64(&site.ca).ok()?;
    let spki = idn_open_protocol::trust::pki::Anchor::new(der)
        .ok()?
        .ca_der_spki()
        .ok()?;
    Some(idn_open_protocol::trust::pki::fingerprint(&spki))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(100)
        .collect()
}

/// A configuration that fails every handshake (certificates that don't load end in a
/// connection error, not a panic).
fn unreachable_cfg() -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(idn_open_protocol::secure::crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth()
}

/// A batch on one stream, then the end of the stream; the answer is a status per item.
async fn send_batch(link: &Link, batch: &[(u64, Item)]) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt;
    let (mut s, mut r) = link.open().await?;
    for (_, it) in batch {
        wire::write_item(&mut s, it).await?;
    }
    s.shutdown().await?;
    let (st, statuses) = tokio::time::timeout(Duration::from_secs(120), wire::read_answer(&mut r))
        .await
        .map_err(|_| err("no answer"))??;
    if st != wire::ST_OK || statuses.len() != batch.len() {
        return Err(err(format!(
            "the Site Manager answered a batch with {st} ({})",
            String::from_utf8_lossy(&statuses)
        )));
    }
    Ok(statuses)
}

async fn ping(link: &Link) -> io::Error {
    loop {
        tokio::time::sleep(PING_EVERY).await;
        let r = tokio::time::timeout(Duration::from_secs(15), async {
            let (mut s, mut r) = link.open().await?;
            wire::write_item(&mut s, &Item::new(wire::OP_PING, "idn.ping", Vec::new())).await?;
            tokio::io::AsyncWriteExt::shutdown(&mut s).await?;
            wire::read_answer(&mut r).await
        })
        .await;
        match r {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return e,
            Err(_) => return err("no answer in 15 s"),
        }
    }
}

/// Connects to the first address that answers: QUIC (`idn://`) first, briefly, then the
/// WebSocket (`wss://`, `ws://`).
pub async fn connect_any(urls: &[String], tls: rustls::ClientConfig) -> io::Result<Link> {
    let mut last = err("the site has no address");
    for u in urls.iter().filter(|u| u.starts_with("idn://")) {
        match tokio::time::timeout(QUIC_FIRST, transport::connect_quic(&u[6..], tls.clone())).await
        {
            Ok(Ok(l)) => return Ok(l),
            Ok(Err(e)) => last = e,
            Err(_) => last = err(format!("{u}: no answer in 2 s")),
        }
    }
    for u in urls
        .iter()
        .filter(|u| u.starts_with("ws://") || u.starts_with("wss://"))
    {
        match transport::connect_ws(u, tls.clone()).await {
            Ok(l) => return Ok(l),
            Err(e) => last = e,
        }
    }
    Err(last)
}
