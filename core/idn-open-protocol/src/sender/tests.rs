use super::{
    Adapt, EncodeCache, FrameQueue, MediaFrame, Plane, RateControl, Wants, sdk_at_least,
    target_size,
};
use idn_common::sync::LockOk as _;
use std::sync::Arc;

#[test]
fn rate_control_converges_and_respects_bounds() {
    // A toy encoder whose size halves every 8 Q steps down from Q92.
    let size = |q: f64| (400_000.0 * 2f64.powf((q - 92.0) / 8.0)) as usize;
    let mut rc = RateControl { q: 92.0, max: 92.0 };
    for _ in 0..40 {
        let b = size(rc.q);
        rc.update(b, 100_000.0);
    }
    let s = size(rc.q) as f64;
    assert!(
        (s / 100_000.0 - 1.0).abs() < 0.15,
        "settled at {s} bytes, Q {}",
        rc.q
    );
    // Generous budget: back up to the configured maximum, never beyond.
    for _ in 0..40 {
        rc.update(size(rc.q), 10_000_000.0);
    }
    assert_eq!(rc.quality(), 92);
    // Impossible budget: stops at the floor.
    for _ in 0..80 {
        rc.update(size(rc.q), 10.0);
    }
    assert_eq!(rc.quality(), 50);
}

#[test]
fn out_of_range_transport_ports_are_not_offers() {
    let w = |xml: &str| super::parse_wants(&crate::control::parse_element(xml).unwrap(), false);
    let ok = w(
        r#"<ndi_enabled_streams video_quic_port_v1="5990" video_quic_id_v1="7" audio_quic_id_v1="8"/>"#,
    );
    assert_eq!(ok.quic, Some((5990, 7, 8)));
    // 70000 would truncate to port 4464.
    let big = w(
        r#"<ndi_enabled_streams video_quic_port_v1="70000" video_quic_id_v1="7" audio_quic_id_v1="8" video_tcp_port_v1="0" audio_tcp_port_v1="5"/>"#,
    );
    assert_eq!((big.quic, big.tcp), (None, None));
}

#[test]
fn multi_tcp_pairing_takes_only_the_receivers_connection() {
    use std::net::{SocketAddr, TcpListener, TcpStream};
    let recv = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = recv.local_addr().unwrap().port();
    let fake_receiver = std::thread::spawn(move || {
        let (a, from) = recv.accept().unwrap();
        // Another host gets in first; then the receiver connects back from its address.
        let other =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        other
            .bind(&SocketAddr::from(([127, 0, 0, 2], 0)).into())
            .unwrap();
        other.connect(&from.into()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let back = TcpStream::connect(from).unwrap();
        (a, other, back)
    });
    let [_a, b] = super::tcp_pair("127.0.0.1".parse().unwrap(), port).unwrap();
    let (_a2, _other, back) = fake_receiver.join().unwrap();
    assert_eq!(b.peer_addr().unwrap(), back.local_addr().unwrap());
}

#[test]
fn receivers_cannot_fill_the_senders_memory_with_metadata() {
    use crate::frame::{Frame, kind};
    use std::io::{Read, Write};
    let sender = super::Sender::start(
        0,
        super::SenderConfig {
            name: "HOST (META)".into(),
            quality: 90,
            capabilities: Vec::new(),
            multicast: false,
            hx: false,
            target_mbps: None,
            color: Default::default(),
            secure: None,
            link: None,
            frame_streams: false,
            adapt_floor: None,
            regions: false,
            mc: Default::default(),
            codec: None,
            route: None,
        },
    )
    .unwrap();
    let mut c = std::net::TcpStream::connect(("127.0.0.1", sender.port())).unwrap();
    let send = |c: &mut std::net::TcpStream, k: u16, xml: &str| {
        c.write_all(&Frame::text(k, 0, xml).encode())
    };
    send(
        &mut c,
        kind::CONTROL,
        r#"<ndi_version text="3" video="7" audio="5"/>"#,
    )
    .unwrap();
    // Ten 900 kB messages: only the newest that fit in the byte budget are kept.
    let big = format!("<app_data>{}</app_data>", "x".repeat(900_000));
    for _ in 0..10 {
        send(&mut c, kind::METADATA, &big).unwrap();
    }
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let kept = loop {
        let m = sender.metadata.lock_ok();
        let bytes: usize = m.iter().map(|(n, x, _)| n.len() + x.len()).sum();
        if (bytes > 0 && m.len() == 4) || std::time::Instant::now() > end {
            break (m.len(), bytes);
        }
        drop(m);
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(kept.1 <= super::Sender::METADATA_BYTES_KEPT, "{kept:?}");
    assert_eq!(kept.0, 4, "{kept:?}");
    // A message over the control limit ends the session.
    let huge = format!("<app_data>{}</app_data>", "x".repeat(2 << 20));
    let _ = send(&mut c, kind::METADATA, &huge);
    c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 4096];
    let closed = loop {
        match c.read(&mut buf) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(e) => break e.kind() == std::io::ErrorKind::ConnectionReset,
        }
    };
    assert!(closed, "the sender closed the connection");
}

