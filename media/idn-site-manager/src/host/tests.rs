//! The hosted Site Manager's tests.

use super::*;
use idn_common::sync::LockOk as _;

/// Started in this process: its authority, discovery server and API answer; a
/// password set through the handle; stopped (no Voice, no mDNS from tests).
#[test]
fn a_site_manager_hosted_in_this_process() {
    // SAFETY: set before the Site Manager reads it; this test is its module's only one.
    unsafe { std::env::set_var("IDN_VOICE_PORT", "off") };
    let dir = idn_common::testing::TempDir::new("idn-sm-host");
    let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = lines.clone();
    let config = Config {
        dir: dir.to_path_buf(),
        passphrase: None,
        discovery_port: idn_common::testing::test_port(),
        api_port: Some(idn_common::testing::test_port()),
        announce: false,
        open_new_site: Some(Duration::from_secs(60)),
        password: Some("a-long-site-password".into()),
        restored: false,
        host: None,
        voice: true,
    };
    let site = start(config.clone(), move |l| seen.lock_ok().push(l)).expect("it starts");
    assert!(!site.authority().ca_fingerprint().is_empty());
    let (port, fp) = site.api().expect("its API serves");
    assert!(!fp.is_empty());
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    assert!(std::net::TcpStream::connect(("127.0.0.1", site.discovery_port())).is_ok());
    let mut left_open = events_channel(port);
    // Typed commands, as a user: their role checked, audited as theirs.
    use crate::typed::SiteCommand;
    site.run("admin", &SiteCommand::Pairing { seconds: Some(60) })
        .expect("the admin may open the site");
    events_health_and_callers(&site);
    assert!(
        site.run("nobody", &SiteCommand::Close {}).is_err(),
        "no such user: refused"
    );
    // The matrix's cells, copies and templates answered (no machines here).
    let cell = SiteCommand::RouteCell {
        receiver_id: "idn/ab/Desk".into(),
        ch: 0,
        tx_id: "A (Mic)".into(),
        tx_ch: 0,
        on: Some(true),
    };
    let e = site.run("admin", &cell).unwrap_err();
    assert!(e.contains("no receiver"), "{e}");
    let copy = SiteCommand::CopySettings {
        from: "ab".into(),
        to: vec!["cd".into()],
        groups: vec![],
        pin: None,
    };
    let e = site.run("admin", &copy).unwrap_err();
    assert!(e.contains("isn't reporting"), "{e}");
    let tpl = SiteCommand::ApplyTemplate {
        name: "Booth".into(),
        to: vec!["cd".into()],
    };
    assert!(site.run("admin", &tpl).unwrap_err().contains("no template"));
    // Join codes (extensions §E.10.10): a new site takes them; issued to an operator
    // and up, shown once, listed without the code, audited.
    assert_eq!(
        site.authority().join_mode(),
        idn_open_protocol::trust::JoinMode::CodeOrApproval
    );
    assert_eq!(
        crate::users::needs("/api/v1/join_code", &serde_json::Value::Null),
        crate::users::Role::Admin,
        "a join code lets a machine in, as approving does"
    );
    let issued = site
        .run("admin", &SiteCommand::JoinCode { cancel: None })
        .expect("a join code");
    let code = issued["code"].as_str().unwrap_or("");
    assert!(
        code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
        "{issued}"
    );
    assert!(issued["expires"].as_u64().is_some(), "{issued}");
    let listed = site.authority().join_codes();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].issuer, "admin");
    let audit = site.api.as_ref().unwrap().state.users.audit_log(50);
    let entry = audit
        .iter()
        .find(|e| e["action"] == "join_code")
        .expect("the issue audited");
    assert!(
        !entry.to_string().contains(code),
        "the code isn't in the audit"
    );
    site.run(
        "admin",
        &SiteCommand::JoinCode {
            cancel: Some(listed[0].id),
        },
    )
    .expect("taken back");
    assert!(site.authority().join_codes().is_empty());
    tokens_by_role(&site);
    shared_front(&site);
    site.set_password("another-long-password").unwrap();
    // The site password is the admin's: their tokens withdrawn.
    let nb = idn_open_protocol::trust::token::not_before_key(crate::users::ADMIN);
    assert!(
        site.authority()
            .options()
            .entries()
            .iter()
            .any(|(_, k, _)| *k == nb),
        "the admin's not-before is a site option"
    );
    let said = lines.lock_ok().join("\n");
    assert!(said.contains("site authority: CA fingerprint"), "{said}");
    assert!(said.contains("window for machines is open"), "{said}");
    let ds_port = site.discovery_port();
    site.stop();
    // Stopped is stopped: the API's port free at once, its push channel ended.
    assert!(
        std::net::TcpListener::bind(("0.0.0.0", port)).is_ok(),
        "the API's port is free once stop() returns"
    );
    left_open
        .get_ref()
        .sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match left_open.read() {
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        assert!(
            std::time::Instant::now() < end,
            "the push channel ended with the Site Manager"
        );
    }
    // A password too short is said, not a failed start; started again on the same
    // ports.
    let seen = lines.clone();
    let again = start(
        Config {
            password: Some("short".into()),
            ..config.clone()
        },
        move |l| seen.lock_ok().push(l),
    )
    .expect("it starts again, a bad password said");
    assert_eq!(again.discovery_port(), ds_port);
    assert_eq!(again.api().map(|a| a.0), Some(port));
    let said = lines.lock_ok().join("\n");
    assert!(said.contains("site password not taken"), "{said}");
    assert_eq!(again.api_error(), None);
    // Its API port taken by another program: no other port tried, the start not failed;
    // the site runs without the API and says why.
    let other = idn_common::testing::TempDir::new("idn-sm-host-taken");
    let seen = lines.clone();
    let taken = start(
        Config {
            dir: other.to_path_buf(),
            discovery_port: idn_common::testing::test_port(),
            password: None,
            ..config
        },
        move |l| seen.lock_ok().push(l),
    )
    .expect("it starts without its API");
    assert_eq!(taken.api(), None);
    let why = taken.api_error().expect("why the API isn't served");
    assert!(why.contains(&port.to_string()), "{why}");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", taken.discovery_port())).is_ok(),
        "its discovery server serves"
    );
    taken.stop();
    again.stop();
}

