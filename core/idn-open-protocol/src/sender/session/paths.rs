//! Opening a receiver's transports (§6): QUIC, UDP, multi-TCP, multicast; and a routing
//! source's session (§1.8).

use super::*;

/// Chooses the transport the way NDI 6.3 senders do (§6.1): QUIC, then multi-TCP (with
/// UDP also offered), then UDP unicast, else the control connection. A transport that
/// can't be opened (QUIC or UDP blocked by a firewall, say) is skipped: media on the
/// control connection always works, NDI receivers accept it whatever they offered
/// (§1.6).
pub(in crate::sender) fn open_path(
    ip: IpAddr,
    w: &Wants,
    client: &Arc<ClientShared>,
) -> io::Result<Path> {
    let skip = |what: &str, e: io::Error| {
        crate::log::log(&format!(
            "receiver {ip}: {what} unavailable ({e}); trying the next transport"
        ));
    };
    // A receiver QUIC didn't reach lately isn't tried again for 5 minutes: each session
    // waited the 5 s handshake out first, 5 s with no picture at every reconnect where
    // UDP is blocked (bug round F, F5-11).
    static QUIC_FAILED: Mutex<Vec<(IpAddr, std::time::Instant)>> = Mutex::new(Vec::new());
    const QUIC_RETRY: Duration = Duration::from_secs(300);
    let quic_failed_lately = {
        let mut f = QUIC_FAILED.lock_ok();
        f.retain(|(_, at)| at.elapsed() < QUIC_RETRY);
        f.iter().any(|(a, _)| *a == ip)
    };
    if let Some((port, vid, aid)) = w.quic.filter(|_| !quic_failed_lately) {
        match open_quic(ip, port, vid, aid, client.clone()) {
            Ok(p) => {
                QUIC_FAILED.lock_ok().retain(|(a, _)| *a != ip);
                return Ok(p);
            }
            Err(e) => {
                let mut f = QUIC_FAILED.lock_ok();
                if f.len() < 1024 {
                    f.push((ip, std::time::Instant::now()));
                }
                drop(f);
                skip("QUIC (not tried again for 5 minutes)", e);
            }
        }
    }
    if let (Some((vp, ap)), Some(_)) = (w.tcp, w.udp) {
        // Multi-TCP when TCP and unicast are offered without QUIC (§6.1).
        match tcp_pair(ip, vp).and_then(|v| Ok((v, tcp_pair(ip, ap)?))) {
            Ok((v, a)) => {
                return Ok(Path::Tcp {
                    video: (v, 0),
                    audio: (a, 0),
                });
            }
            Err(e) => skip("multi-TCP", e),
        }
    }
    if let Some((video, audio)) = w.udp {
        match open_udp(ip, video, audio) {
            Ok(p) => return Ok(p),
            Err(e) => skip("UDP", e),
        }
    }
    Ok(Path::Inline)
}