#[test]
fn silent_and_excess_connections_are_dropped() {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let sender = super::Sender::start(
        0,
        super::SenderConfig {
            name: "HOST (CAP)".into(),
            quality: 90,
            capabilities: Vec::new(),
            multicast: false,
            hx: false,
            target_mbps: None,
            color: Default::default(),
            secure: None,
            link: None,
            frame_streams: false,
            adapt_floor: None,
            regions: false,
            mc: Default::default(),
            codec: None,
            route: None,
        },
    )
    .unwrap();
    // Reads until the sender closes; returns the bytes it sent first.
    let until_closed = |c: &mut std::net::TcpStream, wait: Duration| -> Option<usize> {
        c.set_read_timeout(Some(wait)).unwrap();
        let mut got = 0;
        let mut b = [0u8; 4096];
        loop {
            match c.read(&mut b) {
                Ok(0) => return Some(got),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Some(got),
                Err(_) => return None, // still open
            }
        }
    };
    let t = Instant::now();
    // (From this machine itself: the total is the limit, not one address's 16.)
    let mut silent: Vec<_> = (0..super::MAX_SESSIONS)
        .map(|_| std::net::TcpStream::connect(("127.0.0.1", sender.port())).unwrap())
        .collect();
    // One address is held to 16; this machine's own programs only to the total.
    assert!(super::session_refused(16, 16, false).is_some());
    assert!(super::session_refused(16, 16, true).is_none());
    assert!(super::session_refused(64, 0, true).is_some());
    assert!(super::session_refused(63, 15, false).is_none());
    std::thread::sleep(Duration::from_millis(300));
    let mut over = std::net::TcpStream::connect(("127.0.0.1", sender.port())).unwrap();
    assert_eq!(
        until_closed(&mut over, Duration::from_secs(3)),
        Some(0),
        "refused at once"
    );
    // The silent ones are given up after FIRST_MESSAGE.
    for c in &mut silent {
        assert!(until_closed(c, Duration::from_secs(15)).is_some());
    }
    let took = t.elapsed();
    assert!(
        took >= super::FIRST_MESSAGE && took < super::FIRST_MESSAGE + Duration::from_secs(4),
        "{took:?}"
    );
}

/// The multicast stream's queue, like a receiver's: audio sent on its own is kept
/// while a slow encode drops old pictures (it used to be a 2-slot channel that lost
/// audio blocks, audible as gaps).
#[test]
fn a_busy_encoder_drops_pictures_not_audio_blocks() {
    let frame = |picture: bool, tc: i64| MediaFrame {
        captions: Vec::new(),
        fourcc: crate::shq::Fourcc::Shq2,
        width: if picture { 16 } else { 0 },
        height: if picture { 16 } else { 0 },
        planes: Arc::new(if picture {
            vec![Plane {
                width: 16,
                height: 16,
                data: vec![0; 256],
            }]
        } else {
            Vec::new()
        }),
        frame_rate: (30, 1),
        audio: if picture {
            Vec::new()
        } else {
            vec![vec![0.0; 480]]
        },
        sample_rate: 48_000,
        timecode: tc,
        timestamp: tc,
        metadata: None,
        interlaced: false,
        hx_video: None,
        hx_audio: None,
        content_id: None,
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    };
    let q = FrameQueue::default();
    let mut dropped = 0;
    for i in 0..10 {
        dropped += q.push(Arc::new(frame(true, i * 10))).unwrap();
        for k in 1..4 {
            dropped += q.push(Arc::new(frame(false, i * 10 + k))).unwrap();
        }
    }
    let mut audio = 0;
    while let Some(f) = q.recv_timeout(std::time::Duration::ZERO) {
        audio += usize::from(f.planes.is_empty());
    }
    assert_eq!(audio, 30, "every audio block is still there");
    assert_eq!(dropped, 8, "all but the two newest pictures");
}

