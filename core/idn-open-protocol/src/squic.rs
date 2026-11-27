//! `secure/1` media over QUIC with real TLS 1.3 (spec/extensions/idn-extensions.md §E.8.4):
//! QUIC v1 (quinn, RFC 9000/9001), ALPN `IDN_v1`, mutual authentication with the site
//! certificate. The sender is the QUIC client and the receiver the server, as in
//! chapter 2. Each media stream starts with the session's 16-byte media token, then the
//! §2.3.1 stream token, then §2.3.2 framing.
//!
//! quinn is asynchronous; it runs on a small private tokio runtime, and this module
//! offers blocking/channel interfaces to the thread-based rest of the crate.

use crate::frame::{Frame, HEADER_LEN, Header, Limits};
use crate::quic::server::{MediaKind, stream_token};
use crate::random::secure_random;
use crate::secure::Creds;
use crate::transport::receiver::Sink;
use idn_common::sync::LockOk as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// ALPN of the secure media connection (§E.8.4).
pub const MEDIA_ALPN: &[u8] = b"IDN_v1";

static SIMULATE_BLOCKED: AtomicBool = AtomicBool::new(false);

/// Makes every secure QUIC server ignore incoming connections, as if UDP were blocked.
/// For testing the inline fallback.
#[doc(hidden)]
pub fn simulate_udp_blocked(on: bool) {
    SIMULATE_BLOCKED.store(on, Ordering::Relaxed);
}

/// How long a sender tries secure QUIC before falling back to encrypted inline media.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// quinn's send window for media (see `transport_config`), as for NDI QUIC.
const SEND_WINDOW: u64 = 2 << 20;

fn runtime() -> &'static tokio::runtime::Runtime {
    crate::quic::runtime()
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // Media streams are long-lived and carry up to ~100 Mbit/s: generous flow control,
    // a keep-alive well inside the idle timeout.
    t.stream_receive_window((16u32 << 20).into());
    t.receive_window((32u32 << 20).into());
    // Bytes quinn may hold unacknowledged: a few frames, so that a receiver behind (a slow
    // Wi-Fi link) makes the sender skip frames instead of queueing seconds of video.
    t.send_window(SEND_WINDOW);
    t.max_concurrent_uni_streams(8u32.into());
    t.max_concurrent_bidi_streams(0u32.into());
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    Arc::new(t)
}

fn server_config(creds: &Creds) -> io::Result<quinn::ServerConfig> {
    let mut tls = creds.server_config(MEDIA_ALPN)?;
    tls.max_early_data_size = 0;
    let crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut c = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    c.transport_config(transport_config());
    Ok(c)
}

fn client_config(creds: &Creds) -> io::Result<quinn::ClientConfig> {
    let tls = creds.client_config(MEDIA_ALPN)?;
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(io::Error::other)?;
    let mut c = quinn::ClientConfig::new(Arc::new(crypto));
    c.transport_config(transport_config());
    Ok(c)
}

// ---------------------------------------------------------------------------------------
// Receiver side: the QUIC server.

/// What a receiver advertises for one control session (§E.8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    pub port: u16,
    pub video_id: u32,
    pub audio_id: u32,
    pub token: [u8; 16],
}

impl Offer {
    /// The `ndi_enabled_streams` attributes.
    pub fn attrs(&self) -> Vec<(String, String)> {
        vec![
            ("x_idn_squic_port".into(), self.port.to_string()),
            ("x_idn_squic_video_id".into(), self.video_id.to_string()),
            ("x_idn_squic_audio_id".into(), self.audio_id.to_string()),
            (
                "x_idn_media_token".into(),
                idn_common::hex::hex(&self.token),
            ),
        ]
    }

