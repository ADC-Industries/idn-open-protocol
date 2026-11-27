//! The session's life on the library's runtime: connect (QUIC first, then the
//! WebSocket), read and answer, ping, notice a dead link, connect again with the newest
//! resume token and `have`, and while on the WebSocket try QUIC again every
//! `link_retry_s`, moving the session over when it connects. Every timing comes from
//! `hello`'s `link` block (the configuration's until the first `hello`).

use crate::Config;
use crate::LockOk as _;
use crate::config::{ResumeState, TransportChoice, parse_fingerprint, parse_url, split_host_port};
use crate::conn::{self, Conn, Endpoints, LinkTarget, LinkTimes, Refused, Wire};
use crate::event::{Event, Talker, Transport};
use crate::framing::AudioFrame;
use crate::mic::Mic;
use crate::outbox::{self, Item, Outbox, TalkId};
use crate::speaker::Speaker;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

/// Talkers kept in `have` at most (the server's own cap): the least recently heard go.
const HAVE_MAX: usize = 256;
/// A connection that lived this long resets the back-off; a shorter one counts as a
/// failed try (a server that takes connections and drops them isn't hammered).
const LIVED: Duration = Duration::from_secs(5);

/// The timings `hello` gives (`link`), with the configuration's until then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Link {
    pub ping: Duration,
    pub idle: Duration,
    pub connect: Duration,
    pub retry: Duration,
    pub keepalive: Duration,
    /// QUIC offered by the site (`link.quic` not null).
    pub quic: bool,
}

impl Link {
    fn from_config(c: &Config) -> Link {
        Link {
            ping: Duration::from_secs(10),
            idle: Duration::from_secs(30),
            connect: Duration::from_millis(c.link_connect_ms),
            retry: Duration::from_secs(c.link_retry_s),
            keepalive: Duration::from_millis(c.link_keepalive_ms),
            quic: true,
        }
    }

    /// `hello.link`'s values over these; each held to a sane least (a zero would spin).
    fn update(&mut self, l: &Value) {
        let get = |k: &str| l[k].as_u64();
        if let Some(v) = get("ping_s") {
            self.ping = Duration::from_secs(v.max(1));
        }
        if let Some(v) = get("idle_timeout_s") {
            self.idle = Duration::from_secs(v.max(5));
        }
        if let Some(v) = get("link_connect_ms") {
            self.connect = Duration::from_millis(v.max(100));
        }
        if let Some(v) = get("link_retry_s") {
            self.retry = Duration::from_secs(v.max(1));
        }
        if let Some(v) = get("link_keepalive_ms") {
            self.keepalive = Duration::from_millis(v.max(500));
        }
        if let Some(q) = l.get("quic") {
            self.quic = !q.is_null();
        }
    }

    fn times(&self) -> LinkTimes {
        LinkTimes {
            keepalive: self.keepalive,
            idle: self.idle,
            connect: self.connect,
        }
    }
}

/// What the session knows.
pub(crate) struct State {
    pub token: Option<String>,
    /// The last `seq` received from each talker, and when it was heard.
    pub have: BTreeMap<u32, u32>,
    heard: HashMap<u32, std::time::Instant>,
    pub link: Link,
    /// The link's addresses and certificate as `hello` gives them (`link.quic`).
    quic_addrs: Vec<String>,
    quic_pin: Option<[u8; 32]>,
    pub transport: Option<Transport>,
    /// The connection is to the server pinned: QUIC (always pinned), or a WebSocket whose
    /// certificate matched `cert_sha256`. Only then is `hello`'s `link.quic` learnt.
    pinned: bool,
    /// The talk the app holds: its number and command.
    pub talk: Option<(TalkId, Value)>,
    driver: Option<Value>,
    hellos: u64,
}

impl State {
    pub fn resume(&self) -> Option<ResumeState> {
        let token = self.token.clone()?;
        let have = self
            .have
            .iter()
            .map(|(t, s)| format!("{t}:{s}"))
            .collect::<Vec<_>>()
            .join(",");
        Some(ResumeState { token, have })
    }
}

thread_local! {
    /// The client (its `Shared`'s address) whose event callback this thread runs; 0: none.
    static IN_CALLBACK: Cell<usize> = const { Cell::new(0) };
}

