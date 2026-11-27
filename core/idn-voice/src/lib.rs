//! IDN Voice (spec/extensions/idn-voice.md): voice channels (IEM, Walkie-Talkie), calls and
//! instant talk between users, text messages and recordings, for both Site Managers. The
//! server forwards each talker's Opus packets to the others, unchanged (it never mixes),
//! over WebRTC (UDP) where it connects and over the control WebSocket otherwise; handsets
//! also over IDN's link (QUIC, [`Voice::serve_link`]).
//!
//! A Site Manager embeds it: [`Voice::start`] with its folder and a [`Host`] (its users,
//! logins, audit, MQTT), then mounts [`Voice::router`] (Industrial at `/voice` on its HTTP
//! port; Media serves it over HTTPS on a port of its own).

pub mod feeds;
pub mod http;
pub mod hub;
pub mod link;
pub mod model;
pub mod record;
pub mod rtc;
mod session;
pub mod store;
mod web;
pub mod ws;

pub use link::{LinkOffer, VoiceLink};

use idn_common::switches;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// A site user as the Site Manager knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub roles: Vec<String>,
    pub admin: bool,
}

/// What Voice asks of the Site Manager it runs in. Handsets (§V.13) need nothing more of
/// it: their records and credentials are Voice's own.
pub trait Host: Send + Sync + 'static {
    /// The site user a request's own Site Manager session (its cookie) belongs to.
    fn session_user(&self, headers: &axum::http::HeaderMap) -> Option<User>;
    /// A site user's name and password checked, from `from` (counted as a login).
    fn login(&self, name: &str, password: &str, from: std::net::IpAddr) -> Option<User>;
    /// The site's users (Auth lists, the people to call).
    fn users(&self) -> Vec<User>;
    /// Into the Site Manager's audit.
    fn audit(&self, who: &str, action: &str, detail: serde_json::Value);
    /// Onto the site's MQTT under `idn/site/voice/` (§V.10): `presence` (retained),
    /// `messages/CHANNEL`, `events`. Nothing by default.
    fn publish(&self, _topic: &str, _body: &serde_json::Value, _retain: bool) {}
    /// Whether a plain-HTTP connection from `peer` is a proxy of this Site Manager's (its
    /// tunnel), whose `X-Forwarded-Proto: https` is then believed: handset credentials
    /// cross only HTTPS (§V.13.2). None by default.
    fn trusted_proxy(&self, _peer: std::net::IpAddr) -> bool {
        false
    }
    /// Whether a request's credential is the site's program (Industrial's API key), not
    /// a person's sign-in: its session is then not ended for a user missing from
    /// [`Host::users`] (the program's user, e.g. `api`, is no site account). None by
    /// default.
    fn is_program(&self, _headers: &axum::http::HeaderMap) -> bool {
        false
    }
}

/// `IDN_VOICE_UDP` as set: off (`off`, `0`, `false`, `no`: WebSocket only), `ADDR:PORT`,
/// or a bare `PORT` (every address); unset, `default`. Anything else is logged and the
/// default kept (only the exact `off` and `ADDR:PORT` were taken; a bare port turned
/// WebRTC off without a word; bug round F, F8-6).
pub fn udp_setting(v: Option<&str>, default: Option<SocketAddr>) -> Option<SocketAddr> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return default;
    };
    if idn_common::switches::says_off(v) {
        return None;
    }
    if let Ok(a) = v.parse::<SocketAddr>() {
        return Some(a);
    }
    if let Ok(p) = v.parse::<u16>() {
        return Some(SocketAddr::from(([0, 0, 0, 0], p)));
    }
    idn_common::log::log(&format!(
        "IDN_VOICE_UDP={v:?}: not ADDR:PORT, PORT or off; {default:?} kept"
    ));
    default
}

/// Where Voice keeps things and listens.
#[derive(Debug, Clone)]
pub struct VoiceConfig {
    /// Its folder (`voice/` in the Site Manager's).
    pub dir: PathBuf,
    /// UDP for WebRTC (§V.1: 29207); `None`: WebSocket only.
    pub udp: Option<SocketAddr>,
    /// The address to give clients for WebRTC when the port is forwarded from outside
    /// (`IDN_VOICE_PUBLIC`); else the one each client reached the server by.
    pub public: Option<std::net::IpAddr>,
    /// The path the router is mounted at (`/voice`), for links in the page.
    pub base: String,
    /// `idn-codec` for IEM feeds (§V.9), if this Site Manager has it.
    pub codec: Option<PathBuf>,
    /// How feeds reach this server (e.g. `ws://127.0.0.1:8080/voice/ws`).
    pub feed_url: Option<String>,
    /// Its web client served too (`/`, its script, style and QR codes); off, the API and
    /// `/ws` only (IDN Media: the UI is the host program's).
    pub pages: bool,
    /// The SHA-256 of the certificate clients see (`AA:BB:…`), told to handsets at
    /// enrolment and in their QR code to pin (§V.13): the host's own certificate (IDN
    /// Media: the panel's), else `IDN_VOICE_CERT_SHA256` (a proxy's in front); `None`:
    /// plain HTTP or a certificate from a public authority (a tunnel's).
    pub cert_sha256: Option<String>,
    /// Addresses of this server for handsets (`https://voice.example.com/voice`): in
    /// their enrolment answer after the one they came by, and the first is the QR code's
    /// unless the request names one. `IDN_VOICE_URLS`, comma-separated.
    pub urls: Vec<String>,
}