    /// Parses the attributes from a receiver's `ndi_enabled_streams`.
    pub fn parse(e: &crate::control::Element) -> Option<Self> {
        let num = |k: &str| e.attr(k).and_then(|v| v.parse::<u64>().ok());
        let token = unhex16(e.attr("x_idn_media_token")?)?;
        Some(Offer {
            port: u16::try_from(num("x_idn_squic_port")?).ok()?,
            video_id: u32::try_from(num("x_idn_squic_video_id")?).ok()?,
            audio_id: u32::try_from(num("x_idn_squic_audio_id")?).ok()?,
            token,
        })
    }
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The receiver's secure QUIC endpoint. Kept for the life of an input; each control
/// session gets a fresh token from [`SquicServer::new_session`].
pub struct SquicServer {
    endpoint: quinn::Endpoint,
    port: u16,
    ids: (u32, u32),
    token: Arc<Mutex<Tokens>>,
}

/// The current session's media token, and the last few before it (§E.8.4).
struct Tokens {
    current: [u8; 16],
    old: Vec<[u8; 16]>,
}

/// The `via` a wrong media token is reported with (to the sink, as an error): the input
/// ends the session (§E.8.4, R1-69).
pub const WRONG_TOKEN: &str = "squic-wrong-token";

impl SquicServer {
    /// Binds UDP `port` (0 = any) and accepts sender connections authenticated with
    /// `creds`.
    pub fn start(creds: &Creds, port: u16, limits: Limits, sink: Sink) -> io::Result<Self> {
        let cfg = server_config(creds)?;
        // Port 0: the next free one of IDN's media range (senders connect in to it: a
        // firewall rule names the range; F1-3).
        let sock = if port == 0 {
            idn_common::ports::media_udp(Ipv4Addr::UNSPECIFIED.into())?
        } else {
            std::net::UdpSocket::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))?
        };
        let endpoint = {
            let _g = runtime().enter();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(cfg),
                sock,
                Arc::new(quinn::TokioRuntime),
            )?
        };
        let port = endpoint.local_addr()?.port();
        let ids = (
            u32::from_le_bytes(secure_random()),
            u32::from_le_bytes(secure_random()),
        );
        let token = Arc::new(Mutex::new(Tokens {
            current: secure_random::<16>(),
            old: Vec::new(),
        }));
        let (ep, tok) = (endpoint.clone(), token.clone());
        runtime().spawn(async move {
            while let Some(incoming) = ep.accept().await {
                if SIMULATE_BLOCKED.load(Ordering::Relaxed) {
                    incoming.ignore();
                    continue;
                }
                let (tok, sink) = (tok.clone(), sink.clone());
                tokio::spawn(async move {
                    // A peer without the site certificate fails here, inside rustls.
                    let Ok(conn) = incoming.await else { return };
                    while let Ok(rs) = conn.accept_uni().await {
                        tokio::spawn(read_stream(
                            rs,
                            conn.clone(),
                            tok.clone(),
                            ids,
                            limits,
                            sink.clone(),
                        ));
                    }
                });
            }
        });
        Ok(SquicServer {
            endpoint,
            port,
            ids,
            token,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Switches the credentials for new connections (the next session may use the other
    /// kind: passphrase or device certificate).
    pub fn set_creds(&self, creds: &Creds) -> io::Result<()> {
        self.endpoint.set_server_config(Some(server_config(creds)?));
        Ok(())
    }

    /// A new media token for a new control session; streams carrying an older token are
    /// closed from now on.
    pub fn new_session(&self) -> Offer {
        let token = secure_random::<16>();
        {
            let mut t = self.token.lock_ok();
            let prev = t.current;
            t.old.push(prev);
            if t.old.len() > 8 {
                t.old.remove(0);
            }
            t.current = token;
        }
        Offer {
            port: self.port,
            video_id: self.ids.0,
            audio_id: self.ids.1,
            token,
        }
    }
}

impl Drop for SquicServer {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"closed");
    }
}

async fn read_stream(
    mut rs: quinn::RecvStream,
    conn: quinn::Connection,
    token: Arc<Mutex<Tokens>>,
    ids: (u32, u32),
    limits: Limits,
    sink: Sink,
) {
    let mut head = [0u8; 32];
    if rs.read_exact(&mut head).await.is_err() {
        return;
    }
    // The media token binds the stream to the current authenticated control session.
    let (current, stale) = {
        let t = token.lock_ok();
        // (Compared in constant time: it is a secret of the session.)
        (
            crate::trust::proto::same(&head[..16], &t.current),
            t.old
                .iter()
                .any(|o| crate::trust::proto::same(&head[..16], o)),
        )
    };
    if !current {
        let _ = rs.stop(1u32.into());
        if stale {
            // From one of this receiver's earlier sessions: just late.
            crate::log::log("secure QUIC: stream with a stale media token closed");
        } else {
            // A token this receiver never gave out, from a peer with the site's
            // credentials: possibly an attack. The connection and the session end
            // (§E.8.4).
            crate::log::log("secure QUIC: stream with a wrong media token: connection closed");
            conn.close(3u32.into(), b"wrong media token");
            sink(
                MediaKind::Video,
                WRONG_TOKEN,
                Err(crate::frame::FrameError::BadText),
            );
        }
        return;
    }
    let st: [u8; 16] = head[16..].try_into().unwrap();
    let kind = if st == stream_token(ids.0) {
        MediaKind::Video
    } else if st == stream_token(ids.1) {
        MediaKind::Audio
    } else {
        let _ = rs.stop(1u32.into());
        return;
    };
    loop {
        // A later session's token makes this stream stale.
        let mut len = [0u8; 4];
        if rs.read_exact(&mut len).await.is_err() {
            return;
        }
        if head[..16] != token.lock_ok().current {
            let _ = rs.stop(1u32.into());
            return;
        }
        let msg_len = u32::from_le_bytes(len) as usize;
        let mut hb = [0u8; HEADER_LEN];
        if msg_len < HEADER_LEN || rs.read_exact(&mut hb).await.is_err() {
            return;
        }
        let h = Header::parse(&hb);
        let body_len = match h.check(&limits) {
            Ok(n) if HEADER_LEN + n == msg_len => n,
            Ok(_) => {
                crate::log::log(&format!(
                    "secure QUIC {kind:?}: msg_len {msg_len} does not match header; stream abandoned"
                ));
                return;
            }
            Err(e) => return sink(kind, "squic", Err(e)),
        };
        let Some(body) = crate::quic::read_len(&mut rs, body_len).await else {
            return;
        };
        sink(kind, "squic", Ok(Frame::from_wire(h, body)));
    }
}

