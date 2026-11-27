//! Viewing in a web browser (roadmap R.12): any source on the network, watched on a
//! phone, tablet or computer with no app, over WebRTC: a WHEP request (an SDP offer
//! POSTed to `/whep/SOURCE`, the answer back), as WHEP players and OBS's WHEP source make
//! too; `GET /api/v1/state` lists the sources. No page of its own: the watching and the
//! listening pages are the host program's.
//!
//! Each watched source is received once and shared by everyone watching it: its
//! pictures encoded on the GPU as H.264 (no B-frames, a keyframe every 2 s and when a
//! viewer asks), its sound as Opus (48 kHz stereo); NDI|HX H.264 sources go out as they
//! came. A source no one watches for 10 s is let go. The server side is ICE-lite with
//! a host candidate on the interface the viewer reached it by, so it is for the local
//! network (and VPNs), not the internet.
//!
//! **Listening on phones** (G3, idn-node.md §N.19): with [`ViewerConfig::listen`] the page
//! is for an audience instead: the channels named there (each an IDN source: the main mix,
//! an interpreter's, a hearing-assistance mix), sound only — the sources are asked for
//! no pictures, each channel received and encoded once (Opus, 64 kbit/s by default) for
//! everyone listening; its state's `url` is this server's own address as the asker reached
//! it (this machine's network address when asked on the machine itself), for the page to
//! build its link and QR code from — not a page served here.
//!
//! **Pages on another origin**: with no password, `/whep/*`, `/api/v1/state` and
//! `/api/v1/report` answer CORS (the preflight `OPTIONS`, the asking page's `Origin`
//! allowed, `Location` exposed), so a host's page on its own server can watch and listen.
//! With a password: same-origin only (the login is a cookie of this server's).
//!
//! ```no_run
//! # fn run(ff: std::sync::Arc<idn_ffmpeg::Ffmpeg>) -> std::io::Result<()> {
//! let viewer = idn_ffmpeg::whep::Viewer::start(29218, idn_ffmpeg::whep::ViewerConfig::default(), ff)?;
//! // A WHEP player (or a host's page) POSTs its offer to http://THIS-MACHINE:29218/whep/SOURCE
//! # Ok(()) }
//! ```

use crate::Ffmpeg;
use crate::opus::OpusEncoder;
use idn_common::sync::LockOk as _;
use idn_open_protocol::codec::{Codec, EncoderConfig, VideoEncoder};
use idn_open_protocol::discovery::{Browser, BrowserConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::web::{Handler, PasswordGate, Request, Response, serve};
use serde_json::json;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, Event, IceConnectionState, Input as RtcInput, Output as RtcOutput, Rtc, RtcConfig,
};

/// The default port of the viewing server (WHEP).
pub const DEFAULT_PORT: u16 = idn_common::ports::VIEW;

/// The default port of the listening server (WHEP).
pub const LISTEN_PORT: u16 = idn_common::ports::LISTEN;

/// Viewers at once.
const MAX_SESSIONS: usize = 64;
/// Listeners at once (sound only: a thread and 64 kbit/s each).
const MAX_LISTENERS: usize = 500;
/// Sources watched at once.
const MAX_FEEDS: usize = 16;

/// How the viewing server runs.
#[derive(Clone, Default)]
pub struct ViewerConfig {
    /// A password for the server (`None`: anyone on the network may watch).
    pub password: Option<String>,
    /// 1080p's bit rate, Mbit/s (default 4; scaled by picture area).
    pub mbps_1080p: Option<f64>,
    /// The Discovery Server to list sources from; `None`: mDNS.
    pub discovery_server: Option<String>,
    /// This machine's identity, to receive site-secured sources.
    pub secure: Option<idn_open_protocol::secure::SecureConfig>,
    /// Listening on phones: the channels offered (none: the viewer for every source).
    pub listen: Vec<ListenChannel>,
    /// The title given in the state, for a listening page's heading (default "Listen").
    pub title: Option<String>,
    /// The listening channels' Opus rate, kbit/s (default 64).
    pub listen_kbps: Option<u32>,
}

/// A listening channel: what listeners see, and the IDN source it plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenChannel {
    pub name: String,
    pub source: String,
}

/// A source and the first of the two channels taken from it: `SOURCE|ch=N` (N from 1)
/// takes channels N and N+1 (a desk's matrix or aux pair), `SOURCE` alone the first two.
pub fn source_and_channel(s: &str) -> (&str, usize) {
    match s.rsplit_once("|ch=") {
        Some((src, n)) => match n.trim().parse::<usize>() {
            Ok(n) if n >= 1 => (src.trim(), n - 1),
            _ => (s, 0),
        },
        None => (s, 0),
    }
}

impl ListenChannel {
    /// `Name=SOURCE` or `Name=SOURCE|ch=N` (as `idn listen` and the Node's
    /// `listen.channels` take it; see [`source_and_channel`]).
    pub fn parse(s: &str) -> Option<ListenChannel> {
        let (n, src) = s.split_once('=')?;
        let (name, source) = (n.trim(), src.trim());
        (!name.is_empty() && !source.is_empty()).then(|| ListenChannel {
            name: name.to_owned(),
            source: source.to_owned(),
        })
    }
}