/// Whether this thread is inside `sh`'s event callback (not another client's).
pub(crate) fn in_callback(sh: &Shared) -> bool {
    IN_CALLBACK.with(Cell::get) == sh as *const Shared as usize
}

/// Shared by the app's calls and the session task.
pub(crate) struct Shared {
    pub cfg: Config,
    events: Box<dyn Fn(Event) + Send + Sync>,
    /// Held while the callback runs: a close waits for one under way.
    pub cb: Mutex<()>,
    pub closed: AtomicBool,
    pub connected: AtomicBool,
    pub state: Mutex<State>,
    pub speaker: Mutex<Speaker>,
    pub mic: Mutex<Mic>,
    /// The talk commands and microphone packets, in order (held while away).
    pub outbox: Mutex<Outbox>,
    /// Something was put in the outbox.
    pub out_ready: Notify,
    pub close: watch::Sender<bool>,
    /// The network changed: the back-off's wait cut short (`net`); a WebSocket made
    /// before the change (`net_gen` counts them) given up (`net_drive`).
    pub net: Notify,
    pub net_gen: AtomicU64,
    pub net_drive: Notify,
    pub endpoints: Mutex<Endpoints>,
}

impl Shared {
    pub fn new(
        cfg: Config,
        events: Box<dyn Fn(Event) + Send + Sync>,
        close: watch::Sender<bool>,
    ) -> Shared {
        let state = State {
            token: cfg.resume.as_ref().map(|r| r.token.clone()),
            have: cfg
                .resume
                .as_ref()
                .map(|r| parse_have(&r.have))
                .unwrap_or_default(),
            heard: HashMap::new(),
            link: Link::from_config(&cfg),
            quic_addrs: Vec::new(),
            quic_pin: None,
            transport: None,
            pinned: false,
            talk: None,
            driver: None,
            hellos: 0,
        };
        let outbox = Mutex::new(Outbox::new(cfg.hold_mic_s));
        let mic = Mutex::new(Mic::new(cfg.fec_loss_pct));
        Shared {
            outbox,
            out_ready: Notify::new(),
            cfg,
            events,
            cb: Mutex::new(()),
            closed: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            state: Mutex::new(state),
            speaker: Mutex::new(Speaker::new()),
            mic,
            close,
            net: Notify::new(),
            net_gen: AtomicU64::new(0),
            net_drive: Notify::new(),
            endpoints: Mutex::new(Endpoints::default()),
        }
    }

