//! The phone mixing library (spec/extensions/idn-iem.md §I.8): what an Android or iOS
//! app embeds to take a desk's IEM flow, mix it by the musician's mix and play it.
//!
//! ```no_run
//! use idn_iem::receiver::{Config, Receiver, Transport};
//! let rx = Receiver::connect(Config {
//!     desk: "192.168.1.20".into(),
//!     token: "idn1.…".into(),
//!     trust: Some(idn_iem::receiver::Trust::Certificate(std::fs::read("site-ca.der")?)),
//!     transport: Transport::Auto,
//!     ..Config::default()
//! })?;
//! // In the app's audio callback (stereo, interleaved, 48 kHz):
//! let mut buf = vec![0f32; 2 * 192];
//! rx.render(&mut buf);
//! // From its faders:
//! rx.set_level(3, -6.0)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! - **Connecting** ([`Receiver::connect`]): the flow's description (`GET /api/v1/iem`),
//!   a UDP session (SUBSCRIBE, unicast or multicast; KEEPALIVE every second; subscribed
//!   again on UNKNOWN or when the desk stops answering), the push channel
//!   (`GET /api/v1/events`) for the mix. A REPLY's refusal is a typed [`Error`].
//! - **Receiving:** a jitter buffer by seq ([`jitter`]), one Opus decoder per channel the
//!   mix uses, lost frames concealed (Opus PLC).
//! - **Mixing** ([`mixing`]): levels, pans (a pair's balance), mutes, solos, master; gains
//!   glide over each 5 ms frame. Output: stereo f32, 48 kHz, pulled by
//!   [`Receiver::render`] from the app's audio callback.
//! - **Following the mix:** the desk's `state` (the musician's and the sound person's
//!   changes alike) applied live; this phone's changes ([`Receiver::set_level`] …) heard
//!   at once and sent as partial `iem_mix` commands with per-control sequence numbers.
//!   While a change of this phone's is on its way, the desk's older value for that
//!   control doesn't pull the fader back.
//! - **Talkback** ([`Receiver::talk_start`], [`Receiver::talk_feed`],
//!   [`Receiver::talk_stop`]).
//!
//! Threads: two of its own (the UDP session and audio; the push channel and the
//! description) and, while multicast, a third. [`Receiver::render`] runs on the caller's
//! (audio) thread: it decodes there, and holds a lock only to take the next frame.

pub mod http;
pub mod jitter;
pub mod mixing;
mod net;
pub mod talk;
mod types;

pub use jitter::Counters;
pub use mixing::{Channel, ChannelMix, Mix, Side};
pub use types::*;

use net::*;

use crate::wire::{self, Packet, Status as Reply};
use jitter::Jitter;
use mixing::{Compiled, Engine};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The flow's description (`GET /api/v1/iem`), as far as the phone needs it.
#[derive(Debug, Clone, PartialEq)]
struct Description {
    desk: String,
    flow: u32,
    layout: u16,
    channels: Vec<Channel>,
    multicast: Option<(Ipv4Addr, u16)>,
    unicast: Option<u16>,
    desk_ms: f32,
}