/// One encoded piece of a source, shared by its viewers.
#[derive(Clone)]
enum Media {
    Video {
        data: Arc<[u8]>,
        t: MediaTime,
        wall: Instant,
    },
    Audio {
        data: Arc<[u8]>,
        t: MediaTime,
        wall: Instant,
    },
}

/// A source being watched: received once, encoded once, fanned out.
struct Feed {
    viewers: Mutex<Vec<SyncSender<Media>>>,
    keyframe: AtomicBool,
    stop: AtomicBool,
    last_viewer: Mutex<Instant>,
}

/// The browser-viewing server.
pub struct Viewer {
    port: u16,
    stop: Arc<AtomicBool>,
    state: Arc<State>,
}

struct State {
    cfg: ViewerConfig,
    ff: Arc<Ffmpeg>,
    feeds: Mutex<HashMap<String, Arc<Feed>>>,
    sessions: Mutex<HashMap<String, Arc<AtomicBool>>>,
    browser: Option<Arc<Browser>>,
    gate: Option<PasswordGate>,
    /// Listeners dropped (their connection lost, not left; idn-node.md §N.41): numbered,
    /// the newest 200, and the count per channel.
    drops: Mutex<Drops>,
}

/// Listeners dropped: the last number given, the newest ones, and counts per channel.
#[derive(Default)]
struct Drops {
    seq: u64,
    recent: std::collections::VecDeque<(u64, ListenerDrop)>,
    per_channel: HashMap<String, u64>,
}

/// A listener dropped (its phone's connection lost, not a listener leaving): for the
/// typed event `listener_dropped` (idn-node.md §N.41).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerDrop {
    /// The channel's name (as listeners see it).
    pub channel: String,
    /// The listener's address.
    pub peer: IpAddr,
    /// Why: the connection's error, or `disconnected`.
    pub why: String,
}

fn log(m: &str) {
    idn_open_protocol::log::log(&format!("view: {m}"));
}

impl Viewer {
    /// Serves WHEP and the state on TCP `port` (0: any), all interfaces.
    pub fn start(port: u16, cfg: ViewerConfig, ff: Arc<Ffmpeg>) -> io::Result<Viewer> {
        install_crypto();
        let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
        let port = listener.local_addr()?.port();
        let gate = cfg
            .password
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| PasswordGate::new(p, Duration::from_secs(12 * 3600), "idn_view", "X-IDN-View"))
            .transpose()?;
        let browser = idn_open_protocol::discovery::shared(BrowserConfig {
            interface: None,
            discovery_server: cfg.discovery_server.clone(),
        })
        .ok();
        let state = Arc::new(State {
            cfg,
            ff,
            feeds: Default::default(),
            sessions: Default::default(),
            browser,
            gate,
            drops: Mutex::default(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let st = state.clone();
        let handler: Handler = Arc::new(move |r| handle(&st, r));
        serve(listener, None, handler, stop.clone())?;
        // Sources no one watches any more are let go.
        {
            let (st, stop) = (state.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(2));
                    st.feeds.lock_ok().retain(|name, f| {
                        f.viewers.lock_ok().retain(|v| {
                            !matches!(
                                v.try_send(Media::Audio {
                                    data: Arc::from(&[][..]),
                                    t: MediaTime::ZERO,
                                    wall: Instant::now()
                                }),
                                Err(TrySendError::Disconnected(_))
                            )
                        });
                        let idle = f.viewers.lock_ok().is_empty()
                            && f.last_viewer.lock_ok().elapsed() > Duration::from_secs(10);
                        if idle {
                            f.stop.store(true, Ordering::Relaxed);
                            log(&format!("{name}: no one watching; let go"));
                        }
                        !idle
                    });
                }
            });
        }
        Ok(Viewer { port, stop, state })
    }

    /// The TCP port the page is on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Listeners dropped after place `seq` (oldest first), and the place now (§N.41).
    pub fn drops_since(&self, seq: u64) -> (Vec<ListenerDrop>, u64) {
        let d = self.state.drops.lock_ok();
        (
            d.recent
                .iter()
                .filter(|(s, _)| *s > seq)
                .map(|(_, x)| x.clone())
                .collect(),
            d.seq,
        )
    }

    /// The sources being watched and their viewers.
    pub fn watching(&self) -> Vec<(String, usize)> {
        self.state
            .feeds
            .lock_ok()
            .iter()
            .map(|(n, f)| (n.clone(), f.viewers.lock_ok().len()))
            .collect()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for f in self.state.feeds.lock_ok().values() {
            f.stop.store(true, Ordering::Relaxed);
        }
        for s in self.state.sessions.lock_ok().values() {
            s.store(true, Ordering::Relaxed);
        }
    }
}

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
}