/// Tokens for the engines (§N.31.5): each role gets the scopes it may have, signed by
/// the site; a host's own user gets one without a role; a user changed has their
/// tokens withdrawn (a not-before every machine gets); the audit says who asked.
/// The host's typed view: a machine asking to join, approved for a
/// caller, coming online and going: typed events in order, waited for; its health; the
/// audit with the caller's address and client.
fn events_health_and_callers(site: &Handle) {
    use crate::typed::{SiteCommand, TokenAud};
    use idn_open_protocol::trust::{DeviceRole, Enrollment, EnrollmentConfig};
    let start = site.events(0).next;
    let ddir = idn_common::testing::TempDir::new("idn-sm-host-dev");
    let dev = Enrollment::start(EnrollmentConfig {
        discovery_server: format!("127.0.0.1:{}", site.discovery_port()),
        state_dir: ddir.to_path_buf(),
        name: "Stage PC".into(),
        role: DeviceRole::App,
        passphrase: None,
        admin_code: None,
    })
    .unwrap();
    // Waits for an event `pick` takes; the place after it.
    let wait_for = |from: u64, what: &str, pick: &dyn Fn(&SiteEvent) -> bool| {
        let end = std::time::Instant::now() + Duration::from_secs(15);
        let mut at = from;
        while std::time::Instant::now() < end {
            let got = site.events_wait(at, Duration::from_millis(500));
            assert!(!got.missed);
            for e in &got.events {
                if pick(&e.event) {
                    return (e.clone(), e.seq);
                }
            }
            at = got.next;
        }
        panic!("no {what} event: {:?}", site.events(from).events);
    };
    let (asked, at) = wait_for(
        start,
        "join_requested",
        &|e| matches!(e, SiteEvent::JoinRequested { name, .. } if name == "Stage PC"),
    );
    let SiteEvent::JoinRequested {
        id, fingerprint, ..
    } = asked.event
    else {
        unreachable!()
    };
    assert_eq!(asked.level, "info");
    let hostapp = Caller {
        address: "192.168.1.20".into(),
        client: "Host web".into(),
    };
    // Named as it is approved: the name it asked with gives way.
    site.run_from(
        "admin",
        &hostapp,
        &SiteCommand::Approve {
            id,
            name: Some(" Booth ".into()),
        },
    )
    .expect("approved");
    let fp = fingerprint.clone();
    let (_, at) = wait_for(
        at,
        "device_joined",
        &|e| matches!(e, SiteEvent::DeviceJoined { fingerprint, .. } if *fingerprint == fp),
    );
    let fp = fingerprint.clone();
    let (_, at) = wait_for(
        at,
        "device_online",
        &|e| matches!(e, SiteEvent::DeviceOnline { fingerprint, .. } if *fingerprint == fp),
    );
    let h = site.device_health();
    let me = h
        .iter()
        .find(|d| d.fingerprint == fingerprint)
        .expect("listed");
    assert!(me.online && me.attention.is_empty(), "{me:?}");
    assert_eq!((me.name.as_str(), me.kind.as_str()), ("Booth", "app"));
    assert!(me.services.is_empty(), "no Node: {me:?}");
    assert!(
        me.machine.is_none() && me.node_started.is_none(),
        "no Node: {me:?}"
    );
    let listed = site.authority().devices();
    let rec = listed
        .iter()
        .find(|d| d.fingerprint == fingerprint)
        .unwrap();
    assert_eq!(rec.name, "Booth");
    assert_eq!(dev.name_in_site().as_deref(), Some("Booth"));
    drop(dev);
    let fp = fingerprint.clone();
    let (_, at) = wait_for(
        at,
        "device_offline",
        &|e| matches!(e, SiteEvent::DeviceOffline { fingerprint, .. } if *fingerprint == fp),
    );
    let app = TokenAud {
        engine: "app:hostapp".into(),
        machine: None,
    };
    assert!(
        !site
            .device_health()
            .iter()
            .any(|d| d.fingerprint == fingerprint && d.online)
    );
    // A relay: another machine joins, relays a client app's pairing (with its PIN) and a
    // second one; the host sees them, approves the first (the key comes back through
    // the relay, which keeps nothing), refuses the second; then the machine leaves.
    let rdir = idn_common::testing::TempDir::new("idn-sm-host-relay");
    let relay = Enrollment::start(EnrollmentConfig {
        discovery_server: format!("127.0.0.1:{}", site.discovery_port()),
        state_dir: rdir.to_path_buf(),
        name: "Balcony box".into(),
        role: DeviceRole::App,
        passphrase: None,
        admin_code: None,
    })
    .unwrap();
    let (asked, at) = wait_for(
        at,
        "the relay's join",
        &|e| matches!(e, SiteEvent::JoinRequested { name, .. } if name == "Balcony box"),
    );
    let SiteEvent::JoinRequested {
        id,
        fingerprint: rfp,
        ..
    } = asked.event
    else {
        unreachable!()
    };
    site.run_from("admin", &hostapp, &SiteCommand::Approve { id, name: None })
        .expect("approved");
    let fp = rfp.clone();
    let (_, at) = wait_for(
        at,
        "the relay online",
        &|e| matches!(e, SiteEvent::DeviceOnline { fingerprint, .. } if *fingerprint == fp),
    );
    let ticket = relay
        .request_key("Presenter iPad", Some("2468"))
        .expect("relayed");
    let (asked, at) = wait_for(
        at,
        "key_requested",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Presenter iPad"),
    );
    let SiteEvent::KeyRequested {
        id: kid, via, pin, ..
    } = asked.event
    else {
        unreachable!()
    };
    assert_eq!((via.as_str(), pin), ("Balcony box", true));
    let waiting = site.key_requests();
    let w = waiting.iter().find(|r| r.id == kid).expect("listed");
    assert_eq!(
        w.pin.as_deref(),
        Some("2468"),
        "the PIN, for the host's PIN level"
    );
    assert!(relay.key_answer(ticket).is_none(), "still waiting");
    let made = site
        .approve_key_request(kid, "admin", &hostapp, &app, "control", Some(365))
        .expect("approved");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let grant = loop {
        if let Some(g) = relay.key_answer(ticket) {
            break g;
        }
        assert!(
            std::time::Instant::now() < end,
            "no answer through the relay"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(grant.approved, "{grant:?}");
    assert_eq!(grant.key, made["key"].as_str().unwrap());
    let c = site
        .verify_token(&grant.key, "hostapp")
        .expect("the relayed key works");
    assert!(c.exp - c.iat <= 365 * 86_400 && c.exp - c.iat > 364 * 86_400);
    let listed = site.api_keys();
    let k = listed
        .iter()
        .find(|k| k.id == grant.id)
        .expect("in the list");
    assert_eq!(
        (k.name.as_str(), k.by.as_str()),
        ("Presenter iPad", "admin (via Balcony box)")
    );
    assert_eq!(
        (k.engine.as_str(), k.scope.as_str(), k.revoked),
        ("app:hostapp", "control", false)
    );
    let t2 = relay.request_key("Stranger", None).unwrap();
    let (asked, at) = wait_for(
        at,
        "the second",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Stranger"),
    );
    let SiteEvent::KeyRequested { id: k2, pin, .. } = asked.event else {
        unreachable!()
    };
    assert!(!pin);
    site.deny_key_request(k2, "admin", &hostapp, "not now")
        .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let no = loop {
        if let Some(g) = relay.key_answer(t2) {
            break g;
        }
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!no.approved && no.error == "not now", "{no:?}");
    // The pairing code comes from the site: the relay asks for it and
    // shows it; a client typing it is told to the host as `code_ok` (checked here, used
    // up); a wrong one isn't. Only to a machine marked as a pairing relay;
    // before that, it is refused one.
    let e = relay.pairing_code(Duration::from_secs(5)).unwrap_err();
    assert!(e.to_string().contains("pairing relay"), "{e}");
    site.run(
        "admin",
        &crate::typed::SiteCommand::Relay {
            fingerprint: rfp.clone(),
            on: true,
        },
    )
    .expect("marked a relay");
    assert!(
        site.device_health()
            .iter()
            .any(|d| d.fingerprint == rfp && d.relay)
    );
    let (code, until) = relay.pairing_code(Duration::from_secs(5)).expect("a code");
    assert!(
        code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
        "{code}"
    );
    assert!(until > idn_common::epoch::secs());
    assert_eq!(
        relay.pairing_code(Duration::from_secs(5)).unwrap().0,
        code,
        "the same while it lasts"
    );
    let right = relay.request_key("Presenter Phone", Some(&code)).unwrap();
    let (asked, at) = wait_for(
        at,
        "the coded one",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Presenter Phone"),
    );
    let SiteEvent::KeyRequested {
        id: kc, code_ok, ..
    } = asked.event
    else {
        unreachable!()
    };
    assert!(code_ok, "the site's code, typed right");
    assert!(site.key_requests().iter().any(|r| r.id == kc && r.code_ok));
    site.deny_key_request(kc, "admin", &hostapp, "test")
        .unwrap();
    let _ = right;
    let used = relay.request_key("Presenter Again", Some(&code)).unwrap();
    let (asked, at) = wait_for(
        at,
        "the reused one",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Presenter Again"),
    );
    let SiteEvent::KeyRequested {
        id: ku, code_ok, ..
    } = asked.event
    else {
        unreachable!()
    };
    assert!(!code_ok, "a used code is gone");
    site.deny_key_request(ku, "admin", &hostapp, "test")
        .unwrap();
    let _ = used;
    let (fresh, _, left) = relay.pairing_code_left(Duration::from_secs(5)).unwrap();
    assert!(
        left > Duration::ZERO && left <= Duration::from_secs(300),
        "{left:?}"
    );
    let wrong = if fresh == "000000" {
        "111111"
    } else {
        "000000"
    };
    let tw = relay.request_key("Presenter Wrong", Some(wrong)).unwrap();
    // The relay is told the code was wrong at once (and it still waits for a person).
    let end = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match relay.key_status(tw) {
            idn_open_protocol::trust::KeyStatus::Waiting { code_wrong: true } => break,
            idn_open_protocol::trust::KeyStatus::Waiting { .. } => {}
            other => panic!("answered already: {other:?}"),
        }
        assert!(
            std::time::Instant::now() < end,
            "never told the code was wrong"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (asked, at) = wait_for(
        at,
        "the wrong one",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Presenter Wrong"),
    );
    let SiteEvent::KeyRequested {
        id: kw, code_ok, ..
    } = asked.event
    else {
        unreachable!()
    };
    assert!(!code_ok, "a wrong code");
    // Its client asks again: the relay takes the first back, gone from the host's list
    // at once.
    relay.withdraw_key_request(tw).unwrap();
    let (_, at) = wait_for(
        at,
        "the wrong one taken back",
        &|e| matches!(e, SiteEvent::KeyRequestCancelled { id } if *id == kw),
    );
    assert!(site.key_requests().iter().all(|r| r.id != kw));
    assert!(relay.withdraw_key_request(tw).is_err(), "once");
    assert!(
        site.deny_key_request(kw, "admin", &hostapp, "test")
            .is_err()
    );
    // A code's life shortened (tests): the code now in use was made with the long
    // life, so it is used up first; the next lives 600 ms, then a new one comes.
    site.authority()
        .set_pairing_code_life(Duration::from_millis(600));
    let (cur, _) = relay.pairing_code(Duration::from_secs(5)).unwrap();
    relay.request_key("Presenter Burn", Some(&cur)).unwrap();
    let (asked, at) = wait_for(
        at,
        "the burning one",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Presenter Burn"),
    );
    let SiteEvent::KeyRequested { id: kb, .. } = asked.event else {
        unreachable!()
    };
    site.deny_key_request(kb, "admin", &hostapp, "test")
        .unwrap();
    let (c1, _) = relay.pairing_code(Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(900));
    let (c2, _) = relay.pairing_code(Duration::from_secs(5)).unwrap();
    assert_ne!(c1, c2, "a new code after its life");
    site.authority()
        .set_pairing_code_life(Duration::from_secs(300));
    // A pairing nobody answers ends: the relay is told (refused), not left waiting.
    site.authority()
        .set_key_request_life(Duration::from_millis(800));
    let t3 = relay.request_key("Forgotten", None).unwrap();
    let (asked, at) = wait_for(
        at,
        "the third",
        &|e| matches!(e, SiteEvent::KeyRequested { client, .. } if client == "Forgotten"),
    );
    let SiteEvent::KeyRequested { id: k3, .. } = asked.event else {
        unreachable!()
    };
    let (_, at) = wait_for(
        at,
        "key_request_cancelled",
        &|e| matches!(e, SiteEvent::KeyRequestCancelled { id } if *id == k3),
    );
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let ended = loop {
        if let Some(g) = relay.key_answer(t3) {
            break g;
        }
        assert!(std::time::Instant::now() < end, "the relay was never told");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        !ended.approved && ended.error.contains("ended unanswered"),
        "{ended:?}"
    );
    site.authority()
        .set_key_request_life(Duration::from_secs(600));
    // The relay leaves: listed no more; the key it relayed still works.
    relay.tell_leaving().unwrap();
    let fp = rfp.clone();
    wait_for(
        at,
        "device_left",
        &|e| matches!(e, SiteEvent::DeviceLeft { fingerprint, .. } if *fingerprint == fp),
    );
    assert!(!site.device_health().iter().any(|d| d.fingerprint == rfp));
    assert!(site.verify_token(&grant.key, "hostapp").is_ok());
    drop(relay);
    drop(rdir);
    // A service on this machine joins its own site at once, no click (and again: as it
    // is).
    let odir = idn_common::testing::TempDir::new("idn-sm-host-own");
    let own = site
        .join_own_site(&odir, "Media library", DeviceRole::App)
        .expect("joined its own site");
    assert!(own.creds().is_some());
    assert!(
        site.authority()
            .devices()
            .iter()
            .any(|d| d.fingerprint == own.fingerprint() && !d.revoked)
    );
    drop(own);
    let again = site
        .join_own_site(&odir, "Media library", DeviceRole::App)
        .expect("still a member");
    assert!(again.creds().is_some());
    // A membership of another site in its folder (this site restored from another's
    // backup): set aside, joined here anew with the same
    // identity.
    let oauth = idn_common::testing::TempDir::new("idn-sm-host-other-auth");
    let other =
        idn_open_protocol::trust::Authority::open(idn_open_protocol::trust::AuthorityConfig {
            state_dir: oauth.to_path_buf(),
            passphrase: Some("other".into()),
        })
        .unwrap();
    let ods = idn_open_protocol::discovery::DiscoveryServer::start(0, Some(other.clone())).unwrap();
    let xdir = idn_common::testing::TempDir::new("idn-sm-host-other");
    let x =
        idn_open_protocol::trust::Enrollment::start(idn_open_protocol::trust::EnrollmentConfig {
            discovery_server: format!("127.0.0.1:{}", ods.port()),
            state_dir: xdir.to_path_buf(),
            name: "Media library".into(),
            role: DeviceRole::App,
            passphrase: Some("other".into()),
            admin_code: None,
        })
        .unwrap();
    idn_common::testing::wait_for("in the other site", Duration::from_secs(10), || {
        x.creds().is_some()
    });
    let (xfp, xsite) = (x.fingerprint(), x.ca_id().unwrap());
    assert_ne!(xsite, site.authority().site_id());
    drop(x);
    let moved = site
        .join_own_site(&xdir, "Media library", DeviceRole::App)
        .expect("joined this site anew");
    assert_eq!(moved.ca_id(), Some(site.authority().site_id()));
    assert_eq!(moved.fingerprint(), xfp, "the same identity");
    assert!(
        site.authority()
            .devices()
            .iter()
            .any(|d| d.fingerprint == xfp && !d.revoked)
    );
    assert!(xdir.join(format!("old-site-{xsite}/device.crt")).is_file());
    drop(moved);
    drop(ods);
    drop(xdir);
    drop(oauth);
    // An embedding application's media library on this machine: served with its own
    // membership; a show box of the site finds it and reads the catalog.
    let ldir = idn_common::testing::TempDir::new("idn-sm-host-lib");
    std::fs::create_dir_all(ldir.join("Media")).unwrap();
    std::fs::write(ldir.join("Media/Walk in.mp4"), b"walk-in loop").unwrap();
    let library =
        idn_open_protocol::library::server::LibraryServer::open(&ldir, "Media library").unwrap();
    let _serving = library.serve(again.clone()).expect("served");
    let sdir = idn_common::testing::TempDir::new("idn-sm-host-show");
    let show = site
        .join_own_site(&sdir, "Show box", DeviceRole::App)
        .unwrap();
    let lib = idn_open_protocol::library::Library::find(show, Duration::from_secs(20))
        .expect("the library found");
    assert_eq!(lib.host_name(), "Media library");
    let cat = lib.catalog().unwrap();
    let item = cat
        .items
        .iter()
        .find(|i| i.path == "Media/Walk in.mp4")
        .expect("listed");
    let got = sdir.join("got.mp4");
    lib.fetch(&item.id, &got, &mut |_, _| {}).unwrap();
    assert_eq!(std::fs::read(&got).unwrap(), b"walk-in loop");
    drop(lib);
    drop(_serving);
    drop(library);
    drop(again);
    drop((odir, ldir, sdir));
    // The audit: the approval and a token, with whom the host made them for.
    site.issue_token_from(
        "hostapp:ipad",
        &hostapp,
        &TokenAud {
            engine: "desk".into(),
            machine: None,
        },
        "read",
        Some(Duration::from_secs(60)),
    )
    .expect("a token");
    let audit = site.api.as_ref().unwrap().state.users.audit_log(50);
    for action in ["approve", "token (host)"] {
        let e = audit
            .iter()
            .find(|e| e["action"].as_str().is_some_and(|a| a.ends_with(action)))
            .unwrap_or_else(|| panic!("{action} audited: {audit:?}"));
        assert_eq!(
            (e["peer"].as_str(), e["client"].as_str()),
            (Some("192.168.1.20"), Some("Host web")),
            "{e}"
        );
    }
    // A token for the host program's own API (`app:hostapp`), checked by the host;
    // withdrawn with its user; another app's refused.
    let got = site
        .issue_token_from("hostapp:ann", &hostapp, &app, "control", None)
        .expect("an app token");
    let tok = got["token"].as_str().unwrap().to_owned();
    let c = site.verify_token(&tok, "hostapp").expect("valid");
    assert_eq!(
        (c.sub.as_str(), c.engine.as_str()),
        ("hostapp:ann", "app:hostapp")
    );
    assert!(site.verify_token(&tok, "other").is_err());
    assert!(site.verify_token(&tok, "Bad Name").is_err());
    let desk = site
        .issue_token(
            "hostapp:ann",
            &TokenAud {
                engine: "desk".into(),
                machine: None,
            },
            "read",
            None,
        )
        .unwrap();
    assert!(
        site.verify_token(desk["token"].as_str().unwrap(), "hostapp")
            .is_err()
    );
    assert!(
        site.issue_token(
            "x",
            &TokenAud {
                engine: "app:".into(),
                machine: None
            },
            "read",
            None
        )
        .is_err()
    );
    // A booth's named API key for the host program, made through the handle: checked here, its
    // use noted (`last_used`), revoked.
    let made = site
        .run_from(
            "admin",
            &hostapp,
            &SiteCommand::ApiKey {
                name: "Booth Presenter".into(),
                aud: app.clone(),
                scope: "read".into(),
                days: None,
            },
        )
        .expect("an API key");
    let (key, key_id) = (
        made["key"].as_str().unwrap().to_owned(),
        made["id"].as_str().unwrap().to_owned(),
    );
    let c = site.verify_token(&key, "hostapp").expect("the key is good");
    assert!(c.key && c.sub == format!("key:{key_id}"), "{c:?}");
    let state = &site.api.as_ref().unwrap().state;
    let listed = state.api_keys();
    let k = listed.iter().find(|k| k["id"] == key_id.as_str()).unwrap();
    assert!(k["last_used"].as_u64().is_some(), "{k}");
    // A key's use noted again and again while keys are made: none is lost (a
    // report: a key made as another's use was written vanished from the list, 1 run
    // in 3).
    let used_id = key_id.clone();
    let st = site.api.as_ref().unwrap().state.clone();
    let noting = std::thread::spawn(move || {
        for _ in 0..300 {
            st.key_used(&used_id, "a machine");
        }
    });
    let mut made_ids = Vec::new();
    for i in 0..20 {
        let m = site
            .run_from(
                "admin",
                &hostapp,
                &SiteCommand::ApiKey {
                    name: format!("Browser {i}"),
                    aud: app.clone(),
                    scope: "read".into(),
                    days: Some(365),
                },
            )
            .expect("a key");
        made_ids.push(m["id"].as_str().unwrap().to_owned());
    }
    noting.join().unwrap();
    let listed = site.api_keys();
    for id in &made_ids {
        assert!(
            listed.iter().any(|k| &k.id == id),
            "key {id} lost from the list"
        );
    }
    for id in made_ids {
        site.run_from("admin", &hostapp, &SiteCommand::ApiKeyRevoke { id })
            .expect("each revoked");
    }
    site.run_from("admin", &hostapp, &SiteCommand::ApiKeyRevoke { id: key_id })
        .expect("revoked");
    assert!(site.verify_token(&key, "hostapp").is_err(), "a revoked key");
    // (A not-before a second later than its issue: tokens are stamped in seconds.)
    std::thread::sleep(Duration::from_millis(1100));
    site.withdraw_tokens("hostapp:ann").unwrap();
    assert!(site.verify_token(&tok, "hostapp").is_err(), "withdrawn");
}

fn tokens_by_role(site: &Handle) {
    use crate::typed::{SiteCommand, TokenAud};
    use idn_open_protocol::trust::token::{self, Expect, Scope};
    let ask = |engine: &str, scope: &str| SiteCommand::Token {
        aud: TokenAud {
            engine: engine.into(),
            machine: None,
        },
        scope: scope.into(),
        ttl_s: Some(600),
    };
    for (name, role) in [("viv", "viewer"), ("ollie", "operator")] {
        site.run(
            "admin",
            &SiteCommand::User {
                action: "set".into(),
                name: name.into(),
                role: Some(role.into()),
                password: Some("a-long-password".into()),
            },
        )
        .unwrap();
    }
    let a = site.authority();
    let (public, id) = (a.ca_public(), a.site_id());
    let check = |tok: &str, engine: &str| {
        token::verify(
            tok,
            &Expect {
                ca_public: &public,
                site: &id,
                engine,
                machine: "any-machine",
                now: idn_common::epoch::secs(),
            },
            |_| None,
        )
    };
    let got = site
        .run("viv", &ask("desk", "read"))
        .expect("a viewer reads");
    let c = check(got["token"].as_str().unwrap(), "desk").unwrap();
    assert_eq!(
        (c.sub.as_str(), c.scope, c.machine.as_str()),
        ("viv", Scope::Read, "*")
    );
    assert!(got["exp"].as_u64().unwrap() <= got["iat"].as_u64().unwrap() + 600);
    let e = site.run("viv", &ask("desk", "control")).unwrap_err();
    assert!(e.contains("viewer may not"), "{e}");
    assert!(site.run("ollie", &ask("switcher", "control")).is_ok());
    assert!(site.run("ollie", &ask("desk", "bus:aux:5")).is_ok());
    assert!(site.run("ollie", &ask("node", "read")).is_ok());
    assert!(
        site.run("ollie", &ask("node", "control")).is_err(),
        "the Node is the administrators'"
    );
    assert!(site.run("admin", &ask("node", "control")).is_ok());
    // Over 12 hours, an unknown engine or machine: refused.
    let long = SiteCommand::Token {
        aud: TokenAud {
            engine: "desk".into(),
            machine: None,
        },
        scope: "read".into(),
        ttl_s: Some(12 * 3600 + 1),
    };
    assert!(site.run("admin", &long).is_err());
    assert!(site.run("admin", &ask("mixer", "read")).is_err());
    let elsewhere = SiteCommand::Token {
        aud: TokenAud {
            engine: "desk".into(),
            machine: Some("not-a-machine".into()),
        },
        scope: "read".into(),
        ttl_s: None,
    };
    assert!(site.run("admin", &elsewhere).is_err());
    // The host's own user: no role, at most 12 hours.
    let aud = TokenAud {
        engine: "switcher".into(),
        machine: None,
    };
    let v = site
        .issue_token(
            "hostapp:alice",
            &aud,
            "control",
            Some(Duration::from_secs(60)),
        )
        .unwrap();
    assert_eq!(
        check(v["token"].as_str().unwrap(), "switcher").unwrap().sub,
        "hostapp:alice"
    );
    assert!(
        site.issue_token(
            "hostapp:alice",
            &aud,
            "control",
            Some(Duration::from_secs(43201))
        )
        .is_err()
    );
    // A user changed: their tokens withdrawn on every machine (a site option).
    site.run(
        "admin",
        &SiteCommand::User {
            action: "set".into(),
            name: "ollie".into(),
            role: Some("viewer".into()),
            password: None,
        },
    )
    .unwrap();
    let nb = token::not_before(&a.options(), "fp", "ollie").expect("ollie's not-before");
    assert!(nb + 5 >= idn_common::epoch::secs());
    assert_eq!(token::not_before(&a.options(), "fp", "viv"), None);
    // The audit: who asked, never the token.
    let p = site.api.as_ref().unwrap();
    let log = p.state.users.audit_log(50);
    let line = log
        .iter()
        .find(|l| l["action"] == "token" && l["user"] == "viv" && l["ok"] == false)
        .expect("the refused ask is in the audit");
    assert_eq!(line["detail"]["scope"], "control");
    assert!(
        log.iter()
            .any(|l| l["action"] == "token (host)" && l["user"] == "hostapp:alice")
    );
    assert!(!serde_json::to_string(&log).unwrap().contains("idn1."));
}

/// The shared API front (front.rs): the status as `state`, with a version that moves
/// and the long poll; its schema; an older sequence number skipped; a viewer's change
/// refused and audited as the command's.
fn shared_front(site: &Handle) {
    let st = site.api.as_ref().unwrap().state.clone();
    let ask = |st: &Arc<crate::State>,
               method: &str,
               path: &str,
               query: &str,
               user: &str,
               body: &str,
               extra: &[(&str, &str)]| {
        let sid = st.sessions.create_for(user);
        let mut headers: std::collections::HashMap<String, String> = [
            ("cookie".to_owned(), format!("idn_site={sid}")),
            ("x-idn-node".to_owned(), "1".to_owned()),
        ]
        .into();
        headers.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let resp = st.serve(idn_open_protocol::web::Request {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            headers,
            body: body.as_bytes().to_vec(),
            peer: std::net::IpAddr::from([127, 0, 0, 1]),
            tls: true,
        });
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap_or_default();
        (resp.status, v)
    };
    let (status, state) = ask(&st, "GET", "/api/v1/state", "", "admin", "", &[]);
    assert!(status.starts_with("200"), "{status}");
    assert_eq!(state["me"]["role"], "admin");
    assert!(state["site"]["ca"].is_string());
    let v = state["version"].as_u64().expect("a version");
    let (_, schema) = ask(&st, "GET", "/api/v1/state_schema", "", "admin", "", &[]);
    for k in ["version", "me", "site", "machines", "mqtt"] {
        assert!(schema["properties"][k].is_object(), "{k} in the schema");
    }
    // Not logged in: neither.
    let (status, _) = ask(&st, "GET", "/api/v1/state", "", "", "", &[]);
    assert!(status.starts_with("403"), "{status}");
    // The long poll: answered when the status changes (the site closed).
    let st2 = st.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        ask(&st2, "POST", "/api/v1/close", "", "admin", "{}", &[])
    });
    let at = std::time::Instant::now();
    let (_, later) = ask(
        &st,
        "GET",
        "/api/v1/state",
        &format!("after={v}"),
        "admin",
        "",
        &[],
    );
    assert!(later["version"].as_u64().unwrap() > v);
    assert!(at.elapsed() < Duration::from_secs(10));
    assert_eq!(t.join().unwrap().1["ok"], true);
    // Per-control sequence numbers: an older one isn't applied.
    fn seq(n: &str) -> [(&str, &str); 3] {
        [
            ("x-idn-client", "panel"),
            ("x-idn-control", "pairing"),
            ("x-idn-seq", n),
        ]
    }
    let body = r#"{"seconds": 60}"#;
    let (_, a) = ask(&st, "POST", "/api/v1/pairing", "", "admin", body, &seq("5"));
    assert_eq!(a["ok"], true);
    assert!(a["skipped"].is_null());
    let (_, b) = ask(&st, "POST", "/api/v1/pairing", "", "admin", body, &seq("4"));
    assert_eq!(b["skipped"], true);
    // A viewer: refused, in the audit under the command's name.
    let (status, no) = ask(&st, "POST", "/api/v1/close", "", "viv", "{}", &[]);
    assert!(status.starts_with("403"), "{status}");
    assert_eq!(no["error"], "a viewer may not do this");
    assert!(
        st.users
            .audit_log(20)
            .iter()
            .any(|l| { l["action"] == "close" && l["user"] == "viv" && l["ok"] == false })
    );
}

/// A TLS connection to the API on `port` (its self-signed certificate taken).
fn tls(port: u16) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
    let cfg = Arc::new(idn_open_protocol::web::httpc::client_config(true).unwrap());
    let conn = rustls::ClientConnection::new(cfg, "localhost".try_into().unwrap()).unwrap();
    let s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    rustls::StreamOwned::new(conn, s)
}

