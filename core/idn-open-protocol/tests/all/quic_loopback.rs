//! QUIC client and server against each other over UDP loopback (spec chapters 2 and 4):
//! handshake, two media streams, flow control over several MB, message integrity.

use idn_open_protocol::frame::{Frame, Limits, kind};
use idn_open_protocol::quic::client::Client;
use idn_open_protocol::quic::server::{Event, MediaKind, Server};
use std::time::{Duration, Instant};

use crate::common;

#[test]
fn media_over_quic_loopback() {
    let (vid, aid) = (0xDEAD_BEEF_u32, 12345u32);
    let mut server =
        Server::bind("127.0.0.1:0".parse().unwrap(), vid, aid, Limits::default()).unwrap();
    let addr = server.local_addr().unwrap();

    let sender = std::thread::spawn(move || {
        let mut c = Client::connect(addr, Duration::from_secs(5)).unwrap();
        let v = c.open_media_stream(0, vid);
        let a = c.open_media_stream(1, aid);
        // 40 video messages of 300 kB (12 MB total, far beyond the 64 kB stream window)
        // and 40 small audio messages.
        for i in 0..40u32 {
            let payload: Vec<u8> = (0..300_000u32)
                .map(|j| (i.wrapping_mul(7) ^ j) as u8)
                .collect();
            let vf = Frame {
                kind: kind::VIDEO,
                scrambled: true,
                version: 0,
                ext: vec![i as u8; 57],
                payload,
            };
            c.send_message(v, &vf.encode());
            let af = Frame {
                kind: kind::AUDIO,
                scrambled: true,
                version: 1,
                ext: vec![i as u8; 33],
                payload: vec![i as u8; 100],
            };
            c.send_message(a, &af.encode());
        }
        let start = Instant::now();
        while c.unacked() > 0 && start.elapsed() < Duration::from_secs(20) {
            c.poll_once().unwrap();
        }
        assert_eq!(c.unacked(), 0, "all data acknowledged");
        c.close();
    });

    let (mut video, mut audio, mut connected, mut closed) = (0u32, 0u32, false, false);
    let start = Instant::now();
    while !closed && start.elapsed() < Duration::from_secs(30) {
        for e in server.poll() {
            match e {
                Event::Connected(_) => connected = true,
                Event::Message(MediaKind::Video, f) => {
                    assert_eq!(f.kind, kind::VIDEO);
                    assert_eq!(f.ext, vec![video as u8; 57]);
                    assert_eq!(f.payload.len(), 300_000);
                    assert_eq!(f.payload[1234], (video.wrapping_mul(7) ^ 1234) as u8);
                    video += 1;
                }
                Event::Message(MediaKind::Audio, f) => {
                    assert_eq!(f.ext, vec![audio as u8; 33]);
                    audio += 1;
                }
                Event::Message(k, _) | Event::StreamError(k, _) => panic!("unexpected {k:?}"),
                Event::Closed(_, _) => closed = true,
            }
        }
    }
    sender.join().unwrap();
    assert!(connected);
    assert_eq!((video, audio), (40, 40));
    assert!(closed);
}

/// Same transfer through a proxy that drops 5% of datagrams each way (deterministic), so
/// handshake retransmission, stream retransmission and credit re-sending are exercised.
#[test]
fn media_over_quic_with_loss() {
    use std::net::UdpSocket;
    let (vid, aid) = (1u32, 2u32);
    let mut server =
        Server::bind("127.0.0.1:0".parse().unwrap(), vid, aid, Limits::default()).unwrap();
    let saddr = server.local_addr().unwrap();
    let proxy = UdpSocket::bind("127.0.0.1:0").unwrap();
    let paddr = proxy.local_addr().unwrap();
    proxy
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let px = std::thread::spawn(move || {
        let mut client = None;
        let mut buf = vec![0u8; 65536];
        let mut seed = 0x9e3779b97f4a7c15u64;
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok((n, from)) = proxy.recv_from(&mut buf) else {
                continue;
            };
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if seed % 100 < 5 {
                continue; // drop
            }
            let to = if from == saddr {
                client
            } else {
                client = Some(from);
                Some(saddr)
            };
            if let Some(to) = to {
                let _ = proxy.send_to(&buf[..n], to);
            }
        }
    });
    let sender = std::thread::spawn(move || {
        let mut c = Client::connect(paddr, Duration::from_secs(10)).unwrap();
        let v = c.open_media_stream(0, vid);
        let a = c.open_media_stream(1, aid);
        for i in 0..20u32 {
            let payload: Vec<u8> = (0..100_000u32).map(|j| (i ^ j) as u8).collect();
            c.send_message(
                v,
                &Frame {
                    kind: kind::VIDEO,
                    scrambled: true,
                    version: 0,
                    ext: vec![1; 57],
                    payload,
                }
                .encode(),
            );
            c.send_message(
                a,
                &Frame {
                    kind: kind::AUDIO,
                    scrambled: true,
                    version: 1,
                    ext: vec![2; 33],
                    payload: vec![3; 500],
                }
                .encode(),
            );
        }
        // Hold the connection open until everything has arrived (loss recovery runs in
        // quinn); `close` then flushes and closes.
        std::thread::sleep(Duration::from_secs(5));
        c.close();
    });
    let (mut video, mut audio) = (0u32, 0u32);
    let start = Instant::now();
    while (video < 20 || audio < 20) && start.elapsed() < Duration::from_secs(40) {
        for e in server.poll() {
            match e {
                Event::Message(MediaKind::Video, f) => {
                    assert_eq!(f.payload[777], (video ^ 777) as u8);
                    video += 1;
                }
                Event::Message(MediaKind::Audio, _) => audio += 1,
                Event::StreamError(k, m) => panic!("{k:?}: {m}"),
                _ => {}
            }
        }
    }
    sender.join().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    px.join().unwrap();
    assert_eq!((video, audio), (20, 20));
}