impl Description {
    fn parse(v: &Value) -> Result<Description, Error> {
        let bad = |k: &str| Error::Http(200, format!("the description's {k}"));
        if v["wire_version"].as_u64() != Some(u64::from(wire::VERSION)) {
            return Err(Error::Http(
                200,
                format!(
                    "the desk's IEM wire version {} (this library: {})",
                    v["wire_version"],
                    wire::VERSION
                ),
            ));
        }
        Ok(Description {
            desk: v["desk"].as_str().unwrap_or_default().to_owned(),
            flow: v["flow"]
                .as_u64()
                .and_then(|f| u32::try_from(f).ok())
                .ok_or_else(|| bad("flow"))?,
            layout: v["layout"].as_u64().map_or(0, |l| l as u16),
            channels: mixing::channels_of(&v["channels"]),
            multicast: v["multicast"]["group"]
                .as_str()
                .and_then(|g| g.parse().ok())
                .zip(
                    v["multicast"]["port"]
                        .as_u64()
                        .and_then(|p| u16::try_from(p).ok()),
                ),
            unicast: v["unicast"]["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok()),
            desk_ms: v["latency"]["desk_ms"].as_f64().unwrap_or(10.0) as f32,
        })
    }
}

/// The UDP session's state.
#[derive(Debug, Clone, PartialEq)]
enum Session {
    Connecting,
    Ok,
    /// Refused (said once; tried again every few seconds: a new token, a free place).
    Refused(Error),
}

struct Net {
    session: Session,
    asked: Transport,
    transport: Transport,
    desk_udp: Option<SocketAddr>,
    group: Option<(Ipv4Addr, u16)>,
    flow: u32,
    layout: u16,
    keepalive: Duration,
    timeout: Duration,
    last_reply: Option<Instant>,
    next_subscribe: Instant,
    subscribed_at: Option<Instant>,
    last_keepalive: Instant,
    keepalive_sent: Option<Instant>,
    rtt_ms: Option<f32>,
    audio_at: Option<Instant>,
    mcast_audio_at: Option<Instant>,
    talk_allowed: bool,
    mcast: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    subscribes: u64,
    error: Option<String>,
}

/// A control the phone changed, not yet confirmed by the desk.
#[derive(Debug, Clone, Copy, Default)]
struct Pending {
    /// Changed since last sent.
    dirty: bool,
    /// The command carrying it, awaiting its reply.
    inflight: Option<u64>,
}

struct Ctl {
    mix_id: Option<String>,
    channels: Vec<Channel>,
    /// The mix as played: the desk's with this phone's changes on their way.
    mix: Option<Mix>,
    desk_mix: Option<Mix>,
    pending: BTreeMap<String, Pending>,
    next_id: u64,
    next_seq: u64,
    no_mix_said: bool,
}

type Callback = Arc<dyn Fn(&Event) + Send + Sync>;

/// What [`Receiver::set_event_callback`] takes.
pub type EventCallback = Box<dyn Fn(&Event) + Send + Sync>;

pub(crate) struct Core {
    cfg: Config,
    token: Mutex<String>,
    endpoint: http::Endpoint,
    client: u64,
    socket: Arc<UdpSocket>,
    desc: Mutex<Description>,
    net: Mutex<Net>,
    net_cv: Condvar,
    ctl: Mutex<Ctl>,
    compiled: Mutex<Arc<Compiled>>,
    compiled_version: AtomicU64,
    jitter: Mutex<Jitter>,
    engine: Mutex<Engine>,
    talk: Mutex<talk::Talk>,
    events: Mutex<VecDeque<Event>>,
    events_cv: Condvar,
    callback: Mutex<Option<Callback>>,
    stop: AtomicBool,
    refetch: AtomicBool,
    push_up: AtomicBool,
    /// The sound output's latency, µs.
    output_us: AtomicU64,
}

/// A lock a panicked holder left is still taken. (`idn_common::sync::LockOk`'s shape: this
/// crate depends on nothing of IDN's own, for the phone builds; so too its random id and
/// its push channel's retry wait below.)
pub(crate) trait LockOk<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T>;
}

impl<T> LockOk<T> for Mutex<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A random session id (not 0): the system's generator through ring, as
/// `idn_open_protocol::random` (not a dependency here).
fn random_client() -> u64 {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 8];
    if ring::rand::SystemRandom::new().fill(&mut b).is_err() {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        b = h.finish().to_be_bytes();
    }
    u64::from_be_bytes(b).max(1)
}

/// The phone's side of a desk's IEM flow: connected, receiving, mixing; a clone is the
/// same receiver (it closes when the last goes, or on [`Receiver::close`]).
#[derive(Clone)]
pub struct Receiver {
    core: Arc<Core>,
    _life: Arc<Life>,
}

