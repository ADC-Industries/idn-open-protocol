use super::*;

#[test]
fn site_handshakes_are_capped_per_address_and_in_all() {
    let busy = Arc::new(AtomicUsize::new(0));
    let per: Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>> = Arc::default();
    let a: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    let held: Vec<_> = (0..SITE_HANDSHAKES_PER_ADDRESS)
        .map(|_| SiteHandshake::start(&busy, &per, a).expect("within the cap"))
        .collect();
    assert!(
        SiteHandshake::start(&busy, &per, a).is_none(),
        "one address, one too many"
    );
    let others: Vec<_> = (0..MAX_HANDSHAKES - SITE_HANDSHAKES_PER_ADDRESS)
        .map(|i| {
            let ip = std::net::IpAddr::from([198, 51, 100, i as u8]);
            SiteHandshake::start(&busy, &per, ip).expect("within the total")
        })
        .collect();
    let late: std::net::IpAddr = "192.0.2.1".parse().unwrap();
    assert!(
        SiteHandshake::start(&busy, &per, late).is_none(),
        "the total is full"
    );
    drop(held);
    drop(others);
    assert_eq!(busy.load(Ordering::Relaxed), 0);
    assert!(per.lock_ok().is_empty());
    assert!(SiteHandshake::start(&busy, &per, a).is_some(), "free again");
}

#[test]
fn relayed_trust_connections_are_capped_per_address_and_in_all() {
    let a: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    let held: Vec<Relaying> = (0..RELAYS_PER_ADDRESS)
        .map(|_| Relaying::take(a).expect("within the cap"))
        .collect();
    assert!(Relaying::take(a).is_none(), "one address can't take more");
    let others: Vec<Relaying> = (0..)
        .map_while(|i: u32| Relaying::take(std::net::IpAddr::from([198, 51, 100, i as u8])))
        .collect();
    assert_eq!(
        held.len() + others.len(),
        RELAYS,
        "nor all of them together"
    );
    drop(held);
    drop(others);
    assert!(Relaying::take(a).is_some(), "free again once they end");
}

#[test]
fn the_nodes_refusal_reads_plainly() {
    let e = quinn::ConnectionError::TransportError(quinn_proto::TransportError {
        code: quinn::TransportErrorCode::crypto(80),
        frame: None,
        reason: format!(
            "unexpected error: {}: IDN Node: this program waits for approval in the IDN Node's panel (Programs) (approval)",
            crate::trust::pki::NODE_SIGNING
        ),
    });
    let e = connect_error(&e);
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        e.to_string(),
        "IDN Node: this program waits for approval in the IDN Node's panel (Programs) (approval)"
    );
}
use std::io::{Read, Write};

type Got = Arc<Mutex<Vec<(MediaKind, Frame)>>>;

fn sink() -> (Sink, Got) {
    let got: Got = Default::default();
    let g = got.clone();
    (
        Arc::new(move |k, _, f| {
            if let Ok(f) = f {
                g.lock_ok().push((k, f));
            }
        }),
        got,
    )
}

fn server(secure: Option<SecureConfig>) -> (LinkServer, std::sync::mpsc::Receiver<Accepted>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let s = LinkServer::bind(0, LinkConfig::ephemeral(), secure, move |a| {
        let _ = tx.lock_ok().send(a);
    })
    .unwrap();
    (s, rx)
}

fn dial(port: u16, secure: Option<&SecureConfig>, sink: Sink) -> io::Result<LinkClient> {
    connect(
        SocketAddr::from(([127, 0, 0, 1], port)),
        &LinkConfig::ephemeral(),
        secure,
        Limits::default(),
        sink,
        Duration::from_secs(3),
    )
}

/// Lab items 55 and 58: being behind shows as round-trip time above the path's floor.
#[test]
fn queueing_delay_is_the_round_trip_above_its_floor() {
    assert_eq!(super::queueing(300, 300), 0);
    assert_eq!(super::queueing(565_000, 300), 564_700);
    // The floor is the lowest seen, so a measurement below it is no queueing.
    assert_eq!(super::queueing(200, 300), 0);
}

/// Lab item 58: skipping grows with the delay instead of switching on at a line.
#[test]
fn video_is_skipped_in_proportion_to_the_delay() {
    assert_eq!(super::skip_share(100_000, 150_000), 0.0);
    assert_eq!(super::skip_share(150_000, 150_000), 0.0);
    assert!((super::skip_share(205_000, 150_000) - 0.3667).abs() < 0.001);
    assert_eq!(super::skip_share(300_000, 150_000), 1.0);
    assert_eq!(super::skip_share(3_500_000, 150_000), 1.0);
}