    /// To the app, unless closed; a panic in its callback is caught.
    pub fn emit(&self, e: Event) {
        let _g = self.cb.lock_ok();
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let before = IN_CALLBACK.with(|c| c.replace(self as *const Shared as usize));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.events)(e)));
        IN_CALLBACK.with(|c| c.set(before));
    }

    fn link(&self) -> Link {
        self.state.lock_ok().link
    }

    /// Where QUIC goes and the pin it checks: the configuration's addresses, then
    /// `hello`'s (`link.quic`: its port on each server host, its addresses); the
    /// configuration's `link_cert_sha256`, else `hello`'s (learnt over a pinned
    /// connection only). None: QUIC isn't tried.
    fn quic_target(&self) -> Option<LinkTarget> {
        if self.cfg.transport == TransportChoice::WebSocket {
            return None;
        }
        let st = self.state.lock_ok();
        if !st.link.quic {
            return None;
        }
        let pin = self.cfg.link_cert_sha256.or(st.quic_pin)?;
        let mut addrs = self.cfg.link.clone();
        for a in &st.quic_addrs {
            if !addrs.contains(a) {
                addrs.push(a.clone());
            }
        }
        (!addrs.is_empty()).then_some(LinkTarget { addrs, pin })
    }

    /// `hello`'s `link.quic`: where the link is, and its certificate's hash.
    fn learn_quic(&self, st: &mut State, q: &Value) {
        if !q.is_object() {
            return;
        }
        let mut addrs = Vec::new();
        if let Some(port) = q["port"].as_u64().filter(|p| (1..=65_535).contains(p)) {
            for u in self.cfg.urls.iter().filter_map(|u| parse_url(u)) {
                addrs.push(if u.host.contains(':') {
                    format!("[{}]:{port}", u.host)
                } else {
                    format!("{}:{port}", u.host)
                });
            }
        }
        for a in q["addrs"].as_array().into_iter().flatten() {
            if let Some(a) = a.as_str().filter(|a| split_host_port(a).is_some()) {
                addrs.push(a.to_owned());
            }
        }
        st.quic_addrs = addrs;
        if let Some(pin) = q["cert_sha256"].as_str().and_then(parse_fingerprint) {
            st.quic_pin = Some(pin);
        }
    }

    fn connected_on(&self, conn: &Conn) {
        let mut st = self.state.lock_ok();
        st.transport = Some(conn.transport);
        st.pinned = conn.pinned;
        drop(st);
        self.connected.store(true, Ordering::SeqCst);
    }

    fn disconnected(&self) {
        let mut st = self.state.lock_ok();
        st.transport = None;
        st.pinned = false;
        drop(st);
        self.connected.store(false, Ordering::SeqCst);
    }

    /// The talk the app holds, by number.
    fn held_talk(&self) -> Option<TalkId> {
        self.state.lock_ok().talk.as_ref().map(|(id, _)| *id)
    }

    /// Talk `t` ended by the server for good: what waits of it goes (counted), and if the
    /// app still holds that talk (not a newer one) the microphone stops.
    fn talk_ended(&self, t: TalkId) {
        {
            let mut st = self.state.lock_ok();
            if st.talk.as_ref().is_some_and(|(id, _)| *id == t) {
                st.talk = None;
            } else {
                drop(st);
                self.outbox.lock_ok().end_talk(t);
                return;
            }
        }
        self.mic.lock_ok().clear();
        self.outbox.lock_ok().end_talk(t);
    }

    /// The server ended talk `t` while connected for a reason not the talk's own (no
    /// sound came during a dead zone, the link lost): its packets it dropped go again,
    /// the talk started again first. With none waiting, the talk is over.
    fn talk_interrupted(&self, t: TalkId) {
        let again = self.outbox.lock_ok().interrupted(t);
        if again {
            self.out_ready.notify_one();
        } else {
            self.talk_ended(t);
        }
    }

    /// A sound frame from the server.
    fn on_sound(&self, b: &[u8]) {
        let Some(f) = AudioFrame::parse(b) else {
            return;
        };
        if f.talker != 0 && f.kind != 2 {
            let mut st = self.state.lock_ok();
            st.have.insert(f.talker, f.seq);
            st.heard.insert(f.talker, std::time::Instant::now());
            if st.have.len() > HAVE_MAX {
                let oldest = st.heard.iter().min_by_key(|(_, at)| **at).map(|(t, _)| *t);
                if let Some(o) = oldest {
                    st.have.remove(&o);
                    st.heard.remove(&o);
                }
            }
        }
        self.speaker
            .lock_ok()
            .put((f.kind, f.talker, f.channel), f.seq, f.packet.to_vec());
    }

    /// The jitter settings from `hello`'s `link` and each channel.
    fn jitter_settings(&self, site: &Value, channels: &Value) {
        let ms = |v: &Value, k: &str| v[k].as_u64().map(|n| n.clamp(20, 20_000) as u32);
        let site = (
            ms(site, "jitter_start_ms").unwrap_or(200),
            ms(site, "jitter_max_ms").unwrap_or(2000),
        );
        let mut each = HashMap::new();
        for c in channels.as_array().into_iter().flatten() {
            if let Some(id) = c["id"].as_u64() {
                each.insert(
                    id as u32,
                    (
                        ms(c, "jitter_start_ms").unwrap_or(site.0),
                        ms(c, "jitter_max_ms").unwrap_or(site.1),
                    ),
                );
            }
        }
        self.speaker.lock_ok().set_limits(site, each);
    }

    /// A `hello`: the token, `have` afresh for a new session, the timings, the driver
    /// (`first`: the connection's first). (A held talk is started again by the outbox.)
    fn on_hello(&self, v: Value, first: bool, out: &mut Handled) {
        let resumed = v["resumed"].as_bool() == Some(true);
        let driver_changed = {
            let mut st = self.state.lock_ok();
            st.hellos += 1;
            if let Some(t) = v["resume"].as_str() {
                st.token = Some(t.to_owned());
            }
            if first && !resumed {
                st.have.clear();
                st.heard.clear();
            }
            st.link.update(&v["link"]);
            // (Where the link is, and what to pin there, only from a server pinned.)
            if st.pinned {
                self.learn_quic(&mut st, &v["link"]["quic"]);
            }
            let driver = v["me"]["driver"].clone();
            let driver = (!driver.is_null()).then_some(driver);
            let changed = st.hellos > 1 && driver != st.driver;
            st.driver = driver.clone();
            changed.then_some(driver)
        };
        self.jitter_settings(&v["link"], &v["channels"]);
        out.hello = true;
        self.emit(Event::Hello { resumed, hello: v });
        if let Some(driver) = driver_changed {
            self.emit(Event::Driver { driver });
        }
    }

    /// One JSON message from the server.
    fn on_json(&self, text: &str, first: bool) -> Handled {
        let mut out = Handled::default();
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return out;
        };
        let channel = v["channel"].as_u64().unwrap_or(0) as u32;
        match v["t"].as_str().unwrap_or_default() {
            "hello" => self.on_hello(v, first, &mut out),
            "pong" => self.outbox.lock_ok().pong(),
            "bye" => out.bye = Some(v["why"].as_str().unwrap_or("ended").to_owned()),
            "joined" => self.emit(Event::Joined {
                channel,
                may_talk: v["may_talk"].as_bool().unwrap_or(false),
            }),
            "left" => self.emit(Event::Left {
                channel,
                why: v["why"].as_str().unwrap_or_default().to_owned(),
            }),
            "talkers" => {
                let talkers = v["talkers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| Talker {
                        sid: t["sid"].as_u64().unwrap_or(0) as u32,
                        name: t["name"].as_str().unwrap_or_default().to_owned(),
                    })
                    .collect();
                self.emit(Event::Talking { channel, talkers });
            }
            "talking" => {
                let on = v["on"].as_bool() == Some(true);
                let why = v["why"].as_str().map(str::to_owned);
                // Ended by the app itself: nothing to do. By a lost link, or no sound
                // while away: started again (the packets held go after it). By the
                // server otherwise (a time limit, taken…): the app's talk is over.
                match (on, why.as_deref()) {
                    (true, _) => self.outbox.lock_ok().talking_on(),
                    (false, Some("released" | "replaced" | "a new recording") | None) => {
                        self.outbox.lock_ok().released()
                    }
                    (false, Some("link lost" | "no sound")) => {
                        // (None: the end of a talk before this connection's; nothing.)
                        let t = self.outbox.lock_ok().server_ended();
                        if let Some(t) = t {
                            self.talk_interrupted(t);
                        }
                    }
                    (false, Some(_)) => {
                        let t = self.outbox.lock_ok().server_ended();
                        if let Some(t) = t {
                            self.talk_ended(t);
                        }
                    }
                }
                self.emit(Event::Talk { on, why });
            }
            // Half duplex, someone has the floor: the start refused.
            "busy" => {
                let t = self.outbox.lock_ok().refused();
                if let Some(t) = t {
                    self.talk_ended(t);
                }
                self.emit(Event::Talk {
                    on: false,
                    why: Some("busy".into()),
                });
                self.emit(Event::Other(v));
            }
            // Dispatch took the floor (a `talking` off follows or came: whichever first).
            "taken" => {
                let t = self.outbox.lock_ok().server_ended();
                if let Some(t) = t {
                    self.talk_ended(t);
                }
                self.emit(Event::Other(v));
            }
            // A replay's last frame: the next replay's numbers start again.
            "replay_end" => {
                self.speaker.lock_ok().replay_ended();
                self.emit(Event::Other(v));
            }
            "message" => self.emit(Event::Message(v["message"].clone())),
            "sos" | "sos_sent" | "sos_ack" => self.emit(Event::Sos(v)),
            "error" => {
                let what = v["what"].as_str().map(str::to_owned);
                if what.as_deref() == Some("talk") {
                    let t = self.outbox.lock_ok().refused();
                    if let Some(t) = t {
                        self.talk_ended(t);
                    }
                }
                self.emit(Event::Error {
                    what,
                    error: v["error"].as_str().unwrap_or_default().to_owned(),
                });
            }
            "channels" => {
                let (start, max) = self.speaker.lock_ok().site_limits();
                let site = json!({"jitter_start_ms": start, "jitter_max_ms": max});
                self.jitter_settings(&site, &v["channels"]);
                self.emit(Event::Other(v));
            }
            _ => self.emit(Event::Other(v)),
        }
        out
    }
}

