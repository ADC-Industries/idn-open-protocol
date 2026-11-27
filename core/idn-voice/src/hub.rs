//! The live side (§V.4–§V.8): who is connected, in which channels, who talks where (the
//! half-duplex floor, priority, limits), calls and instant talk, and the forwarding of each
//! talker's Opus packets to the others — unchanged, never mixed — with their recording.

use crate::Host;
use crate::model::{Access, Channel, Handset, InstantFrom, Settings, UserSettings};
use crate::record::Recorder;
use crate::store::{self, Store, Transmission};
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod calls;
mod channels;
pub mod frames;
mod handsets;
mod messages;
mod presence;
mod publish;
mod sessions;
mod talk;
mod tick;
pub use frames::{Budget, FRAME_QUEUE_MAX, FrameQueue};
pub use handsets::CREDENTIAL_PREFIX;
use publish::QueuedHost;

/// A connected session's number.
pub type Sid = u32;

/// Talker slots a WebRTC client receives (§V.8).
pub const SLOTS: usize = 6;
/// The longest talk, whatever the channel (§V.12).
const MAX_TALK: Duration = Duration::from_secs(600);
/// A call that isn't answered in this long is missed (§V.5).
const RING_FOR: Duration = Duration::from_secs(30);
/// After instant talk, the one spoken to may answer into it for this long (§V.5).
const ANSWER_WINDOW: Duration = Duration::from_secs(10);
/// Members a channel takes (§V.12).
pub const MAX_MEMBERS: usize = 200;

/// Who a session is.
#[derive(Debug, Clone, Default)]
pub struct Who {
    /// Shown. A site user's is also who they are; a handset's isn't (see [`Who::key`]).
    pub name: String,
    /// A handset's identity, apart from the name shown (§V.13.3): `driver:<id>` (the
    /// site program's id for the driver) or `handset:<id>`. Two drivers of one name, or
    /// a driver named as a site user, are never each other.
    pub ident: Option<String>,
    /// A site user (else a guest, or a program). A handset counts as one (§V.13).
    pub user: bool,
    pub admin: bool,
    pub roles: Vec<String>,
    /// A program of a channel (§V.9, §V.10): `Some(channel id)`, with its kind.
    pub feed: Option<u32>,
    pub program: Program,
    /// A handset's session (§V.13): `name` is then its driver's, or "Handset NAME".
    pub handset: Option<HandsetTag>,
    /// Channels its driver was put in by the site's program: entered past their Password
    /// or Auth gate (those alone).
    pub granted: Vec<u32>,
    /// Signed in with the site program's credential ([`crate::Host::is_program`]): no
    /// person's account, so not checked against the site's users (Industrial's API key,
    /// user `api`, was signed out within seconds).
    pub api: bool,
}

impl Who {
    /// Who the session is, for everything that is someone's (direct messages, calls,
    /// instant talk, recordings, settings, presence): a site user's name, a handset's
    /// `ident`.
    pub fn key(&self) -> &str {
        self.ident.as_deref().unwrap_or(&self.name)
    }

    /// Whether `target` (a name or handset key, as calls and direct messages give it) is
    /// this session: a site user by name, a handset by its key alone.
    pub fn answers_to(&self, target: &str) -> bool {
        self.user
            && self.feed.is_none()
            && self.key() == target
            && self.handset.is_some() == is_handset_key(target)
    }
}

/// Whether a key is a handset's (`driver:…`, `handset:…`).
pub fn is_handset_key(k: &str) -> bool {
    k.starts_with("driver:") || k.starts_with("handset:")
}

/// Which handset a session is, and its driver.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HandsetTag {
    pub id: String,
    pub name: String,
    /// The driver's id (the program's), if one is assigned.
    pub driver: Option<String>,
    /// The credential's hash it signed in with: a session is only made, or resumed, while
    /// it is still the handset's (not revoked, not enrolled again since).
    pub secret: String,
}

/// What a channel's program does there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Program {
    #[default]
    None,
    /// An IEM feed: talks all the time.
    Feed,
    /// A source talking while it has sound (talkback): starts and ends its own talks.
    Vox,
    /// The channel as an IDN source: listens.
    Out,
}

/// What a session's connection is sent.
/// The largest sound packet taken, bytes.
pub const MAX_PACKET: usize = 4000;

#[derive(Debug)]
pub enum Out {
    Json(Value),
    /// A binary WebSocket message (§V.8): kind, talker, channel, seq, the packet.
    Frame(Vec<u8>),
    Close(String),
}

/// Sound over WebRTC: handed to the WebRTC thread (`rtc.rs`).
pub trait RtcSink: Send + Sync {
    fn send(&self, sid: Sid, slot: usize, data: &[u8]);
    fn forget(&self, sid: Sid);
}