/// A path segment as the page encoded it (`encodeURIComponent`).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 3 <= b.len())
            .then(|| {
                std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            })
            .flatten();
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The paths a page on another origin (a host's watching and listening pages) uses: the
/// state, a WHEP offer, ending its session and its playback report.
fn cross_origin_path(path: &str) -> bool {
    path == "/api/v1/state" || path == "/api/v1/report" || path.starts_with("/whep/")
}

/// CORS for a page on another origin (bug run O): with no password the server is open to
/// anyone on the network anyway, so the asking page's origin is allowed (its `Origin`
/// echoed; the preflight `OPTIONS` answered). With a password the login's cookie is the
/// gate: same-origin only (no CORS), the page served from this server's address.
fn with_cors(resp: Response, origin: &str) -> Response {
    resp.with("Access-Control-Allow-Origin", origin)
        .with("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS")
        .with("Access-Control-Allow-Headers", "Content-Type")
        .with("Access-Control-Expose-Headers", "Location")
        .with("Access-Control-Max-Age", "600")
        .with("Vary", "Origin")
}

fn handle(st: &Arc<State>, r: Request) -> Response {
    let origin = (st.gate.is_none() && cross_origin_path(&r.path))
        .then(|| r.header("origin").map(str::to_owned))
        .flatten();
    if r.method == "OPTIONS"
        && origin.is_some()
        && idn_open_protocol::web::lan_host_ok(r.header("host"))
    {
        return with_cors(
            Response::new("204 No Content", "text/plain", ""),
            origin.as_deref().unwrap_or(""),
        );
    }
    let resp = handle_request(st, r);
    match origin {
        Some(o) => with_cors(resp, &o),
        None => resp,
    }
}

fn handle_request(st: &Arc<State>, r: Request) -> Response {
    let err = |status: &'static str, e: &str| {
        Response::new(
            status,
            "application/json",
            json!({"ok": false, "error": e}).to_string(),
        )
    };
    // Open without a password: only to names of this machine (DNS rebinding).
    if st.gate.is_none() && !idn_open_protocol::web::lan_host_ok(r.header("host")) {
        return Response::text(
            "421 Misdirected Request",
            "this page answers to this machine's address or name; add the name to IDN_VIEW_HOSTS, or set a password",
        );
    }
    if let Some(g) = &st.gate {
        if let Some(resp) = g.handle(&r, &log) {
            return resp;
        }
        let page = r.method == "GET" && matches!(r.path.as_str(), "/" | "/api/v1/state");
        if !page && !g.allowed(&r) {
            return err("401 Unauthorized", "log in first");
        }
    }
    if !st.cfg.listen.is_empty()
        && let Some(resp) = handle_listen(st, &r)
    {
        return resp;
    }
    match (r.method.as_str(), r.path.as_str()) {
        // No page of its own (the watching page is the host program's): what this is.
        ("GET", "/") => Response::json(
            json!({"service": "IDN view (WHEP)", "state": "/api/v1/state", "whep": "/whep/SOURCE"})
                .to_string(),
        ),
        ("GET", "/api/v1/state") if st.gate.as_ref().is_some_and(|g| !g.allowed(&r)) => {
            // Locked: nothing about the sources until the login.
            Response::json(json!({ "sources": [], "watching": {}, "locked": true }).to_string())
                .with("Cache-Control", "no-store")
        }
        ("GET", "/api/v1/state") => {
            let mut names: Vec<String> = st
                .browser
                .as_ref()
                .map(|b| b.sources().into_iter().map(|s| s.name).collect())
                .unwrap_or_default();
            names.sort();
            let watching: HashMap<String, usize> = st
                .feeds
                .lock_ok()
                .iter()
                .map(|(n, f)| (n.clone(), f.viewers.lock_ok().len()))
                .collect();
            Response::json(
                json!({
                    "sources": names,
                    "watching": watching,
                    "locked": false,
                })
                .to_string(),
            )
            .with("Cache-Control", "no-store")
        }
        ("POST", p) if p.starts_with("/whep/") && !p.starts_with("/whep/session/") => {
            // (No control characters: a `%0A` in the name wrote lines of the caller's
            // choosing into the log; bug run 4.)
            let source: String = percent_decode(&p["/whep/".len()..])
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            let Ok(sdp) = std::str::from_utf8(&r.body) else {
                return err("400 Bad Request", "the offer isn't text");
            };
            match start_session(st, &source, sdp, r.peer) {
                Ok((id, answer)) => Response::new("201 Created", "application/sdp", answer)
                    .with("Location", &format!("/whep/session/{id}")),
                Err(e) => err("400 Bad Request", &e.to_string()),
            }
        }
        // A viewer's playback, for the log.
        ("POST", "/api/v1/report") => {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&r.body) {
                let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
                log(&format!(
                    "{}: {:.0} pictures/s ({}x{}), {} decoded, {} video packets lost, {} freezes; sound {} packets, {} lost",
                    r.peer,
                    n("fps"),
                    n("width"),
                    n("height"),
                    n("frames"),
                    n("video_lost"),
                    n("freezes"),
                    n("audio_packets"),
                    n("audio_lost")
                ));
            }
            Response::json("{}")
        }
        ("DELETE", p) if p.starts_with("/whep/session/") => {
            if let Some(s) = st.sessions.lock_ok().remove(&p["/whep/session/".len()..]) {
                s.store(true, Ordering::Relaxed);
            }
            Response::new("200 OK", "application/json", "{}")
        }
        _ => err("404 Not Found", "no such page"),
    }
}