/// What handling a message asks of the connection.
#[derive(Default)]
pub(crate) struct Handled {
    /// The server ended the session (`bye`).
    pub bye: Option<String>,
    /// A `hello` came (timings may have changed).
    pub hello: bool,
}

/// `TALKER:SEQ,…` read (what can't be read is left out).
pub(crate) fn parse_have(s: &str) -> BTreeMap<u32, u32> {
    s.split(',')
        .filter_map(|p| {
            let (t, q) = p.trim().split_once(':')?;
            Some((t.parse().ok()?, q.parse().ok()?))
        })
        .collect()
}

/// A task aborted when dropped.
struct Task<T>(JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for Task<T> {
    type Output = Option<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        Pin::new(&mut self.0).poll(cx).map(Result::ok)
    }
}

/// A candidate's result, or never while there is none.
async fn finished<T>(c: &mut Option<Task<T>>) -> Option<T> {
    match c {
        Some(t) => t.await,
        None => std::future::pending().await,
    }
}

type Connected = Result<(Conn, Value), Refused>;

/// One connection made: QUIC first (within `link_connect_ms`), then each WebSocket.
async fn connect(sh: &Shared) -> Connected {
    let (resume, link) = {
        let st = sh.state.lock_ok();
        (st.resume(), st.link)
    };
    let cfg = &sh.cfg;
    let mut last = None;
    if let Some(to) = sh.quic_target() {
        match timeout(
            to.budget(link.connect),
            conn::quic(cfg, &to, &sh.endpoints, link.times(), resume.as_ref()),
        )
        .await
        {
            Ok(Ok(c)) => return Ok(c),
            Ok(Err(e)) => last = Some(e),
            Err(_) => {
                last = Some(Refused::Other(format!(
                    "QUIC: no answer in {} ms",
                    link.connect.as_millis()
                )))
            }
        }
    }
    if cfg.transport != TransportChoice::Quic {
        let within = Duration::from_millis(cfg.ws_connect_ms);
        for url in &cfg.urls {
            match timeout(within, conn::websocket(cfg, url, resume.as_ref())).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(Refused::Credential(e))) => return Err(Refused::Credential(e)),
                Ok(Err(e)) => last = Some(e),
                Err(_) => {
                    last = Some(Refused::Other(format!(
                        "{url}: no answer in {} ms",
                        cfg.ws_connect_ms
                    )))
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| Refused::Other("nowhere to connect".into())))
}