struct Life {
    core: Arc<Core>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Life {
    fn drop(&mut self) {
        self.core.shut();
        for t in self.threads.lock_ok().drain(..) {
            let _ = t.join();
        }
    }
}

impl Receiver {
    /// Connects to the desk: fetches the flow's description, opens the session (waiting
    /// for the desk's answer: a refusal comes back as its [`Error`]) and the push channel,
    /// and waits briefly for the mix.
    pub fn connect(cfg: Config) -> Result<Receiver, Error> {
        if !(cfg.buffer_ms.is_finite()
            && cfg.min_buffer_ms.is_finite()
            && cfg.max_buffer_ms.is_finite())
            || cfg.max_buffer_ms < cfg.min_buffer_ms
        {
            return Err(Error::Config("buffer: min ≤ max, in ms".into()));
        }
        let endpoint = http::Endpoint::new(&cfg.desk, cfg.trust.as_ref())?;
        let desc = fetch_description(&endpoint, &cfg.token)?;
        let Some(uport) = cfg.udp_port.or(desc.unicast) else {
            return Err(Error::NoFlow);
        };
        let transport = match cfg.transport {
            Transport::Multicast if desc.multicast.is_none() => return Err(Error::NoMulticast),
            Transport::Auto if desc.multicast.is_none() => Transport::Unicast,
            Transport::Auto => Transport::Multicast,
            t => t,
        };
        let bind: SocketAddr = if endpoint.addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = UdpSocket::bind(bind).map_err(|e| Error::Io(format!("UDP: {e}")))?;
        let _ = socket.set_read_timeout(Some(Duration::from_millis(20)));
        let frame_ms = crate::FRAME_SAMPLES as f32 * 1000.0 / crate::RATE as f32;
        let now = Instant::now();
        let core = Arc::new(Core {
            token: Mutex::new(cfg.token.clone()),
            endpoint: endpoint.clone(),
            client: random_client(),
            socket: Arc::new(socket),
            net: Mutex::new(Net {
                session: Session::Connecting,
                asked: cfg.transport,
                transport,
                desk_udp: Some(SocketAddr::new(endpoint.addr.ip(), uport)),
                group: desc.multicast,
                flow: desc.flow,
                layout: desc.layout,
                keepalive: Duration::from_millis(u64::from(crate::KEEPALIVE_MS)),
                timeout: Duration::from_millis(u64::from(crate::TIMEOUT_MS)),
                last_reply: None,
                next_subscribe: now,
                subscribed_at: None,
                last_keepalive: now,
                keepalive_sent: None,
                rtt_ms: None,
                audio_at: None,
                mcast_audio_at: None,
                talk_allowed: cfg.talk,
                mcast: None,
                subscribes: 0,
                error: None,
            }),
            net_cv: Condvar::new(),
            ctl: Mutex::new(Ctl {
                mix_id: cfg.mix.clone(),
                channels: desc.channels.clone(),
                mix: None,
                desk_mix: None,
                pending: BTreeMap::new(),
                next_id: 1,
                next_seq: 1,
                no_mix_said: false,
            }),
            desc: Mutex::new(desc),
            compiled: Mutex::default(),
            compiled_version: AtomicU64::new(0),
            jitter: Mutex::new(Jitter::new(
                frame_ms,
                cfg.buffer_ms,
                cfg.min_buffer_ms,
                cfg.max_buffer_ms,
            )),
            engine: Mutex::new(Engine::new()),
            talk: Mutex::new(talk::Talk::new()),
            events: Mutex::default(),
            events_cv: Condvar::new(),
            callback: Mutex::default(),
            stop: AtomicBool::new(false),
            refetch: AtomicBool::new(false),
            push_up: AtomicBool::new(false),
            output_us: AtomicU64::new(0),
            cfg,
        });
        let life = Arc::new(Life {
            core: core.clone(),
            threads: Mutex::default(),
        });
        for (name, f) in [
            ("idn-iem-net", net_loop as fn(Arc<Core>)),
            ("idn-iem-push", push_loop as fn(Arc<Core>)),
        ] {
            let c = core.clone();
            let t = std::thread::Builder::new()
                .name(name.into())
                .spawn(move || f(c))
                .map_err(|e| Error::Io(e.to_string()))?;
            life.threads.lock_ok().push(t);
        }
        let rx = Receiver {
            core: core.clone(),
            _life: life,
        };
        // The session: answered, or refused.
        let end = Instant::now() + core.cfg.connect_timeout;
        {
            let mut n = core.net.lock_ok();
            loop {
                match &n.session {
                    Session::Ok => break,
                    Session::Refused(Error::Busy(_)) => {}
                    Session::Refused(e) => return Err(e.clone()),
                    Session::Connecting => {}
                }
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(Error::Timeout);
                }
                n = core
                    .net_cv
                    .wait_timeout(n, left)
                    .map(|r| r.0)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
        }
        // The mix, briefly (the push channel's first state).
        let end = Instant::now() + Duration::from_secs(1).min(core.cfg.connect_timeout);
        while Instant::now() < end && core.ctl.lock_ok().mix.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(rx)
    }