/// The listening page's own requests (§N.19); `None`: the viewer's (reports, ending a
/// session).
fn handle_listen(st: &Arc<State>, r: &Request) -> Option<Response> {
    let title = st.cfg.title.clone().unwrap_or_else(|| "Listen".into());
    Some(match (r.method.as_str(), r.path.as_str()) {
        // No page of its own (the listening page is the host program's): what this is.
        ("GET", "/") => Response::json(
            json!({"service": "IDN listening (WHEP)", "title": title, "state": "/api/v1/state", "whep": "/whep/CHANNEL"})
                .to_string(),
        ),
        ("GET", "/api/v1/state") => {
            let feeds = st.feeds.lock_ok();
            let channels: Vec<serde_json::Value> = st
                .cfg
                .listen
                .iter()
                .map(|c| {
                    let n = feeds
                        .get(&c.source)
                        .filter(|f| !f.stop.load(Ordering::Relaxed))
                        .map_or(0, |f| f.viewers.lock_ok().len());
                    // Listeners dropped (their connection lost) since the start (§N.41).
                    let dropped = st.drops.lock_ok().per_channel.get(&c.name).copied().unwrap_or(0);
                    json!({"name": c.name, "listening": n, "dropped": dropped})
                })
                .collect();
            let locked = st.gate.as_ref().is_some_and(|g| !g.allowed(r));
            Response::json(
                json!({"title": title, "channels": if locked { Vec::new() } else { channels }, "locked": locked, "url": page_url(r)}).to_string(),
            )
            .with("Cache-Control", "no-store")
        }
        // Only the channels: an audience's page doesn't open other sources.
        ("POST", p) if p.starts_with("/whep/") && !p.starts_with("/whep/session/") => {
            let name = percent_decode(&p["/whep/".len()..]);
            let Some(c) = st.cfg.listen.iter().find(|c| c.name == name) else {
                return Some(Response::new(
                    "404 Not Found",
                    "application/json",
                    json!({"ok": false, "error": "no such channel"}).to_string(),
                ));
            };
            let Ok(sdp) = std::str::from_utf8(&r.body) else {
                return Some(Response::new("400 Bad Request", "application/json", "{}"));
            };
            match start_session(st, &c.source.clone(), sdp, r.peer) {
                Ok((id, answer)) => Response::new("201 Created", "application/sdp", answer)
                    .with("Location", &format!("/whep/session/{id}")),
                Err(e) => Response::new(
                    "503 Service Unavailable",
                    "application/json",
                    json!({"ok": false, "error": e.to_string()}).to_string(),
                ),
            }
        }
        _ => return None,
    })
}

/// The state's `url`: this server's address as the asker reached it — this machine's
/// network address when that was this machine itself (a QR code with 127.0.0.1 takes a
/// phone nowhere). Not a page here (the listening page is the host's): the base its link
/// and QR code are built from.
fn page_url(r: &Request) -> String {
    let (name, port) = idn_open_protocol::web::split_host(r.header("host").unwrap_or(""));
    let name = if name.is_empty() || idn_open_protocol::web::is_loopback_name(name) {
        idn_common::net::outgoing_ip()
            .map_or_else(idn_open_protocol::discovery::hostname, |a| a.to_string())
    } else {
        name.to_owned()
    };
    // An IPv6 address goes in brackets in an address.
    let name = if name.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{name}]")
    } else {
        name
    };
    match port {
        Some(p) if p != 80 => format!("http://{name}:{p}/"),
        _ => format!("http://{name}/"),
    }
}

