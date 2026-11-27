//! Runs idn-recv against a scripted local sender that sends media on the control
//! connection (spec §1.6), using a spec test-vector payload.

use crate::common;
use common::idn;

use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
use idn_open_protocol::media::{AudioHeader, VideoHeader, fowt_encode};
use std::io::Write;
use std::net::TcpListener;

#[test]
fn receiver_decodes_inline_media() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = idn_common::testing::TempDir::new("idn-recv-loopback");
    let child = idn("recv")
        .args([
            &format!("127.0.0.1:{port}"),
            "--seconds",
            "3",
            "--transport",
            "inline",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut rx = s.try_clone().unwrap();
        let limits = Limits::default();
        let mut got = Vec::new();
        // Read the receiver's 5 handshake messages.
        for _ in 0..5 {
            let f = read_frame(&mut rx, &limits).unwrap().unwrap();
            assert_eq!(f.kind, kind::CONTROL);
            got.push(f.text_payload().unwrap().to_owned());
        }
        let ver = r#"<ndi_version text="3" video="7" audio="5" sdk="x" platform="LINUX"/>"#;
        s.write_all(&Frame::text(kind::CONTROL, 0, ver).encode())
            .unwrap();
        s.write_all(&Frame::text(kind::CONTROL, 0, r#"<ndi_identify name="T (S)"/>"#).encode())
            .unwrap();
        let payload = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.payload.bin"
        ))
        .unwrap();
        let vh = VideoHeader {
            fourcc: *b"SHQ2",
            width: 64,
            height: 16,
            frame_rate_n: 30000,
            frame_rate_d: 1001,
            aspect_ratio: 4.0,
            field_id: 0,
            stream: 1,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        let ah = AudioHeader {
            fourcc: AudioHeader::FOWT,
            samples: 4,
            channels: 2,
            sample_rate: 48000,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        let audio = fowt_encode(&[vec![0.1, -0.2, 0.3, 0.0], vec![0.0; 4]]);
        for _ in 0..3 {
            let v = Frame {
                kind: kind::VIDEO,
                scrambled: true,
                version: 0,
                ext: vh.encode(),
                payload: payload.clone(),
            };
            s.write_all(&v.encode()).unwrap();
            let a = Frame {
                kind: kind::AUDIO,
                scrambled: true,
                version: 1,
                ext: ah.encode(),
                payload: audio.clone(),
            };
            s.write_all(&a.encode()).unwrap();
        }
        // Keep the connection open until the receiver's time is up.
        let _ = read_frame(&mut rx, &limits);
        got
    });
    let got = server.join().unwrap();
    let child = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&child.stdout);
    println!("{stdout}");
    assert!(child.status.success());
    assert!(got[0].starts_with("<ndi_version"));
    assert!(got[3].starts_with("<ndi_enabled_streams") && !got[3].contains("_port_"));
    assert!(stdout.contains("video messages decoded: 3"), "{stdout}");
    assert!(stdout.contains("audio messages decoded: 3"), "{stdout}");
    let y4m = std::fs::read(out.join("video-0.y4m")).unwrap();
    let want = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.planes.yuv"
    ))
    .unwrap();
    let body = &y4m[y4m.iter().position(|&b| b == b'\n').unwrap() + 1..];
    assert_eq!(&body[6..6 + want.len()], &want[..]);
}

