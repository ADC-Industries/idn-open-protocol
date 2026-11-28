//! Receiver-side endpoints for the chapter 6 transports. Each runs on its own threads
//! and hands complete media frames to a callback.

use super::tcp::{ChunkReader, TICK, TcpReassembler};
use super::udp::{UdpReassembler, is_probe, probe_reply};
use crate::control::parse_element;
use crate::frame::{Frame, FrameError, Limits};
use crate::quic::server::MediaKind;
use idn_common::sync::LockOk as _;
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Called for every reassembled message: (media, transport name, frame or parse error).
pub type Sink = Arc<dyn Fn(MediaKind, &'static str, Result<Frame, FrameError>) + Send + Sync>;

/// Stops its threads when dropped.
pub struct Endpoint {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Endpoint {
    fn new() -> Self {
        Endpoint {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[allow(clippy::too_many_arguments)] // one call site per transport; a struct would add nothing
fn udp_loop(
    sock: UdpSocket,
    kind: MediaKind,
    id: u32,
    via: &'static str,
    unicast: bool,
    limits: Limits,
    sink: Sink,
    stop: Arc<AtomicBool>,
    sealed: Option<(Arc<crate::mcast::Keyring>, u8)>,
    generation: Option<Arc<std::sync::atomic::AtomicU64>>,
    // Unicast: the identifiers, renewed with each session (`which`: 0 video, 1 audio).
    live: Option<(crate::quic::server::StreamIds, usize)>,
) {
    let mut id = id;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let fresh = |id: u32| {
        let mut r = UdpReassembler::new(id, limits);
        r.wrap = sealed.as_ref().map(|s| s.1);
        r
    };
    // Acquire: a generation seen means the identifiers renewed before it are seen too.
    let gen_now = || generation.as_ref().map_or(0, |g| g.load(Ordering::Acquire));
    // The generation first, then the identifiers as they are now: a thread started late,
    // after a session had already renewed them, kept the one it was spawned with and
    // ignored every probe and packet of that session (recv_loopback under load).
    let mut seen_gen = gen_now();
    if let Some((ids, which)) = &live {
        let now = ids.get();
        id = if *which == 0 { now.0 } else { now.1 };
    }
    let mut r = fresh(id);
    let mut buf = vec![0u8; 65_536];
    let mut out = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(got) => got,
            Err(e) => {
                // A lasting error (the socket broken) would otherwise spin this loop;
                // the timeout and one-off errors retry at once.
                if !idn_common::backoff::retry_at_once(&e) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
        };
        let d = &buf[..n];
        // A new control session: its sequence starts again (§6.5); nothing of the last
        // session's groups may be mixed in (R1-76), nor anything sent with its identifier.
        if gen_now() != seen_gen {
            seen_gen = gen_now();
            if let Some((ids, which)) = &live {
                let now = ids.get();
                id = if *which == 0 { now.0 } else { now.1 };
            }
            r = fresh(id);
        }
        if unicast && is_probe(d, &[id]) {
            let _ = sock.send_to(&probe_reply(d.try_into().unwrap()), from);
            continue;
        }
        match &sealed {
            // Encrypted multicast (§E.17.3): only datagrams that open are packets.
            Some((keys, stream)) => match keys.open(*stream, d) {
                Some(p) => r.push(&p, &mut out),
                None => continue,
            },
            None => r.push(d, &mut out),
        }
        for f in out.drain(..) {
            sink(kind, via, f);
        }
    }
    let st = r.stats;
    crate::log::log(&format!(
        "  {via} {kind:?} id {id}: {} packets, {} repaired by FEC, {} lost, {} messages, {} dropped",
        st.packets, st.recovered, st.lost, st.messages, st.dropped_messages
    ));
}

/// UDP unicast (§6.3): one socket per media type on the advertised port.
pub struct UdpUnicast {
    pub video: (u16, u32),
    pub audio: (u16, u32),
    pub endpoint: Endpoint,
    /// Bump it when a new control session starts: the streams start afresh.
    pub generation: Arc<std::sync::atomic::AtomicU64>,
    /// The identifiers: renewed for each session.
    pub ids: crate::quic::server::StreamIds,
}

impl UdpUnicast {
    /// Binds the video and audio ports (0 = ephemeral) with fresh random identifiers.
    /// The identifiers are renewed for each session through [`UdpUnicast::ids`] (before
    /// the generation is counted on).
    pub fn start(video_port: u16, audio_port: u16, limits: Limits, sink: Sink) -> io::Result<Self> {
        // The endpoint first: a failure half-way stops the threads already started (R1-75).
        let mut endpoint = Endpoint::new();
        let stop = endpoint.stop.clone();
        let generation: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        let ids = crate::quic::server::StreamIds::random();
        let mut adv = Vec::new();
        for (port, kind) in [
            (video_port, MediaKind::Video),
            (audio_port, MediaKind::Audio),
        ] {
            // 0: the next free port of IDN's media range (idn_common::ports:
            // a firewall rule names it; never NDI's or Dante's).
            let sock = if port == 0 {
                idn_common::ports::media_udp(Ipv4Addr::UNSPECIFIED.into())?
            } else {
                UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?
            };
            let _ = socket2::SockRef::from(&sock).set_recv_buffer_size(8 << 20);
            let which = adv.len();
            let id = if which == 0 { ids.get().0 } else { ids.get().1 };
            adv.push((sock.local_addr()?.port(), id));
            let (sink, stop, session) = (sink.clone(), stop.clone(), generation.clone());
            let live = (ids.clone(), which);
            endpoint.threads.push(std::thread::spawn(move || {
                udp_loop(
                    sock,
                    kind,
                    id,
                    "udp",
                    true,
                    limits,
                    sink,
                    stop,
                    None,
                    Some(session),
                    Some(live),
                )
            }));
        }
        Ok(UdpUnicast {
            video: adv[0],
            audio: adv[1],
            endpoint,
            generation,
            ids,
        })
    }
}

/// A stream announced in `ndi_multicast_v2` (§6.4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MulticastStream {
    pub kind: String,
    pub addr: SocketAddrV4,
    pub id: u32,
}

/// Parses the `stream` children of an `ndi_multicast_v2` message.
pub fn parse_multicast_announcement(xml: &str) -> Vec<MulticastStream> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<stream") {
        rest = &rest[i..];
        if let Some(e) = parse_element(rest).filter(|e| e.name == "stream")
            && let (Some(t), Some(Ok(addr)), Some(Ok(id))) = (
                e.attr("type"),
                e.attr("address").map(str::parse),
                e.attr("id").map(str::parse),
            )
        {
            out.push(MulticastStream {
                kind: t.to_owned(),
                addr,
                id,
            });
        }
        rest = &rest[1..];
    }
    out
}

/// Multicast (§6.4): joins the announced groups for the wanted streams. With `keys`,
/// the streams are encrypted (§E.17) and only datagrams that open are taken.
pub fn start_multicast(
    streams: &[MulticastStream],
    low: bool,
    iface: Option<Ipv4Addr>,
    limits: Limits,
    sink: Sink,
    keys: Option<Arc<crate::mcast::Keyring>>,
) -> io::Result<Endpoint> {
    let want_video = if low { "vmclq" } else { "vmchq" };
    let ifaces: Vec<Ipv4Addr> = match iface {
        Some(a) => vec![a],
        None => {
            let v: Vec<Ipv4Addr> = crate::discovery::ifaces::ipv4_multicast_interfaces()
                .iter()
                .map(|i| i.addr)
                .collect();
            if v.is_empty() {
                vec![Ipv4Addr::UNSPECIFIED]
            } else {
                v
            }
        }
    };
    let mut endpoint = Endpoint::new();
    let stop = endpoint.stop.clone();
    // One stream of each kind at most: the sender's announcement could list any number,
    // a socket and a thread each (deep bug run 2026-11-09).
    let (mut have_video, mut have_audio) = (false, false);
    for s in streams {
        let kind = match s.kind.as_str() {
            k if k == want_video && !have_video => {
                have_video = true;
                MediaKind::Video
            }
            "amc" if !have_audio => {
                have_audio = true;
                MediaKind::Audio
            }
            _ => continue,
        };
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;
        // Only the groups this socket joined: every IDN stream of a kind is on the same
        // port now (idn_common::ports), and Linux by default hands a socket the groups
        // any socket on the machine joined (IP_MULTICAST_ALL). Windows and the BSDs
        // filter by the socket's own groups already.
        #[cfg(target_os = "linux")]
        sock.set_multicast_all_v4(false)?;
        sock.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, s.addr.port())).into())?;
        let _ = sock.set_recv_buffer_size(8 << 20);
        let joined = ifaces
            .iter()
            .filter(|i| sock.join_multicast_v4(s.addr.ip(), i).is_ok())
            .count();
        if joined == 0 {
            return Err(io::Error::other(format!("could not join {}", s.addr)));
        }
        let sock: UdpSocket = sock.into();
        let (sink, stop, id) = (sink.clone(), stop.clone(), s.id);
        let sealed = keys.clone().zip(crate::mcast::stream_of(&s.kind));
        endpoint.threads.push(std::thread::spawn(move || {
            udp_loop(
                sock,
                kind,
                id,
                "multicast",
                false,
                limits,
                sink,
                stop,
                sealed,
                None,
                None,
            )
        }));
    }
    Ok(endpoint)
}