#[test]
fn client_gives_up_on_a_receiver_that_vanished() {
    let (vid, aid) = (7u32, 8u32);
    let mut server =
        Server::bind("127.0.0.1:0".parse().unwrap(), vid, aid, Limits::default()).unwrap();
    let (relay, cut) = common::udp_relay(server.local_addr().unwrap());
    let srv = std::thread::spawn(move || {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(40) {
            server.poll();
        }
    });
    let mut c = Client::connect(relay, Duration::from_secs(5)).unwrap();
    let v = c.open_media_stream(0, vid);
    std::thread::sleep(Duration::from_secs(1));
    // Gone without a word: nothing closed, the receiver just goes silent (power loss).
    cut.store(true, std::sync::atomic::Ordering::Relaxed);
    // Keep sending into the void: the client must notice and stop, not retransmit forever.
    let gone = Instant::now();
    c.send_message(v, &vec![0u8; 500_000]);
    while c.closed().is_none() && gone.elapsed() < Duration::from_secs(45) {
        c.poll_once().unwrap();
    }
    assert_eq!(c.closed().as_deref(), Some("idle timeout"));
    let after = gone.elapsed();
    assert!(
        after > Duration::from_secs(25) && after < Duration::from_secs(35),
        "{after:?}"
    );
    srv.join().unwrap();
}

/// A stream opened for one session ends once the receiver renews its identifiers for the
/// next: an earlier sender can't keep sending into a later session.
#[test]
fn a_stream_from_an_earlier_session_ends_when_the_ids_are_renewed() {
    let (vid, aid) = (7u32, 8u32);
    let mut server =
        Server::bind("127.0.0.1:0".parse().unwrap(), vid, aid, Limits::default()).unwrap();
    let addr = server.local_addr().unwrap();
    let ids = server.ids();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let sender = std::thread::spawn(move || {
        let mut c = Client::connect(addr, Duration::from_secs(5)).unwrap();
        let v = c.open_media_stream(0, vid);
        let frame = |i: u8| Frame {
            kind: kind::VIDEO,
            scrambled: true,
            version: 0,
            ext: vec![i; 8],
            payload: vec![i; 100],
        };
        c.send_message(v, &frame(1).encode());
        let flush = |c: &mut Client| {
            let start = Instant::now();
            while c.unacked() > 0 && start.elapsed() < Duration::from_secs(5) {
                c.poll_once().unwrap();
            }
        };
        flush(&mut c);
        go_rx.recv().unwrap(); // the ids are renewed
        for i in 2..6 {
            c.send_message(v, &frame(i).encode());
        }
        flush(&mut c);
        std::thread::sleep(Duration::from_millis(300));
        c.close();
    });
    let mut got = Vec::new();
    let mut renewed = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let mut closed = false;
        for e in server.poll() {
            match e {
                Event::Message(MediaKind::Video, f) => got.push(f.ext[0]),
                Event::Closed(..) => closed = true,
                _ => {}
            }
        }
        if !renewed && got == [1] {
            ids.renew();
            renewed = true;
            go_tx.send(()).unwrap();
        }
        if closed {
            break;
        }
    }
    sender.join().unwrap();
    assert_eq!(got, [1], "nothing after the renewal");
}