/// The default transport: the receiver advertises QUIC, and the scripted sender connects
/// to it and sends media there (chapter 2).
#[test]
fn receiver_decodes_quic_media() {
    use idn_open_protocol::control::parse_element;
    use idn_open_protocol::quic::client::Client;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = idn_common::testing::TempDir::new("idn-recv-quic");
    let child = idn("recv")
        .args([
            &format!("127.0.0.1:{port}"),
            "--seconds",
            "3",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut s, _) = listener.accept().unwrap();
    let mut rx = s.try_clone().unwrap();
    let limits = Limits::default();
    let mut enabled = None;
    for _ in 0..5 {
        let f = read_frame(&mut rx, &limits).unwrap().unwrap();
        let e = parse_element(f.text_payload().unwrap()).unwrap();
        if e.name == "ndi_enabled_streams" {
            enabled = Some(e);
        }
    }
    let e = enabled.expect("ndi_enabled_streams sent");
    assert!(e.attr("video_tcp_port_v1").is_none() && e.attr("video_multicast_v2").is_none());
    let qport: u16 = e.attr("video_quic_port_v1").unwrap().parse().unwrap();
    let vid: u32 = e.attr("video_quic_id_v1").unwrap().parse().unwrap();
    let aid: u32 = e.attr("audio_quic_id_v1").unwrap().parse().unwrap();
    s.write_all(
        &Frame::text(
            kind::CONTROL,
            0,
            r#"<ndi_version text="3" video="7" audio="5"/>"#,
        )
        .encode(),
    )
    .unwrap();

    let mut c = Client::connect(
        format!("127.0.0.1:{qport}").parse().unwrap(),
        std::time::Duration::from_secs(3),
    )
    .unwrap();
    let v = c.open_media_stream(0, vid);
    let a = c.open_media_stream(1, aid);
    let payload = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.payload.bin"
    ))
    .unwrap();
    let vh = VideoHeader {
        fourcc: *b"SHQ2",
        width: 64,
        height: 16,
        frame_rate_n: 25,
        frame_rate_d: 1,
        aspect_ratio: 4.0,
        field_id: 0,
        stream: 1,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    let ah = AudioHeader {
        fourcc: AudioHeader::FOWT,
        samples: 4,
        channels: 1,
        sample_rate: 48000,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    for _ in 0..5 {
        c.send_message(
            v,
            &Frame {
                kind: kind::VIDEO,
                scrambled: true,
                version: 0,
                ext: vh.encode(),
                payload: payload.clone(),
            }
            .encode(),
        );
        c.send_message(
            a,
            &Frame {
                kind: kind::AUDIO,
                scrambled: true,
                version: 1,
                ext: ah.encode(),
                payload: fowt_encode(&[vec![0.5, 0.0, -0.5, 0.25]]),
            }
            .encode(),
        );
    }
    let t = std::time::Instant::now();
    while c.unacked() > 0 && t.elapsed() < std::time::Duration::from_secs(3) {
        c.poll_once().unwrap();
    }
    let child = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&child.stdout);
    println!("{stdout}");
    assert!(child.status.success());
    assert!(stdout.contains("video messages decoded: 5"), "{stdout}");
    assert!(
        stdout.contains("5 x video via quic") && stdout.contains("5 x audio via quic"),
        "{stdout}"
    );
    let y4m = std::fs::read(out.join("video-0.y4m")).unwrap();
    let want = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.planes.yuv"
    ))
    .unwrap();
    let body = &y4m[y4m.iter().position(|&b| b == b'\n').unwrap() + 1..];
    assert_eq!(&body[6..6 + want.len()], &want[..]);
}