#[test]
fn a_receiver_far_behind_on_hx_restarts_on_a_keyframe() {
    let hx = |n: usize, key: bool| MediaFrame {
        captions: Vec::new(),
        fourcc: crate::shq::Fourcc::Shq2,
        width: 64,
        height: 36,
        planes: Arc::default(),
        frame_rate: (30, 1),
        audio: vec![vec![0.0; 1600]],
        sample_rate: 48_000,
        timecode: n as i64,
        timestamp: n as i64,
        metadata: None,
        interlaced: false,
        hx_video: Some((*b"H264", key, vec![n as u8])),
        hx_audio: None,
        content_id: None,
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    };
    let q = FrameQueue::default();
    let mut dropped = 0;
    // Nobody reads: the 65th frame finds the queue full.
    for n in 0..70 {
        dropped += q.push(Arc::new(hx(n, n % 30 == 0))).unwrap();
    }
    let mut got = Vec::new();
    while let Some(f) = q.recv_timeout(std::time::Duration::ZERO) {
        got.push((f.timecode, f.hx_video.as_ref().map(|h| h.1)));
    }
    // The HX pictures left start on the newest queued keyframe (60) and run on; the
    // audio of the dropped ones stays (up to the queue's size).
    let pictures: Vec<_> = got.iter().filter(|g| g.1.is_some()).collect();
    assert_eq!(
        pictures.first().map(|p| (p.0, p.1)),
        Some((60, Some(true))),
        "{pictures:?}"
    );
    assert_eq!(pictures.len(), 10);
    assert!(pictures.windows(2).all(|w| w[1].0 == w[0].0 + 1));
    assert_eq!(got.len(), FrameQueue::MAX);
    assert_eq!(dropped, 60 + 6);
}

/// Far behind on HX with the newest keyframe at the very front of the full queue: no
/// picture goes out without its keyframe.
#[test]
fn a_full_hx_queue_never_loses_its_front_keyframe() {
    let hx = |n: usize, key: bool| MediaFrame {
        captions: Vec::new(),
        fourcc: crate::shq::Fourcc::Shq2,
        width: 64,
        height: 36,
        planes: Arc::default(),
        frame_rate: (30, 1),
        audio: vec![vec![0.0; 1600]],
        sample_rate: 48_000,
        timecode: n as i64,
        timestamp: n as i64,
        metadata: None,
        interlaced: false,
        hx_video: Some((*b"H264", key, vec![n as u8])),
        hx_audio: None,
        content_id: None,
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    };
    let q = FrameQueue::default();
    let drain = |q: &FrameQueue| -> Vec<(i64, bool)> {
        std::iter::from_fn(|| q.recv_timeout(std::time::Duration::ZERO))
            .filter_map(|f| f.hx_video.as_ref().map(|h| (f.timecode, h.1)))
            .collect()
    };
    // A keyframe, then deltas: the 65th frame finds the keyframe at the full queue's
    // front, and the receiver catches up right then.
    for n in 0..65 {
        q.push(Arc::new(hx(n, n == 0))).unwrap();
    }
    let caught_up = drain(&q);
    assert!(
        caught_up.first().is_none_or(|p| p.1),
        "pictures without their keyframe: {:?}",
        &caught_up[..caught_up.len().min(3)]
    );
    // Then it restarts on the next keyframe.
    for n in 65..90 {
        q.push(Arc::new(hx(n, n == 80))).unwrap();
    }
    let pictures = drain(&q);
    assert_eq!(pictures.first(), Some(&(80, true)), "{pictures:?}");
    assert!(pictures.windows(2).all(|w| w[1].0 == w[0].0 + 1));
}

#[test]
fn receivers_of_one_picture_share_one_encode() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = Arc::new(EncodeCache::default());
    let encodes = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (cache, encodes, start) = (cache.clone(), encodes.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                cache.get_or_encode(Some((7, true, true, (1920, 1080), 92, 0, None)), || {
                    encodes.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    vec![1, 2, 3]
                })
            })
        })
        .collect();
    for t in threads {
        assert_eq!(*t.join().unwrap(), [1, 2, 3]);
    }
    assert_eq!(encodes.load(Ordering::Relaxed), 1);
}

#[test]
fn adaptation_steps_down_to_the_floor_and_back_up() {
    let mut a = Adapt::new(92, 50);
    // Trouble every second: down 4 a time, never below 50.
    for n in 1..=20u64 {
        a.adjust(n, 0, 0);
    }
    assert_eq!(a.quality(), 50);
    // Clean, but not for 3 s yet: no change.
    a.adjust(20, 0, 0);
    assert_eq!(a.quality(), 50);
    // Clean for 3 s: up one step per look, up to 92.
    a.clean_since -= std::time::Duration::from_secs(3);
    for _ in 0..60 {
        a.adjust(20, 0, 0);
    }
    assert_eq!(a.quality(), 92);
    // Losses at the receiver count as trouble too.
    a.adjust(20, 1, 0);
    assert_eq!(a.quality(), 88);
    // A floor above the output's quality is clamped to it.
    assert_eq!(Adapt::new(40, 50).quality(), 40);
}