/// A viewer for `source`: the SDP answer, its session running on a thread.
fn start_session(
    st: &Arc<State>,
    source: &str,
    offer: &str,
    peer: IpAddr,
) -> io::Result<(String, String)> {
    // Each viewer is a thread and a socket, each source watched a receiver and an
    // encoder: only so many, whoever asks (without a password, anyone on the network).
    let listening = !st.cfg.listen.is_empty();
    let most = if listening {
        MAX_LISTENERS
    } else {
        MAX_SESSIONS
    };
    if st.sessions.lock_ok().len() >= most {
        return Err(io::Error::other(if listening {
            "too many listeners"
        } else {
            "too many viewers"
        }));
    }
    {
        let feeds = st.feeds.lock_ok();
        let live = feeds
            .values()
            .filter(|f| !f.stop.load(Ordering::Relaxed))
            .count();
        if !feeds.contains_key(source) && live >= MAX_FEEDS {
            return Err(io::Error::other("too many sources watched at once"));
        }
    }
    let offer =
        SdpOffer::from_sdp_string(offer).map_err(|e| io::Error::other(format!("offer: {e}")))?;
    let mut rtc = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(!listening)
        .enable_opus(true, false)
        .build(Instant::now());
    let mut ip = idn_common::net::local_addr_towards(peer)
        .ok_or_else(|| io::Error::other("no address of this machine's toward the viewer"))?;
    if ip.is_loopback() {
        // Firefox won't use 127.0.0.1 for WebRTC: the machine's own address instead.
        ip = idn_common::net::outgoing_ip().unwrap_or(ip);
    }
    // One of IDN's media ports (a firewall rule names the range; the system's pick was
    // outside it, and phones got the page but no sound or picture; F1-3).
    let socket = idn_common::ports::media_udp(ip)?;
    let local = socket.local_addr()?;
    rtc.add_local_candidate(
        Candidate::host(local, "udp").map_err(|e| io::Error::other(e.to_string()))?,
    );
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| io::Error::other(format!("offer: {e}")))?;
    let feed = feed_for(st, source);
    let (tx, rx) = sync_channel::<Media>(256);
    feed.viewers.lock_ok().push(tx);
    *feed.last_viewer.lock_ok() = Instant::now();
    feed.keyframe.store(true, Ordering::Relaxed);
    // Unguessable: the id is all it takes to end a viewer's session.
    let id = idn_open_protocol::random::token_hex(16);
    let stop = Arc::new(AtomicBool::new(false));
    st.sessions.lock_ok().insert(id.clone(), stop.clone());
    let (st2, id2, name) = (st.clone(), id.clone(), source.to_owned());
    std::thread::Builder::new()
        .name("idn-whep".into())
        .spawn(move || {
            log(&format!("{peer} watching {name}"));
            let why = match run_session(rtc, socket, rx, &feed, &stop, listening) {
                Ok(()) => "left".to_owned(),
                Err(e) => e.to_string(),
            };
            log(&format!("{peer} stopped watching {name}: {why}"));
            st2.sessions.lock_ok().remove(&id2);
            // A listener whose connection was lost (not ended from its page, not the
            // source let go): dropped (§N.41).
            if listening && !stop.load(Ordering::Relaxed) && !feed.stop.load(Ordering::Relaxed) {
                let channel = st2
                    .cfg
                    .listen
                    .iter()
                    .find(|c| c.source == name)
                    .map_or_else(|| name.clone(), |c| c.name.clone());
                let mut d = st2.drops.lock_ok();
                d.seq += 1;
                let s = d.seq;
                *d.per_channel.entry(channel.clone()).or_default() += 1;
                d.recent.push_back((
                    s,
                    ListenerDrop {
                        channel,
                        peer,
                        why: if why == "left" {
                            "disconnected".into()
                        } else {
                            why
                        },
                    },
                ));
                while d.recent.len() > 200 {
                    d.recent.pop_front();
                }
            }
        })?;
    Ok((id, answer.to_sdp_string()))
}

fn run_session(
    mut rtc: Rtc,
    socket: UdpSocket,
    rx: Receiver<Media>,
    feed: &Feed,
    stop: &AtomicBool,
    sound_only: bool,
) -> io::Result<()> {
    let e = |x: str0m::RtcError| io::Error::other(x.to_string());
    // How long to wait on the socket between looks at the feed: a 20 ms sound packet waits
    // at most half of one; pictures, 5 ms. (Hundreds of listeners: fewer wake-ups.)
    let most = Duration::from_millis(if sound_only { 10 } else { 5 });
    let local = socket.local_addr()?;
    let mut buf = vec![0u8; 2000];
    type Track = Option<(Mid, str0m::format::PayloadParams)>;
    let (mut video, mut audio): (Track, Track) = (None, None);
    let mut mids: Vec<(Mid, MediaKind)> = Vec::new();
    let mut connected = false;
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) && !feed.stop.load(Ordering::Relaxed) {
        let timeout = match rtc.poll_output().map_err(e)? {
            RtcOutput::Timeout(t) => t,
            RtcOutput::Transmit(t) => {
                let _ = socket.send_to(&t.contents, t.destination);
                continue;
            }
            RtcOutput::Event(ev) => {
                match ev {
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Ok(());
                    }
                    Event::Connected => connected = true,
                    Event::MediaAdded(m) => mids.push((m.mid, m.kind)),
                    Event::KeyframeRequest(_) => feed.keyframe.store(true, Ordering::Relaxed),
                    _ => {}
                }
                continue;
            }
        };
        if !connected && started.elapsed() > Duration::from_secs(20) {
            return Err(io::Error::other(
                "the browser never connected (a firewall on UDP?)",
            ));
        }
        // The payload types once the media are known.
        for (mid, kind) in mids.drain(..) {
            let Some(w) = rtc.writer(mid) else { continue };
            let pick = |want: RtcCodec| {
                let mut ps: Vec<_> = w
                    .payload_params()
                    .filter(|p| p.spec().codec == want)
                    .cloned()
                    .collect();
                // H.264: packetization mode 1, the High profile first (the GPU's), then any.
                ps.sort_by_key(|p| {
                    let f = p.spec().format;
                    (
                        f.packetization_mode != Some(1),
                        f.profile_level_id.map(|x| x >> 16) != Some(0x64),
                    )
                });
                ps.into_iter().next()
            };
            match kind {
                MediaKind::Video => video = pick(RtcCodec::H264).map(|p| (mid, p)),
                MediaKind::Audio => audio = pick(RtcCodec::Opus).map(|p| (mid, p)),
            }
        }
        // Media waiting from the feed.
        while let Ok(m) = rx.try_recv() {
            if !connected {
                continue;
            }
            let (target, data, t, wall) = match &m {
                Media::Video { data, t, wall } => (&video, data, *t, *wall),
                Media::Audio { data, t, wall } => (&audio, data, *t, *wall),
            };
            if data.is_empty() {
                continue;
            }
            if let Some((mid, p)) = target
                && let Some(w) = rtc.writer(*mid)
            {
                let _ = w.write(p.pt(), wall, t, data.clone());
            }
        }
        let wait = timeout.saturating_duration_since(Instant::now()).min(most);
        if wait.is_zero() {
            rtc.handle_input(RtcInput::Timeout(Instant::now()))
                .map_err(e)?;
            continue;
        }
        socket.set_read_timeout(Some(wait))?;
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                rtc.handle_input(RtcInput::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents,
                    },
                ))
                .map_err(e)?;
            }
            Err(x)
                if matches!(
                    x.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                rtc.handle_input(RtcInput::Timeout(Instant::now()))
                    .map_err(e)?;
            }
            Err(x) => return Err(x),
        }
    }
    Ok(())
}