/// How a connection ended.
enum End {
    /// The app closed the client.
    Closed,
    /// The server ended the session for good (`bye`).
    Bye(String),
    /// The link went: connect again and resume.
    Dropped(String),
}

/// The wait before the `attempt`th try after failures: 1, 2, 4, 8, then 10 s
/// (VOICE-CLIENT.md §6; `idn_common::backoff`'s shape, not a dependency here).
pub(crate) fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(match attempt {
        0 => 0,
        n => (1u64 << (n - 1).min(4)).min(10),
    })
}

/// The session until closed or ended for good.
pub(crate) async fn run(sh: Arc<Shared>, mut close: watch::Receiver<bool>) {
    let mut attempt = 0u32;
    loop {
        // (A network change from here on gives up the connection this makes.)
        let net_at = sh.net_gen.load(Ordering::SeqCst);
        let r = tokio::select! {
            r = connect(&sh) => r,
            _ = async { let _ = close.wait_for(|c| *c).await; } => break,
        };
        let why = match r {
            Ok((c, hello)) => {
                let began = Instant::now();
                match drive(&sh, c, hello, net_at, &mut close).await {
                    End::Closed => break,
                    End::Bye(why) => {
                        sh.emit(Event::Closed { why });
                        break;
                    }
                    End::Dropped(why) => {
                        // Held from now until the next connection; what wasn't
                        // confirmed goes again then.
                        sh.disconnected();
                        requeue(&sh);
                        if began.elapsed() >= LIVED {
                            attempt = 0;
                            sh.emit(Event::Reconnecting { attempt, why });
                            continue;
                        }
                        why
                    }
                }
            }
            Err(Refused::Credential(why)) => {
                sh.emit(Event::Error {
                    what: None,
                    error: why.clone(),
                });
                sh.emit(Event::Closed { why });
                break;
            }
            Err(Refused::Other(why)) => why,
        };
        attempt += 1;
        sh.emit(Event::Reconnecting { attempt, why });
        tokio::select! {
            _ = tokio::time::sleep(backoff(attempt)) => {}
            _ = sh.net.notified() => {}
            _ = async { let _ = close.wait_for(|c| *c).await; } => break,
        }
    }
    sh.disconnected();
}