/// UDP unicast for video and multi-TCP for audio (chapter 6), each the way §6.2/§6.3
/// describe a sender doing it.
#[test]
fn receiver_decodes_udp_and_multitcp_media() {
    use idn_open_protocol::control::parse_element;
    use idn_open_protocol::transport::{tcp, udp};
    use std::net::{TcpStream, UdpSocket};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = idn_common::testing::TempDir::new("idn-recv-ch6");
    let child = idn("recv")
        .args([
            &format!("127.0.0.1:{port}"),
            "--seconds",
            // (Long enough for a slow start under the whole suite: the probes below
            // wait for it; a fast one ends when the media is checked.)
            "8",
            "--transport",
            "udp,tcp",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut s, _) = listener.accept().unwrap();
    let mut rx = s.try_clone().unwrap();
    let mut enabled = None;
    for _ in 0..5 {
        let f = read_frame(&mut rx, &Limits::default()).unwrap().unwrap();
        let e = parse_element(f.text_payload().unwrap()).unwrap();
        if e.name == "ndi_enabled_streams" {
            enabled = Some(e);
        }
    }
    let e = enabled.unwrap();
    assert!(e.attr("video_quic_port_v1").is_none());
    let num = |k: &str| -> u64 { e.attr(k).unwrap().parse().unwrap() };
    s.write_all(
        &Frame::text(
            kind::CONTROL,
            0,
            r#"<ndi_version text="3" video="7" audio="5"/>"#,
        )
        .encode(),
    )
    .unwrap();

    // Video over UDP unicast: probe twice from one port, then media from another port.
    let vdest = format!("127.0.0.1:{}", num("video_unicast_port_v2"));
    let vid = num("video_unicast_id_v2") as u32;
    let probe_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    probe_sock
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    // (A probe answered within a second, asked again up to ten times: the whole suite at
    // once on one machine sometimes kept the receiver from answering five in time.)
    for pass in 0..2 {
        // Late answers to the first pass's probes let go, so the second pass is answered
        // by a probe of its own.
        if pass > 0 {
            let mut b = [0u8; 16];
            while probe_sock.recv_from(&mut b).is_ok() {}
        }
        let mut answered = false;
        for _ in 0..10 {
            probe_sock.send_to(&udp::probe(vid), &vdest).unwrap();
            let mut b = [0u8; 16];
            if let Ok((n, _)) = probe_sock.recv_from(&mut b) {
                assert_eq!(&b[..n], &udp::probe_reply(&udp::probe(vid)));
                answered = true;
                break;
            }
        }
        assert!(answered, "the receiver didn't answer the probe");
    }
    let media_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let payload = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.payload.bin"
    ))
    .unwrap();
    let vh = VideoHeader {
        fourcc: *b"SHQ2",
        width: 64,
        height: 16,
        frame_rate_n: 25,
        frame_rate_d: 1,
        aspect_ratio: 4.0,
        field_id: 0,
        stream: 1,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    let vf = Frame {
        kind: kind::VIDEO,
        scrambled: true,
        version: 0,
        ext: vh.encode(),
        payload,
    }
    .encode();
    let mut seq = 0u16;
    for _ in 0..4 {
        for p in udp::packetize(&vf, vid, &mut seq) {
            media_sock.send_to(&p, &vdest).unwrap();
        }
    }

    // Audio over multi-TCP: connect from a port we also listen on; accept the back-connection.
    let back = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    back.set_reuse_address(true).unwrap();
    // (Unix: SO_REUSEPORT too; Windows has only SO_REUSEADDR, which allows it.)
    #[cfg(unix)]
    back.set_reuse_port(true).unwrap();
    back.bind(
        &"127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
    )
    .unwrap();
    back.listen(4).unwrap();
    let back: TcpListener = back.into();
    let local = back.local_addr().unwrap();
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    sock.set_reuse_address(true).unwrap();
    #[cfg(unix)]
    sock.set_reuse_port(true).unwrap();
    sock.bind(&local.into()).unwrap();
    let adest: std::net::SocketAddr = format!("127.0.0.1:{}", num("audio_tcp_port_v1"))
        .parse()
        .unwrap();
    sock.connect(&adest.into()).unwrap();
    let mut a1: TcpStream = sock.into();
    let (mut a2, _) = back.accept().unwrap();
    let mut tick = [0u8; 6];
    use std::io::Read;
    a1.read_exact(&mut tick).unwrap();
    assert_eq!(tick, tcp::TICK);
    // No tick on the receiver's own connection (06-media-transports.md §6.5).
    a2.set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();
    assert!(a2.read(&mut tick).is_err());
    let ah = AudioHeader {
        fourcc: AudioHeader::FOWT,
        samples: 4000,
        channels: 2,
        sample_rate: 48000,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    let af = Frame {
        kind: kind::AUDIO,
        scrambled: true,
        version: 1,
        ext: ah.encode(),
        payload: fowt_encode(&[vec![0.25; 4000], vec![-0.5; 4000]]),
    }
    .encode();
    let mut tseq = 0u32;
    for _ in 0..3 {
        for (i, c) in tcp::chunkify(&af, &mut tseq).into_iter().enumerate() {
            if i % 2 == 0 {
                a1.write_all(&c).unwrap()
            } else {
                a2.write_all(&c).unwrap()
            }
        }
        a1.write_all(&tcp::TICK).unwrap();
        a2.write_all(&tcp::TICK).unwrap();
    }

    let child = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&child.stdout);
    println!("{stdout}");
    assert!(child.status.success());
    assert!(stdout.contains("4 x video via udp"), "{stdout}");
    assert!(stdout.contains("3 x audio via tcp"), "{stdout}");
    let y4m = std::fs::read(out.join("video-0.y4m")).unwrap();
    let want = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.planes.yuv"
    ))
    .unwrap();
    let body = &y4m[y4m.iter().position(|&b| b == b'\n').unwrap() + 1..];
    assert_eq!(&body[6..6 + want.len()], &want[..]);
}

