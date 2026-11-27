//! The site link (roadmap R.18, spec/extensions/idn-remote.md): a remote site's sources
//! at the main site and the main site's at the remote one, over one QUIC connection
//! across the internet (or a VPN).
//!
//! Both ends run the same session. Each tells the other which of its sources may cross
//! (its catalogue, X.3); the other offers each as an ordinary IDN source under its own
//! name (`HOST (name)`), and asks for it once a receiver there connects. The end that
//! has the source receives it, encodes it on its GPU at the link's bit rate (NDI|HX
//! sources cross as they are), and sends it on a stream of its own; the far end decodes
//! it for SpeedHQ receivers and passes the H.264/HEVC on as it is to receivers that take
//! it (`codec/1`). Sound crosses as 24-bit samples.
//!
//! ```no_run
//! # fn run(s: idn_ffmpeg::sitelink::LinkSettings, ff: std::sync::Arc<idn_ffmpeg::Ffmpeg>) -> std::io::Result<()> {
//! use idn_ffmpeg::sitelink::{Gateway, RemoteLink};
//! let gateway = Gateway::serve(29205, s.clone(), ff.clone())?; // the main site
//! let remote = RemoteLink::connect("church.example.org", s, ff); // a remote site
//! # Ok(()) }
//! ```

use crate::Ffmpeg;
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig,
};
use idn_open_protocol::codec::{
    Codec, CodedPicture, DecoderFactory, EncoderConfig, EncoderFactory, VideoDecoder, VideoEncoder,
};
use idn_open_protocol::discovery::{Browser, BrowserConfig, groups_match};
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent};
use idn_open_protocol::link::{LinkConfig, SiteConn, SiteListener, SiteSend, site_connect};
use idn_open_protocol::secure::SecureConfig;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod wire;
use wire::*;

mod media;
mod offers;
mod rate;

/// The gateway's UDP port when none is given (X.1).
pub const DEFAULT_PORT: u16 = idn_common::ports::SITE_LINK;

/// Bit rate reserved for a stream before its picture size is known (1080p).
const FIRST_RESERVE: u64 = 8_000_000;
/// The lowest bit rate a video stream is given when the link is nearly full.
const MIN_VIDEO: u64 = 1_500_000;

/// How a link end runs.
#[derive(Clone)]
pub struct LinkSettings {
    /// The link's bandwidth each way, Mbit/s (X.4; default 40, for an upload of 50+).
    pub limit_mbps: f64,
    /// The groups whose sources cross (default `public`).
    pub groups: Vec<String>,
    /// This site's Discovery Server, where the far side's sources are registered and
    /// this side's are found; `None`: mDNS.
    pub discovery_server: Option<String>,
    /// This machine's site identity (`secure/2`): the link's and the sources'.
    pub secure: SecureConfig,
    /// This machine's link key.
    pub link: LinkConfig,
    /// The codec video crosses in (H.264 by default: every GPU decodes it).
    pub codec: Codec,
    /// 1080p's bit rate across the link, Mbit/s (default 8; scaled by picture area).
    pub mbps_1080p: f64,
    /// Gateway: the site authority's Discovery Server, to which remote machines' trust
    /// connections are relayed (joining and membership over the link, X.12.2); `None`:
    /// no relay.
    pub trust_upstream: Option<SocketAddr>,
    /// Remote link: the TCP port of the campus Discovery Server it runs (discovery for
    /// the campus, trust relayed to the main site, the site announced on the campus's
    /// LAN); `None`: none (default 5959).
    pub campus_server: Option<u16>,
    /// How sound crosses (X.12.8).
    pub sound: SoundCoding,
}

/// How a source's sound crosses the link (X.12.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SoundCoding {
    /// 24-bit samples while the link has room; Opus once it is tight: a limit under
    /// 20 Mbit/s, or the video lowered to fit (adaptation). Stereo 48 kHz sound only;
    /// other sound always as samples.
    #[default]
    Auto,
    /// Always as 24-bit samples (about 2.3 Mbit/s for stereo).
    Pcm,
    /// Always Opus where it can (stereo 48 kHz, about 128 kbit/s; the far side's hello
    /// says it decodes Opus).
    Opus,
}

impl SoundCoding {
    /// From `auto`, `pcm` or `opus`.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(SoundCoding::Auto),
            "pcm" => Some(SoundCoding::Pcm),
            "opus" => Some(SoundCoding::Opus),
            _ => None,
        }
    }
}

/// A link under this is tight: its sound goes as Opus (`SoundCoding::Auto`).
const TIGHT_MBPS: f64 = 20.0;

