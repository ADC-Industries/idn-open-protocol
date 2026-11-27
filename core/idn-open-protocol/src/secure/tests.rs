//! Tests of site keys, TLS and the negotiation.

use super::negotiate::{read_until, write_text};
use super::tls::loopback_pair_with;
use super::*;

#[test]
fn a_site_key_planted_outside_the_spki_is_not_accepted() {
    let site = SiteKey::from_passphrase("correct horse battery staple").unwrap();
    // An attacker's own Ed25519 certificate whose issuer name is a 44-character
    // placeholder, then replaced by the site key's SPKI pattern.
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "A".repeat(44));
    let mut der = params.self_signed(&kp).unwrap().der().to_vec();
    let planted = crate::trust::pki::spki(&site.public_key);
    let at = der.windows(44).position(|w| w == [b'A'; 44]).unwrap();
    der[at..at + 44].copy_from_slice(&planted);
    let own: [u8; 32] = kp.public_key_raw().try_into().unwrap();
    assert!(
        der.windows(44).position(|w| w == planted.as_slice())
            < der
                .windows(44)
                .position(|w| w == crate::trust::pki::spki(&own).as_slice()),
        "the planted key comes first"
    );
    let v = SiteVerifier::new(site.clone(), &crate::secure::crypto_provider());
    assert!(
        v.check(&CertificateDer::from(der)).is_err(),
        "planted site key must not pass"
    );
    assert!(
        v.check(&site.cert).is_ok(),
        "the real site certificate passes"
    );
}

/// A peer that stays connected but stops reading: the threads that copy to it must
/// give up (a write timeout on the network socket), not stay blocked for as long as
/// the peer keeps the connection open, leaking two threads per session.
#[test]
fn a_peer_that_stops_reading_doesnt_hold_the_tls_threads() {
    let key = SiteKey::from_passphrase("stalled reader").unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let k2 = key.clone();
    let client =
        std::thread::spawn(move || connect_tls(&k2, TcpStream::connect(addr).unwrap()).unwrap());
    let (net, _) = l.accept().unwrap();
    let mut app = accept_tls(&key, net).unwrap();
    // The peer's application never reads: its side fills, then the network does.
    let _peer_app = client.join().unwrap();
    app.set_write_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let chunk = vec![7u8; 256 * 1024];
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let gone = loop {
        match app.write(&chunk) {
            Ok(_) => {}
            Err(e) if crate::transport::is_poll_timeout(&e) => {}
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => break true, // the copying thread ended: the pipe is closed
        }
        if std::time::Instant::now() > end {
            break false;
        }
    };
    assert!(
        gone,
        "the TLS writer thread is still blocked on the stalled peer"
    );
}

#[test]
fn derivation_is_deterministic_and_keyed() {
    let a = SiteKey::from_passphrase("correct horse battery staple").unwrap();
    let b = SiteKey::from_passphrase("correct horse battery staple").unwrap();
    let c = SiteKey::from_passphrase("another site").unwrap();
    assert_eq!(a.key_id, b.key_id);
    assert_eq!(a.key_id.len(), 16);
    assert_eq!(a.cert, b.cert, "deterministic certificate");
    assert_ne!(a.key_id, c.key_id);
    assert_ne!(a.public_key, c.public_key);
    assert!(!format!("{a:?}").contains(&idn_common::hex::hex(&a.public_key)));
}

fn pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    (c, l.accept().unwrap().0)
}

#[test]
fn tls_roundtrip_same_site_and_rejects_other_site() {
    let k = SiteKey::from_passphrase("site one").unwrap();
    let (c, s) = pair();
    let ks = k.clone();
    let srv = std::thread::spawn(move || {
        let mut p = accept_tls(&ks, s).unwrap();
        let mut buf = vec![0u8; 300_000];
        p.read_exact(&mut buf).unwrap();
        p.write_all(&buf).unwrap(); // echo
    });
    let mut p = connect_tls(&k, c).unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    p.write_all(&data).unwrap();
    let mut back = vec![0u8; data.len()];
    p.read_exact(&mut back).unwrap();
    assert_eq!(back, data);
    srv.join().unwrap();

    // A peer with a different passphrase cannot complete the handshake.
    let other = SiteKey::from_passphrase("site two").unwrap();
    let (c, s) = pair();
    let srv = std::thread::spawn(move || accept_tls(&k, s).is_err());
    assert!(connect_tls(&other, c).is_err());
    assert!(srv.join().unwrap());
}

#[test]
fn sticky_store_persists() {
    let dir = idn_common::testing::TempDir::new("sticky");
    let path = dir.join("sticky.txt");
    let s = StickyStore::with_file(path.clone());
    s.remember("0123456789abcdef", "HOST (CAM 1)");
    assert!(StickyStore::with_file(path.clone()).contains("0123456789abcdef", "HOST (CAM 1)"));
    s.forget_all();
    assert!(!StickyStore::with_file(path).contains("0123456789abcdef", "HOST (CAM 1)"));
}