/// QUIC media (chapter 2): connects and opens the video and audio streams.
pub(in crate::sender) fn open_quic(
    ip: IpAddr,
    port: u16,
    vid: u32,
    aid: u32,
    client: Arc<ClientShared>,
) -> io::Result<Path> {
    let mut c = QuicClient::connect(SocketAddr::new(ip, port), Duration::from_secs(5))?;
    let v = c.open_media_stream(0, vid);
    let a = c.open_media_stream(1, aid);
    // A driver thread hands messages to quinn (which runs on its own threads) and
    // watches for the connection ending.
    let (tx, rx) = sync_channel::<QuicFrame>(8);
    let failed: Arc<Mutex<Option<String>>> = Default::default();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (failed, stop) = (failed.clone(), stop.clone());
        std::thread::spawn(move || {
            use std::sync::mpsc::RecvTimeoutError;
            // After HX video was skipped, skip it until a keyframe (§7.1.4).
            let mut hx_need_key = false;
            while !stop.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame) => {
                        // Bound the backlog: while the receiver is behind, skip new
                        // video once about two frames are waiting (at least 512 kB),
                        // so latency stays at a few frames instead of growing to
                        // seconds (16 MB of 1 MB frames was 5–10 s under load). A
                        // frame's video goes whole or not at all. Audio is small and
                        // skipped only far behind.
                        let video: u64 = frame
                            .msgs
                            .iter()
                            .filter(|m| m.0)
                            .map(|m| m.1.len() as u64)
                            .sum();
                        let limit = (2 * video).clamp(512 << 10, 16 << 20);
                        let mut send_video = video == 0 || c.unacked() < limit;
                        if let Some(key) = frame.hx_key {
                            if key {
                                hx_need_key = false;
                            }
                            send_video &= !hx_need_key;
                            hx_need_key |= !send_video;
                        }
                        if video > 0 && !send_video {
                            client.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        for (is_video, m) in &frame.msgs {
                            if *is_video && !send_video {
                                continue;
                            }
                            if *is_video || c.unacked() < 16 << 20 {
                                c.send_message(if *is_video { v } else { a }, m);
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if let Some(r) = c.closed() {
                    *failed.lock_ok() = Some(r);
                    return;
                }
            }
            c.close();
        });
    }
    Ok(Path::Quic { tx, failed, stop })
}

/// UDP unicast (§6.3): probes the receiver's ports, then sends from a fresh socket.
pub(in crate::sender) fn open_udp(
    ip: IpAddr,
    (vp, vid): (u16, u32),
    (ap, aid): (u16, u32),
) -> io::Result<Path> {
    // Probe each port twice from one socket (§6.3.1), then send media from another.
    let probe = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    probe.set_read_timeout(Some(Duration::from_millis(300)))?;
    for (port, id) in [(vp, vid), (ap, aid)] {
        for _ in 0..2 {
            probe.send_to(&udp::probe(id), SocketAddr::new(ip, port))?;
            let mut b = [0u8; 16];
            let _ = probe.recv_from(&mut b);
        }
    }
    let sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    let _ = socket2::SockRef::from(&sock).set_send_buffer_size(8 << 20);
    Ok(Path::Udp {
        sock,
        video: (SocketAddr::new(ip, vp), vid, 0),
        audio: (SocketAddr::new(ip, ap), aid, 0),
    })
}

/// Opens the two connections of one multi-TCP media type (§6.2.1): connect to the
/// receiver from a local port this side also listens on, then accept the receiver's
/// connection back to it.
pub(in crate::sender) fn tcp_pair(ip: IpAddr, port: u16) -> io::Result<[TcpStream; 2]> {
    use idn_common::sync::LockOk as _;
    use socket2::{Domain, Socket, Type};
    // Linux: SO_REUSEADDR alone, the connecting socket bound before the listener listens
    // (the two share the port; no other program's listener can then, which SO_REUSEPORT
    // allowed between programs: a receiver's connection back could reach another
    // program's pair; bug round F, F1-5). Elsewhere SO_REUSEPORT as before (the BSDs need
    // it for a second socket on the port).
    let mk = || -> io::Result<Socket> {
        let s = Socket::new(Domain::IPV4, Type::STREAM, None)?;
        s.set_reuse_address(true)?;
        #[cfg(all(unix, not(target_os = "linux")))]
        s.set_reuse_port(true)?;
        Ok(s)
    };
    let l = mk()?;
    // The receiver connects back to this port: one of IDN's media range (a firewall rule
    // names it). Checked free with a plain socket first: with SO_REUSEPORT two of ours
    // could share a port and take each other's connections back. In this process, the
    // check, the bind and the listen take turns, and a port is kept from other pairs while
    // its listener waits (two pairs at once passed the check for the same port, and one
    // took the other's connection back: another session's pictures; F5-3).
    let any = std::net::Ipv4Addr::UNSPECIFIED;
    static PICKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static WAITING: std::sync::Mutex<std::collections::BTreeSet<u16>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    struct Waiting(u16);
    impl Drop for Waiting {
        fn drop(&mut self) {
            WAITING.lock_ok().remove(&self.0);
        }
    }
    let local = {
        let _turn = PICKING.lock_ok();
        let free = idn_common::ports::media_ports()
            .find(|p| {
                !WAITING.lock_ok().contains(p) && std::net::TcpListener::bind((any, *p)).is_ok()
            })
            .unwrap_or(0);
        if l.bind(&SocketAddr::new(any.into(), free).into()).is_err() {
            l.bind(&SocketAddr::new(any.into(), 0).into())?;
        }
        let local = l.local_addr()?.as_socket().unwrap();
        let c = mk()?;
        if cfg!(target_os = "linux") {
            c.bind(&local.into())?;
            l.listen(4)?;
        } else {
            l.listen(4)?;
            c.bind(&local.into())?;
        }
        (local, c)
    };
    let (local, c) = local;
    WAITING.lock_ok().insert(local.port());
    let _waiting = Waiting(local.port());
    c.connect_timeout(&SocketAddr::new(ip, port).into(), Duration::from_secs(5))?;
    let a: TcpStream = c.into();
    let l: TcpListener = l.into();
    l.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let b = loop {
        match l.accept() {
            // Only the receiver's own connection back is its second media connection.
            Ok((b, from)) if from.ip() == ip => break b,
            Ok(_) if std::time::Instant::now() < deadline => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "multi-TCP: the receiver did not connect back",
                ));
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(e),
        }
    };
    b.set_nonblocking(false)?;
    for s in [&a, &b] {
        s.set_nodelay(true)?;
        s.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    }
    Ok([a, b])
}