impl LinkSettings {
    /// The defaults for this machine's identity.
    pub fn new(secure: SecureConfig) -> Self {
        LinkSettings {
            limit_mbps: 40.0,
            groups: vec!["public".into()],
            discovery_server: None,
            trust_upstream: None,
            campus_server: Some(idn_open_protocol::discovery::ds::DEFAULT_PORT),
            secure,
            link: LinkConfig::default(),
            codec: Codec::H264,
            mbps_1080p: 8.0,
            sound: SoundCoding::Auto,
        }
    }
}

/// One link's state, for the panels and the command line (X.8).
#[derive(Debug, Clone, Default)]
pub struct LinkStatus {
    /// The far end's name (from its site certificate) and address.
    pub peer: String,
    /// Connected now.
    pub connected: bool,
    /// What happened last (connecting, refused, lost, …).
    pub note: String,
    /// Round trip, ms.
    pub rtt_ms: f64,
    /// Bandwidth in use each way, Mbit/s, and the limit.
    pub out_mbps: f64,
    pub in_mbps: f64,
    pub limit_mbps: f64,
    /// Sources sent across now.
    pub sending: Vec<String>,
    /// The far side's sources offered here, and those being received.
    pub offered: Vec<String>,
    pub receiving: Vec<String>,
    /// Lost bytes over the last 2 s, %.
    pub loss_pct: f64,
    /// The video's bit rate as a share of its target now (adaptation, X.12.4), %.
    pub video_pct: u32,
}

impl LinkStatus {
    /// As JSON, for the Node's report and the Site Manager (X.12.5).
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "peer": self.peer, "connected": self.connected, "note": self.note,
            "rtt_ms": self.rtt_ms, "out_mbps": self.out_mbps, "in_mbps": self.in_mbps, "limit_mbps": self.limit_mbps,
            "sending": self.sending, "offered": self.offered.len(), "receiving": self.receiving,
            "loss_pct": self.loss_pct, "video_pct": self.video_pct,
        })
    }
}

/// Names this machine offers on behalf of a far site: never sent back across.
fn offered_here() -> &'static Mutex<HashSet<String>> {
    static S: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn log(m: &str) {
    idn_open_protocol::log::log(&format!("site link: {m}"));
}

/// `HOST (name)` → (`HOST`, `name`).
fn split_name(full: &str) -> Option<(&str, &str)> {
    let (host, rest) = full.split_once(" (")?;
    Some((host, rest.strip_suffix(')')?))
}

/// The main site's end: accepts remote links on a forwarded UDP port.
pub struct Gateway {
    listener: SiteListener,
    sessions: Arc<Mutex<Vec<Arc<Session>>>>,
}

impl Gateway {
    /// Listens on UDP `port` (0: any) for remote links of this site.
    pub fn serve(port: u16, settings: LinkSettings, ff: Arc<Ffmpeg>) -> io::Result<Gateway> {
        let sessions: Arc<Mutex<Vec<Arc<Session>>>> = Default::default();
        let (link, secure) = (settings.link.clone(), settings.secure.clone());
        let settings = Arc::new(settings);
        let list = sessions.clone();
        let listener =
            SiteListener::bind(port, link, secure, settings.trust_upstream, move |conn| {
                let (settings, ff, list) = (settings.clone(), ff.clone(), list.clone());
                std::thread::spawn(move || {
                    log(&format!("{} connected from {}", conn.peer_name, conn.peer));
                    let s = Arc::new(Session::new(conn, settings, ff));
                    list.lock_ok().push(s.clone());
                    let why = s.run(false);
                    log(&format!("{}: {why}", s.conn.peer_name));
                    list.lock_ok().retain(|x| !Arc::ptr_eq(x, &s));
                });
            })?;
        Ok(Gateway { listener, sessions })
    }

    /// The UDP port listened on.
    pub fn port(&self) -> u16 {
        self.listener.port()
    }

    /// The links connected now.
    pub fn status(&self) -> Vec<LinkStatus> {
        self.sessions.lock_ok().iter().map(|s| s.status()).collect()
    }
}