// ---------------------------------------------------------------------------------------
// Sender side: the QUIC client.

/// Why a secure QUIC connection could not be used.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing answered in time (UDP blocked?): fall back to encrypted inline media.
    Unreachable(String),
    /// The peer answered but the connection failed (certificate, ALPN, protocol): no
    /// fallback, the session ends (§E.8.4).
    Rejected(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ConnectError::Unreachable(e) => write!(f, "secure QUIC unreachable: {e}"),
            ConnectError::Rejected(e) => write!(f, "secure QUIC rejected: {e}"),
        }
    }
}

/// An open secure media connection: queue messages with [`SquicSender::send`].
pub struct SquicSender {
    video: tokio::sync::mpsc::Sender<Vec<u8>>,
    audio: tokio::sync::mpsc::Sender<Vec<u8>>,
    failed: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    conn: quinn::Connection,
    /// Bytes queued with `send` and not yet taken by quinn.
    pending: Arc<AtomicU64>,
}

impl SquicSender {
    /// Queues one media message (frame bytes, §2.3.2 adds the length); returns false if
    /// it was skipped. A receiver that is behind loses whole messages, never parts of one:
    /// new video is skipped once about two frames are waiting (at least 512 kB), as on
    /// the NDI QUIC path, so latency stays at a few frames; audio only far behind.
    pub fn send(&self, is_video: bool, frame_bytes: Vec<u8>) -> bool {
        let len = frame_bytes.len() as u64 + 4;
        let limit = if is_video {
            (2 * len).clamp(512 << 10, 16 << 20)
        } else {
            16 << 20
        };
        if self.pending.load(Ordering::Relaxed) >= limit {
            return false;
        }
        self.pending.fetch_add(len, Ordering::Relaxed);
        let tx = if is_video { &self.video } else { &self.audio };
        if tx.try_send(frame_bytes).is_err() {
            self.pending.fetch_sub(len, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Why the connection failed, once it has.
    pub fn failed(&self) -> Option<String> {
        self.failed.lock_ok().clone()
    }
}

impl Drop for SquicSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.conn.close(0u32.into(), b"done");
    }
}

/// Connects to a receiver's offer and opens the two media streams.
pub fn connect(
    creds: &Creds,
    ip: std::net::IpAddr,
    offer: &Offer,
    timeout: Duration,
) -> Result<SquicSender, ConnectError> {
    let cfg = client_config(creds).map_err(|e| ConnectError::Rejected(e.to_string()))?;
    let offer = *offer;
    runtime().block_on(async move {
        let mut ep = quinn::Endpoint::client(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0))
            .map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        ep.set_default_client_config(cfg);
        let connecting = ep
            .connect(SocketAddr::new(ip, offer.port), "idn-site")
            .map_err(|e| ConnectError::Rejected(e.to_string()))?;
        let conn = match tokio::time::timeout(timeout, connecting).await {
            Err(_) => {
                return Err(ConnectError::Unreachable(format!(
                    "no answer from UDP {} within {timeout:?}",
                    offer.port
                )));
            }
            Ok(Err(quinn::ConnectionError::TimedOut)) => {
                return Err(ConnectError::Unreachable("timed out".into()));
            }
            Ok(Err(e)) => return Err(ConnectError::Rejected(e.to_string())),
            Ok(Ok(c)) => c,
        };
        let failed: Arc<Mutex<Option<String>>> = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicU64::new(0));
        let open = |id: u32| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            let (conn, failed, stop, pending) =
                (conn.clone(), failed.clone(), stop.clone(), pending.clone());
            let mut head = offer.token.to_vec();
            head.extend_from_slice(&stream_token(id));
            tokio::spawn(async move {
                let r: Result<(), String> = async {
                    let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
                    s.write_all(&head).await.map_err(|e| e.to_string())?;
                    while let Some(m) = rx.recv().await {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        s.write_all(&(m.len() as u32).to_le_bytes())
                            .await
                            .map_err(|e| e.to_string())?;
                        s.write_all(&m).await.map_err(|e| e.to_string())?;
                        pending.fetch_sub(m.len() as u64 + 4, Ordering::Relaxed);
                    }
                    let _ = s.finish();
                    Ok(())
                }
                .await;
                if let Err(e) = r {
                    failed.lock_ok().get_or_insert(e);
                }
            });
            tx
        };
        let video = open(offer.video_id);
        let audio = open(offer.audio_id);
        // Keep the endpoint alive as long as the connection.
        let ep2 = ep.clone();
        let c2 = conn.clone();
        tokio::spawn(async move {
            let _ = c2.closed().await;
            ep2.wait_idle().await;
        });
        Ok(SquicSender {
            video,
            audio,
            failed,
            stop,
            conn,
            pending,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::kind;

    #[test]
    fn offer_attrs_roundtrip() {
        let o = Offer {
            port: 4321,
            video_id: 7,
            audio_id: 9,
            token: secure_random(),
        };
        let mut e = crate::xml::Element::new("ndi_enabled_streams");
        for (k, v) in o.attrs() {
            e = e.with_attr(&k, &v);
        }
        let parsed = crate::control::parse_element(&e.to_xml()).unwrap();
        assert_eq!(Offer::parse(&parsed), Some(o));
    }

    type Collected = Arc<Mutex<Vec<(MediaKind, Frame)>>>;

    fn collecting_sink() -> (Sink, Collected) {
        let got: Arc<Mutex<Vec<(MediaKind, Frame)>>> = Default::default();
        let g = got.clone();
        (
            Arc::new(move |k, _via, f: Result<Frame, _>| g.lock_ok().push((k, f.unwrap()))),
            got,
        )
    }

    fn wait_for(got: &Collected, n: usize) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock_ok().len() < n && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_token_never_given_out_closes_the_connection_and_is_reported() {
        let key = Creds::Passphrase(crate::secure::SiteKey::from_passphrase("squic test").unwrap());
        let alarms: Arc<Mutex<Vec<&'static str>>> = Default::default();
        let a = alarms.clone();
        let sink: Sink = Arc::new(move |_k, via, f: Result<Frame, _>| {
            if f.is_err() {
                a.lock_ok().push(via);
            }
        });
        let server = SquicServer::start(&key, 0, Limits::default(), sink).unwrap();
        let offer = server.new_session();
        let forged = Offer {
            token: secure_random(),
            ..offer
        };
        let tx = connect(&key, Ipv4Addr::LOCALHOST.into(), &forged, CONNECT_TIMEOUT).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while tx.failed().is_none() && std::time::Instant::now() < end {
            // The sender notices the close on its next write.
            tx.send(true, Frame::text(kind::VIDEO, 0, "x").encode());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tx.failed().is_some(), "the connection is closed");
        // Reported (once per stream: video and audio).
        let a = alarms.lock_ok();
        assert!(
            !a.is_empty() && a.iter().all(|v| *v == WRONG_TOKEN),
            "{a:?}"
        );
    }

    #[test]
    fn media_flows_and_is_bound_to_the_session_token() {
        let key = Creds::Passphrase(crate::secure::SiteKey::from_passphrase("squic test").unwrap());
        let (sink, got) = collecting_sink();
        let server = SquicServer::start(&key, 0, Limits::default(), sink).unwrap();
        let offer = server.new_session();
        let ip = Ipv4Addr::LOCALHOST.into();
        let tx = connect(&key, ip, &offer, CONNECT_TIMEOUT).unwrap();
        let video = Frame {
            kind: kind::VIDEO,
            scrambled: true,
            version: 0,
            ext: vec![1; 20],
            payload: vec![7; 100_000],
        };
        let audio = Frame {
            kind: kind::AUDIO,
            scrambled: true,
            version: 1,
            ext: vec![2; 20],
            payload: vec![9; 4000],
        };
        tx.send(true, video.encode());
        tx.send(false, audio.encode());
        wait_for(&got, 2);
        {
            let g = got.lock_ok();
            assert_eq!(g.len(), 2);
            let v = g.iter().find(|(k, _)| *k == MediaKind::Video).unwrap();
            assert_eq!(
                (v.1.ext.as_slice(), v.1.payload.len()),
                (video.ext.as_slice(), 100_000)
            );
            assert!(
                g.iter()
                    .any(|(k, f)| *k == MediaKind::Audio && f.payload == audio.payload)
            );
        }
        // A new control session invalidates the old token: the old connection's streams
        // are closed and deliver nothing more.
        let offer2 = server.new_session();
        tx.send(true, video.encode());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(got.lock_ok().len(), 2, "stale-token media must be dropped");
        let tx2 = connect(&key, ip, &offer2, CONNECT_TIMEOUT).unwrap();
        tx2.send(true, video.encode());
        wait_for(&got, 3);
        assert_eq!(got.lock_ok().len(), 3);
    }

    #[test]
    fn other_site_is_rejected_and_silence_is_unreachable() {
        let key = Creds::Passphrase(crate::secure::SiteKey::from_passphrase("site one").unwrap());
        let other = Creds::Passphrase(crate::secure::SiteKey::from_passphrase("site two").unwrap());
        let (sink, got) = collecting_sink();
        let server = SquicServer::start(&key, 0, Limits::default(), sink).unwrap();
        let offer = server.new_session();
        let ip = Ipv4Addr::LOCALHOST.into();
        match connect(&other, ip, &offer, CONNECT_TIMEOUT) {
            Err(ConnectError::Rejected(_)) => {}
            Err(e) => panic!("expected a rejection, got {e}"),
            Ok(_) => panic!("a different site key must not connect"),
        }
        assert!(got.lock_ok().is_empty());
        // Nothing listening (as with UDP blocked): unreachable, which allows the fallback.
        let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let o = Offer {
            port: dead.local_addr().unwrap().port(),
            ..offer
        };
        match connect(&key, ip, &o, Duration::from_millis(500)) {
            Err(ConnectError::Unreachable(_)) => {}
            Err(e) => panic!("expected unreachable, got {e}"),
            Ok(_) => panic!("nothing listens there"),
        }
    }

    #[test]
    fn a_stalled_link_skips_video_instead_of_queueing_it() {
        use std::net::UdpSocket;
        let key = Creds::Passphrase(crate::secure::SiteKey::from_passphrase("slow link").unwrap());
        let (sink, _got) = collecting_sink();
        let server = SquicServer::start(&key, 0, Limits::default(), sink).unwrap();
        let offer = server.new_session();
        // A relay to the server that can go silent, like a Wi-Fi link that stalls.
        let front = UdpSocket::bind("127.0.0.1:0").unwrap();
        let back = UdpSocket::bind("127.0.0.1:0").unwrap();
        back.connect(("127.0.0.1", server.port())).unwrap();
        for s in [&front, &back] {
            s.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        }
        let relay_port = front.local_addr().unwrap().port();
        let cut = Arc::new(AtomicBool::new(false));
        let c = cut.clone();
        std::thread::spawn(move || {
            let mut client = None;
            let mut buf = vec![0u8; 65536];
            loop {
                while let Ok((n, from)) = front.recv_from(&mut buf) {
                    client = Some(from);
                    if !c.load(Ordering::Relaxed) {
                        let _ = back.send(&buf[..n]);
                    }
                }
                while let Ok(n) = back.recv(&mut buf) {
                    if let (Some(to), false) = (client, c.load(Ordering::Relaxed)) {
                        let _ = front.send_to(&buf[..n], to);
                    }
                }
            }
        });
        let o = Offer {
            port: relay_port,
            ..offer
        };
        let tx = connect(&key, Ipv4Addr::LOCALHOST.into(), &o, CONNECT_TIMEOUT).unwrap();
        cut.store(true, Ordering::Relaxed);
        // 100 frames of 300 kB (30 MB) into a dead link: only a few frames' worth may be
        // accepted (quinn's 2 MB window plus about two frames waiting).
        let frame = vec![7u8; 300_000];
        let accepted = (0..100)
            .filter(|_| {
                let ok = tx.send(true, frame.clone());
                std::thread::sleep(Duration::from_millis(3));
                ok
            })
            .count();
        assert!(
            (3..=15).contains(&accepted),
            "{accepted} of 100 frames queued"
        );
        assert!(tx.pending.load(Ordering::Relaxed) <= 2 * 300_004 + 300_004);
    }
}