    /// Fills `out` with the mix: stereo, interleaved, 48 kHz (`out.len() / 2` frames).
    /// Call it from the audio output's callback, at its pace: the jitter buffer's clock.
    pub fn render(&self, out: &mut [f32]) {
        let core = &self.core;
        let mut eng = core.engine.lock_ok();
        eng.render(out, |e| {
            let v = core.compiled_version.load(Ordering::Acquire);
            if v != e.version
                && let Ok(c) = core.compiled.try_lock()
            {
                e.compiled = c.clone();
                e.version = v;
            }
            core.jitter.lock_ok().pop()
        });
    }

    /// The mix as played now (`None` until the desk's state came).
    pub fn mix(&self) -> Option<Mix> {
        self.core.ctl.lock_ok().mix.clone()
    }

    /// The flow's channels, in order.
    pub fn channels(&self) -> Vec<Channel> {
        self.core.ctl.lock_ok().channels.clone()
    }

    /// A channel's level (dB, −144 off … +10). `ch`: the desk channel's number; a linked
    /// pair by either side.
    pub fn set_level(&self, ch: u16, db: f32) -> Result<(), Error> {
        if !db.is_finite() {
            return Err(Error::Config("level: a number of dB".into()));
        }
        let db = db.clamp(mixing::LEVEL_MIN, mixing::LEVEL_MAX);
        self.core
            .edit(Some(ch), "level_db", |m, id| entry(m, id).level_db = db)
    }

    /// A channel's pan (−1 left … +1 right; a pair's balance).
    pub fn set_pan(&self, ch: u16, pan: f32) -> Result<(), Error> {
        if !pan.is_finite() {
            return Err(Error::Config("pan: −1 to 1".into()));
        }
        let pan = pan.clamp(-1.0, 1.0);
        self.core
            .edit(Some(ch), "pan", |m, id| entry(m, id).pan = pan)
    }

    pub fn set_mute(&self, ch: u16, on: bool) -> Result<(), Error> {
        self.core
            .edit(Some(ch), "mute", |m, id| entry(m, id).mute = on)
    }

    pub fn set_solo(&self, ch: u16, on: bool) -> Result<(), Error> {
        self.core
            .edit(Some(ch), "solo", |m, id| entry(m, id).solo = on)
    }

    /// The mix's master level (dB, −144 … +10).
    pub fn set_master(&self, db: f32) -> Result<(), Error> {
        if !db.is_finite() {
            return Err(Error::Config("master: a number of dB".into()));
        }
        let db = db.clamp(mixing::LEVEL_MIN, mixing::LEVEL_MAX);
        self.core.edit(None, "master_db", |m, _| m.master_db = db)
    }

    /// Puts a channel in the mix (at 0 dB, centre) or takes it out.
    pub fn set_in_mix(&self, ch: u16, on: bool) -> Result<(), Error> {
        self.core.edit(Some(ch), "in", |m, id| {
            let id = id.unwrap_or_default();
            if on {
                m.channels.entry(id).or_default();
            } else {
                m.channels.remove(&id);
            }
        })?;
        if !on {
            // (Its other controls' changes on their way go with it.)
            let mut c = self.core.ctl.lock_ok();
            let pre = format!("c{}.", c.lead(ch)?);
            c.pending
                .retain(|k, _| !k.starts_with(&pre) || k.ends_with(".in"));
        }
        Ok(())
    }