/// Where a talk goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Channel(u32),
    /// Every channel the talker is in and may talk into (all-call, dispatch).
    All,
    /// A person (instant talk) or the other side of a call: their sessions.
    User(String),
    Call(u64),
}

struct Recording {
    t: Transmission,
    rec: Recorder,
}

struct Talk {
    target: Target,
    started: Instant,
    /// Half duplex: when the talk must end.
    until: Option<Instant>,
    /// Its last sound packet (the site's `talk_silence_s`).
    heard: Option<Instant>,
    recordings: Vec<Recording>,
}

struct Session {
    who: Who,
    /// Its JSON (and replays): never dropped (a client that falls 10,000 behind is
    /// closed instead, ws.rs).
    tx: tokio::sync::mpsc::UnboundedSender<Out>,
    /// Its WebSocket's sound (the fallback and reliable path).
    frames: Arc<FrameQueue>,
    /// Which connection it is on: a resumed handset's goes up by one, so the connection
    /// it left can't end it (§V.13).
    conn: u64,
    /// A handset whose link dropped: since when it waits for its resume (§V.13).
    away: Option<Instant>,
    /// A handset's resume tokens: the one it last resumed with (or its first) and those
    /// sent since in `hello`s it may not have got (a driver change on a dead link).
    resume: Vec<String>,
    /// Channels it was put in by its driver's assignment (left when that changes).
    by_driver: BTreeSet<u32>,
    channels: BTreeMap<u32, bool>,
    rtc: bool,
    talk: Option<Talk>,
    seq: u32,
    /// The talker each slot carries (WebRTC).
    slots: [Option<Sid>; SLOTS],
    /// Its last messages' times: thirty a minute at most (each kept on disk; E5-3).
    sent: std::collections::VecDeque<Instant>,
}

struct Call {
    a: Sid,
    /// The callee's key ([`Who::key`]: any of their sessions may answer) and, once
    /// answered, the one.
    b_name: String,
    b: Option<Sid>,
    since: Instant,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<Sid, Session>,
    next_sid: Sid,
    channels: BTreeMap<u32, Channel>,
    /// Half duplex: who has the floor of a channel.
    floor: HashMap<u32, Sid>,
    calls: BTreeMap<u64, Call>,
    next_call: u64,
    /// (spoken to, from) keys → when: instant talk to answer back into.
    answerable: HashMap<(String, String), Instant>,
    /// Sessions replaying a recording now (one each: a replay reads its file whole; bug
    /// round E, E5-11).
    replaying: std::collections::HashSet<Sid>,
    settings: Settings,
    /// Channels whose member list changed since the last tick: sent then, once (a list to
    /// each member at each join made 200 joining cost 40,000 lists, profile round 8).
    members_dirty: BTreeSet<u32>,
    /// SOS alerts not yet acknowledged, by number (§V.13).
    alerts: BTreeMap<u64, handsets::Alert>,
    next_alert: u64,
    /// Sound frames dropped for WebSocket listeners too far behind, since the start.
    frames_dropped: u64,
    /// Where Voice's QUIC is offered (§V.14), if its listener runs.
    link_offer: Option<crate::link::LinkOffer>,
    /// The number of the last offer made: a listener clears the offer only if it is still
    /// its own (two listeners on one Voice: dropping either cleared the other's).
    link_offer_id: u64,
}

pub struct Hub {
    st: Mutex<State>,
    /// Handsets (§V.13) by id, as stored. Taken before `st` when both are (a session of
    /// a handset made under both, so a revoke can't slip between), never after.
    hs: Mutex<BTreeMap<String, Handset>>,
    /// The site's byte budget for queued sound (`frame_budget_mb`).
    pub budget: Arc<Budget>,
    /// Wrong enrolment codes (§V.13.1): by address (IPv6 by /64) and site-wide.
    pub(crate) enrols: Mutex<handsets::EnrolTries>,
    pub store: Store,
    pub dir: PathBuf,
    pub host: Arc<dyn Host>,
    rtc: Mutex<Option<Arc<dyn RtcSink>>>,
    /// When the signed-in sessions were last checked against the site's users.
    users_checked: Mutex<Instant>,
    /// The site's `link` setting, watched by the sessions on the link (§V.14): turned
    /// off, they end and their clients go to the WebSocket.
    link_on: tokio::sync::watch::Sender<bool>,
}

/// Milliseconds since 1970.
pub(crate) fn now_ms() -> i64 {
    idn_common::epoch::millis() as i64
}

fn send(s: &Session, v: Value) {
    let _ = s.tx.send(Out::Json(v));
}

/// The frame a WebSocket listener is sent (§V.8).
fn frame(kind: u8, talker: Sid, channel: u32, seq: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + data.len());
    b.push(kind);
    b.extend_from_slice(&talker.to_le_bytes());
    b.extend_from_slice(&channel.to_le_bytes());
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(data);
    b
}