/// Starts the shared multicast stream (§6.4): random groups in 239.255.0.0/16, TTL 1,
/// unscrambled frames; in encrypted mode each message signed and each packet sealed
/// (§E.17). Returns the frame input and what the stream offers receivers.
pub(in crate::sender) fn start_multicast(
    quality: u8,
    mc: &crate::mcast::McConfig,
    stop: Arc<AtomicBool>,
    demand: Arc<Mutex<(usize, usize)>>,
    cache: Arc<EncodeCache>,
) -> io::Result<(Arc<FrameQueue>, McOffer)> {
    let r = crate::random::secure_random::<18>();
    // A fixed port per kind (idn_common::ports, as Dante's and AES67's), each stream on a
    // random group address of its own: (proxy video, video, sound).
    let (proxy, video, audio) = idn_common::ports::multicast_ports();
    let kinds = [proxy, video, audio];
    let group = |i: usize| -> (SocketAddr, u32) {
        let ip = std::net::Ipv4Addr::new(239, 255, r[i].max(1), r[i + 1].max(1));
        let port = kinds[i / 6];
        (
            SocketAddr::new(ip.into(), port),
            u32::from_le_bytes([r[i + 2], r[i + 3], r[i + 4], r[i + 5]]),
        )
    };
    let (lq, hq, am) = (group(0), group(6), group(12));
    let group =
        (mc.mode == crate::mcast::Mode::Encrypted).then(|| crate::mcast::Group::new(mc.clone()));
    let xml = format!(
        "<ndi_multicast_v2{}><stream type=\"vmclq\" address=\"{}\" id=\"{}\"/><stream type=\"vmchq\" address=\"{}\" id=\"{}\"/><stream type=\"amc\" address=\"{}\" id=\"{}\"/></ndi_multicast_v2>",
        if group.is_some() {
            " x_idn_mc=\"1\""
        } else {
            ""
        },
        lq.0,
        lq.1,
        hq.0,
        hq.1,
        am.0,
        am.1
    );
    let offer = McOffer {
        xml,
        mode: mc.mode,
        group: group.clone(),
    };
    let sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_multicast_ttl_v4(1)?;
    let tx = Arc::new(FrameQueue::default());
    let rx = FrameRx(tx.clone());
    std::thread::spawn(move || {
        let mut seqs = [0u16; 3];
        while !stop.load(Ordering::Relaxed) {
            if let Some(g) = &group {
                g.tick();
            }
            let Some(f) = rx.0.recv_timeout(Duration::from_millis(200)) else {
                continue;
            };
            // Send each group only while some receiver on multicast needs it.
            let (high, low) = *demand.lock_ok();
            if high + low == 0 {
                continue;
            }
            let mut all: Vec<(usize, Vec<u8>)> = Vec::new();
            // Multicast frames are not scrambled: bit 15 of `type` is clear (§6.4.2).
            let audio_only = Wants {
                audio: true,
                new_format: true,
                plain: true,
                ..Default::default()
            };
            for (is_video, m) in media_frames_or_skip(
                &f,
                &Wants {
                    video: high > 0,
                    ..audio_only.clone()
                },
                quality,
                &cache,
                None,
            ) {
                all.push((if is_video { 0 } else { 2 }, m));
            }
            if low > 0 {
                for (_, m) in media_frames_or_skip(
                    &f,
                    &Wants {
                        video: true,
                        new_format: true,
                        low: true,
                        plain: true,
                        ..Default::default()
                    },
                    quality,
                    &cache,
                    None,
                ) {
                    all.push((1, m));
                }
            }
            for (i, m) in all {
                let (to, id) = [hq, lq, am][i];
                match &group {
                    None => {
                        for p in udp::packetize(&m, id, &mut seqs[i]) {
                            let _ = sock.send_to(&p, to);
                        }
                    }
                    // Signed, packetised a little smaller, then sealed (§E.17.3, §E.17.4).
                    Some(g) => {
                        let stream = i as u8;
                        let m = g.sign_message(stream, &m);
                        if g.test_drop(&m) {
                            continue;
                        }
                        let size = udp::NDI_DATA_PACKET - crate::mcast::OVERHEAD;
                        for p in udp::packetize_sized(&m, id, &mut seqs[i], size) {
                            let _ = sock.send_to(&g.seal(stream, &p), to);
                        }
                    }
                }
            }
        }
    });
    Ok((tx, offer))
}