/// The feed for `source`, started if no one watches it yet.
fn feed_for(st: &Arc<State>, source: &str) -> Arc<Feed> {
    let mut feeds = st.feeds.lock_ok();
    if let Some(f) = feeds
        .get(source)
        .filter(|f| !f.stop.load(Ordering::Relaxed))
    {
        return f.clone();
    }
    let f = Arc::new(Feed {
        viewers: Default::default(),
        keyframe: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        last_viewer: Mutex::new(Instant::now()),
    });
    feeds.insert(source.to_owned(), f.clone());
    let (st, f2, name) = (st.clone(), f.clone(), source.to_owned());
    std::thread::Builder::new()
        .name("idn-whep-feed".into())
        .spawn(move || {
            if let Err(e) = run_feed(&st, &name, &f2) {
                log(&format!("{name}: {e}"));
            }
            f2.stop.store(true, Ordering::Relaxed);
        })
        .expect("thread");
    f
}

fn fan_out(f: &Feed, m: Media) {
    f.viewers
        .lock_ok()
        .retain(|v| !matches!(v.try_send(m.clone()), Err(TrySendError::Disconnected(_))));
}

/// Receives `name` and feeds its viewers until no one watches.
fn run_feed(st: &State, feed: &str, f: &Feed) -> io::Result<()> {
    // A listening channel may name its channels (`|ch=N`); each such choice is a feed.
    let (name, first) = source_and_channel(feed);
    let listening = !st.cfg.listen.is_empty();
    let kbps = if listening {
        st.cfg.listen_kbps.unwrap_or(64)
    } else {
        128
    };
    let input = Input::open(InputConfig {
        discovery_server: st.cfg.discovery_server.clone(),
        secure: st.cfg.secure.clone(),
        receiver_name: if listening {
            "listening on phones".into()
        } else {
            "browser viewing".into()
        },
        // Listening: the sources send no pictures (none received, none decoded).
        receive_video: !listening,
        queue: 4,
        // (Made Opus for the browser: lossless would only double the network's load.)
        lossless_audio: false,
        ..InputConfig::new(name)
    })?;
    log(&format!("{name}: receiving"));
    let t0 = Instant::now();
    let since = |w: Instant| w.duration_since(t0);
    let mut enc: Option<(Box<dyn VideoEncoder>, (usize, usize))> = None;
    let mut opus: Option<OpusEncoder> = None;
    // Sound at 44.1 or 96 kHz to Opus's 48 kHz (§N.40): a sinc filter kept across blocks.
    let mut to48 = To48k::default();
    let mut aac: Option<(Vec<u8>, Box<dyn idn_open_protocol::codec::AudioDecoder>)> = None;
    let mbps = st.cfg.mbps_1080p.unwrap_or(4.0);
    let mut sent_audio_samples: u64 = 0;
    while !f.stop.load(Ordering::Relaxed) {
        let Some(ev) = input.recv(Duration::from_millis(200)) else {
            continue;
        };
        match ev {
            InputEvent::Video(v) => {
                let p = &v.picture;
                let size = (p.width, p.height);
                if enc.as_ref().is_none_or(|e| e.1 != size) {
                    let fps = (v.header.frame_rate_n.max(1), v.header.frame_rate_d.max(1));
                    let cfg = EncoderConfig {
                        codec: Codec::H264,
                        width: size.0,
                        height: size.1,
                        frame_rate: fps,
                        bitrate: ((mbps * 1e6) * (size.0 * size.1) as f64 / (1920.0 * 1080.0))
                            .max(800_000.0) as u64,
                        keyframe_interval: (fps.0.saturating_mul(2) / fps.1.max(1)).max(1),
                    };
                    // The GPU's H.264, else the CPU's (OpenH264, libx264).
                    enc = Some((st.ff.h264(&cfg)?.0, size));
                }
                let wall = Instant::now();
                let pts = since(wall).as_nanos() as i64 / 100;
                let force = f.keyframe.swap(false, Ordering::Relaxed);
                for e in enc
                    .as_mut()
                    .unwrap()
                    .0
                    .encode(&p.planes, p.fourcc, pts, force)?
                {
                    let mut data = e.data;
                    if e.keyframe && !e.extra.is_empty() && !has_sps(&data) {
                        let mut d = e.extra.clone();
                        d.extend_from_slice(&data);
                        data = d;
                    }
                    fan_out(
                        f,
                        Media::Video {
                            data: data.into(),
                            t: MediaTime::from_90khz((since(wall).as_micros() * 9 / 100) as u64),
                            wall,
                        },
                    );
                }
                // Its memory back to the decoder for the next picture.
                v.picture.recycle();
            }
            // NDI|HX H.264: as it came (keyframe requests can't reach it; its own interval).
            InputEvent::Compressed(c) if c.codec == Codec::H264 => {
                let wall = Instant::now();
                fan_out(
                    f,
                    Media::Video {
                        data: c.data.into(),
                        t: MediaTime::from_90khz((since(wall).as_micros() * 9 / 100) as u64),
                        wall,
                    },
                );
            }
            InputEvent::Audio(a) => {
                let o = match opus.as_mut() {
                    Some(o) => o,
                    None => opus.insert(OpusEncoder::open_kbps(kbps)?),
                };
                let ch = to48.run(
                    a.channels.get(first..).unwrap_or_default(),
                    a.header.sample_rate,
                );
                for pkt in o.push(&ch)? {
                    let wall = Instant::now();
                    fan_out(
                        f,
                        Media::Audio {
                            data: pkt.into(),
                            t: MediaTime::new(
                                sent_audio_samples,
                                str0m::media::Frequency::FORTY_EIGHT_KHZ,
                            ),
                            wall,
                        },
                    );
                    sent_audio_samples += 960;
                }
            }
            InputEvent::CompressedAudio(c) => {
                if aac.as_ref().is_none_or(|a| a.0 != c.config) {
                    aac = Some((
                        c.config.clone(),
                        DecoderFactoryExt::open_aac(&*st.ff, &c.config)?,
                    ));
                }
                let channels = aac.as_mut().unwrap().1.decode(&c.data)?;
                if channels.is_empty() {
                    continue;
                }
                let o = match opus.as_mut() {
                    Some(o) => o,
                    None => opus.insert(OpusEncoder::open_kbps(kbps)?),
                };
                let ch = to48.run(
                    channels.get(first..).unwrap_or_default(),
                    c.header.sample_rate,
                );
                for pkt in o.push(&ch)? {
                    let wall = Instant::now();
                    fan_out(
                        f,
                        Media::Audio {
                            data: pkt.into(),
                            t: MediaTime::new(
                                sent_audio_samples,
                                str0m::media::Frequency::FORTY_EIGHT_KHZ,
                            ),
                            wall,
                        },
                    );
                    sent_audio_samples += 960;
                }
            }
            _ => {}
        }
        if !f.viewers.lock_ok().is_empty() {
            *f.last_viewer.lock_ok() = Instant::now();
        }
    }
    Ok(())
}

