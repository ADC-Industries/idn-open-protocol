//! QUIC server for the receiver role (spec chapters 2 and 4), on quinn with the NDI
//! stub crypto (`quic/ndi_crypto.rs`): accepts senders' connections and turns their
//! media streams (§2.3) into messages. quinn provides loss recovery, congestion control,
//! flow control, idle timeouts and connection limits.

use super::ndi_crypto;
use crate::frame::{Frame, HEADER_LEN, Header, Limits};
use idn_common::sync::LockOk as _;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

/// The server's connection ID length (§4.1).
pub const CID_LEN: usize = 9;
/// No packet from a sender for this long ends its connection (§4.4).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Connections served at once; more are refused (a receiver has one sender per session,
/// plus a few during reconnects). Unauthenticated peers can't exhaust the receiver.
const MAX_CONNECTIONS: usize = 16;
/// Connections from one address at once: a single host holding connections open can't
/// lock out the real sender.
const MAX_PER_ADDRESS: usize = 4;
/// Events waiting for the input; when it falls behind, flow control slows the senders.
const EVENT_QUEUE: usize = 256;

/// Fixed bytes 4..15 of a stream token (§2.3.1).
const TOKEN_TAIL: [u8; 12] = [
    0xf8, 0x23, 0x4e, 0x82, 0xb0, 0xa0, 0x1c, 0xf8, 0x1e, 0x91, 0x2d, 0xfb,
];
const TOKEN_XOR: [u8; 4] = [0xce, 0xca, 0xcd, 0x2b];

/// The 16-byte token that opens a media stream for identifier `id` (§2.3.1).
pub fn stream_token(id: u32) -> [u8; 16] {
    let mut t = [0u8; 16];
    for (i, b) in id.to_le_bytes().iter().enumerate() {
        t[i] = b ^ TOKEN_XOR[i];
    }
    t[4..].copy_from_slice(&TOKEN_TAIL);
    t
}

/// A receiver's stream identifiers (video, audio), renewed for each session: a sender
/// (or anyone who saw them) told them for one session can't send into a later one, and an
/// earlier sender's streams end.
#[derive(Debug, Clone, Default)]
pub struct StreamIds(Arc<[AtomicU32; 2]>);

impl StreamIds {
    /// Fresh random identifiers.
    pub fn random() -> Self {
        let s = StreamIds::default();
        s.renew();
        s
    }

    pub fn set(&self, video: u32, audio: u32) {
        self.0[0].store(video, Ordering::Relaxed);
        self.0[1].store(audio, Ordering::Relaxed);
    }

    /// New random identifiers, returned.
    pub fn renew(&self) -> (u32, u32) {
        let id = || u32::from_le_bytes(crate::random::secure_random::<4>());
        let (v, a) = (id(), id());
        self.set(v, a);
        (v, a)
    }

    pub fn get(&self) -> (u32, u32) {
        (
            self.0[0].load(Ordering::Relaxed),
            self.0[1].load(Ordering::Relaxed),
        )
    }
}

/// Which media a stream carries, from its token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    /// A token for neither advertised id: reported, never delivered.
    Unknown([u8; 16]),
}

#[derive(Debug)]
pub enum Event {
    Connected(SocketAddr),
    /// One media message from a stream (§2.3.2), extension header already descrambled.
    Message(MediaKind, Frame),
    /// A stream's framing was invalid; the stream is abandoned.
    StreamError(MediaKind, String),
    /// A connection ended; "idle timeout" when the sender went silent.
    Closed(SocketAddr, String),
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().unwrap()));
    // Senders open two bidirectional streams (§2.3.1) and no unidirectional ones (§4.4).
    t.max_concurrent_bidi_streams(4u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    // Room for a few large video frames in flight per stream.
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((16u32 << 20).into());
    t.keep_alive_interval(None);
    Arc::new(t)
}

/// A reason as the rest of the crate reports it.
pub(crate) fn close_reason(e: &quinn::ConnectionError) -> String {
    match e {
        quinn::ConnectionError::TimedOut => "idle timeout".into(),
        quinn::ConnectionError::LocallyClosed => "closed".into(),
        quinn::ConnectionError::ApplicationClosed(c) => {
            format!("peer closed (code {})", c.error_code)
        }
        quinn::ConnectionError::ConnectionClosed(c) => {
            format!("peer closed (code {:?})", c.error_code)
        }
        other => other.to_string(),
    }
}

/// The receiver's QUIC endpoint; [`poll`](Server::poll) returns what happened since.
pub struct Server {
    endpoint: quinn::Endpoint,
    rx: tokio::sync::mpsc::Receiver<Event>,
    ids: StreamIds,
}