/// A routing source's session (§1.8): `ndi_routing` naming the target (kind 1, then kind
/// 3 on each change) and a small placeholder picture; the receiver keeps the connection
/// open and sends no media request on it. Ends when the receiver leaves or the output
/// stops.
pub(in crate::sender) fn serve_routing(
    w: &Arc<Mutex<TcpStream>>,
    sock: TcpStream,
    route: &Route,
    here: Option<IpAddr>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let gone = Arc::new(AtomicBool::new(false));
    {
        let gone = gone.clone();
        let mut r = sock;
        r.set_read_timeout(None)?;
        std::thread::Builder::new()
            .name("idn-route-rx".into())
            .spawn(move || {
                // What the receiver says here (its enabled streams, tally) needs no answer.
                while let Ok(Some(_)) = read_frame(&mut r, &CONTROL_LIMITS) {}
                gone.store(true, Ordering::Relaxed);
            })?;
    }
    let (mut seen, mut first) = (u64::MAX, true);
    while !stop.load(Ordering::Relaxed) && !gone.load(Ordering::Relaxed) {
        let (target, n) = route.wait(seen, Duration::from_millis(250));
        if n == seen {
            continue;
        }
        seen = n;
        let Some((name, mut addr)) = target else {
            continue;
        };
        // A target on the router's own machine is found at 127.0.0.1, which a receiver
        // elsewhere can't reach (an NDI receiver on another machine got nothing): the
        // address the receiver reached the router at instead.
        if (addr.ip().is_loopback() || addr.ip().is_unspecified())
            && let Some(ip) = here.filter(|ip| !ip.is_loopback())
        {
            addr.set_ip(ip);
        }
        let xml = format!(
            r#"<ndi_routing name="{}" ip="{addr}" version="embedded_v4"/>"#,
            control::escape_attr(&name)
        );
        send_text(w, if first { kind::CONTROL } else { kind::METADATA }, &xml)?;
        if std::mem::take(&mut first) {
            w.lock_ok().write_all(&routing_placeholder())?;
        }
    }
    Ok(())
}

/// The placeholder picture a routing source sends after its first `ndi_routing` (§1.8):
/// 16 × 8 black SpeedHQ 4:2:2.
pub(in crate::sender) fn routing_placeholder() -> Vec<u8> {
    let (w, h) = (16usize, 8usize);
    let planes = [
        Plane {
            width: w,
            height: h,
            data: vec![16; w * h],
        },
        Plane {
            width: w / 2,
            height: h,
            data: vec![128; w / 2 * h],
        },
        Plane {
            width: w / 2,
            height: h,
            data: vec![128; w / 2 * h],
        },
    ];
    let payload = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
            width: w,
            height: h,
            planes: &planes,
            precise: None,
        },
        &EncodeOptions {
            quality: 50,
            new_format: true,
            ..Default::default()
        },
    );
    let vh = VideoHeader {
        fourcc: fourcc_bytes(Fourcc::Shq2),
        width: w as u32,
        height: h as u32,
        frame_rate_n: 30000,
        frame_rate_d: 1001,
        aspect_ratio: w as f32 / h as f32,
        field_id: 0,
        stream: 1,
        timecode: crate::frame::TIMECODE_SYNTHESIZE,
        timestamp: 0,
        metadata: None,
    };
    Frame::encode_parts(kind::VIDEO, 0, true, &vh.encode(), &payload)
}
