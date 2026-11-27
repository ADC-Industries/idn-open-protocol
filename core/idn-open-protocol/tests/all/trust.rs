//! Site trust (§E.10): joining by passphrase and by pairing window + code, deny, revoke,
//! persistence, and a Discovery Server without an authority.

use crate::common::{device, free_port, site, wait_connected, wait_event, wait_state};
use idn_common::testing::{TempDir, wait_for, wait_until};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::trust::{
    Authority, AuthorityConfig, AuthorityEvent, Enrollment, OptionScope, TrustState,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Joined by code with confirmation on (the IDN Node's way): the
/// authority's approval alone doesn't enrol; the code confirmed here does, and "it
/// doesn't match" leaves the device out.
#[test]
fn a_join_by_code_waits_for_this_device_to_confirm_the_code() {
    let site = site("trust-confirm", None);
    let (authority, server) = (&site.authority, &site.server);
    authority.open_pairing(Duration::from_secs(60));
    for (tag, confirm) in [("confirm-yes", true), ("confirm-no", false)] {
        let name = format!("{tag} (App)");
        let d = device(server, &site.dir.join(tag), &name, None);
        d.confirm_codes(true);
        let Some(AuthorityEvent::JoinRequested(req)) = wait_event(
            authority,
            8,
            |e| matches!(e, AuthorityEvent::JoinRequested(r) if r.name == name),
        ) else {
            panic!("no join request");
        };
        authority.approve(req.id).unwrap();
        let TrustState::ConfirmCode { code } =
            wait_state(&d, 5, |s| matches!(s, TrustState::ConfirmCode { .. }))
        else {
            panic!("{:?}", d.state())
        };
        assert_eq!(code, req.code);
        // Not trusted yet.
        assert!(d.ca_id().is_none());
        d.confirm_code(confirm);
        if confirm {
            assert!(matches!(
                wait_state(&d, 5, |s| matches!(s, TrustState::Enrolled { .. })),
                TrustState::Enrolled { .. }
            ));
        } else {
            std::thread::sleep(Duration::from_millis(500));
            assert!(d.ca_id().is_none(), "{:?}", d.state());
            // The join ends (lab 328-B): the authority takes the certificate back and
            // doesn't list the device, and the device doesn't ask again by itself (its
            // retries came every 5 s, each a new code, with the window still open).
            assert!(
                wait_event(authority, 5, |e| matches!(
                    e,
                    AuthorityEvent::Withdrawn { name: n, .. } if *n == name
                ))
                .is_some(),
                "no withdrawal"
            );
            assert!(authority.devices().iter().all(|x| x.name != name));
            assert!(
                wait_event(authority, 7, |e| matches!(
                    e,
                    AuthorityEvent::JoinRequested(r) if r.name == name
                ))
                .is_none(),
                "it asked to join again by itself"
            );
            assert!(matches!(d.state(), TrustState::Error(_)), "{:?}", d.state());
        }
    }
    // The confirmed one stays in the site.
    assert!(
        authority
            .devices()
            .iter()
            .any(|x| x.name == "confirm-yes (App)")
    );
}

#[test]
fn join_approve_deny_revoke_and_persist() {
    let site = site("trust-join", Some("St Mark"));
    let (authority, server) = (&site.authority, &site.server);
    let ca = authority.ca_fingerprint();

    // 1. The passphrase joins by itself (hybrid).
    let a = device(server, &site.dir.join("a"), "A (App)", Some("St Mark"));
    wait_connected(&a);
    assert!(
        wait_event(
            authority,
            3,
            |e| matches!(e, AuthorityEvent::Approved(d) if d.name == "A (App)")
        )
        .is_some()
    );

    // 2. Without it: told to wait while the window is closed...
    let bdir = site.dir.join("b");
    let b = device(server, &bdir, "B (App)", None);
    assert_eq!(
        wait_state(&b, 5, |s| *s == TrustState::WaitingForPairing),
        TrustState::WaitingForPairing
    );
    // ...then, with the window open, both sides show the same code; approve.
    authority.open_pairing(Duration::from_secs(60));
    let Some(AuthorityEvent::JoinRequested(req)) = wait_event(
        authority,
        8,
        |e| matches!(e, AuthorityEvent::JoinRequested(r) if r.name == "B (App)"),
    ) else {
        panic!("no join request");
    };
    let TrustState::Pending { code } =
        wait_state(&b, 5, |s| matches!(s, TrustState::Pending { .. }))
    else {
        panic!("{:?}", b.state())
    };
    assert_eq!(code, req.code, "codes match on both sides");
    assert_eq!(req.fingerprint, b.fingerprint());
    authority.approve(req.id).unwrap();
    assert!(matches!(
        wait_state(&b, 5, |s| matches!(
            s,
            TrustState::Enrolled { connected: true }
        )),
        TrustState::Enrolled { connected: true }
    ));

    // 3. A wrong passphrase is not approved automatically: it waits for approval.
    let c = device(server, &site.dir.join("c"), "C (App)", Some("wrong"));
    let Some(AuthorityEvent::JoinRequested(rc)) = wait_event(
        authority,
        8,
        |e| matches!(e, AuthorityEvent::JoinRequested(r) if r.name == "C (App)"),
    ) else {
        panic!("wrong passphrase should fall back to approval");
    };
    assert!(matches!(
        wait_state(&c, 3, |s| matches!(s, TrustState::Pending { .. })),
        TrustState::Pending { .. }
    ));
    // 4. Deny it: refused, and again on retry.
    authority.deny(rc.id).unwrap();
    assert_eq!(
        wait_state(&c, 5, |s| *s == TrustState::Denied),
        TrustState::Denied
    );

    // 5. Revoke B: it is told, loses its certificate, and can't simply join again.
    authority.revoke(&b.fingerprint()).unwrap();
    assert_eq!(
        wait_state(&b, 5, |s| *s == TrustState::Revoked),
        TrustState::Revoked
    );
    assert!(!bdir.join("device.crt").exists());
    let devices = authority.devices();
    assert!(devices.iter().any(|d| d.name == "B (App)" && d.revoked));
    assert!(
        devices
            .iter()
            .any(|d| d.name == "A (App)" && !d.revoked && d.online)
    );
    // Asking again while revoked is refused; it stays "Revoked".
    b.retry_now();
    assert!(
        wait_event(
            authority,
            5,
            |e| matches!(e, AuthorityEvent::Refused { name, .. } if name == "B (App)")
        )
        .is_some()
    );
    assert_eq!(b.state(), TrustState::Revoked);

    // 6. Accept it again without restarting it: clear, retry, approve.
    authority.clear_denied(&b.fingerprint()).unwrap();
    b.retry_now();
    let Some(AuthorityEvent::JoinRequested(again)) = wait_event(
        authority,
        8,
        |e| matches!(e, AuthorityEvent::JoinRequested(r) if r.name == "B (App)"),
    ) else {
        panic!("cleared device did not ask again");
    };
    authority.approve(again.id).unwrap();
    assert!(matches!(
        wait_state(&b, 5, |s| matches!(
            s,
            TrustState::Enrolled { connected: true }
        )),
        TrustState::Enrolled { connected: true }
    ));
    // Listed once, in its latest state.
    let bs: Vec<_> = authority
        .devices()
        .into_iter()
        .filter(|d| d.name == "B (App)")
        .collect();
    assert_eq!(bs.len(), 1, "{bs:?}");
    assert!(!bs[0].revoked);
    let devices = authority.devices();

    // 7. The authority's state survives a restart: same CA, same devices.
    drop(site.ds);
    let again = Authority::open(AuthorityConfig {
        state_dir: site.dir.join("authority"),
        passphrase: None,
    })
    .unwrap();
    assert_eq!(again.ca_fingerprint(), ca);
    assert_eq!(again.devices().len(), devices.len());
}

/// A device stays enrolled through a restore of an older authority backup, and later
/// revocations still reach it (devices.tsv without it, crl.number behind it).
#[test]
fn restoring_an_older_backup_keeps_devices_enrolled_and_revocations_working() {
    let t = TempDir::new("trust-restore");
    let (adir, backup) = (t.join("auth"), t.join("backup"));
    let port = free_port();
    let server = format!("127.0.0.1:{port}");
    let open = || {
        let a = Authority::open(AuthorityConfig {
            state_dir: adir.clone(),
            passphrase: Some("hall".into()),
        })
        .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            match DiscoveryServer::start(port, Some(a.clone())) {
                Ok(ds) => return (a, ds),
                Err(e) if Instant::now() > end => panic!("{e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    };
    let (a, ds) = open();
    let x = device(&server, &t.join("x"), "X (App)", Some("hall"));
    wait_connected(&x);
    // The backup: X only.
    std::fs::create_dir_all(&backup).unwrap();
    for f in std::fs::read_dir(&adir).unwrap() {
        let f = f.unwrap().path();
        std::fs::copy(&f, backup.join(f.file_name().unwrap())).unwrap();
    }
    let ydir = t.join("y");
    let y = device(&server, &ydir, "Y (App)", Some("hall"));
    wait_connected(&y);
    // Some revocations after the backup move the CRL number on.
    for name in ["Z1", "Z2"] {
        let z = device(
            &server,
            &t.join(name),
            &format!("{name} (App)"),
            Some("hall"),
        );
        wait_connected(&z);
        a.revoke(&z.fingerprint()).unwrap();
        assert_eq!(
            wait_state(&z, 5, |s| *s == TrustState::Revoked),
            TrustState::Revoked
        );
    }

    // Restore the backup.
    drop((ds, a));
    for f in std::fs::read_dir(&backup).unwrap() {
        let f = f.unwrap().path();
        std::fs::copy(&f, adir.join(f.file_name().unwrap())).unwrap();
    }
    let (a, _ds) = open();
    // Y starts again (sessions of the old authority instance live on in this process).
    drop(y);
    let y = device(&server, &ydir, "Y (App)", Some("hall"));
    wait_connected(&y);
    assert!(ydir.join("device.crt").exists());
    // (The authority records the enrolment a moment after the device has it: under load
    // the check came too early.)
    let known = |a: &Authority| {
        a.devices()
            .iter()
            .any(|d| d.name == "Y (App)" && !d.revoked)
    };
    let ok = wait_until(Duration::from_secs(10), || known(&a));
    assert!(ok, "Y is known again: {:?}", a.devices());
    // A revocation now still reaches Y: it takes the new CRL.
    let before = std::fs::read(ydir.join("crl.der")).unwrap();
    a.revoke(&x.fingerprint()).unwrap();
    wait_for("Y to take the new CRL", Duration::from_secs(5), || {
        std::fs::read(ydir.join("crl.der")).unwrap() != before
    });
    wait_connected(&y);
}

#[test]
fn a_server_without_authority_is_detected() {
    let t = TempDir::new("trust-noauth");
    let ds = DiscoveryServer::start(0, None).unwrap();
    let e = device(
        &format!("127.0.0.1:{}", ds.port()),
        &t,
        "X (App)",
        Some("pw"),
    );
    assert_eq!(
        wait_state(&e, 6, |s| *s == TrustState::NoAuthority),
        TrustState::NoAuthority
    );
}

mod sessions {
    use super::*;
    use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
    use idn_open_protocol::color::AlphaMode;
    use idn_open_protocol::input::{Input, InputConfig, InputEvent, Transport};
    use idn_open_protocol::secure::SecureConfig;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn output(
        port: u16,
        secure: Option<SecureConfig>,
    ) -> (Arc<Output>, Arc<AtomicBool>, Publisher) {
        let publisher = Publisher::new(PublisherConfig {
            mdns: Some(false),
            secure,
            ..Default::default()
        })
        .unwrap();
        let out = Arc::new(
            Output::new(
                &publisher,
                OutputConfig {
                    port,
                    ..OutputConfig::new("Trusted")
                },
            )
            .unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (o, s) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let _ = o.send(VideoFrame::rgba(
                    160,
                    90,
                    vec![90u8; 160 * 90 * 4].into(),
                    AlphaMode::Opaque,
                ));
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        (out, stop, publisher)
    }

    fn videos(input: &Input, secs: u64) -> usize {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut n = 0;
        while Instant::now() < end {
            if let Some(InputEvent::Video(_)) = input.recv(Duration::from_millis(50)) {
                n += 1;
            }
        }
        n
    }

    /// Over the IDN link (§E.11, mode s2), or with the link off on the receiver over TCP
    /// with secure QUIC media (§E.8.4, §E.10.7): encrypted with device certificates, and
    /// cut off by a revocation.
    fn enrolled_devices_encrypt_and_revocation_cuts_them_off(tag: &str, link: bool) {
        let site = site(&format!("trust-{tag}"), Some("hall"));
        let (e_out, e_in) = (
            site.member(&format!("{tag}out (App)")),
            site.member(&format!("{tag}in (App)")),
        );
        let port = free_port();
        let (_out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out.clone())));
        let mut cfg = InputConfig {
            secure: Some(SecureConfig::with_site(e_in.clone())),
            // Over the network, which is what is tested: on this machine the ring could
            // take the pictures first (now and then, on a loaded machine).
            machine: false,
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        };
        if !link {
            cfg.transports = vec![Transport::Quic];
        }
        let input = Input::open(cfg).unwrap();
        assert!(videos(&input, 4) > 20);
        let st = input.stats();
        assert!(
            st.encrypted && st.site_trust,
            "secure/2 in use: {:?}",
            st.extensions
        );
        let (via, mode) = if link {
            ("video via link", Some("s2"))
        } else {
            ("video via squic", None)
        };
        assert!(st.via.keys().any(|k| k == via), "{:?}", st.via);
        assert_eq!(st.link_mode, mode);

        // Revoke the receiving device: the live session is cut and it can't come back,
        // not even in plain text or with the passphrase.
        site.authority.revoke(&e_in.fingerprint()).unwrap();
        // Told at once (lab X-t4: the notice used to be lost and took ~8 s).
        assert_eq!(
            wait_state(&e_in, 2, |s| *s == TrustState::Revoked),
            TrustState::Revoked
        );
        std::thread::sleep(Duration::from_millis(500));
        while input.try_recv().is_some() {}
        let before = input.stats().reconnects;
        let (mut n, mut told) = (0, false);
        let end = Instant::now() + Duration::from_secs(6);
        while Instant::now() < end {
            match input.recv(Duration::from_millis(50)) {
                Some(InputEvent::Video(_)) => n += 1,
                Some(InputEvent::Error(e)) if e.contains("refused by the source") => told = true,
                _ => {}
            }
        }
        assert_eq!(n, 0, "revoked device still receiving");
        assert!(told, "the revoked receiver wasn't told why (lab item 92)");
        // And it doesn't storm the sender with reconnects (lab: 421 in 8.5 s).
        let attempts = input.stats().reconnects - before;
        assert!(
            attempts <= 4,
            "{attempts} reconnects in 6 s after revocation"
        );
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn enrolled_devices_encrypt_over_the_link_and_revocation_cuts_them_off() {
        enrolled_devices_encrypt_and_revocation_cuts_them_off("s1", true);
    }

    #[test]
    fn enrolled_devices_encrypt_over_tcp_and_revocation_cuts_them_off() {
        enrolled_devices_encrypt_and_revocation_cuts_them_off("s1t", false);
    }

    #[test]
    fn a_session_upgrades_to_site_trust_once_the_device_is_approved() {
        let site = site("trust-s3", Some("hall"));
        let e_out = site.member("out3 (App)");
        let port = free_port();
        let (_out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out)));
        // No passphrase: it needs approval. Until then it is an outsider, and a site
        // member gives it nothing.
        site.authority.open_pairing(Duration::from_secs(60));
        let e_in = device(&site.server, &site.dir.join("in3"), "in3 (App)", None);
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(e_in.clone())),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert_eq!(videos(&input, 2), 0);
        assert!(!input.stats().site_trust);
        let Some(AuthorityEvent::JoinRequested(r)) = wait_event(&site.authority, 8, |e| {
            matches!(e, AuthorityEvent::JoinRequested(_))
        }) else {
            panic!()
        };
        site.authority.approve(r.id).unwrap();
        // It reconnects by itself and receives, now with site trust.
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end && !input.stats().site_trust {
            input.recv(Duration::from_millis(50));
        }
        assert!(input.stats().site_trust, "upgraded to secure/2");
        assert!(videos(&input, 2) > 10);
        stop.store(true, Ordering::Relaxed);
    }

    /// Over TCP such a pair is unencrypted (§E.10.7); the IDN link gives it mode open
    /// (§E.11.4): encrypted, not authenticated.
    #[test]
    fn devices_outside_the_site_get_nothing_from_enrolled_ones() {
        // A device in a site takes no unauthenticated IDN
        // connection, neither over the link's mode open nor in plain text.
        let site = site("trust-s2", Some("hall"));
        let e_out = site.member("out2 (App)");
        let port = free_port();
        let (_out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out)));
        // Knows the passphrase but has no site certificate; and one with no encryption.
        for secure in [Some(SecureConfig::new("hall").unwrap()), None] {
            let input = Input::open(InputConfig {
                secure,
                ..InputConfig::new(&format!("127.0.0.1:{port}"))
            })
            .unwrap();
            // Nothing, and told why.
            let (mut n, mut told) = (0, false);
            let end = Instant::now() + Duration::from_secs(4);
            while Instant::now() < end {
                match input.recv(Duration::from_millis(50)) {
                    Some(InputEvent::Video(_)) => n += 1,
                    Some(InputEvent::Error(e)) if e.contains("refused by the source") => {
                        told = true
                    }
                    _ => {}
                }
            }
            assert_eq!(n, 0, "an outsider received from a site member");
            assert!(told, "the refused receiver wasn't told why");
        }
        // A member of the site still does.
        let e_in = site.member("in2 (App)");
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(e_in)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert!(videos(&input, 4) > 20);
        assert!(input.stats().site_trust);
        stop.store(true, Ordering::Relaxed);
    }

    /// A receiver's messages say whether they came from a device of the site
    /// (`Output::recv_messages`): a mixer takes a cue's scene only from one (§M.21).
    #[test]
    fn a_receivers_message_says_whether_it_came_from_the_site() {
        let site = site("trust-msg", Some("hall"));
        let (e_out, e_in) = (site.member("msgout (App)"), site.member("msgin (App)"));
        let wait_for = |out: &Output| {
            let end = Instant::now() + Duration::from_secs(6);
            loop {
                let m = out.recv_messages();
                if !m.is_empty() || Instant::now() > end {
                    return m;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        // Both in the site.
        let port = free_port();
        let (out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out)));
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(e_in)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert!(videos(&input, 2) > 5);
        input.send_metadata(r#"<idn_desk_snapshot name="Sermon"/>"#);
        let got = wait_for(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].site && got[0].xml.contains("Sermon"), "{got:?}");
        stop.store(true, Ordering::Relaxed);
        // Neither in a site: the message comes, but not from the site.
        let port = free_port();
        let (out, stop, _p) = output(port, None);
        let input = Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
        assert!(videos(&input, 2) > 5);
        input.send_metadata(r#"<idn_desk_snapshot name="Sermon"/>"#);
        let got = wait_for(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(!got[0].site, "{got:?}");
        stop.store(true, Ordering::Relaxed);
    }

    /// Remote control (R.25): keyboard and mouse from a device of the site are taken
    /// (`Output::recv_control`); from outside it, refused (tests/remote.rs).
    #[test]
    fn control_from_a_device_of_the_site_is_taken() {
        use idn_open_protocol::remote::Control;
        let site = site("trust-ctl", Some("hall"));
        let (e_out, e_in) = (site.member("ctlout (App)"), site.member("ctlin (App)"));
        let port = free_port();
        let (out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out)));
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(e_in)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert!(videos(&input, 2) > 5);
        let sent = vec![
            Control::Move { x: 0.25, y: 0.5 },
            Control::Button {
                button: 0,
                down: true,
            },
            Control::Button {
                button: 0,
                down: false,
            },
            Control::Key {
                code: "Enter".into(),
                down: true,
            },
        ];
        input.send_control(&sent);
        let end = Instant::now() + Duration::from_secs(6);
        let (mut got, mut refused) = (Vec::new(), 0);
        while got.len() < sent.len() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
            let (e, r) = out.recv_control();
            got.extend(e);
            refused += r;
        }
        assert_eq!((got, refused), (sent, 0));
        stop.store(true, Ordering::Relaxed);
    }

    /// Linked computers (§E.28): two machines in no site that list each other encrypt
    /// with their device keys (over the IDN link here, and over TCP), and control from
    /// one is taken by the other; a machine not linked both ways isn't authenticated and
    /// its control is refused; a machine in a site doesn't offer its links.
    #[test]
    fn linked_computers_encrypt_and_take_control_without_a_site() {
        use idn_open_protocol::remote::Control;
        use idn_open_protocol::trust::pair::Linked;
        // No authority anywhere: nothing listens at this server.
        let nowhere = format!("127.0.0.1:{}", free_port());
        let t = TempDir::new("trust-linked");
        let dev = |tag: &str| device(&nowhere, &t.join(tag), &format!("{tag} (App)"), None);
        let (a, b, c) = (dev("pra"), dev("prb"), dev("prc"));
        let link = |e: &Enrollment, to: &Enrollment, name: &str| {
            e.set_pairs(vec![Linked {
                fp: to.fingerprint(),
                name: name.into(),
                since: 0,
                once: false,
            }])
        };
        link(&a, &b, "B");
        link(&b, &a, "A");
        // C lists A, but A doesn't list C.
        link(&c, &a, "A");
        let port = free_port();
        let (out, stop, _p) = output(port, Some(SecureConfig::with_site(a.clone())));
        let open = |e: &Arc<Enrollment>, name: &str| {
            Input::open(InputConfig {
                receiver_name: name.into(),
                secure: Some(SecureConfig::with_site(e.clone())),
                ..InputConfig::new(&format!("127.0.0.1:{port}"))
            })
            .unwrap()
        };
        let input = open(&b, "linked");
        assert!(videos(&input, 2) > 5);
        let st = input.stats();
        assert!(st.encrypted && st.linked && !st.site_trust, "{st:?}");
        let sent = vec![Control::Rel { dx: 3, dy: -4 }];
        input.send_control(&sent);
        let end = Instant::now() + Duration::from_secs(6);
        let mut got = Vec::new();
        while got.is_empty() && Instant::now() < end {
            let (e, r) = out.wait_control(Duration::from_millis(500));
            assert_eq!(r, 0);
            got.extend(e);
        }
        assert_eq!(got, sent);
        drop(input);
        // Over TCP with secure QUIC (the IDN link off on the receiver): `pair/1` too.
        let tcp = Input::open(InputConfig {
            receiver_name: "linked over TCP".into(),
            secure: Some(SecureConfig::with_site(b.clone())),
            transports: vec![Transport::Quic],
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert!(videos(&tcp, 3) > 5);
        let st = tcp.stats();
        assert!(
            st.encrypted && st.linked && st.link_mode.is_none(),
            "{st:?}"
        );
        drop(tcp);
        // Not linked both ways: not authenticated (the IDN link's mode open still
        // encrypts), control refused.
        let other = open(&c, "not linked");
        // (5 s: with the other tests running alongside, 2 s was too few once.)
        assert!(videos(&other, 5) > 5);
        let st = other.stats();
        assert!(!st.linked && !st.site_trust, "{st:?}");
        other.send_control(&sent);
        let end = Instant::now() + Duration::from_secs(6);
        let mut refused = 0;
        while refused == 0 && Instant::now() < end {
            let (e, r) = out.wait_control(Duration::from_millis(500));
            assert!(e.is_empty());
            refused += r;
        }
        assert_eq!(refused, 1);
        stop.store(true, Ordering::Relaxed);
        // In a site: its links aren't offered.
        assert!(
            SecureConfig::with_site(a.clone())
                .version(true)
                .contains("pair/1")
        );
        let site = site("trust-prsite", Some("hall"));
        let member = site.member("prmember (App)");
        link(&member, &a, "A");
        assert!(
            !SecureConfig::with_site(member)
                .version(true)
                .contains("pair/1")
        );
    }

    /// Remote control for games (R.25 stage 2): relative mouse, a gamepad and a latency
    /// probe from a device of the site wake `Output::wait_control`, and the probe's answer
    /// comes back to the receiver as metadata.
    #[test]
    fn game_control_is_taken_and_a_probe_answered() {
        use idn_open_protocol::input::InputEvent;
        use idn_open_protocol::remote::{Control, probe_answer, probe_of};
        let site = site("trust-game", Some("hall"));
        let (e_out, e_in) = (site.member("gameout (App)"), site.member("gamein (App)"));
        let port = free_port();
        let (out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out)));
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(e_in)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert!(videos(&input, 2) > 5);
        let sent = vec![
            Control::Rel { dx: -7, dy: 12 },
            Control::Pad {
                id: 1,
                buttons: 0b101,
                axes: [-32768, 32767, 0, 100, 0, 32767],
            },
            Control::Probe(4242),
        ];
        input.send_control(&sent);
        let end = Instant::now() + Duration::from_secs(6);
        let mut got = Vec::new();
        while got.len() < sent.len() && Instant::now() < end {
            let (e, r) = out.wait_control(Duration::from_secs(1));
            assert_eq!(r, 0);
            got.extend(e);
        }
        assert_eq!(got, sent);
        // Answered (idn-codec does so after its next picture): back at the receiver.
        out.send_metadata(&probe_answer(4242)).unwrap();
        let end = Instant::now() + Duration::from_secs(6);
        let mut answered = None;
        while answered.is_none() && Instant::now() < end {
            if let Some(InputEvent::Metadata { xml, .. }) = input.recv(Duration::from_millis(50)) {
                answered = probe_of(&xml);
            }
        }
        assert_eq!(answered, Some(4242));
        // Nothing waiting: it returns at its timeout, empty.
        let t = Instant::now();
        assert_eq!(out.wait_control(Duration::from_millis(100)), (vec![], 0));
        assert!(t.elapsed() >= Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
    }

    /// Lab item 92: the site's "require encryption" reaches a running output. An NDI
    /// receiver watching it is cut off within seconds, and refused when it reconnects.
    #[test]
    fn requiring_encryption_on_the_authority_cuts_off_running_ndi_sessions() {
        use idn_open_protocol::control;
        use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
        use std::io::Write;
        let site = site("trust-s5", Some("hall"));
        let e_out = site.member("out5 (App)");
        let port = free_port();
        let sc = SecureConfig::with_site(e_out.clone());
        let (_out, stop, _p) = output(port, Some(sc.clone()));
        // An NDI receiver: no IDN extensions in its version; media on the connection.
        let ndi = || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            for m in [
                control::ndi_version(),
                control::ndi_identify("NDIHOST (Lab)"),
                control::ndi_video("high"),
                control::ndi_enabled_streams_inline(true, false, false),
            ] {
                s.write_all(&Frame::text(kind::CONTROL, 0, &m).encode())
                    .unwrap();
            }
            s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            s
        };
        // Video frames until `until`, and whether the connection ended.
        let watch = |s: &mut std::net::TcpStream, until: Instant| {
            let (mut n, mut ended) = (0, false);
            while Instant::now() < until {
                match read_frame(s, &Limits::default()) {
                    Ok(Some(f)) if f.kind == kind::VIDEO => n += 1,
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        ended = true;
                        break;
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => {
                        ended = true;
                        break;
                    }
                }
            }
            (n, ended)
        };
        let mut s = ndi();
        let (n, ended) = watch(&mut s, Instant::now() + Duration::from_secs(2));
        assert!(
            n > 10 && !ended,
            "NDI receivers are allowed in a site: {n} {ended}"
        );
        assert!(!sc.requires());
        site.authority
            .set_option(&OptionScope::Site, "secure.require", "true")
            .unwrap();
        let (_, ended) = watch(&mut s, Instant::now() + Duration::from_secs(6));
        assert!(
            ended,
            "the running NDI session went on after secure.require"
        );
        assert!(sc.requires());
        let mut again = ndi();
        let (n, _) = watch(&mut again, Instant::now() + Duration::from_secs(3));
        assert_eq!(n, 0, "a reconnecting NDI receiver got video");
        stop.store(true, Ordering::Relaxed);
    }

    /// Lab item 92: a receiver revoked while it was offline still holds its certificate.
    /// The sender turns it down after the receiver's side of the link handshake is done;
    /// the receiver says so instead of "broken pipe".
    #[test]
    fn a_receiver_revoked_while_offline_is_told_why() {
        let site = site("trust-s6", Some("hall"));
        let e_out = site.member("out6 (App)");
        let e_in = site.member("in6 (App)");
        let fp = e_in.fingerprint();
        // Its saved state as it was before the revocation (a machine switched off).
        let src = site.dir.join("in6 (App)");
        let dir = site.dir.join("in6-offline");
        std::fs::create_dir_all(&dir).unwrap();
        // (Its files written whole, through a temporary file renamed into place: one
        // listed may be gone a moment later, its final name copied instead.)
        for f in std::fs::read_dir(&src).unwrap() {
            let f = f.unwrap().path();
            if f.is_file()
                && let Err(e) = std::fs::copy(&f, dir.join(f.file_name().unwrap()))
            {
                assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
            }
        }
        drop(e_in);
        site.authority.revoke(&fp).unwrap();
        let port = free_port();
        let (_out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out)));
        // Back from its saved state, the authority out of reach: it doesn't know.
        assert!(
            dir.join("device.crt").exists(),
            "the receiver kept its certificate"
        );
        let offline = device("127.0.0.1:9", &dir, "in6 (App)", Some("hall"));
        std::thread::sleep(Duration::from_millis(500));
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(offline)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        let (mut n, mut why) = (0, String::new());
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            match input.recv(Duration::from_millis(50)) {
                Some(InputEvent::Video(_)) => n += 1,
                Some(InputEvent::State(idn_open_protocol::input::ConnectionState::Lost(r)))
                    if why.is_empty() =>
                {
                    why = r
                }
                Some(InputEvent::Error(e)) if why.is_empty() => why = e,
                _ => {}
            }
        }
        assert_eq!(n, 0);
        assert!(why.contains("refused this device's certificate"), "{why}");
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_revoked_machines_output_is_refused_by_members() {
        // Lab item 81 gap a: a revoked machine reached members as an unknown device.
        let site = site("trust-s4", Some("hall"));
        let (e_out, e_in) = (site.member("out4 (App)"), site.member("in4 (App)"));
        let port = free_port();
        let (_out, stop, _p) = output(port, Some(SecureConfig::with_site(e_out.clone())));
        site.authority.revoke(&e_out.fingerprint()).unwrap();
        assert_eq!(
            wait_state(&e_out, 5, |s| *s == TrustState::Revoked),
            TrustState::Revoked
        );
        let input = Input::open(InputConfig {
            secure: Some(SecureConfig::with_site(e_in)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        assert_eq!(
            videos(&input, 5),
            0,
            "a member received from a revoked machine"
        );
        stop.store(true, Ordering::Relaxed);
    }
}

/// TR-4 / R1-114: an enrollment the application drops ends its connection, so the
/// authority sees its pending request go (it no longer lives on beside a new one).
#[test]
fn a_dropped_enrollment_withdraws_its_pending_join() {
    let site = site("trust-drop", None);
    let authority = &site.authority;
    authority.open_pairing(Duration::from_secs(60));
    let e = device(&site.server, &site.dir.join("dev"), "Old name (App)", None);
    assert!(matches!(
        wait_state(&e, 10, |s| matches!(s, TrustState::Pending { .. })),
        TrustState::Pending { .. }
    ));
    assert_eq!(authority.pending().len(), 1);
    drop(e);
    assert!(
        wait_event(authority, 5, |ev| matches!(
            ev,
            AuthorityEvent::JoinCancelled(_)
        ))
        .is_some(),
        "the pending join was withdrawn"
    );
    assert!(authority.pending().is_empty());
}

/// A named API key's use (idn-node.md §N.42): an engine on an enrolled machine checks
/// the key's token, the machine tells its site, the authority says `KeyUsed` (once a
/// minute a key: a second check says nothing).
#[test]
fn an_api_keys_use_reaches_the_site() {
    let site = site("trust-keys", Some("St Luke"));
    let authority = &site.authority;
    let dev = site.member("Booth (App)");
    let (tok, _) = authority
        .issue_key(
            "a1b2c3",
            "Stream Deck",
            "switcher",
            "*",
            idn_open_protocol::trust::token::Scope::Control,
        )
        .unwrap();
    let check = idn_open_protocol::web::TokenCheck::from_enrollment("switcher", dev.clone());
    // The site's keys reach the device a moment after it joins.
    let end = Instant::now() + Duration::from_secs(10);
    let claims = loop {
        match check.check(&tok) {
            Ok(c) => break c,
            Err(e) if Instant::now() > end => panic!("{e:?}"),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(claims.key);
    let ev = wait_event(authority, 5, |e| {
        matches!(e, AuthorityEvent::KeyUsed { .. })
    });
    match ev {
        Some(AuthorityEvent::KeyUsed { key, machine }) => {
            assert_eq!((key.as_str(), machine.as_str()), ("a1b2c3", "Booth (App)"));
        }
        other => panic!("{other:?}"),
    }
    check.check(&tok).unwrap();
    assert!(
        wait_event(authority, 2, |e| matches!(
            e,
            AuthorityEvent::KeyUsed { .. }
        ))
        .is_none()
    );
}