/// Byes after which the session is resumed rather than ended: a client too far behind,
/// and a resume onto another connection (a move-over this side gave up on).
fn resumable(why: &str) -> bool {
    matches!(why, "too far behind" | "resumed on another connection")
}

/// A timer from `start` every `period`; ticks missed (while QUIC wasn't to be tried, a
/// stalled loop) are not made up in a burst.
fn every(start: Instant, period: Duration) -> tokio::time::Interval {
    let mut i = tokio::time::interval_at(start, period);
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    i
}

/// Sends what the outbox holds into `conn`, in order: commands as they come up, live
/// packets at once, a backlog one packet a call (`paced`: the pace's tick; not paced, a
/// backlog waits for it). A full writer leaves the rest for later.
fn drain(sh: &Shared, conn: &Conn, paced: bool) {
    let mut ob = sh.outbox.lock_ok();
    let backlog = ob.waiting() > outbox::LIVE;
    if backlog && !paced {
        return;
    }
    let mut sounds = 0;
    while let Some(item) = ob.front() {
        let w = match item {
            Item::Talk { json, .. } | Item::Cmd(json) => Wire::Json(json.clone()),
            Item::Sound { .. } if backlog && sounds == 1 => break,
            Item::Sound { bytes, .. } => {
                sounds += 1;
                Wire::Sound(bytes.clone())
            }
        };
        if conn.tx.try_send(w).is_err() {
            break;
        }
        ob.sent();
    }
}

/// A `ping`, marked in the outbox: its `pong` confirms what was sent before it.
async fn ping(sh: &Shared, conn: &Conn, within: Duration) -> bool {
    sh.outbox.lock_ok().ping();
    let p = Wire::Json(json!({"t": "ping"}).to_string());
    matches!(timeout(within, conn.tx.send(p)).await, Ok(Ok(())))
}

/// The connection changed under a talk: what wasn't confirmed goes again, after the
/// talk is started again.
fn requeue(sh: &Shared) {
    let held = sh.held_talk();
    sh.outbox.lock_ok().requeue(held);
    sh.out_ready.notify_one();
}