use idn_open_protocol::codec::DecoderFactory as DecoderFactoryExt;

/// Whether an access unit already starts with its parameter sets.
fn has_sps(au: &[u8]) -> bool {
    au.windows(4)
        .take(64)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
}

/// A stream's sound to stereo at 48 kHz (Opus's rate), through a sinc filter kept
/// across blocks: IDN runs at 44.1 and 96 kHz too (§N.40); this was a nearest-sample
/// conversion while other rates were rare.
#[derive(Default)]
pub(crate) struct To48k {
    conv: Option<(u32, idn_open_protocol::audio::resample::Rate)>,
}

impl To48k {
    pub(crate) fn run(&mut self, ch: &[Vec<f32>], rate: u32) -> Vec<Vec<f32>> {
        let two: Vec<Vec<f32>> = match ch {
            [] => return Vec::new(),
            [m] => vec![m.clone(), m.clone()],
            [l, r, ..] => vec![l.clone(), r.clone()],
        };
        if rate == 48_000
            || !idn_open_protocol::media::AUDIO_RATES.contains(&rate)
            || two.iter().any(|c| c.is_empty())
        {
            self.conv = None;
            return two;
        }
        if self.conv.as_ref().is_none_or(|(r, _)| *r != rate) {
            self.conv = idn_open_protocol::audio::resample::Rate::new(rate, 48_000, 2, 480, 0.0)
                .ok()
                .map(|c| (rate, c));
        }
        let Some((_, c)) = self.conv.as_mut() else {
            return resample_to_48k(ch, rate);
        };
        c.push(&two);
        let mut out = vec![Vec::new(), Vec::new()];
        let _ = c.pull(|o, n| {
            for (d, s) in out.iter_mut().zip(o) {
                d.extend_from_slice(&s[..n]);
            }
        });
        out
    }
}