/// Parses one reassembled media message (a complete §1.2 frame) with the default limits.
pub fn parse_message_bytes(m: &[u8]) -> Result<Frame, FrameError> {
    super::udp::parse_message(m, &Limits::default())
}

/// The host media connections may come from: the current session's sender (`None`
/// between sessions). The input sets it when it connects to a sender.
/// The session's sender host, and which session (a pair of a session that ended is
/// closed, even from the same host: its pictures went on into the next session's; bug
/// round F, F5-1).
pub type Allowed = Arc<Mutex<Option<(IpAddr, u64)>>>;

/// `ip` is the allowed host (IPv4 and its IPv6-mapped form alike).
pub fn is_allowed(a: &Allowed, ip: IpAddr) -> bool {
    a.lock_ok()
        .is_some_and(|(s, _)| s.to_canonical() == ip.to_canonical())
}

/// The session allowed now.
fn allowed_session(a: &Allowed) -> Option<u64> {
    a.lock_ok().map(|(_, s)| s)
}

/// Sender connections served at once per media type (a session uses one; a few more
/// cover reconnects). More are refused.
const MAX_PAIRS: usize = 4;

/// Multi-TCP (§6.2): a listener per media type; for each sender connection the receiver
/// connects back to the sender's source port, and merges both connections. Only
/// connections from the `allowed` host are served.
pub struct MultiTcp {
    pub video_port: u16,
    pub audio_port: u16,
    pub endpoint: Endpoint,
}