    /// Push-to-talk pressed.
    pub fn talk_start(&self) -> Result<(), Error> {
        if !self.core.net.lock_ok().talk_allowed {
            return Err(Error::NotAllowed(if self.core.cfg.talk {
                "this token may not talk back".into()
            } else {
                "connected without talk (Config::talk)".into()
            }));
        }
        self.core.talk.lock_ok().start().map_err(Error::Io)
    }

    /// The microphone's samples while talking (interleaved, `channels` a frame, at any
    /// `rate`): mono 48 kHz Opus, a TALK packet every 10 ms. Ignored while not talking.
    pub fn talk_feed(&self, pcm: &[f32], rate: u32, channels: u16) -> Result<(), Error> {
        let core = &self.core;
        let mut t = core.talk.lock_ok();
        if !t.on {
            return Ok(());
        }
        let mut err = None;
        t.feed(pcm, rate, channels, |d, n, last, seq| {
            if let Err(e) = core.send_talk(d, n, last, seq) {
                err = Some(e);
            }
        });
        err.map_or(Ok(()), Err)
    }

    /// Push-to-talk released: what's left goes, the last frame marked LAST.
    pub fn talk_stop(&self) -> Result<(), Error> {
        let core = &self.core;
        let mut err = None;
        core.talk.lock_ok().stop(|d, n, last, seq| {
            if let Err(e) = core.send_talk(d, n, last, seq) {
                err = Some(e);
            }
        });
        err.map_or(Ok(()), Err)
    }