impl VoiceConfig {
    pub fn new(dir: PathBuf) -> Self {
        VoiceConfig {
            dir,
            udp: Some(SocketAddr::from((
                [0, 0, 0, 0],
                idn_common::ports::VOICE_RTC,
            ))),
            public: switches::value("IDN_VOICE_PUBLIC").and_then(|v| v.trim().parse().ok()),
            base: "/voice".into(),
            codec: None,
            feed_url: None,
            pages: true,
            cert_sha256: switches::value("IDN_VOICE_CERT_SHA256")
                .and_then(|v| model::fingerprint_text(&v)),
            urls: urls_setting(switches::value("IDN_VOICE_URLS").as_deref()),
        }
    }
}

/// `IDN_VOICE_URLS` as set: each `http://` or `https://` address, without a trailing `/`.
pub fn urls_setting(v: Option<&str>) -> Vec<String> {
    v.unwrap_or_default()
        .split(',')
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .collect()
}

/// A running Voice server. Cheap to clone.
#[derive(Clone)]
pub struct Voice {
    pub hub: Arc<hub::Hub>,
    pub cfg: Arc<VoiceConfig>,
    pub(crate) logins: Arc<http::Logins>,
    pub(crate) feeds: Arc<feeds::Feeds>,
    pub(crate) rtc: Option<rtc::RtcHandle>,
}

impl Voice {
    /// Opens the store and starts the WebRTC thread (if a UDP port is set and free),
    /// the quarter-second tick, the retention and the IEM feeds.
    pub fn start(cfg: VoiceConfig, host: Arc<dyn Host>) -> std::io::Result<Voice> {
        let store = store::Store::open(&cfg.dir)?;
        let hub = hub::Hub::new(store, cfg.dir.clone(), host)?;
        let rtc = match cfg.udp {
            Some(addr) => match rtc::start(addr, cfg.public, hub.clone()) {
                Ok(h) => Some(h),
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "voice: WebRTC off (UDP {addr}: {e}); WebSocket only"
                    ));
                    None
                }
            },
            None => None,
        };
        let cfg = Arc::new(cfg);
        let v = Voice {
            feeds: feeds::Feeds::new(cfg.clone()),
            hub,
            cfg,
            logins: Arc::new(http::Logins::default()),
            rtc,
        };
        // (Threads, not tasks: a Site Manager may build its routes before its runtime runs.)
        // (Weak: the thread ends once the Site Manager lets Voice go.)
        let weak = Arc::downgrade(&v.hub);
        std::thread::Builder::new()
            .name("idn-voice-tick".into())
            .spawn(move || {
                let mut n = 0u64;
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let Some(h) = weak.upgrade() else { return };
                    h.tick();
                    // Each hour, from the first minute: what is past its retention.
                    if n % 14_400 == 240 {
                        prune(&h);
                    }
                    n += 1;
                }
            })?;
        v.feeds.sync(&v.hub.channels());
        Ok(v)
    }

    /// The HTTP routes: the page, the WebSocket, logins, the administration and program API.
    pub fn router(&self) -> axum::Router {
        http::router(self.clone())
    }

    /// Serves the routes at their base path over plain HTTP on `listener`, on `rt`: for
    /// programs on this machine (IEM feeds) where the HTTPS port's certificate is the
    /// site's own. Bind it to loopback.
    pub fn serve_local(
        &self,
        listener: std::net::TcpListener,
        rt: &tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let app = axum::Router::new().nest_service(&self.cfg.base, self.router());
        rt.spawn(async move {
            let Ok(l) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            use axum::serve::ListenerExt;
            let l = l.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            let _ = axum::serve(
                l,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });
        Ok(())
    }

    /// Serves the routes at their base path over HTTPS on `listener` (IDN Site Manager:
    /// port 29203 with its panel's certificate), on `rt`; `/` goes to the page. HTTP/1.1
    /// (WebSockets upgrade there).
    pub fn serve_tls(
        &self,
        listener: std::net::TcpListener,
        tls: Arc<rustls::ServerConfig>,
        rt: &tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        use tower::ServiceExt;
        let mut cfg = (*tls).clone();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        listener.set_nonblocking(true)?;
        let base = self.cfg.base.clone();
        let to = format!("{base}/");
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(move || async move { axum::response::Redirect::to(&to) }),
            )
            .nest_service(&base, self.router());
        rt.spawn(async move {
            let Ok(l) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            loop {
                let (tcp, peer) = match l.accept().await {
                    Ok(x) => x,
                    Err(_) => {
                        // (Out of descriptors: no busy spin, deep bug run 2026-11-09.)
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // Each packet at once (Nagle held them up to 40 ms, profile round 8).
                let _ = tcp.set_nodelay(true);
                let (acceptor, app) = (acceptor.clone(), app.clone());
                tokio::spawn(async move {
                    let Ok(Ok(tls)) =
                        tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await
                    else {
                        return;
                    };
                    let svc = hyper::service::service_fn(
                        move |mut req: hyper::Request<hyper::body::Incoming>| {
                            req.extensions_mut()
                                .insert(axum::extract::ConnectInfo(peer));
                            req.extensions_mut().insert(http::OverTls);
                            app.clone().oneshot(req.map(axum::body::Body::new))
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Ok(())
    }
}

/// Forgets what is older than the site's retention (§V.7).
fn prune(h: &hub::Hub) {
    let days = h.settings().keep_days;
    if days == 0 {
        return;
    }
    let before = hub::now_ms() - days as i64 * 86_400_000;
    if let Ok(ids) = h.store.prune(before) {
        for id in ids {
            record::remove(&h.dir, id);
        }
    }
}