#[test]
fn hevc_goes_to_receivers_of_sdk_4_2_and_later() {
    assert!(sdk_at_least("4.2.0.0", [4, 2]));
    assert!(sdk_at_least("6.3.2.0", [4, 2]));
    assert!(sdk_at_least("5.0", [4, 2]));
    assert!(!sdk_at_least("4.1.3.0", [4, 2]));
    assert!(!sdk_at_least("3.8.0.0", [4, 2]));
    assert!(!sdk_at_least("", [4, 2]));
}

#[test]
fn requested_sizes_fit_inside_keep_the_aspect_or_take_an_exact_fraction() {
    let f = |w: usize, h: usize| MediaFrame {
        captions: Vec::new(),
        fourcc: crate::shq::Fourcc::Shq2,
        width: w,
        height: h,
        planes: Arc::default(),
        frame_rate: (30, 1),
        audio: Vec::new(),
        sample_rate: 48_000,
        timecode: 0,
        timestamp: 0,
        metadata: None,
        interlaced: false,
        hx_video: None,
        hx_audio: None,
        content_id: None,
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    };
    let want = |size: Option<(u32, u32)>, low: bool| Wants {
        size,
        low,
        ..Default::default()
    };
    assert_eq!(
        target_size(&f(1920, 1080), &want(Some((480, 270)), false)),
        (480, 270)
    );
    assert_eq!(
        target_size(&f(1920, 1080), &want(Some((480, 480)), false)),
        (480, 270)
    );
    assert_eq!(
        target_size(&f(1920, 1080), &want(Some((4000, 4000)), false)),
        (1920, 1080)
    );
    assert_eq!(target_size(&f(1920, 1080), &want(None, true)), (640, 360));
    assert_eq!(
        target_size(&f(1920, 1080), &want(None, false)),
        (1920, 1080)
    );
    // Rounded down to even sizes, still inside what was asked for.
    assert_eq!(
        target_size(&f(1000, 563), &want(Some((333, 999)), false)),
        (332, 186)
    );
    // A size just under an exact fraction gets the fraction (the scaler's fast path): a
    // switcher's multiview tile from 1080p and 2160p; one further off keeps its own.
    assert_eq!(
        target_size(&f(1920, 1080), &want(Some((476, 268)), false)),
        (480, 270)
    );
    assert_eq!(
        target_size(&f(3840, 2160), &want(Some((476, 268)), false)),
        (480, 270)
    );
    assert_eq!(
        target_size(&f(1920, 1080), &want(Some((450, 254)), false)),
        (450, 252)
    );
}

#[test]
fn odd_heights_go_out_even_and_huge_frame_metadata_is_left_off() {
    let (w, h) = (64usize, 33usize);
    let p = |pw: usize| Plane {
        width: pw,
        height: h,
        data: vec![100; pw * h],
    };
    let f = MediaFrame {
        captions: Vec::new(),
        fourcc: crate::shq::Fourcc::Shq2,
        width: w,
        height: h,
        planes: Arc::new(vec![p(w), p(w / 2), p(w / 2)]),
        frame_rate: (30, 1),
        audio: Vec::new(),
        sample_rate: 48_000,
        timecode: 0,
        timestamp: 0,
        metadata: Some(format!("<big>{}</big>", "x".repeat(5000))),
        interlaced: false,
        hx_video: None,
        hx_audio: None,
        content_id: None,
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    };
    let wants = Wants {
        video: true,
        new_format: true,
        ..Default::default()
    };
    let header = |f: &Arc<MediaFrame>| {
        let m = super::media_frames(f, &wants, 92, &EncodeCache::default(), None);
        let (fr, _) = crate::frame::Frame::decode(&m[0].1, &Default::default())
            .unwrap()
            .unwrap();
        crate::media::VideoHeader::parse(&fr.ext).unwrap()
    };
    let f = Arc::new(f);
    let vh = header(&f);
    assert_eq!((vh.width, vh.height), (64, 32), "§2.4: even height");
    assert_eq!(vh.metadata, None, "over 4 KB of header: left off");
    let mut f = Arc::try_unwrap(f).ok().expect("the only reference");
    f.metadata = Some("<small/>".into());
    assert_eq!(header(&Arc::new(f)).metadata.as_deref(), Some("<small/>"));
}