/// Sound at other rates: nearest-sample resampling to 48 kHz, stereo (one block alone, no
/// state; the voice feed's, and [`To48k`]'s fallback).
pub(crate) fn resample_to_48k(ch: &[Vec<f32>], rate: u32) -> Vec<Vec<f32>> {
    let two: Vec<&Vec<f32>> = match ch {
        [] => return Vec::new(),
        [m] => vec![m, m],
        [l, r, ..] => vec![l, r],
    };
    // An empty channel, or a rate no sound has (the input refuses those already): as is.
    if rate == 48_000
        || !idn_open_protocol::media::AUDIO_RATES.contains(&rate)
        || two.iter().any(|c| c.is_empty())
    {
        return two.into_iter().cloned().collect();
    }
    two.iter()
        .map(|c| idn_open_protocol::audio::resample::simple(c, rate, 48_000))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(host: &str) -> Request {
        Request {
            method: "GET".into(),
            path: "/api/v1/state".into(),
            query: String::new(),
            headers: [("host".to_owned(), host.to_owned())].into(),
            body: Vec::new(),
            peer: "127.0.0.1".parse().unwrap(),
            tls: false,
        }
    }

    #[test]
    fn listening_page_address_and_channels() {
        assert_eq!(
            page_url(&request("192.168.1.20:29216")),
            "http://192.168.1.20:29216/"
        );
        assert_eq!(page_url(&request("church.local")), "http://church.local/");
        assert_eq!(
            page_url(&request("church.local:80")),
            "http://church.local/"
        );
        assert_eq!(
            page_url(&request("[fd00::5]:29216")),
            "http://[fd00::5]:29216/"
        );
        // Opened on this machine: its network address, which a phone can reach.
        for own in ["127.0.0.1:29216", "localhost:29216", "[::1]:29216"] {
            let u = page_url(&request(own));
            assert!(
                u.ends_with(":29216/") && !u.contains("127.0.0.1") && !u.contains("localhost"),
                "{own}: {u}"
            );
        }
        assert_eq!(
            ListenChannel::parse(" Español = MIXER (Matrix 2) "),
            Some(ListenChannel {
                name: "Español".into(),
                source: "MIXER (Matrix 2)".into()
            })
        );
        assert_eq!(
            source_and_channel("BOOTH (Mixer)|ch=37"),
            ("BOOTH (Mixer)", 36)
        );
        assert_eq!(source_and_channel("BOOTH (Mixer)"), ("BOOTH (Mixer)", 0));
        assert_eq!(source_and_channel("X (1)|ch=0"), ("X (1)|ch=0", 0));
        assert_eq!(ListenChannel::parse("English"), None);
        assert_eq!(ListenChannel::parse("=X (1)"), None);
    }

    #[test]
    fn cross_origin_paths_and_headers() {
        for p in [
            "/api/v1/state",
            "/api/v1/report",
            "/whep/CAM",
            "/whep/session/1",
        ] {
            assert!(cross_origin_path(p), "{p}");
        }
        for p in ["/", "/login", "/api/v1/other"] {
            assert!(!cross_origin_path(p), "{p}");
        }
        let r = with_cors(Response::json("{}"), "http://player.local:8080");
        let h = |k: &str| {
            r.headers
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            h("Access-Control-Allow-Origin"),
            Some("http://player.local:8080")
        );
        assert_eq!(h("Access-Control-Expose-Headers"), Some("Location"));
        assert!(h("Access-Control-Allow-Methods").is_some_and(|m| m.contains("DELETE")));
    }

    #[test]
    fn names_and_parameter_sets() {
        assert_eq!(
            percent_decode("CAM-1%20(Pulpit%20%2B%20wide)"),
            "CAM-1 (Pulpit + wide)"
        );
        assert!(has_sps(&[0, 0, 0, 1, 0x67, 1, 2]));
        assert!(!has_sps(&[0, 0, 0, 1, 0x65, 1, 2]));
        let r = resample_to_48k(&[vec![0.0; 441]], 44_100);
        assert_eq!((r.len(), r[0].len()), (2, 480));
        // A source's absurd rate or an empty channel: no giant block, no panic.
        assert_eq!(resample_to_48k(&[vec![0.0; 1024]], 1)[0].len(), 1024);
        let r = resample_to_48k(&[vec![0.0; 441], vec![]], 44_100);
        assert_eq!((r[0].len(), r[1].len()), (441, 0));
    }
}