impl Server {
    pub fn bind(
        addr: SocketAddr,
        video_id: u32,
        audio_id: u32,
        limits: Limits,
    ) -> io::Result<Self> {
        // Port 0: the next free port of IDN's media range (idn_common::ports).
        let sock = if addr.port() == 0 {
            idn_common::ports::media_udp(addr.ip())?
        } else {
            UdpSocket::bind(addr)?
        };
        let _ = socket2::SockRef::from(&sock).set_recv_buffer_size(8 << 20);
        let mut ep = quinn::EndpointConfig::default();
        ep.cid_generator(|| Box::new(quinn_proto::RandomConnectionIdGenerator::new(CID_LEN)));
        ep.grease_quic_bit(false);
        let mut sc = quinn::ServerConfig::with_crypto(Arc::new(ndi_crypto::ServerConfig));
        sc.transport_config(transport());
        sc.migration(false);
        let endpoint = {
            let _g = super::runtime().enter();
            quinn::Endpoint::new(ep, Some(sc), sock, Arc::new(quinn::TokioRuntime))?
        };
        let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
        let ids = StreamIds::default();
        ids.set(video_id, audio_id);
        let (ep, served) = (endpoint.clone(), ids.clone());
        let active = Arc::new(AtomicUsize::new(0));
        let per_address: Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>> =
            Default::default();
        // Each address's connections, oldest first: when an address is at its limit, its
        // oldest goes (a sender restarted a few times left connections that counted for
        // 30 s, until idle, and its next one fell back from QUIC; bug round F, F5-9).
        let open: Live = Default::default();
        super::runtime().spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let ip = incoming.remote_address().ip();
                {
                    let mut by_ip = per_address.lock_ok();
                    let mine = by_ip.entry(ip).or_default();
                    if *mine >= MAX_PER_ADDRESS
                        && let Some((_, oldest)) = open
                            .lock_ok()
                            .get_mut(&ip)
                            .filter(|v| !v.is_empty())
                            .map(|v| v.remove(0))
                    {
                        oldest.close(0u32.into(), b"replaced by a newer connection");
                        *mine -= 1;
                    }
                    if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS || *mine >= MAX_PER_ADDRESS
                    {
                        if *mine == 0 {
                            by_ip.remove(&ip);
                        }
                        incoming.refuse();
                        continue;
                    }
                    *mine += 1;
                }
                let (tx, active, per_address) = (tx.clone(), active.clone(), per_address.clone());
                let ids = served.clone();
                active.fetch_add(1, Ordering::Relaxed);
                let open = open.clone();
                tokio::spawn(async move {
                    static SERIAL: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
                    let opened = serve(incoming, ids, limits, tx, |c| {
                        open.lock_ok().entry(ip).or_default().push((serial, c));
                    })
                    .await;
                    // Counted down once: here, unless it was evicted for a newer one (taken
                    // out of the list and counted down then). One that never opened was
                    // never listed.
                    let listed = {
                        let mut o = open.lock_ok();
                        let v = o.entry(ip).or_default();
                        let before = v.len();
                        v.retain(|(n, _)| *n != serial);
                        let was = v.len() != before;
                        if v.is_empty() {
                            o.remove(&ip);
                        }
                        was
                    };
                    active.fetch_sub(1, Ordering::Relaxed);
                    if listed || !opened {
                        let mut by_ip = per_address.lock_ok();
                        if let Some(n) = by_ip.get_mut(&ip) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                by_ip.remove(&ip);
                            }
                        }
                    }
                });
            }
        });
        Ok(Server { endpoint, rx, ids })
    }

    /// The identifiers streams must open with: [`StreamIds::renew`] for a new session.
    pub fn ids(&self) -> StreamIds {
        self.ids.clone()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Waits up to about 5 ms for something to happen; returns all that did.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        let first = super::runtime().block_on(async {
            tokio::time::timeout(Duration::from_millis(5), self.rx.recv()).await
        });
        if let Ok(Some(e)) = first {
            out.push(e);
            while let Ok(e) = self.rx.try_recv() {
                out.push(e);
            }
        }
        out
    }

    /// Closes every connection (CONNECTION_CLOSE to each sender).
    pub fn close_all(&mut self) {
        self.endpoint.close(0u32.into(), b"");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"");
    }
}

/// Each address's open connections (a serial number each), oldest first.
type Live = Arc<
    std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, Vec<(u64, quinn::Connection)>>>,
>;

/// One sender's connection: its media streams, then how it ended. `opened`: the
/// connection once it is up. Whether it came up.
async fn serve(
    incoming: quinn::Incoming,
    ids: StreamIds,
    limits: Limits,
    tx: tokio::sync::mpsc::Sender<Event>,
    opened: impl FnOnce(quinn::Connection),
) -> bool {
    let Ok(conn) = incoming.await else {
        return false;
    };
    opened(conn.clone());
    let peer = conn.remote_address();
    let _ = tx.send(Event::Connected(peer)).await;
    let why = loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                tokio::spawn(read_stream(
                    conn.clone(),
                    send,
                    recv,
                    ids.clone(),
                    limits,
                    tx.clone(),
                ));
            }
            Err(e) => break close_reason(&e),
        }
    };
    let _ = tx.send(Event::Closed(peer, why)).await;
    true
}

