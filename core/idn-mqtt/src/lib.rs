//! IDN's MQTT 5 broker: IDN Industrial's Site Manager's (idn-industrial.md §I.7: the
//! application server's way in), an Industrial Node's local endpoint, and IDN Media's
//! Site Manager's (idn-node.md §N.34: a site's control, programs and outside systems
//! meeting) — the [`Role`] says which. QoS 0 and
//! 1 (2 is answered as 1), retained messages, persistent sessions kept on the volume
//! (an application server that is down finds its messages waiting), wills, and logins
//! per application. Topics under `idn/` that only the Site Manager writes (what
//! machines send, their status) are closed to clients; commands to machines and answers
//! to their requests are handed to the Site Manager as they are published.

use idn_common::sync::LockOk as _;
use idn_open_protocol::web::{Logins, PasswordChecks};
mod v311;

use bytes::{Bytes, BytesMut};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use rumqttc::v5::mqttbytes::v5::{
    ConnAck, ConnAckProperties, ConnectReturnCode, Disconnect, DisconnectReasonCode, Packet,
    PingResp, PubAck, PubAckReason, Publish, PublishProperties, RetainForwardRule, SubAck,
    SubscribeReasonCode, UnsubAck, UnsubAckReason,
};
use rumqttc::v5::mqttbytes::{self as mb, QoS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

mod packets;
use packets::*;

mod client;
mod store;
mod users;

fn err(m: impl std::fmt::Display) -> io::Error {
    idn_common::err::other(format_args!("mqtt: {m}"))
}

/// The largest packet taken or sent (a 16 MiB item plus its topic and properties).
const MAX_PACKET: u32 = (16 << 20) + 65536;
/// QoS 1 messages in flight to one client at once (at most what it allows).
const IN_FLIGHT: u16 = 100;

const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
/// A persistent session's waiting messages: client id, a NUL, the sequence (u64 BE).
const QUEUE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("queue");
const RETAINED: TableDefinition<&str, &[u8]> = TableDefinition::new("retained");
/// Logins: user name → Argon2id hash.
const USERS: TableDefinition<&str, &str> = TableDefinition::new("users");

/// A message as the broker keeps and routes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub topic: String,
    pub payload: Bytes,
    pub qos: u8,
    pub retain: bool,
    pub props: Props,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Props {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_topic: Option<String>,
    /// Base64 in storage.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_b64")]
    pub correlation: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_format: Option<u8>,
    /// When it expires (ms since 1970), from the message expiry interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
}

mod opt_b64 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(&idn_common::text::base64(b)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'d, D: Deserializer<'d>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        Ok(s.and_then(|s| idn_common::text::unbase64_r(s).ok()))
    }
}