/// A push channel's client socket.
type Ws =
    tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>;

/// The push channel: refused without a session; logged in, the status at once, a
/// command answered through the role checks, the audit when asked for. Another,
/// returned open.
fn events_channel(port: u16) -> Ws {
    use std::io::{Read, Write};
    use tungstenite::client::IntoClientRequest;
    let url = format!("wss://localhost:{port}/api/v1/events");
    assert!(
        tungstenite::client(url.as_str(), tls(port)).is_err(),
        "no session: refused"
    );
    // Logged in (the site password).
    let body = r#"{"password":"a-long-site-password"}"#;
    let mut s = tls(port);
    write!(
        s,
        "POST /api/v1/login HTTP/1.1\r\nHost: localhost:{port}\r\nX-IDN-Node: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    let _ = s.read_to_string(&mut answer);
    let cookie = answer
        .lines()
        .find_map(|l| l.strip_prefix("Set-Cookie: "))
        .and_then(|c| c.split(';').next())
        .expect("a session cookie")
        .to_owned();
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    let (mut ws, _) = tungstenite::client(req, tls(port)).expect("it upgrades");
    let read = |ws: &mut tungstenite::WebSocket<_>, want: &str| -> serde_json::Value {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < end, "no {want} message");
            if let Ok(tungstenite::Message::Text(t)) = ws.read() {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == want {
                    return v;
                }
            }
        }
    };
    // The status as a state (the shared front's push channel), with its version.
    let st = read(&mut ws, "state");
    assert!(st["state"]["machines"].is_array());
    assert!(st["version"].as_u64().is_some_and(|v| v > 0), "{st}");
    // What this user may change, as the status says it.
    let caps = &st["state"]["me"]["capabilities"];
    assert!(
        caps.as_array()
            .is_some_and(|c| c.iter().any(|p| p == "route_cell")),
        "{caps}"
    );
    ws.send(tungstenite::Message::text(
        r#"{"id": 1, "cmd": "pairing", "body": {"seconds": "60"}}"#,
    ))
    .unwrap();
    let r = read(&mut ws, "reply");
    assert_eq!(
        (r["id"].as_u64(), r["ok"].as_bool()),
        (Some(1), Some(true)),
        "{r}"
    );
    ws.send(tungstenite::Message::text(
        r#"{"subscribe": ["audit", "matrix"]}"#,
    ))
    .unwrap();
    let m = read(&mut ws, "matrix");
    assert!(
        m["matrix"]["receivers"].is_array() && m["matrix"]["read_only"] == false,
        "{m}"
    );
    // The audit's lines as they are written, from the subscription on.
    ws.send(tungstenite::Message::text(
        r#"{"id": 2, "cmd": "pairing", "body": {"seconds": "60"}}"#,
    ))
    .unwrap();
    let a = read(&mut ws, "audit");
    assert!(a.to_string().contains(r#""action":"pairing""#), "{a}");
    let _ = ws.close(None);
    let mut req = format!("wss://localhost:{port}/api/v1/events")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    tungstenite::client(req, tls(port)).expect("it upgrades").0
}