impl MultiTcp {
    pub fn start(
        video_port: u16,
        audio_port: u16,
        limits: Limits,
        allowed: Allowed,
        sink: Sink,
    ) -> io::Result<Self> {
        let mut endpoint = Endpoint::new();
        let stop = endpoint.stop.clone();
        let mut ports = Vec::new();
        for (port, kind) in [
            (video_port, MediaKind::Video),
            (audio_port, MediaKind::Audio),
        ] {
            let l = if port == 0 {
                idn_common::ports::media_tcp(Ipv4Addr::UNSPECIFIED.into())?
            } else {
                TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?
            };
            l.set_nonblocking(true)?;
            ports.push(l.local_addr()?.port());
            let (sink, stop, allowed) = (sink.clone(), stop.clone(), allowed.clone());
            let serving = Arc::new(AtomicUsize::new(0));
            endpoint.threads.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((a, peer)) = l.accept() else {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    };
                    let from_sender = is_allowed(&allowed, peer.ip());
                    if !from_sender || serving.load(Ordering::Relaxed) >= MAX_PAIRS {
                        continue; // dropped: closes the connection
                    }
                    let _ = a.set_nonblocking(false);
                    let session = (allowed.clone(), allowed_session(&allowed));
                    let (sink, stop, serving) = (sink.clone(), stop.clone(), serving.clone());
                    serving.fetch_add(1, Ordering::Relaxed);
                    let spawned = std::thread::Builder::new().spawn({
                        let serving = serving.clone();
                        move || {
                            if let Err(e) =
                                serve_pair(a, peer, kind, limits, sink.clone(), stop, session)
                            {
                                sink(kind, "tcp", Err(FrameError::BadText));
                                crate::log::log(&format!("multi-TCP {kind:?} from {peer}: {e}"));
                            }
                            serving.fetch_sub(1, Ordering::Relaxed);
                        }
                    });
                    if spawned.is_err() {
                        serving.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        Ok(MultiTcp {
            video_port: ports[0],
            audio_port: ports[1],
            endpoint,
        })
    }
}

fn serve_pair(
    mut a: TcpStream,
    peer: SocketAddr,
    kind: MediaKind,
    limits: Limits,
    sink: Sink,
    stop: Arc<AtomicBool>,
    (allowed, session): (Allowed, Option<u64>),
) -> io::Result<()> {
    // One tick on the sender's connection only; nothing on our own (§6.5).
    let b = TcpStream::connect_timeout(&peer, Duration::from_secs(3))?;
    a.write_all(&TICK)?;
    let asm = Arc::new(Mutex::new(TcpReassembler::new(limits)));
    let mut readers = Vec::new();
    let done = Arc::new(AtomicBool::new(false));
    for conn in [a, b] {
        let (asm, sink, stop) = (asm.clone(), sink.clone(), stop.clone());
        let (allowed, done) = (allowed.clone(), done.clone());
        conn.set_read_timeout(Some(Duration::from_millis(200)))?;
        // A sender that lost power or its cable is noticed (R1-74); one that is just
        // quiet (paused) is not.
        crate::transport::keepalive(&conn, Duration::from_secs(3));
        readers.push(std::thread::spawn(move || -> io::Result<()> {
            let mut r = ChunkReader::new(io::BufReader::with_capacity(1 << 18, conn));
            let mut out = Vec::new();
            // (Either side ending ends both: the second reader was left running when the
            // first returned an error, and pairs over the limit ran; F5-10.)
            let end = |r: io::Result<()>| {
                done.store(true, Ordering::Relaxed);
                r
            };
            while !stop.load(Ordering::Relaxed) && !done.load(Ordering::Relaxed) {
                // The session ended (or another began): its pair is closed.
                if allowed_session(&allowed) != session {
                    return end(Ok(()));
                }
                let c = match r.read_chunk() {
                    Ok(Some(c)) => c,
                    Ok(None) => return end(Ok(())),
                    Err(e) if crate::transport::is_poll_timeout(&e) => {
                        continue;
                    }
                    Err(e) => return end(Err(e)),
                };
                // Handed on under the lock: a message the other connection finished just
                // after could otherwise go first (an HX frame before its reference, sound
                // blocks swapped, deep bug run 2026-11-09). The sink only queues.
                let mut g = asm.lock_ok();
                if let Err(e) = g.push(c, &mut out) {
                    return end(Err(e));
                }
                for f in out.drain(..) {
                    sink(kind, "tcp", f);
                }
                drop(g);
            }
            Ok(())
        }));
    }
    // Both joined before the pair counts as served no more.
    let mut first = Ok(());
    for r in readers {
        let r = r
            .join()
            .map_err(|_| io::Error::other("reader panicked"))
            .and_then(|r| r);
        if first.is_ok() {
            first = r;
        }
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicast_announcement() {
        let xml = r#"<ndi_multicast_v2><stream type="vmclq" address="239.255.155.123:20392" id="878253263"/><stream type="vmchq" address="239.255.207.98:36049" id="2504055324"/><stream type="amc" address="239.255.239.90:48384" id="3727282431"/></ndi_multicast_v2>"#;
        let s = parse_multicast_announcement(xml);
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[1],
            MulticastStream {
                kind: "vmchq".into(),
                addr: "239.255.207.98:36049".parse().unwrap(),
                id: 2504055324
            }
        );
    }

    /// A UDP loop whose thread starts after a session already renewed the identifiers
    /// answers the new session's probe (it kept the identifier it was spawned with).
    #[test]
    fn a_late_udp_loop_takes_the_renewed_identifier() {
        use super::super::udp::probe;
        let ids = crate::quic::server::StreamIds::random();
        let spawned_with = ids.get().0;
        let generation: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        // The session's renewal, before the loop runs.
        let (renewed, _) = ids.renew();
        generation.fetch_add(1, Ordering::Release);
        assert_ne!(renewed, spawned_with);
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = sock.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let sink: Sink = Arc::new(|_, _, _| {});
        let t = {
            let (stop, g, live) = (stop.clone(), generation.clone(), (ids.clone(), 0));
            std::thread::spawn(move || {
                udp_loop(
                    sock,
                    MediaKind::Video,
                    spawned_with,
                    "udp",
                    true,
                    Limits::default(),
                    sink,
                    stop,
                    None,
                    Some(g),
                    Some(live),
                )
            })
        };
        let me = UdpSocket::bind("127.0.0.1:0").unwrap();
        me.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut answered = false;
        for _ in 0..20 {
            me.send_to(&probe(renewed), dest).unwrap();
            let mut b = [0u8; 16];
            if let Ok((n, _)) = me.recv_from(&mut b) {
                assert_eq!(&b[..n], &probe_reply(&probe(renewed)));
                answered = true;
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(answered, "the renewed identifier's probe wasn't answered");
    }

    #[test]
    fn multi_tcp_serves_only_the_session_sender() {
        use std::io::Read;
        use std::time::Instant;
        let calls = Arc::new(AtomicUsize::new(0));
        let sink: Sink = {
            let calls = calls.clone();
            Arc::new(move |_, _, _| {
                calls.fetch_add(1, Ordering::Relaxed);
            })
        };
        let allowed = Allowed::default();
        let t = MultiTcp::start(0, 0, Limits::default(), allowed.clone(), sink).unwrap();
        let port = t.video_port;
        // No session yet, then a session with another host: both refused (closed at
        // once, nothing reaches the input).
        for ip in [None, Some("127.0.0.2".parse().unwrap())] {
            *allowed.lock_ok() = ip.map(|ip| (ip, 1));
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            assert!(matches!(c.read(&mut [0u8; 16]), Ok(0) | Err(_)));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        // The session's sender is served (its connect-back fails here: no listener, which
        // the input hears about).
        *allowed.lock_ok() = Some(("127.0.0.1".parse().unwrap(), 2));
        let _c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::Relaxed) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