/// Whether `who` has the dispatch permission under `settings`.
fn is_dispatch(who: &Who, settings: &Settings) -> bool {
    who.admin || listed(who, &settings.dispatch).is_some()
}

/// Whether `who` matches an entry of a list of users and roles.
fn listed(who: &Who, list: &[crate::model::Allowed]) -> Option<bool> {
    let mut talk = None;
    for a in list {
        // (A user entry names a site user: never a handset, whatever its name.)
        let hit = (who.handset.is_none() && a.user.as_deref() == Some(who.name.as_str()))
            || a.role
                .as_deref()
                .is_some_and(|r| who.roles.iter().any(|x| x == r));
        if who.user && hit {
            talk = Some(talk.unwrap_or(false) || a.talk);
        }
    }
    talk
}

/// What `who` may do in `c`: (listen, talk), or none.
pub fn may(who: &Who, c: &Channel, password_ok: bool) -> Option<bool> {
    if who.feed.is_some() {
        return (who.feed == Some(c.id)).then_some(who.program != Program::Out);
    }
    let talk_kind = !c.is_iem();
    if who.admin || who.granted.contains(&c.id) {
        return Some(talk_kind);
    }
    let ok = match &c.access {
        Access::Guest => Some(true),
        Access::Open => who.user.then_some(true),
        Access::Password { .. } => password_ok.then_some(true),
        Access::Auth { allow } => listed(who, allow),
    };
    ok.map(|talk| talk && talk_kind)
}

impl Hub {
    pub fn new(store: Store, dir: PathBuf, host: Arc<dyn Host>) -> std::io::Result<Arc<Hub>> {
        // Recordings of talks cut short by a stop (never indexed, never removed by the
        // retention): removed now (E5-11).
        if let Ok(rd) = std::fs::read_dir(dir.join("rec")) {
            for e in rd.filter_map(Result::ok) {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = name
                    .strip_suffix(".vfr")
                    .and_then(|n| n.parse::<u64>().ok())
                    && matches!(store.transmission(id), Ok(None))
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let channels = store.channels()?.into_iter().map(|c| (c.id, c)).collect();
        let settings = store.settings();
        let link_on = tokio::sync::watch::Sender::new(settings.link);
        let settings_budget = settings.frame_budget_mb as u64 * 1_000_000;
        let hs = store
            .handsets()?
            .into_iter()
            .map(|h| (h.id.clone(), h))
            .collect();
        Ok(Arc::new(Hub {
            st: Mutex::new(State {
                channels,
                settings,
                next_sid: 1,
                next_call: 1,
                next_alert: 1,
                ..Default::default()
            }),
            hs: Mutex::new(hs),
            budget: Arc::new(Budget::new(settings_budget)),
            enrols: Mutex::default(),
            store,
            dir,
            host: QueuedHost::wrap(host),
            rtc: Mutex::new(None),
            users_checked: Mutex::new(Instant::now()),
            link_on,
        }))
    }

    pub fn set_rtc(&self, sink: Arc<dyn RtcSink>) {
        *self.rtc.lock_ok() = Some(sink);
    }

    fn rtc(&self) -> Option<Arc<dyn RtcSink>> {
        self.rtc.lock_ok().clone()
    }

    pub fn settings(&self) -> Settings {
        self.st.lock_ok().settings.clone()
    }

    pub fn channels(&self) -> Vec<Channel> {
        self.st.lock_ok().channels.values().cloned().collect()
    }

    /// Whether `who` has the dispatch permission (§V.2).
    pub fn dispatch(&self, who: &Who) -> bool {
        is_dispatch(who, &self.st.lock_ok().settings)
    }

    /// Where Voice's QUIC is now; the offer's number, for [`Hub::clear_link_offer`].
    pub(crate) fn set_link_offer(&self, offer: Option<crate::link::LinkOffer>) -> u64 {
        let mut st = self.st.lock_ok();
        st.link_offer_id += 1;
        st.link_offer = offer;
        st.link_offer_id
    }

    /// No QUIC offered, if offer `id` is still the one made last.
    pub(crate) fn clear_link_offer(&self, id: u64) {
        let mut st = self.st.lock_ok();
        if st.link_offer_id == id {
            st.link_offer = None;
        }
    }

    /// Done once the site's `link` setting is off (at once if it is).
    pub(crate) async fn link_off(&self) {
        let mut on = self.link_on.subscribe();
        let _ = on.wait_for(|on| !*on).await;
    }

    /// Where Voice's QUIC is, while its listener runs and the site's `link` is on.
    pub fn link_offer(&self) -> Option<crate::link::LinkOffer> {
        let st = self.st.lock_ok();
        st.link_offer.clone().filter(|_| st.settings.link)
    }
}