/// A media stream: its token (§2.3.1), then messages (§2.3.2) until it ends.
async fn read_stream(
    conn: quinn::Connection,
    // Kept open for the life of the stream (the receiver never sends on it).
    _send: quinn::SendStream,
    mut rs: quinn::RecvStream,
    ids: StreamIds,
    limits: Limits,
    tx: tokio::sync::mpsc::Sender<Event>,
) {
    let mut tok = [0u8; 16];
    if rs.read_exact(&mut tok).await.is_err() {
        return;
    }
    let now = ids.get();
    let (kind, which) = if tok == stream_token(now.0) {
        (MediaKind::Video, 0)
    } else if tok == stream_token(now.1) {
        (MediaKind::Audio, 1)
    } else {
        // Only the sender we told the ids to (over the control connection) knows them:
        // anyone else is injecting. Closing the whole connection makes every guess cost
        // a handshake.
        let e = "unknown stream token; connection closed".to_string();
        let _ = tx
            .send(Event::StreamError(MediaKind::Unknown(tok), e))
            .await;
        conn.close(1u32.into(), b"unknown stream token");
        return;
    };
    let mine = if which == 0 { now.0 } else { now.1 };
    loop {
        let mut len = [0u8; 4];
        if rs.read_exact(&mut len).await.is_err() {
            return; // the stream (or connection) ended
        }
        // A session since: this stream was for an earlier one.
        let current = ids.get();
        if (if which == 0 { current.0 } else { current.1 }) != mine {
            let _ = rs.stop(0u32.into());
            return;
        }
        let msg_len = u32::from_le_bytes(len) as usize;
        let mut hb = [0u8; HEADER_LEN];
        if rs.read_exact(&mut hb).await.is_err() {
            return;
        }
        let h = Header::parse(&hb);
        let body_len = match h.check(&limits) {
            Ok(n) if HEADER_LEN + n == msg_len => n,
            Ok(_) => {
                let e = format!("msg_len {msg_len} does not match header {h:?}");
                let _ = tx.send(Event::StreamError(kind, e)).await;
                let _ = rs.stop(1u32.into());
                return;
            }
            Err(e) => {
                let _ = tx.send(Event::StreamError(kind, e.to_string())).await;
                let _ = rs.stop(1u32.into());
                return;
            }
        };
        let Some(body) = crate::quic::read_len(&mut rs, body_len).await else {
            return;
        };
        if tx
            .send(Event::Message(kind, Frame::from_wire(h, body)))
            .await
            .is_err()
        {
            return; // the server was dropped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn one_address_cannot_take_every_connection() {
        let mut server =
            Server::bind("127.0.0.1:0".parse().unwrap(), 1, 2, Limits::default()).unwrap();
        let addr = server.local_addr().unwrap();
        let t = Duration::from_secs(3);
        let _held: Vec<_> = (0..MAX_PER_ADDRESS)
            .map(|_| super::super::client::Client::connect(addr, t).expect("within the cap"))
            .collect();
        // A fifth from the same address (a sender restarted, its old connection not yet
        // idle) is taken, and the address's oldest closed for it (bug round F, F5-9): the
        // address still holds no more than its share.
        let _fifth = super::super::client::Client::connect(addr, t).expect("the newest is taken");
        let end = Instant::now() + Duration::from_secs(5);
        let mut replaced = false;
        while !replaced && Instant::now() < end {
            replaced = server
                .poll()
                .iter()
                .any(|e| matches!(e, Event::Closed(_, why) if why == "closed"));
        }
        assert!(replaced, "the oldest connection closed for the newest");
    }

    #[test]
    fn a_stream_with_an_unknown_token_is_never_delivered() {
        use super::super::client::Client;
        let mut server =
            Server::bind("127.0.0.1:0".parse().unwrap(), 1, 2, Limits::default()).unwrap();
        let addr = server.local_addr().unwrap();
        let msg = Frame::text(crate::frame::kind::METADATA, 0, "<x/>").encode();
        let mut events = Vec::new();
        fn collect(server: &mut Server, events: &mut Vec<Event>, until: fn(&[Event]) -> bool) {
            let end = Instant::now() + Duration::from_secs(5);
            while !until(events) && Instant::now() < end {
                events.extend(server.poll());
            }
        }

        let mut guess = Client::connect(addr, Duration::from_secs(3)).unwrap();
        let s = guess.open_media_stream(0, 99);
        guess.send_message(s, &msg);
        collect(&mut server, &mut events, |ev| {
            ev.iter().any(|e| matches!(e, Event::Closed(..)))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::StreamError(MediaKind::Unknown(_), _))),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::Message(..))),
            "{events:?}"
        );
        let end = Instant::now() + Duration::from_secs(3);
        while guess.closed().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            guess.closed().is_some(),
            "the guessing connection is closed"
        );

        // The real sender's stream still works.
        let mut real = Client::connect(addr, Duration::from_secs(3)).unwrap();
        let s = real.open_media_stream(0, 1);
        real.send_message(s, &msg);
        collect(&mut server, &mut events, |ev| {
            ev.iter().any(|e| matches!(e, Event::Message(..)))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Message(MediaKind::Video, _)))
        );
    }
}
