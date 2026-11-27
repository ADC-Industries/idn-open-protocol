//! Tests of the site authority.

use super::*;

#[test]
fn alerts_are_kept_newest_first_and_trimmed() {
    let dir = idn_common::testing::TempDir::new("auth-alerts");
    std::fs::create_dir_all(&dir).unwrap();
    for k in 0..2001u32 {
        let m = Element::new("idn_node_alert")
            .with_attr("at", &k.to_string())
            .with_attr("app", "idn-switcher")
            .with_attr("level", "warning")
            .with_text(&format!("YouTube\treconnecting\n{k}"));
        keep_alert(&dir, "abc", &m).unwrap();
    }
    let a = read_alerts(&dir);
    assert_eq!(a.len(), 1000);
    assert_eq!(a[0].0, 2000);
    assert_eq!(a[999].0, 1001);
    // Tabs and line ends in the text don't break the lines.
    assert_eq!(a[0].4, "YouTube reconnecting 2000");
    // A typed event's name and fields (§N.41) come back apart from the text.
    let m = Element::new("idn_node_alert")
        .with_attr("at", "3000")
        .with_attr("app", "idn-switcher")
        .with_attr("level", "warning")
        .with_attr("event", "lost")
        .with_attr("detail", r#"{"input":3}"#)
        .with_text("Cam 3 lost");
    keep_alert(&dir, "abc", &m).unwrap();
    let e = &read_alert_entries(&dir)[0];
    assert_eq!(
        (e.text.as_str(), e.event.as_str(), e.detail.as_str()),
        ("Cam 3 lost", "lost", r#"{"input":3}"#)
    );
    assert_eq!(read_alerts(&dir)[0].4, "Cam 3 lost");
    assert_eq!(
        (a[0].2.as_str(), a[0].3.as_str()),
        ("idn-switcher", "warning")
    );
}

/// One join over a fresh connection to `a`: the status the authority answers.
fn join_state(a: &Authority, join: &Element) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (s, _) = l.accept().unwrap();
    let a2 = a.clone();
    let t = std::thread::spawn(move || a2.serve(s));
    let _ok = proto::recv_into(&mut c, &mut Vec::new(), proto::MAX_PLAIN).unwrap();
    proto::send(&mut c, &join.to_xml()).unwrap();
    let st = proto::recv_into(&mut c, &mut Vec::new(), proto::MAX_PLAIN)
        .unwrap()
        .and_then(|e| e.attr("state").map(str::to_owned))
        .unwrap_or_default();
    drop(c);
    let _ = t.join();
    st
}

#[test]
fn a_passphrase_proof_is_good_once_and_for_its_own_name() {
    let dir = idn_common::testing::TempDir::new("auth-proof");
    let a = Authority::open(AuthorityConfig {
        state_dir: dir.to_path_buf(),
        passphrase: Some("St Mark".into()),
    })
    .unwrap();
    let site = crate::secure::SiteKey::from_passphrase("St Mark").unwrap();
    let ca = a.0.ca.key().spki();
    let dev = KeyFile::generate().unwrap();
    let spki = dev.spki();
    let now = idn_common::epoch::secs();
    let join = |name: &str, mac_name: &str, at: u64, second: bool| {
        let mut e = Element::new("idn_join")
            .with_attr("name", name)
            .with_attr("kind", "app")
            .with_attr("key", &b64(&spki));
        if second {
            e = e.with_attr("psk_at", &at.to_string()).with_attr(
                "psk_mac2",
                &b64(&site.join_mac2(&spki, &ca, mac_name, "app", at)),
            );
        }
        e
    };
    let good = join("Cam (App)", "Cam (App)", now, true);
    assert_eq!(join_state(&a, &good), "approved");
    // The same proof again (a replay), the proof under another name, a stale one, and
    // no proof: none is approved by itself (the pairing window is shut).
    assert_eq!(join_state(&a, &good), "closed", "replayed");
    assert_eq!(
        join_state(&a, &join("Evil (App)", "Cam (App)", now + 1, true)),
        "closed",
        "another name"
    );
    assert_eq!(
        join_state(&a, &join("Cam (App)", "Cam (App)", now - 3600, true)),
        "closed",
        "an hour old"
    );
    assert_eq!(
        join_state(&a, &join("Cam (App)", "Cam (App)", now, false)),
        "closed",
        "the first form alone"
    );
}

#[test]
fn revoke_covers_every_certificate_of_the_device() {
    let dir = idn_common::testing::TempDir::new("auth-revoke");
    let a = Authority::open(AuthorityConfig {
        state_dir: dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    // The same device key enrolled twice (it joined again after losing its first
    // certificate), plus another device.
    let dev = KeyFile::generate().unwrap();
    let other = KeyFile::generate().unwrap();
    let (_, first) = a.enrol(&dev.spki(), "Cam", "camera").unwrap();
    let (_, second) = a.enrol(&dev.spki(), "Cam", "camera").unwrap();
    a.enrol(&other.spki(), "Other", "camera").unwrap();
    assert_ne!(first.info.serial, second.info.serial);
    a.revoke(&first.info.fingerprint).unwrap();
    {
        let st = a.0.state.lock_ok();
        let fp = &first.info.fingerprint;
        assert!(
            st.devices
                .iter()
                .filter(|r| &r.info.fingerprint == fp)
                .all(|r| r.revoked)
        );
        assert!(
            st.devices
                .iter()
                .any(|r| r.info.name == "Other" && !r.revoked)
        );
        let (_, crl) = x509_parser::parse_x509_crl(&st.crl_der).unwrap();
        assert_eq!(
            crl.iter_revoked_certificates().count(),
            2,
            "both serials listed"
        );
    }
    // Revoking again finds nothing active.
    assert!(a.revoke(&first.info.fingerprint).is_err());
}

#[test]
fn huge_pairing_windows_and_unread_events_are_bounded() {
    let dir = idn_common::testing::TempDir::new("auth-bounds");
    let a = Authority::open(AuthorityConfig {
        state_dir: dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    a.open_pairing(Duration::MAX); // used to overflow the clock and poison the lock
    assert!(a.pairing_open());
    a.close_pairing();
    for _ in 0..1000 {
        a.event(AuthorityEvent::JoinCancelled(1));
    }
    assert_eq!(a.0.events.0.lock_ok().len(), MAX_EVENTS);
}

#[test]
fn a_stalled_device_does_not_hold_up_the_authority() {
    use std::time::Instant;
    let dir = idn_common::testing::TempDir::new("auth-stall");
    let a = Authority::open(AuthorityConfig {
        state_dir: dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    // An "online device" that never reads: fill its socket buffers, so that the next
    // CRL push blocks until the write timeout.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let w = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let _peer = l.accept().unwrap();
    w.set_nonblocking(true).unwrap();
    let junk = vec![0u8; 1 << 16];
    while std::io::Write::write(&mut &w, &junk).is_ok() {}
    w.set_nonblocking(false).unwrap();
    w.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    a.0.state
        .lock_ok()
        .online
        .push(("stalled".into(), 99, Arc::new(Mutex::new(w))));
    let pusher = {
        let a = a.clone();
        std::thread::spawn(move || a.reissue_crl())
    };
    std::thread::sleep(Duration::from_millis(300)); // the push is blocked by now
    let t = Instant::now();
    let _ = a.devices();
    a.open_pairing(Duration::from_secs(1));
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    pusher.join().unwrap().unwrap();
    assert!(
        a.0.state.lock_ok().online.is_empty(),
        "the stalled device was dropped"
    );
}

/// The authority's hello answer and its first answer to `join`, for a device saying
/// `hello` (§E.10.2, §E.10.10).
fn answers(a: &Authority, hello: &str, join: &Element) -> (Element, Element) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (s, _) = l.accept().unwrap();
    let (a2, h) = (a.clone(), hello.to_owned());
    let t = std::thread::spawn(move || a2.serve_hello(s, &h));
    let ok = proto::recv_into(&mut c, &mut Vec::new(), proto::MAX_PLAIN)
        .unwrap()
        .unwrap();
    proto::send(&mut c, &join.to_xml()).unwrap();
    let first = proto::recv_into(&mut c, &mut Vec::new(), proto::MAX_PLAIN)
        .unwrap()
        .unwrap();
    drop(c);
    let _ = t.join();
    (ok, first)
}

#[test]
fn devices_are_told_the_join_mode_and_version_1_is_not_served() {
    let dir = idn_common::testing::TempDir::new("auth-hello");
    let a = Authority::open(AuthorityConfig {
        state_dir: dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let dev = KeyFile::generate().unwrap();
    let join = Element::new("idn_join")
        .with_attr("name", "Lobby (App)")
        .with_attr("kind", "app")
        .with_attr("key", &b64(&dev.spki()));
    const V2: &str = r#"<idn_trust version="2"/>"#;
    a.set_join_mode(crate::trust::JoinMode::CodeOrApproval)
        .unwrap();
    a.open_pairing(Duration::from_secs(60));
    let (ok, first) = answers(&a, V2, &join);
    assert_eq!(ok.attr("version"), Some("2"));
    assert_eq!(ok.attr("join"), Some("code_or_approval"));
    assert_eq!(first.name, "idn_commit");
    // Code and approval: a join without a code is told it needs one.
    a.set_join_mode(crate::trust::JoinMode::CodeAndApproval)
        .unwrap();
    let (_, first) = answers(&a, V2, &join);
    assert_eq!(first.attr("state"), Some("code_needed"));
    // A version 1 hello, or one without a version, gets no answer: the connection is
    // closed.
    for hello in [r#"<idn_trust version="1"/>"#, "<idn_trust/>"] {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        let (a2, h) = (a.clone(), hello.to_owned());
        std::thread::spawn(move || a2.serve_hello(s, &h))
            .join()
            .unwrap();
        assert!(
            matches!(
                proto::recv_into(&mut c, &mut Vec::new(), proto::MAX_PLAIN),
                Ok(None) | Err(_)
            ),
            "{hello}"
        );
    }
}

#[test]
fn join_names_cannot_carry_terminal_controls() {
    assert_eq!(
        display_name("Stage \x1b[2J\x1b]0;x\x07left"),
        "Stage  [2J ]0;x left"
    );
    assert_eq!(
        display_name("a\u{202e}gnp.exe\r\napproved"),
        "a gnp.exe  approved"
    );
    assert_eq!(display_name(&"é".repeat(500)).chars().count(), MAX_NAME);
    assert_eq!(display_name("  Lobby TV  "), "Lobby TV");
}