#[test]
fn modes_follow_the_credentials_both_sides_have() {
    let site_a = SecureConfig::new("St Mark").unwrap();
    let site_b = SecureConfig::new("another site").unwrap();
    let mut strict = SecureConfig::new("St Mark").unwrap();
    strict.require = true;
    let cases: [(Option<&SecureConfig>, Option<&SecureConfig>, Option<&str>); 5] = [
        (None, None, Some("open")),
        (Some(&site_a), Some(&site_a), Some("s1")),
        (Some(&site_a), Some(&site_b), Some("open")),
        (Some(&site_a), None, Some("open")),
        (Some(&strict), None, None), // requires encryption: nothing in common
    ];
    for (srv, cli, want) in cases {
        let (s, rx) = server(srv.cloned());
        let r = dial(s.port(), cli, sink().0);
        match want {
            Some(m) => {
                let mut c = r.unwrap_or_else(|e| panic!("{m}: {e}"));
                assert_eq!(c.mode.name(), m);
                // A QUIC stream becomes visible to the peer with its first bytes (a
                // receiver sends ndi_version at once).
                c.control.write_all(b"x").unwrap();
                let a = rx.recv_timeout(Duration::from_secs(3)).unwrap();
                assert_eq!(a.mode.name(), m);
                assert_eq!(a.peer_fingerprint.len(), 64);
                assert_eq!(c.peer_fingerprint.len(), 64);
            }
            None => {
                // No mode in common is a reason to fall back, not an attack.
                let e = r.err().expect("expected no common mode");
                assert_ne!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            }
        }
    }
}

#[test]
fn control_and_media_flow() {
    let (s, rx) = server(None);
    let (snk, got) = sink();
    let mut c = dial(s.port(), None, snk).unwrap();
    c.control.write_all(b"hello sender").unwrap();
    let mut a = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut b = [0u8; 12];
    a.control.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"hello sender");
    a.control.write_all(b"hello receiver").unwrap();
    let mut b = [0u8; 14];
    c.control.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"hello receiver");

    let media = a.conn.media(false);
    let video = Frame::text(crate::frame::kind::VIDEO, 0, "<v/>");
    let audio = Frame::text(crate::frame::kind::AUDIO, 0, "<a/>");
    assert!(media.send(true, video.encode()));
    assert!(media.send(false, audio.encode()));
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while got.lock_ok().len() < 2 && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    let g = got.lock_ok();
    assert!(
        g.iter()
            .any(|(k, f)| *k == MediaKind::Video && f.payload == video.payload)
    );
    assert!(
        g.iter()
            .any(|(k, f)| *k == MediaKind::Audio && f.payload == audio.payload)
    );
    drop(g);

    // The sender ends the session: the receiver's control stream sees the end.
    drop(a.control);
    c.control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(c.control.read(&mut [0u8; 8]).unwrap(), 0);
}

#[test]
fn key_memory_reports_a_changed_key_and_keeps_it() {
    let dir = idn_common::testing::TempDir::new("link-keys");
    let m = KeyMemory::with_file(dir.join("peers.tsv"));
    assert!(!m.check("HOST (Program)", "aa"));
    assert!(!m.check("host (program)", "aa"), "same key, any case");
    assert!(m.check("HOST (Program)", "bb"), "changed");
    assert!(
        !m.check("HOST (Program)", "bb"),
        "the new key is remembered"
    );
    let again = KeyMemory::with_file(dir.join("peers.tsv"));
    assert!(
        again.check("HOST (Program)", "cc"),
        "remembered across restarts"
    );
    again.forget("HOST (Program)");
    assert!(!again.check("HOST (Program)", "dd"));
}

#[test]
fn link_is_added_to_the_announcement() {
    let v = r#"<ndi_version text="3" x_idn_ext="resume/1 stats/1"/>"#;
    assert_eq!(
        announce(v),
        r#"<ndi_version text="3" x_idn_ext="resume/1 stats/1 link/1"/>"#
    );
    assert_eq!(announce(&announce(v)), announce(v));
    assert!(offered("resume/1 link/1") && !offered("resume/1 link/2"));
}

#[test]
fn per_frame_streams_abandon_a_late_frame_and_deliver_the_rest() {
    let (s, rx) = server(None);
    let (snk, got) = sink();
    let mut c = dial(s.port(), None, snk).unwrap();
    c.control.write_all(b"x").unwrap();
    let a = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let media = a.conn.media(true);
    let frame = |n: u8, size: usize| {
        let mut f = Frame::text(crate::frame::kind::VIDEO, 0, "<v/>");
        f.payload = vec![n; size];
        f.encode()
    };
    // 1 ms is far too short for 4 MB: abandoned, never delivered.
    media.set_deadline(Duration::from_millis(1));
    assert!(media.send(true, frame(1, 4 << 20)));
    std::thread::sleep(Duration::from_millis(200));
    // Then normal frames with a normal deadline.
    media.set_deadline(Duration::from_millis(500));
    for n in 2..6 {
        assert!(media.send(true, frame(n, 1000)));
        std::thread::sleep(Duration::from_millis(20));
    }
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while got.lock_ok().len() < 4 && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    let g = got.lock_ok();
    let firsts: Vec<u8> = g.iter().map(|(_, f)| f.payload[0]).collect();
    assert_eq!(
        firsts,
        [2, 3, 4, 5],
        "the late frame never arrives, the others in order"
    );
    assert_eq!(media.abandoned(), 1);
}