impl Msg {
    pub fn new(topic: &str, payload: impl Into<Bytes>, qos: u8, retain: bool) -> Msg {
        Msg {
            topic: topic.to_owned(),
            payload: payload.into(),
            qos,
            retain,
            props: Props::default(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let props = serde_json::to_vec(&self.props).unwrap_or_default();
        let mut b = Vec::with_capacity(8 + self.topic.len() + props.len() + self.payload.len());
        b.push(self.qos);
        b.push(self.retain as u8);
        b.extend_from_slice(&(self.topic.len() as u16).to_le_bytes());
        b.extend_from_slice(self.topic.as_bytes());
        b.extend_from_slice(&(props.len() as u32).to_le_bytes());
        b.extend_from_slice(&props);
        b.extend_from_slice(&self.payload);
        b
    }

    fn decode(b: &[u8]) -> Option<Msg> {
        let tl = u16::from_le_bytes(b.get(2..4)?.try_into().ok()?) as usize;
        let topic = std::str::from_utf8(b.get(4..4 + tl)?).ok()?.to_owned();
        let at = 4 + tl;
        let pl = u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
        let props = serde_json::from_slice(b.get(at + 4..at + 4 + pl)?).ok()?;
        Some(Msg {
            qos: *b.first()?,
            retain: *b.get(1)? != 0,
            topic,
            props,
            payload: Bytes::copy_from_slice(b.get(at + 4 + pl..)?),
        })
    }

    fn expired(&self) -> bool {
        self.props.expires_ms.is_some_and(|e| now_ms() > e)
    }

    fn from_publish(p: &Publish) -> io::Result<Msg> {
        let topic = std::str::from_utf8(&p.topic)
            .map_err(|_| err("topic isn't text"))?
            .to_owned();
        let pr = p.properties.clone().unwrap_or_default();
        Ok(Msg {
            topic,
            payload: p.payload.clone(),
            qos: (p.qos as u8).min(1),
            retain: p.retain,
            props: Props {
                response_topic: pr.response_topic,
                correlation: pr.correlation_data.map(|c| c.to_vec()),
                user: pr.user_properties,
                content_type: pr.content_type,
                payload_format: pr.payload_format_indicator,
                expires_ms: pr
                    .message_expiry_interval
                    .map(|s| now_ms() + s as i64 * 1000),
            },
        })
    }

    fn to_publish(&self, pkid: u16, dup: bool) -> Publish {
        let remaining = self
            .props
            .expires_ms
            .map(|e| ((e - now_ms()).max(1000) / 1000) as u32);
        let props = PublishProperties {
            payload_format_indicator: self.props.payload_format,
            message_expiry_interval: remaining,
            topic_alias: None,
            response_topic: self.props.response_topic.clone(),
            correlation_data: self.props.correlation.clone().map(Bytes::from),
            user_properties: self.props.user.clone(),
            subscription_identifiers: Vec::new(),
            content_type: self.props.content_type.clone(),
        };
        let mut p = Publish::new(
            self.topic.clone(),
            if self.qos >= 1 {
                QoS::AtLeastOnce
            } else {
                QoS::AtMostOnce
            },
            self.payload.clone(),
            Some(props),
        );
        p.retain = self.retain;
        p.pkid = pkid;
        p.dup = dup;
        p
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Sub {
    filter: String,
    qos: u8,
    nolocal: bool,
    /// Retain as published.
    rap: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionRec {
    subs: Vec<Sub>,
    /// Session expiry interval, seconds (0: ends with the connection).
    expiry_s: u32,
    /// When its client went away (ms), if it did.
    gone_ms: Option<i64>,
    #[serde(default)]
    next_seq: u64,
    /// The login that made it: another login's client with the same id may not take it
    /// over, nor its messages (bug round E, E8-2). `None`: an open (loopback) client's.
    #[serde(default)]
    user: Option<String>,
}

/// What a connection is told to do.
enum Out {
    /// A message for it (not stored: QoS 0, or a session that ends with the connection).
    Deliver(Msg),
    /// Its stored queue has more.
    Queued,
    /// Taken over by a newer connection with the same client id.
    Kick,
    /// The broker is turned off: told so, then closed (its will published).
    Shutdown,
    /// Too far behind: closed (its will published).
    Behind,
    /// Its login was removed or its password changed: closed (its will published).
    LoginGone,
}

struct Live {
    tx: mpsc::UnboundedSender<Out>,
    conn: u64,
    /// Payload bytes handed to it and not yet written: past [`MAX_BEHIND`], it is
    /// disconnected (a client reading slower than its subscriptions arrive grew the broker
    /// without limit; bug round E, E8-3).
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

/// Payload bytes a connection may be behind before it is disconnected.
pub(crate) const MAX_BEHIND: usize = 32 << 20;

impl Live {
    /// Hands `m` to the connection, or tells it it is too far behind.
    fn deliver(&self, m: Msg) {
        use std::sync::atomic::Ordering;
        let n = m.payload.len();
        if self.queued.fetch_add(n, Ordering::Relaxed) + n > MAX_BEHIND {
            self.queued.fetch_sub(n, Ordering::Relaxed);
            let _ = self.tx.send(Out::Behind);
            return;
        }
        let _ = self.tx.send(Out::Deliver(m));
    }
}

#[derive(Default)]
struct State {
    live: HashMap<String, Live>,
    sessions: HashMap<String, SessionRec>,
    /// QoS 1 messages each client sent lately: (packet id, topic and payload's hash,
    /// when). One sent again (DUP: its PUBACK was lost with the connection) is acked, not
    /// acted on twice — a command to a machine was run twice (deep bug run 2026-11-09).
    recent: HashMap<String, std::collections::VecDeque<(u16, u64, std::time::Instant)>>,
}

impl State {
    /// Remembers a QoS 1 message as taken (after it was handed on and kept).
    fn taken(&mut self, client: &str, pkid: u16, m: &Msg) {
        let hash = Self::hash_of(m);
        self.recent
            .entry(client.to_owned())
            .or_default()
            .push_back((pkid, hash, std::time::Instant::now()));
    }

    fn hash_of(m: &Msg) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        m.topic.hash(&mut h);
        m.payload.hash(&mut h);
        h.finish()
    }

    /// Whether this QoS 1 message, resent (DUP), was taken already.
    fn seen_before(&mut self, client: &str, pkid: u16, m: &Msg, dup: bool) -> bool {
        let hash = Self::hash_of(m);
        let keep = std::time::Duration::from_secs(600);
        // (Clients gone for good let go now and then.)
        if self.recent.len() > 1024 {
            self.recent
                .retain(|_, q| q.back().is_some_and(|e| e.2.elapsed() <= keep));
        }
        let q = self.recent.entry(client.to_owned()).or_default();
        while q.front().is_some_and(|e| e.2.elapsed() > keep) || q.len() > 256 {
            q.pop_front();
        }
        dup && q.iter().any(|e| e.0 == pkid && e.1 == hash)
    }
}

/// What clients publish that the Site Manager acts on (§I.7.2): commands to machines
/// (`idn/<machine>/cmd/<type>`), answers to machines' requests (`idn/<machine>/res/<id>`).
#[derive(Debug)]
pub enum Hook {
    Command {
        machine: String,
        kind: String,
        msg: Msg,
        /// Said once the command is held (saved): the client's PUBACK waits for it.
        saved: Saved,
    },
    Answer {
        machine: String,
        id: String,
        msg: Msg,
    },
    /// An application refused an item (§I.7.5): `idn/<machine>/nack/<item id>`, the
    /// payload `{"code", "reason"}` (or the reason as text).
    Nack {
        machine: String,
        id: String,
        msg: Msg,
    },
    /// A session subscribed: messages kept for want of a subscriber may now have one.
    Subscribed,
    /// For a machine's local MQTT (§I.7.4): published to `idn/<machine>/to/<topic>`,
    /// to be published there as `<topic>`.
    Down {
        machine: String,
        topic: String,
        msg: Msg,
    },
    /// A local broker's (a Node's): something a client published, to be bridged up.
    Published(Msg),
    /// A local broker's message taken into the Node's outbox: the PUBACK waits for it.
    Local { msg: Msg, saved: Saved },
}

/// Told when what a hook carries is safely kept, so the publisher's PUBACK says so only
/// then: acked first, a Site Manager or a Node stopping in between lost a command or a
/// device's message the device thought delivered (deep bug run 2026-11-09). Dropped
/// without a word: taken as kept (as before).
#[derive(Debug)]
pub struct Saved(Option<tokio::sync::oneshot::Sender<bool>>);

impl Saved {
    fn new() -> (Saved, tokio::sync::oneshot::Receiver<bool>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Saved(Some(tx)), rx)
    }

    /// Kept (`true`), or not (the publisher is told it failed).
    pub fn done(mut self, ok: bool) {
        if let Some(t) = self.0.take() {
            let _ = t.send(ok);
        }
    }
}

impl Drop for Saved {
    fn drop(&mut self) {
        if let Some(t) = self.0.take() {
            let _ = t.send(true);
        }
    }
}

/// What kind of broker: the Site Manager's (topics under `idn/` that only it writes are
/// closed to clients; commands, answers and down-link topics handed to it), or a Node's
/// local one (any topic; everything published handed over, to go up the link).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Site,
    Local,
    /// IDN Media's Site Manager's (§N.34): `idn/site/…` is the Site Manager's own; every
    /// other topic is open to logged-in clients (the site's programs, Companion, Home
    /// Assistant, …), with nothing handed over.
    Media,
}

impl Role {
    /// Whether a client may not publish to `topic`.
    fn reserves(self, topic: &str) -> bool {
        match self {
            Role::Site => reserved(topic),
            Role::Local => false,
            Role::Media => {
                (topic == "idn/site" || topic.starts_with("idn/site/")) && !voice_command(topic)
            }
        }
    }
}

/// Milliseconds since 1970.
pub fn now_ms() -> i64 {
    idn_common::epoch::millis() as i64
}

struct Inner {
    db: Database,
    role: Role,
    state: Mutex<State>,
    hooks: mpsc::UnboundedSender<Hook>,
    next_conn: std::sync::atomic::AtomicU64,
    /// Password checks running at once (Argon2: tens of ms and 19 MiB each), off the
    /// runtime's threads.
    checks: PasswordChecks,
    /// Failed logins by address and by name (as the Site Managers' web logins: 5 an
    /// address and 20 a name in a minute).
    logins: Logins,
}

/// The broker. Cheap to clone.
#[derive(Clone)]
pub struct Broker(Arc<Inner>);

fn db_err(e: impl std::fmt::Display) -> io::Error {
    err(format!("store: {e}"))
}

/// Messages kept for one session at most (the oldest go).
const MAX_QUEUED: u64 = 10_000;
/// The longest a session is kept after its client goes: 7 days (a 3.1.1 client's
/// persistent session was kept for 136 years; MQTT 5's asked for were taken as given).
pub const MAX_SESSION_EXPIRY: u32 = 7 * 24 * 3600;

fn queue_key(client: &str, seq: u64) -> Vec<u8> {
    let mut k = client.as_bytes().to_vec();
    k.push(0);
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// Topics only the Site Manager writes (what machines sent and their status).
/// Commands to the Site Manager's IDN Voice (idn-voice.md §V.10): clients may publish there.
fn voice_command(topic: &str) -> bool {
    topic.starts_with("idn/site/voice/cmd/")
}

fn reserved(topic: &str) -> bool {
    if voice_command(topic) {
        return false;
    }
    let l: Vec<&str> = topic.split('/').collect();
    if l.first() != Some(&"idn") {
        return false;
    }
    if l.get(1) == Some(&"site") {
        return true;
    }
    matches!(
        l.get(2),
        Some(&("status" | "v" | "m" | "f" | "req" | "chat-in" | "local"))
    )
}

impl Broker {
    /// Opens the Site Manager's broker; it reads [`Hook`]s from the receiver.
    pub fn open(path: &Path) -> io::Result<(Broker, mpsc::UnboundedReceiver<Hook>)> {
        Self::open_as(path, Role::Site)
    }

    /// Opens a broker of either [`Role`].
    pub fn open_as(path: &Path, role: Role) -> io::Result<(Broker, mpsc::UnboundedReceiver<Hook>)> {
        let db = Database::create(path).map_err(db_err)?;
        let mut sessions = HashMap::new();
        {
            let w = db.begin_write().map_err(db_err)?;
            {
                let t = w.open_table(SESSIONS).map_err(db_err)?;
                for e in t.iter().map_err(db_err)? {
                    let (k, v) = e.map_err(db_err)?;
                    if let Ok(mut s) = serde_json::from_slice::<SessionRec>(v.value()) {
                        // Everyone was away while the broker was.
                        s.gone_ms.get_or_insert(now_ms());
                        // (Kept from before the cap: at most the longest now, E8-1.)
                        s.expiry_s = s.expiry_s.min(MAX_SESSION_EXPIRY);
                        sessions.insert(k.value().to_owned(), s);
                    }
                }
            }
            w.open_table(QUEUE).map_err(db_err)?;
            w.open_table(RETAINED).map_err(db_err)?;
            w.open_table(USERS).map_err(db_err)?;
            w.commit().map_err(db_err)?;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let b = Broker(Arc::new(Inner {
            db,
            role,
            state: Mutex::new(State {
                live: HashMap::new(),
                sessions,
                recent: HashMap::new(),
            }),
            hooks: tx,
            next_conn: 1.into(),
            checks: PasswordChecks::default(),
            // No per-name limit: machine accounts are shared by many programs (one still
            // retrying an old password kept the updated ones out), and their passwords are
            // machine-made.
            logins: Logins::new(idn_open_protocol::web::LoginLimits {
                per_name: usize::MAX,
                ..Default::default()
            }),
        }));
        // Sessions whose expiry passed are forgotten, with their queues.
        let me = b.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let _ = me.expire();
            }
        });
        Ok((b, rx))
    }

    // ---- logins

    // ---- routing

    /// Publishes as the Site Manager (any topic). Returns how many persistent sessions
    /// stored it and how many connected clients it went to straight.
    pub fn publish(&self, msg: &Msg) -> io::Result<(usize, usize)> {
        self.route(msg, None, false)
    }

    /// As [`Broker::publish`], to persistent sessions only (a message published before,
    /// kept for want of one).
    pub fn publish_persistent(&self, msg: &Msg) -> io::Result<usize> {
        self.route(msg, None, true).map(|r| r.0)
    }

    /// Whether some persistent session would store a QoS 1 message on `topic`.
    pub fn has_persistent_subscriber(&self, topic: &str) -> bool {
        let st = self.0.state.lock_ok();
        st.sessions.values().any(|s| {
            s.expiry_s > 0
                && s.subs
                    .iter()
                    .any(|f| f.qos >= 1 && mb::matches(topic, &f.filter))
        })
    }

    fn route(
        &self,
        msg: &Msg,
        from: Option<&str>,
        persistent_only: bool,
    ) -> io::Result<(usize, usize)> {
        // Nothing to keep (not retained, no persistent session takes it): straight to the
        // connected clients, no transaction. A commit syncs the disk, and one for each
        // message to connected clients alone limited a Site Manager's intake. A session
        // subscribing meanwhile is as one subscribing just after.
        if !(msg.retain && !persistent_only) {
            let mut st = self.0.state.lock_ok();
            let State { live, sessions, .. } = &mut *st;
            let best = |s: &SessionRec, client: &str| {
                s.subs
                    .iter()
                    .filter(|f| !(f.nolocal && from == Some(client)))
                    .filter(|f| mb::matches(&msg.topic, &f.filter))
                    .max_by_key(|f| f.qos)
                    .map(|f| f.qos)
            };
            let keeps = sessions
                .iter()
                .any(|(c, s)| s.expiry_s > 0 && best(s, c).is_some_and(|q| msg.qos.min(q) >= 1));
            if !keeps {
                let mut sent = 0;
                if !persistent_only {
                    for (client, s) in sessions.iter() {
                        let Some(q) = best(s, client) else { continue };
                        let Some(l) = live.get(client) else { continue };
                        // (Not retained here: a retained one always takes the path below.)
                        let mut m = msg.clone();
                        m.qos = msg.qos.min(q);
                        m.retain = false;
                        l.deliver(m);
                        sent += 1;
                    }
                }
                return Ok((0, sent));
            }
        }
        let w = self.0.db.begin_write().map_err(db_err)?;
        if msg.retain && !persistent_only {
            let mut t = w.open_table(RETAINED).map_err(db_err)?;
            if msg.payload.is_empty() {
                t.remove(msg.topic.as_str()).map_err(db_err)?;
            } else {
                t.insert(msg.topic.as_str(), msg.encode().as_slice())
                    .map_err(db_err)?;
            }
        }
        let (mut stored, mut sent) = (0, 0);
        let mut notify = Vec::new();
        {
            let mut st = self.0.state.lock_ok();
            let State { live, sessions, .. } = &mut *st;
            let mut q = w.open_table(QUEUE).map_err(db_err)?;
            let mut changed = Vec::new();
            for (client, s) in sessions.iter_mut() {
                // The subscription granting the most, of those matching.
                let best = s
                    .subs
                    .iter()
                    .filter(|f| !(f.nolocal && from == Some(client.as_str())))
                    .filter(|f| mb::matches(&msg.topic, &f.filter))
                    .max_by_key(|f| f.qos);
                let Some(sub) = best else { continue };
                let mut m = msg.clone();
                m.qos = msg.qos.min(sub.qos);
                m.retain = sub.rap && msg.retain;
                if s.expiry_s > 0 && m.qos >= 1 {
                    s.next_seq += 1;
                    q.insert(
                        queue_key(client, s.next_seq).as_slice(),
                        m.encode().as_slice(),
                    )
                    .map_err(db_err)?;
                    // The newest MAX_QUEUED kept for a session (its numbers follow on, so
                    // the one that many back goes): an application gone for good grew the
                    // store until the disk filled (bug round E, E8-1).
                    if let Some(old) = s.next_seq.checked_sub(MAX_QUEUED) {
                        q.remove(queue_key(client, old).as_slice())
                            .map_err(db_err)?;
                    }
                    stored += 1;
                    changed.push(client.clone());
                    if let Some(l) = live.get(client) {
                        notify.push(l.tx.clone());
                    }
                } else if !persistent_only && let Some(l) = live.get(client) {
                    l.deliver(m);
                    sent += 1;
                }
            }
            let mut t = w.open_table(SESSIONS).map_err(db_err)?;
            for c in changed {
                if let Some(s) = sessions.get(&c) {
                    t.insert(c.as_str(), serde_json::to_vec(s).map_err(err)?.as_slice())
                        .map_err(db_err)?;
                }
            }
        }
        w.commit().map_err(db_err)?;
        for n in notify {
            let _ = n.send(Out::Queued);
        }
        Ok((stored, sent))
    }

    // ---- connections

    /// Hands `m` to the program; for a command (or a local message to bridge up), what
    /// says when it is kept.
    fn hook(&self, m: &Msg) -> Option<tokio::sync::oneshot::Receiver<bool>> {
        if voice_command(&m.topic) && self.0.role != Role::Local {
            let _ = self.0.hooks.send(Hook::Published(m.clone()));
            return None;
        }
        if self.0.role == Role::Local {
            let (saved, rx) = Saved::new();
            // (Nobody taking them: the device is told it failed, not acked; bug run B.)
            if let Err(tokio::sync::mpsc::error::SendError(Hook::Local { saved, .. })) =
                self.0.hooks.send(Hook::Local {
                    msg: m.clone(),
                    saved,
                })
            {
                saved.done(false);
            }
            return Some(rx);
        }
        if self.0.role == Role::Media {
            return None;
        }
        let l: Vec<&str> = m.topic.split('/').collect();
        if l.first() != Some(&"idn") || l.len() < 4 {
            return None;
        }
        let machine = l[1].to_owned();
        match l[2] {
            "cmd" => {
                let (saved, rx) = Saved::new();
                let _ = self.0.hooks.send(Hook::Command {
                    machine,
                    kind: l[3..].join("."),
                    msg: m.clone(),
                    saved,
                });
                return Some(rx);
            }
            "res" => {
                let _ = self.0.hooks.send(Hook::Answer {
                    machine,
                    id: l[3].to_owned(),
                    msg: m.clone(),
                });
            }
            "nack" => {
                let _ = self.0.hooks.send(Hook::Nack {
                    machine,
                    id: l[3].to_owned(),
                    msg: m.clone(),
                });
            }
            "to" => {
                let _ = self.0.hooks.send(Hook::Down {
                    machine,
                    topic: l[3..].join("/"),
                    msg: m.clone(),
                });
            }
            _ => {}
        }
        None
    }

    /// Ends every client's connection (DISCONNECT "server shutting down"; their wills
    /// published, persistent sessions kept): the broker turned off.
    pub fn disconnect_all(&self) {
        let st = self.0.state.lock_ok();
        for l in st.live.values() {
            let _ = l.tx.send(Out::Shutdown);
        }
    }

    /// Clients connected now (client ids).
    pub fn clients(&self) -> Vec<String> {
        let st = self.0.state.lock_ok();
        let mut v: Vec<String> = st.live.keys().cloned().collect();
        v.sort();
        v
    }

    /// Whether any persistent session (session expiry above 0) is held, connected or
    /// not. Cheap: reads no queue.
    pub fn has_persistent_session(&self) -> bool {
        self.0
            .state
            .lock_ok()
            .sessions
            .values()
            .any(|s| s.expiry_s > 0)
    }

    /// Persistent sessions and the messages waiting for each.
    pub fn waiting(&self) -> HashMap<String, usize> {
        let names: HashSet<String> = self
            .0
            .state
            .lock_ok()
            .sessions
            .iter()
            .filter(|(_, s)| s.expiry_s > 0)
            .map(|(c, _)| c.clone())
            .collect();
        names
            .into_iter()
            .map(|c| {
                let n = self.queued(&c, 0, usize::MAX).map_or(0, |v| v.len());
                (c, n)
            })
            .collect()
    }
}

/// A message as it crosses the link between a Node's local broker and the Site Manager's
/// (§I.7.4): JSON with the payload in base64.
pub fn bridge_encode(m: &Msg) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "topic": m.topic,
        "payload": idn_common::text::base64(&m.payload),
        "retain": m.retain,
        "qos": m.qos,
        "props": m.props,
    }))
    .unwrap_or_default()
}

pub fn bridge_decode(b: &[u8]) -> Option<Msg> {
    let v: serde_json::Value = serde_json::from_slice(b).ok()?;
    let topic = v.get("topic")?.as_str()?.to_owned();
    let payload = idn_common::text::unbase64_r(v.get("payload")?.as_str()?).ok()?;
    Some(Msg {
        topic,
        payload: Bytes::from(payload),
        qos: v.get("qos").and_then(|q| q.as_u64()).unwrap_or(1).min(1) as u8,
        retain: v.get("retain").and_then(|r| r.as_bool()).unwrap_or(false),
        props: v
            .get("props")
            .and_then(|p| serde_json::from_value(p.clone()).ok())
            .unwrap_or_default(),
    })
}

/// How long a write may wait for the client to read: one that stopped reading (a laptop
/// asleep, a program hung) with its connection open is dropped, instead of its messages
/// piling up in the broker's memory.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests;