#[test]
fn read_until_clears_its_timeout_on_every_exit() {
    let (mut a, mut b) = pair();
    let ms = std::time::Duration::from_millis;
    // Nothing arrives: no match, and the socket is left without a read timeout.
    assert!(
        read_until(&mut a, "ndi_version", ms(100))
            .unwrap()
            .1
            .is_none()
    );
    assert_eq!(a.read_timeout().unwrap(), None);
    // Other frames, then the deadline.
    write_text(&mut b, "<ndi_identify name=\"X\"/>").unwrap();
    let (got, hit) = read_until(&mut a, "ndi_version", ms(200)).unwrap();
    assert_eq!((got.len(), hit.is_none()), (1, true));
    assert_eq!(a.read_timeout().unwrap(), None);
    // The match.
    write_text(&mut b, "<ndi_version text=\"3\"/>").unwrap();
    assert!(
        read_until(&mut a, "ndi_version", ms(500))
            .unwrap()
            .1
            .is_some()
    );
    assert_eq!(a.read_timeout().unwrap(), None);
}

#[test]
fn a_deadline_in_the_middle_of_a_frame_loses_nothing() {
    let (mut a, mut b) = pair();
    let first = crate::frame::Frame::text(crate::frame::kind::CONTROL, 0, "<one/>").encode();
    let second = crate::frame::Frame::text(crate::frame::kind::CONTROL, 0, "<two/>").encode();
    b.write_all(&first[..5]).unwrap();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        b.write_all(&first[5..]).unwrap();
        b.write_all(&second).unwrap();
        b
    });
    // The 200 ms deadline passes while the first frame is half here: it is still read
    // whole, and the connection stays in step for the next one.
    let (got, hit) = read_until(&mut a, "two", std::time::Duration::from_millis(200)).unwrap();
    assert!(hit.is_none());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].text_payload().unwrap(), "<one/>");
    let _b = writer.join().unwrap();
    let next = crate::frame::read_frame(&mut a, &crate::frame::Limits::default())
        .unwrap()
        .unwrap();
    assert_eq!(next.text_payload().unwrap(), "<two/>");
}

#[test]
fn plain_text_frames_injected_before_tls_are_not_part_of_the_session() {
    let cfg = SecureConfig::new("injection test").unwrap();
    let evil = r#"<ndi_routing ip="192.0.2.1:5961" name="EVIL (CAM)"/>"#;
    let (mut c, mut s) = pair();
    // On-path injection: a forged routing message ahead of each side's ndi_version.
    write_text(&mut s, evil).unwrap();
    write_text(&mut c, evil).unwrap();
    let sc = cfg.clone();
    let sender = std::thread::spawn(move || negotiate_sender(s, &sc, &sc.version(true)));
    let rx = negotiate_receiver(c, &cfg, &cfg.version(false), "HOST (SRC)").unwrap();
    let tx = sender.join().unwrap().unwrap();
    for n in [&rx, &tx] {
        assert!(n.secure);
        let texts: Vec<&str> = n.pre.iter().filter_map(|f| f.text_payload().ok()).collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].starts_with("<ndi_version"), "{texts:?}");
    }
}

#[test]
fn the_plaintext_bridge_takes_only_its_own_connection() {
    let mut intruder = None;
    let (app, pipe) = loopback_pair_with(|addr| {
        intruder = Some(TcpStream::connect(addr).unwrap());
    })
    .unwrap();
    assert_eq!(pipe.peer_addr().unwrap(), app.local_addr().unwrap());
    // The intruder's connection was accepted and dropped: it reads end of stream.
    let mut intruder = intruder.unwrap();
    intruder
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut b = [0u8; 1];
    assert!(matches!(intruder.read(&mut b), Ok(0) | Err(_)));
    // And the pair works.
    let (mut app, mut pipe) = (app, pipe);
    app.write_all(b"x").unwrap();
    pipe.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"x");
}

#[test]
fn bytes_after_close_notify_end_the_session() {
    let k = SiteKey::from_passphrase("site one").unwrap();
    let (mut c, s) = pair();
    let ks = k.clone();
    let srv = std::thread::spawn(move || {
        let mut p = accept_tls(&ks, s).unwrap();
        p.set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut got = Vec::new();
        let r = p.read_to_end(&mut got);
        (r.is_ok(), got)
    });
    let name = ServerName::try_from("idn-site").unwrap();
    let mut conn =
        rustls::ClientConnection::new(Arc::new(k.client_config().unwrap()), name).unwrap();
    while conn.is_handshaking() {
        conn.complete_io(&mut c).unwrap();
    }
    conn.writer().write_all(b"hello").unwrap();
    conn.send_close_notify();
    let mut out = Vec::new();
    while conn.wants_write() {
        conn.write_tls(&mut out).unwrap();
    }
    // More after close_notify, in the same segment, with the connection left open.
    out.extend_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x05, 1, 2, 3, 4, 5]);
    c.write_all(&out).unwrap();
    let (clean_eof, got) = srv.join().unwrap();
    assert!(clean_eof, "the plaintext end is closed (not left waiting)");
    assert_eq!(got, b"hello");
    drop(c);
}

#[test]
fn a_trickled_handshake_is_given_up() {
    let k = SiteKey::from_passphrase("site one").unwrap();
    let (mut c, s) = pair();
    let t = std::time::Instant::now();
    let srv = std::thread::spawn(move || accept_tls(&k, s).is_err());
    // A handshake record header announcing 16000 bytes, then a byte every 500 ms:
    // each read succeeds well within its own timeout.
    let _ = c.write_all(&[0x16, 0x03, 0x01, 0x3e, 0x80]);
    while !srv.is_finished() && t.elapsed() < std::time::Duration::from_secs(20) {
        let _ = c.write_all(&[0]);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(srv.join().unwrap());
    assert!(
        t.elapsed() < std::time::Duration::from_secs(12),
        "{:?}",
        t.elapsed()
    );
}