    /// How it goes now.
    pub fn status(&self) -> Status {
        // (One lock at a time: the threads take them in their own orders.)
        let core = &self.core;
        let (buffer_ms, target_ms, loss, counters) = {
            let j = core.jitter.lock_ok();
            (j.level_ms(), j.target_ms(), j.loss(), j.counters)
        };
        let talking = core.talk.lock_ok().on;
        let mix = {
            let c = core.ctl.lock_ok();
            c.mix.as_ref().map(|m| m.id.clone()).or(c.mix_id.clone())
        };
        let channels = core.compiled.lock_ok().chans.len();
        let desc = core.desc.lock_ok().clone();
        let output_ms = core.output_us.load(Ordering::Relaxed) as f32 / 1000.0;
        let n = core.net.lock_ok();
        let session = n.session == Session::Ok;
        let fresh = n
            .audio_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1));
        let rtt = n.rtt_ms;
        Status {
            connected: session && fresh,
            session,
            push: core.push_up.load(Ordering::Relaxed),
            transport: n.transport,
            asked: n.asked,
            buffer_ms,
            target_ms,
            loss_pct: loss * 100.0,
            rtt_ms: rtt,
            desk_ms: desc.desk_ms,
            output_ms,
            latency_ms: desc.desk_ms + rtt.map_or(1.0, |r| r / 2.0) + buffer_ms + output_ms,
            talk_allowed: n.talk_allowed,
            talking,
            channels,
            counters,
            subscribes: n.subscribes,
            error: n.error.clone(),
            desk: desc.desk,
            mix,
        }
    }

    /// The next event, waiting up to `timeout` for one.
    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        let core = &self.core;
        let mut q = core.events.lock_ok();
        let end = Instant::now() + timeout;
        loop {
            if let Some(e) = q.pop_front() {
                return Some(e);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || core.stop.load(Ordering::Relaxed) {
                return None;
            }
            q = core
                .events_cv
                .wait_timeout(q, left)
                .map(|r| r.0)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    /// Events to `f` as they happen (on the library's threads; keep it short), instead
    /// of [`next_event`](Self::next_event)'s queue; `None` back to the queue.
    pub fn set_event_callback(&self, f: Option<EventCallback>) {
        *self.core.callback.lock_ok() = f.map(Arc::from);
    }

    /// The sound output's latency (ms), for [`Status::latency_ms`].
    pub fn set_output_latency_ms(&self, ms: f32) {
        let us = if ms.is_finite() {
            (ms.max(0.0) * 1000.0) as u64
        } else {
            0
        };
        self.core.output_us.store(us, Ordering::Relaxed);
    }

    /// A new token (the old one expiring): used from now on, the session renewed with it.
    pub fn set_token(&self, token: &str) {
        *self.core.token.lock_ok() = token.to_owned();
        let mut n = self.core.net.lock_ok();
        n.next_subscribe = Instant::now();
        n.session = Session::Connecting;
    }

    /// Unicast, multicast, or auto, from now on.
    pub fn set_transport(&self, t: Transport) -> Result<(), Error> {
        let core = &self.core;
        let group = core.desc.lock_ok().multicast;
        let now = match t {
            Transport::Multicast if group.is_none() => return Err(Error::NoMulticast),
            Transport::Unicast => Transport::Unicast,
            _ if group.is_none() => Transport::Unicast,
            _ => Transport::Multicast,
        };
        let mut n = core.net.lock_ok();
        n.asked = t;
        if n.transport != now {
            n.transport = now;
            n.mcast_audio_at = None;
            n.next_subscribe = Instant::now();
            n.session = Session::Connecting;
        }
        Ok(())
    }

    /// Ends the session (BYE) and stops; [`render`](Self::render) plays silence after.
    pub fn close(&self) {
        self.core.shut();
    }

    /// The last frame played: its desk time (ns), its AUDIO flags and when it was
    /// played (tests measure the latency with it).
    #[doc(hidden)]
    pub fn last_frame(&self) -> Option<(u64, u8, Instant)> {
        self.core.engine.lock_ok().last
    }
}

/// A channel's entry in `m` (a new one at 0 dB, centre, as the desk makes it).
fn entry(m: &mut Mix, id: Option<u16>) -> &mut ChannelMix {
    m.channels.entry(id.unwrap_or_default()).or_default()
}

fn fetch_description(endpoint: &http::Endpoint, token: &str) -> Result<Description, Error> {
    let (status, body) = endpoint.get("/api/v1/iem", token)?;
    if status != 200 {
        return Err(http::status_error(status, &body, token));
    }
    let v: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::Http(status, format!("the description: {e}")))?;
    Description::parse(&v)
}

impl Ctl {
    /// `ch`'s mix entry's channel (a pair's left side).
    fn lead(&self, ch: u16) -> Result<u16, Error> {
        self.channels
            .iter()
            .find(|c| c.id == ch)
            .map(Channel::lead)
            .ok_or(Error::UnknownChannel(ch))
    }

    /// The desk's mix with this phone's changes on their way laid over it.
    fn merged(&self) -> Option<Mix> {
        let mut m = self.desk_mix.clone()?;
        let Some(local) = &self.mix else {
            return Some(m);
        };
        if local.id != m.id {
            return Some(m);
        }
        for key in self.pending.keys() {
            if key == "master_db" {
                m.master_db = local.master_db;
                continue;
            }
            let Some((id, field)) = key
                .strip_prefix('c')
                .and_then(|k| k.split_once('.'))
                .and_then(|(i, f)| Some((i.parse::<u16>().ok()?, f)))
            else {
                continue;
            };
            let Some(le) = local.channels.get(&id) else {
                m.channels.remove(&id);
                continue;
            };
            let e = m.channels.entry(id).or_default();
            match field {
                "level_db" => e.level_db = le.level_db,
                "pan" => e.pan = le.pan,
                "mute" => e.mute = le.mute,
                "solo" => e.solo = le.solo,
                _ => {}
            }
        }
        Some(m)
    }