/// Routing source (§1.8): A routes to B, then (kind 3) to C.
#[test]
fn receiver_follows_routing_source() {
    use idn_open_protocol::control::parse_element;
    use std::sync::mpsc;
    let payload = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.payload.bin"
    ))
    .unwrap();
    let vh = VideoHeader {
        fourcc: *b"SHQ2",
        width: 64,
        height: 16,
        frame_rate_n: 25,
        frame_rate_d: 1,
        aspect_ratio: 4.0,
        field_id: 0,
        stream: 1,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    let video = Frame {
        kind: kind::VIDEO,
        scrambled: true,
        version: 0,
        ext: vh.encode(),
        payload,
    }
    .encode();

    // Targets B and C: each serves `n` video frames, then reports whether it was closed.
    let target = |n: usize| {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let video = video.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = s.try_clone().unwrap();
            for _ in 0..5 {
                read_frame(&mut r, &Limits::default()).unwrap().unwrap();
            }
            for _ in 0..n {
                s.write_all(&video).unwrap();
            }
            // Read until the receiver closes us (or the test ends).
            r.set_read_timeout(Some(std::time::Duration::from_secs(6)))
                .unwrap();
            let closed = matches!(read_frame(&mut r, &Limits::default()), Ok(None) | Err(_));
            let _ = tx.send(closed);
        });
        (port, rx)
    };
    let (pb, closed_b) = target(2);
    let (pc, _closed_c) = target(3);

    let la = TcpListener::bind("127.0.0.1:0").unwrap();
    let pa = la.local_addr().unwrap().port();
    let out = idn_common::testing::TempDir::new("idn-route");
    let child = idn("recv")
        .args([
            &format!("127.0.0.1:{pa}"),
            "--seconds",
            "4",
            "--transport",
            "inline",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut a, _) = la.accept().unwrap();
    let mut ar = a.try_clone().unwrap();
    for _ in 0..5 {
        read_frame(&mut ar, &Limits::default()).unwrap().unwrap();
    }
    a.write_all(
        &Frame::text(
            kind::CONTROL,
            0,
            &format!(r#"<ndi_routing name="SND (B)" ip="127.0.0.1:{pb}" version="embedded_v4"/>"#),
        )
        .encode(),
    )
    .unwrap();
    a.write_all(&video).unwrap(); // placeholder picture
    // The receiver turns all streams off on the routing connection (kind 3).
    let off = read_frame(&mut ar, &Limits::default()).unwrap().unwrap();
    assert_eq!(off.kind, kind::METADATA);
    let e = parse_element(off.text_payload().unwrap()).unwrap();
    assert_eq!(
        (e.name.as_str(), e.attr("video"), e.attr("audio")),
        ("ndi_enabled_streams", Some("false"), Some("false"))
    );
    std::thread::sleep(std::time::Duration::from_millis(700));
    a.write_all(
        &Frame::text(
            kind::METADATA,
            0,
            &format!(r#"<ndi_routing name="SND (C)" ip="127.0.0.1:{pc}" version="embedded_v4"/>"#),
        )
        .encode(),
    )
    .unwrap();
    assert!(
        closed_b
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        "old target closed"
    );

    let child = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&child.stdout);
    println!("{stdout}");
    assert!(child.status.success());
    assert!(
        stdout.contains("routed to \"SND (B)\"") && stdout.contains("routed to \"SND (C)\""),
        "{stdout}"
    );
    // 2 frames from B + 3 from C; A's placeholder is not counted.
    assert!(stdout.contains("video messages decoded: 5"), "{stdout}");
}