impl Gateway {
    /// Closes every link, telling the remote sites (they reconnect when it is back).
    pub fn close(&self) {
        for s in self.sessions.lock_ok().drain(..) {
            s.stop.store(true, Ordering::Relaxed);
            s.conn.close("the gateway stopped");
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.close();
    }
}

/// A remote site's end: dials the main site's gateway and stays connected, reconnecting
/// by itself (back-off to a minute).
pub struct RemoteLink {
    _campus: Option<idn_open_protocol::discovery::DiscoveryServer>,
    stop: Arc<AtomicBool>,
    current: Arc<Mutex<Option<Arc<Session>>>>,
    status: Arc<Mutex<LinkStatus>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RemoteLink {
    /// Connects to `addr` (`host` or `host:port`; the default port is 29205), looked up
    /// again on each attempt (dynamic DNS).
    pub fn connect(addr: &str, mut settings: LinkSettings, ff: Arc<Ffmpeg>) -> RemoteLink {
        let stop = Arc::new(AtomicBool::new(false));
        let campus = settings
            .campus_server
            .and_then(|port| campus_server(addr, port, &settings.link, &stop));
        // The campus's sources are found, and the main site's offered, on its server.
        if let Some(c) = &campus
            && settings.discovery_server.is_none()
        {
            settings.discovery_server = Some(format!("127.0.0.1:{}", c.port()));
        }
        let current: Arc<Mutex<Option<Arc<Session>>>> = Default::default();
        let status = Arc::new(Mutex::new(LinkStatus {
            peer: addr.to_owned(),
            note: "connecting".into(),
            limit_mbps: settings.limit_mbps,
            ..Default::default()
        }));
        let thread = {
            let (stop, current, status, addr) = (
                stop.clone(),
                current.clone(),
                status.clone(),
                addr.to_owned(),
            );
            let settings = Arc::new(settings);
            std::thread::Builder::new()
                .name("idn-site-link".into())
                .spawn(move || {
                    let mut fails = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        let r = resolve(&addr).and_then(|a| {
                            site_connect(
                                a,
                                &settings.link,
                                &settings.secure,
                                Duration::from_secs(10),
                            )
                        });
                        let wait = match r {
                            Ok(conn) => {
                                log(&format!("connected to {} at {}", conn.peer_name, conn.peer));
                                let s = Arc::new(Session::new(conn, settings.clone(), ff.clone()));
                                *current.lock_ok() = Some(s.clone());
                                // Closed while it connected (close() found none to end):
                                // ended here, not run on with nobody to stop it (bug run B).
                                if stop.load(Ordering::Relaxed) {
                                    *current.lock_ok() = None;
                                    break;
                                }
                                let started = Instant::now();
                                let why = s.run(true);
                                *current.lock_ok() = None;
                                log(&format!("{why}; reconnecting"));
                                status.lock_ok().note = format!("lost: {why}");
                                // A link that held a while starts the back-off again.
                                if started.elapsed() > Duration::from_secs(60) {
                                    fails = 0;
                                }
                                fails += 1;
                                Duration::from_secs(1)
                            }
                            Err(e) => {
                                fails += 1;
                                status.lock_ok().note = e.to_string();
                                if fails <= 3 || fails.is_multiple_of(10) {
                                    log(&format!("{addr}: {e}"));
                                }
                                Duration::from_secs((1u64 << fails.min(6)).min(60))
                            }
                        };
                        let until = Instant::now() + wait;
                        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                })
                .expect("thread")
        };
        RemoteLink {
            _campus: campus,
            stop,
            current,
            status,
            thread: Some(thread),
        }
    }

    /// The link's state.
    pub fn status(&self) -> LinkStatus {
        match &*self.current.lock_ok() {
            Some(s) => s.status(),
            None => self.status.lock_ok().clone(),
        }
    }
}

impl RemoteLink {
    /// Closes the link for good, telling the gateway.
    pub fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.current.lock_ok().take() {
            s.stop.store(true, Ordering::Relaxed);
            s.conn.close("the remote link stopped");
        }
    }
}

impl Drop for RemoteLink {
    fn drop(&mut self) {
        self.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The campus Discovery Server (X.12.2): discovery answered here, trust connections
/// relayed to the main site through the gateway at `addr`, and the site announced on the
/// campus's LAN once its CA is known (so its machines find it and join by themselves).
fn campus_server(
    addr: &str,
    port: u16,
    link: &LinkConfig,
    stop: &Arc<AtomicBool>,
) -> Option<idn_open_protocol::discovery::DiscoveryServer> {
    let (a, l) = (addr.to_owned(), link.clone());
    let relay: Arc<dyn Fn(std::net::TcpStream, Vec<u8>) + Send + Sync> =
        Arc::new(move |tcp, hello| {
            match resolve(&a).and_then(|gw| {
                idn_open_protocol::link::relay_trust_to(gw, &l, tcp, hello, Duration::from_secs(10))
            }) {
                Ok(()) => {}
                Err(e) => log(&format!("trust relay to {a}: {e}")),
            }
        });
    let ds = match idn_open_protocol::discovery::DiscoveryServer::start_with_relay(port, relay) {
        Ok(d) => d,
        Err(e) => {
            log(&format!(
                "campus Discovery Server on TCP {port}: {e} (another one runs there? then it is the campus's)"
            ));
            return None;
        }
    };
    log(&format!(
        "campus Discovery Server on TCP {}; joining and membership go to the main site through the link",
        ds.port()
    ));
    let (a, l, stop, p) = (addr.to_owned(), link.clone(), stop.clone(), ds.port());
    std::thread::spawn(move || {
        let mut tries = 0u32;
        while !stop.load(Ordering::Relaxed) {
            match resolve(&a).and_then(|gw| {
                idn_open_protocol::link::site_ca_via(gw, &l, Duration::from_secs(10))
            }) {
                Ok(ca) => match idn_open_protocol::discovery::site::SiteAnnouncer::start(p, &ca) {
                    Ok(ann) => {
                        log("the site announced on the campus's network");
                        // Kept until the link stops.
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        drop(ann);
                        return;
                    }
                    Err(e) => log(&format!("announcing the site: {e}")),
                },
                Err(e) => {
                    tries += 1;
                    if tries <= 3 || tries.is_multiple_of(20) {
                        log(&format!("the site's CA through {a}: {e}"));
                    }
                }
            }
            for _ in 0..100 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    Some(ds)
}

fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let with_port = if addr
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        addr.to_owned()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| io::Error::other(format!("{addr}: no IPv4 address")))
}

/// A source sent across.
struct Sub {
    stop: Arc<AtomicBool>,
    /// Bits a second reserved for it.
    reserved: Arc<AtomicU64>,
    /// The far end asked for a keyframe.
    keyframe: Arc<AtomicBool>,
    /// What the far side's receivers send the source: tally, metadata (PTZ…), X.12.3.
    back: Arc<Mutex<Vec<Back>>>,
}

/// From a far receiver to the real source.
enum Back {
    Tally(bool, bool),
    Meta(String),
}

/// A far source offered here.
struct Offer {
    output: Arc<Output>,
    /// Its receivers' tally as last told to the far end.
    tally: (bool, bool),
    subscribed: bool,
    unused_since: Option<Instant>,
    /// Refused by the far end: not asked again before then.
    retry_at: Option<Instant>,
    receiving: Arc<AtomicBool>,
}

/// One connected link, the same at both ends.
struct Session {
    conn: SiteConn,
    settings: Arc<LinkSettings>,
    ff: Arc<Ffmpeg>,
    ctl: Mutex<Option<SiteSend>>,
    stop: AtomicBool,
    subs: Mutex<HashMap<String, Sub>>,
    offers: Mutex<HashMap<String, Offer>>,
    publishers: Mutex<HashMap<String, Publisher>>,
    /// Our catalogue as last sent: name → groups.
    catalogue: Mutex<HashMap<String, Vec<String>>>,
    received: AtomicU64,
    rates: Mutex<(Instant, u64, u64, f64, f64)>,
    /// Adaptation (X.12.4): the video bit rate as a share of each stream's target, in %,
    /// and whether a picture's send blocked lately (the link's back-pressure).
    scale: std::sync::atomic::AtomicU32,
    blocked: AtomicBool,
    /// Bytes sent and lost at the last measurement, the loss then (%), clean since.
    adapt: Mutex<(u64, u64, f64, Option<Instant>)>,
    /// The far side decodes Opus sound (its hello said so; X.12.8).
    far_opus: AtomicBool,
}

impl Session {
    fn new(conn: SiteConn, settings: Arc<LinkSettings>, ff: Arc<Ffmpeg>) -> Self {
        Session {
            conn,
            settings,
            ff,
            ctl: Mutex::new(None),
            stop: AtomicBool::new(false),
            subs: Default::default(),
            offers: Default::default(),
            publishers: Default::default(),
            catalogue: Default::default(),
            received: AtomicU64::new(0),
            rates: Mutex::new((Instant::now(), 0, 0, 0.0, 0.0)),
            scale: std::sync::atomic::AtomicU32::new(100),
            blocked: AtomicBool::new(false),
            adapt: Mutex::new((0, 0, 0.0, None)),
            far_opus: AtomicBool::new(false),
        }
    }

    fn send_ctl(&self, v: Value) {
        if let Some(s) = self.ctl.lock_ok().as_mut()
            && s.send(v.to_string().as_bytes()).is_err()
        {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Runs until the link closes; says why.
    fn run(self: &Arc<Self>, dialer: bool) -> String {
        let opened = if dialer {
            self.conn.open_bi()
        } else {
            self.conn.accept_bi()
        };
        let (send, mut recv) = match opened {
            Ok(x) => x,
            Err(e) => return format!("no control stream: {e}"),
        };
        *self.ctl.lock_ok() = Some(send);
        self.send_ctl(json!({"t": "hello", "version": 1, "limit_mbps": self.settings.limit_mbps, "opus": true}));
        // The control stream.
        {
            let me = self.clone();
            std::thread::spawn(move || {
                while let Ok(Some(m)) = recv.recv() {
                    if let Ok(v) = serde_json::from_slice::<Value>(&m) {
                        me.control(&v);
                    }
                }
                me.stop.store(true, Ordering::Relaxed);
            });
        }
        // Media from the far side.
        {
            let me = self.clone();
            std::thread::spawn(move || {
                while let Ok(r) = me.conn.accept_uni() {
                    let me = me.clone();
                    std::thread::spawn(move || me.receive(r));
                }
                me.stop.store(true, Ordering::Relaxed);
            });
        }
        let browser = idn_open_protocol::discovery::shared(BrowserConfig {
            interface: None,
            discovery_server: self.settings.discovery_server.clone(),
        });
        let mut tick = 0u64;
        let why = loop {
            if let Some(why) = self.conn.closed() {
                break why;
            }
            if self.stop.load(Ordering::Relaxed) {
                break "the far end went away".to_owned();
            }
            if tick.is_multiple_of(10) && self.conn.revoked() {
                self.conn.close("revoked");
                break format!("{} is revoked", self.conn.peer_name);
            }
            if tick.is_multiple_of(4)
                && let Ok(b) = &browser
            {
                self.send_catalogue(b);
            }
            self.use_offers();
            self.measure();
            tick += 1;
            std::thread::sleep(Duration::from_millis(500));
        };
        self.stop.store(true, Ordering::Relaxed);
        for (_, s) in self.subs.lock_ok().drain() {
            s.stop.store(true, Ordering::Relaxed);
        }
        // `offers` before `offered_here`, as `offer` takes them (the
        // other order deadlocked with a catalogue arriving as the link dropped). Taken out
        // first, so neither is held while the outputs are dropped.
        let gone: Vec<_> = self.offers.lock_ok().drain().collect();
        {
            let mut offered = offered_here().lock_ok();
            for (name, _) in &gone {
                offered.remove(name);
            }
        }
        drop(gone);
        // The connection closed on every way out (ended by `stop`, it stayed open with its
        // stream threads while the next link was dialled; bug run 4).
        self.conn.close(&why);
        why
    }

    fn control(self: &Arc<Self>, v: &Value) {
        let source = v["source"].as_str().unwrap_or_default().to_owned();
        match v["t"].as_str().unwrap_or_default() {
            // First on the stream, before any subscription.
            "hello" => self
                .far_opus
                .store(v["opus"].as_bool().unwrap_or(false), Ordering::Relaxed),
            "catalogue" => self.offer(v["sources"].as_array().cloned().unwrap_or_default()),
            "subscribe" => self.subscribe(&source),
            "unsubscribe" => {
                if let Some(s) = self.subs.lock_ok().remove(&source) {
                    s.stop.store(true, Ordering::Relaxed);
                }
            }
            "keyframe" => {
                if let Some(s) = self.subs.lock_ok().get(&source) {
                    s.keyframe.store(true, Ordering::Relaxed);
                }
            }
            // The far receivers' tally and metadata, for the real source (X.12.3).
            "tally" | "meta" => {
                if let Some(s) = self.subs.lock_ok().get(&source) {
                    let b = if v["t"] == "tally" {
                        Back::Tally(
                            v["program"].as_bool().unwrap_or(false),
                            v["preview"].as_bool().unwrap_or(false),
                        )
                    } else {
                        Back::Meta(v["xml"].as_str().unwrap_or_default().to_owned())
                    };
                    s.back.lock_ok().push(b);
                }
            }
            "refused" => {
                let why = v["why"].as_str().unwrap_or_default();
                log(&format!(
                    "{} won't send {source}: {why}",
                    self.conn.peer_name
                ));
                if let Some(o) = self.offers.lock_ok().get_mut(&source) {
                    o.subscribed = false;
                    o.retry_at = Some(Instant::now() + Duration::from_secs(30));
                }
            }
            _ => {}
        }
    }
}

struct VideoMsg<'a> {
    codec: Codec,
    keyframe: bool,
    fps: (u32, u32),
    pts: i64,
    extra: &'a [u8],
    data: &'a [u8],
}

#[cfg(test)]
mod tests;