    /// The command carrying pending `key`'s value now.
    fn command(&mut self, key: &str) -> Option<(u64, Value)> {
        let m = self.mix.as_ref()?;
        let body = if key == "master_db" {
            json!({"id": m.id, "master_db": m.master_db})
        } else {
            let (id, field) = key.strip_prefix('c')?.split_once('.')?;
            let ch: u16 = id.parse().ok()?;
            let e = m.channels.get(&ch);
            let v = match (field, e) {
                ("in", None) => Value::Null,
                ("in", Some(e)) => {
                    json!({"level_db": e.level_db, "pan": e.pan, "mute": e.mute, "solo": e.solo})
                }
                (_, None) => return None,
                ("level_db", Some(e)) => json!({"level_db": e.level_db}),
                ("pan", Some(e)) => json!({"pan": e.pan}),
                ("mute", Some(e)) => json!({"mute": e.mute}),
                ("solo", Some(e)) => json!({"solo": e.solo}),
                _ => return None,
            };
            json!({"id": m.id, "channels": {id: v}})
        };
        let id = self.next_id;
        self.next_id += 1;
        let seq = self.next_seq;
        self.next_seq += 1;
        let msg = json!({"id": id, "cmd": "iem_mix", "body": body,
                         "control": format!("iem.{}.{key}", m.id), "seq": seq});
        Some((id, msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_from_json() {
        let c = Config::from_json(
            &json!({"desk": "10.0.0.2", "token": "t", "transport": "multicast",
            "ca_sha256": "AB:CD", "buffer_ms": 20, "udp_port": 29224, "talk": true}),
        )
        .unwrap();
        assert_eq!(c.transport, Transport::Multicast);
        assert_eq!(c.trust, Some(Trust::Sha256("AB:CD".into())));
        assert_eq!((c.buffer_ms, c.udp_port, c.talk), (20.0, Some(29224), true));
        assert!(Config::from_json(&json!({"desk": "x", "transport": "radio"})).is_err());
        assert!(Config::from_json(&json!({})).is_err());
    }

    #[test]
    fn the_merge_keeps_changes_on_their_way() {
        let mut desk = Mix {
            id: "d".into(),
            master_db: -3.0,
            ..Default::default()
        };
        desk.channels.insert(1, ChannelMix::default());
        desk.channels.insert(2, ChannelMix::default());
        let mut local = desk.clone();
        local.master_db = -10.0;
        local.channels.get_mut(&1).unwrap().level_db = -6.0;
        local.channels.insert(
            5,
            ChannelMix {
                pan: 0.5,
                ..Default::default()
            },
        );
        let mut c = Ctl {
            mix_id: Some("d".into()),
            channels: Vec::new(),
            mix: Some(local),
            desk_mix: Some(desk.clone()),
            pending: BTreeMap::new(),
            next_id: 1,
            next_seq: 1,
            no_mix_said: false,
        };
        for k in ["master_db", "c1.level_db", "c5.pan"] {
            c.pending.insert(
                k.into(),
                Pending {
                    dirty: true,
                    inflight: None,
                },
            );
        }
        // The sound person moved channel 2 meanwhile, and channel 1's pan.
        let mut d = desk;
        d.channels.get_mut(&2).unwrap().level_db = -20.0;
        d.channels.get_mut(&1).unwrap().pan = -1.0;
        c.desk_mix = Some(d);
        let m = c.merged().unwrap();
        assert_eq!(m.master_db, -10.0);
        assert_eq!((m.channels[&1].level_db, m.channels[&1].pan), (-6.0, -1.0));
        assert_eq!(m.channels[&2].level_db, -20.0);
        assert_eq!(m.channels[&5].pan, 0.5);
        let (id, msg) = c.command("c1.level_db").unwrap();
        assert_eq!(id, 1);
        assert_eq!(
            msg["body"],
            json!({"id": "d", "channels": {"1": {"level_db": -6.0}}})
        );
        assert_eq!(
            (msg["control"].as_str(), msg["seq"].as_u64()),
            (Some("iem.d.c1.level_db"), Some(1))
        );
        assert_eq!(c.command("master_db").unwrap().1["seq"], 2);
    }
}