/// One connection's life, with moves to QUIC. `net_at`: the network changes counted when
/// it was made.
async fn drive(
    sh: &Arc<Shared>,
    mut conn: Conn,
    hello: Value,
    mut net_at: u64,
    close: &mut watch::Receiver<bool>,
) -> End {
    let start = Instant::now();
    sh.connected_on(&conn);
    sh.emit(Event::Connected(conn.transport));
    let mut h = sh.on_json(&hello.to_string(), true);
    let mut link = sh.link();
    // Anything from the server shows the link lives; nothing for `idle_timeout_s`: dead.
    let mut last_rx = start;
    let mut ping_every = every(start + link.ping, link.ping);
    let mut retry = every(start + link.retry, link.retry);
    // While anything sent is unconfirmed: a ping a second (at most that much is sent
    // twice after a silent drop).
    let mut confirm = every(start + Duration::from_secs(1), Duration::from_secs(1));
    let mut pace = every(start, outbox::PACE);
    let mut candidate: Option<Task<Connected>> = None;
    loop {
        if h.hello {
            let now = sh.link();
            if now.ping != link.ping {
                ping_every = every(Instant::now() + now.ping, now.ping);
            }
            if now.retry != link.retry {
                retry = every(Instant::now() + now.retry, now.retry);
            }
            link = now;
            h.hello = false;
        }
        if let Some(why) = h.bye.take() {
            // The server moved the session to the QUIC candidate: take it.
            if candidate.is_some() && why == "resumed on another connection" {
                match timeout(link.connect, finished(&mut candidate)).await {
                    Ok(Some(Ok((c, hello)))) => {
                        candidate = None;
                        conn = c;
                        net_at = sh.net_gen.load(Ordering::SeqCst);
                        last_rx = Instant::now();
                        requeue(sh);
                        sh.connected_on(&conn);
                        sh.emit(Event::Connected(conn.transport));
                        h = sh.on_json(&hello.to_string(), true);
                        continue;
                    }
                    _ => return End::Dropped(why),
                }
            }
            return if resumable(&why) {
                End::Dropped(why)
            } else {
                End::Bye(why)
            };
        }
        let quic_retry = conn.transport == Transport::WebSocket
            && candidate.is_none()
            && sh.quic_target().is_some();
        let (waiting, unconfirmed) = {
            let ob = sh.outbox.lock_ok();
            (ob.waiting() > 0, ob.unconfirmed())
        };
        tokio::select! {
            biased;
            _ = async { let _ = close.wait_for(|c| *c).await; } => {
                // What the app said last goes first (a talk let go: its last packets and
                // its end), within a second; then the session ends now, not waiting for
                // a resume.
                let until = Instant::now() + Duration::from_secs(1);
                loop {
                    let waiting = sh.outbox.lock_ok().waiting();
                    if waiting == 0 || Instant::now() >= until {
                        break;
                    }
                    drain(sh, &conn, true);
                    tokio::time::sleep(outbox::PACE).await;
                }
                let _ = timeout(
                    Duration::from_secs(1),
                    conn.tx.send(Wire::Json(json!({"t": "bye"}).to_string())),
                ).await;
                conn.flush(Duration::from_millis(500)).await;
                return End::Closed;
            }
            _ = tokio::time::sleep_until(last_rx + link.idle) => {
                return End::Dropped(format!(
                    "nothing from the server in {} s", link.idle.as_secs()
                ));
            }
            m = conn.rx.recv() => match m {
                None => {
                    // The server may be moving the session to the candidate.
                    if candidate.is_some() {
                        h.bye = Some("resumed on another connection".into());
                        continue;
                    }
                    return End::Dropped("the connection ended".into());
                }
                Some(Wire::Json(t)) => {
                    last_rx = Instant::now();
                    h = sh.on_json(&t, false);
                }
                Some(Wire::Sound(b)) => {
                    last_rx = Instant::now();
                    sh.on_sound(&b);
                }
            },
            _ = sh.out_ready.notified() => drain(sh, &conn, false),
            _ = pace.tick(), if waiting => drain(sh, &conn, true),
            _ = sh.net_drive.notified() => {
                // QUIC moves to the new network by itself; a TCP connection made on the
                // old one is dead: resume on a new one now, not at the idle timeout. (A
                // change from before this connection was made is no reason.)
                if conn.transport == Transport::WebSocket
                    && sh.net_gen.load(Ordering::SeqCst) > net_at
                {
                    return End::Dropped("the network changed".into());
                }
            }
            _ = ping_every.tick() => {
                if !ping(sh, &conn, link.idle).await {
                    return End::Dropped("the link is stalled".into());
                }
            }
            _ = confirm.tick(), if unconfirmed => {
                if !ping(sh, &conn, link.idle).await {
                    return End::Dropped("the link is stalled".into());
                }
            }
            _ = retry.tick(), if quic_retry => {
                let (resume, link) = {
                    let st = sh.state.lock_ok();
                    (st.resume(), st.link)
                };
                let Some(to) = sh.quic_target() else { continue };
                let sh2 = sh.clone();
                candidate = Some(Task(tokio::spawn(async move {
                    match timeout(
                        to.budget(link.connect),
                        conn::quic(&sh2.cfg, &to, &sh2.endpoints, link.times(), resume.as_ref()),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(Refused::Other("QUIC: no answer".into())),
                    }
                })));
            }
            r = finished(&mut candidate) => {
                candidate = None;
                if let Some(Ok((c, hello))) = r {
                    // On QUIC now; the WebSocket is let go without a word (the server
                    // moved the session already, and ended its talk: what wasn't
                    // confirmed there goes again here).
                    conn = c;
                    net_at = sh.net_gen.load(Ordering::SeqCst);
                    last_rx = Instant::now();
                    requeue(sh);
                    sh.connected_on(&conn);
                    sh.emit(Event::Connected(conn.transport));
                    h = sh.on_json(&hello.to_string(), true);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn have_and_backoff() {
        let h = parse_have("17:204, 22:9,bad,3:x");
        assert_eq!(h, BTreeMap::from([(17, 204), (22, 9)]));
        let st = State {
            token: Some("tok".into()),
            have: h,
            heard: HashMap::new(),
            link: Link::from_config(&Config::new("c")),
            quic_addrs: Vec::new(),
            quic_pin: None,
            transport: None,
            pinned: false,
            talk: None,
            driver: None,
            hellos: 0,
        };
        assert_eq!(
            st.resume(),
            Some(ResumeState {
                token: "tok".into(),
                have: "17:204,22:9".into()
            })
        );
        let waits: Vec<u64> = (0..8).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(waits, [0, 1, 2, 4, 8, 10, 10, 10]);
    }

    #[test]
    fn hello_s_link_block_sets_the_timings() {
        let mut l = Link::from_config(&Config::new("c"));
        assert_eq!(l.connect, Duration::from_millis(3000));
        l.update(
            &json!({"ping_s": 5, "idle_timeout_s": 40, "link_connect_ms": 1500,
                         "link_retry_s": 30, "link_keepalive_ms": 4000, "quic": null,
                         "jitter_start_ms": 200}),
        );
        assert_eq!(
            l,
            Link {
                ping: Duration::from_secs(5),
                idle: Duration::from_secs(40),
                connect: Duration::from_millis(1500),
                retry: Duration::from_secs(30),
                keepalive: Duration::from_millis(4000),
                quic: false,
            }
        );
        // Zeros don't make a spinning timer.
        l.update(&json!({"ping_s": 0, "link_retry_s": 0}));
        assert_eq!((l.ping.as_secs(), l.retry.as_secs()), (1, 1));
    }

    fn shared(cfg: &str) -> Shared {
        let mut cfg = Config::from_json(cfg).unwrap();
        cfg.check().unwrap();
        Shared::new(cfg, Box::new(|_| {}), watch::channel(false).0)
    }

    #[test]
    fn hello_says_where_the_link_is_and_its_certificate() {
        let sh = shared(
            r#"{"credential": "c", "urls": ["https://10.0.0.5:29203/voice", "https://[fd00::5]:29203/voice"]}"#,
        );
        assert_eq!(sh.quic_target(), None, "no address, no pin yet");
        let hello = |quic: Value| {
            json!({"t": "hello", "sid": 1, "resume": "t1", "link": {"quic": quic}}).to_string()
        };
        let offer = || {
            json!({"port": 29203, "addrs": ["voice.example.com:29203", "bad"],
                   "cert_sha256": "ab".repeat(32)})
        };
        // Over a connection not pinned (plain, or a public authority's certificate):
        // nothing learnt.
        sh.on_json(&hello(offer()), true);
        assert_eq!(sh.quic_target(), None);
        sh.state.lock_ok().pinned = true;
        sh.on_json(
            &hello(
                json!({"port": 29203, "addrs": ["voice.example.com:29203", "bad"],
                          "cert_sha256": "ab".repeat(32)}),
            ),
            true,
        );
        assert_eq!(
            sh.quic_target(),
            Some(LinkTarget {
                addrs: vec![
                    "10.0.0.5:29203".into(),
                    "[fd00::5]:29203".into(),
                    "voice.example.com:29203".into()
                ],
                pin: [0xab; 32],
            })
        );
        assert_eq!(sh.state.lock_ok().token.as_deref(), Some("t1"));
        // The site turned it off.
        sh.on_json(&hello(Value::Null), false);
        assert_eq!(sh.quic_target(), None);
        // A configured pin is never replaced by hello's.
        let mine = shared(
            r#"{"credential": "c", "urls": ["https://h/voice"], "link_cert_sha256": "CDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCDCD"}"#,
        );
        mine.state.lock_ok().pinned = true;
        mine.on_json(&hello(offer()), true);
        assert_eq!(mine.quic_target().unwrap().pin, [0xcd; 32]);
        // A WebSocket-only client never tries it.
        let ws = shared(
            r#"{"credential": "c", "urls": ["https://h/voice"], "link": ["h:1"], "link_cert_sha256": "ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB", "transport": "websocket"}"#,
        );
        assert_eq!(ws.quic_target(), None);
    }
}
