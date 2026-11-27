//! Joining a site with a typed code (§E.10.10): the join modes, single use, wrong tries
//! and burning, expiry, codes kept across a restart, and a rogue authority.

use crate::common::{Site, device_config, open_site, site, wait_event, wait_state};
use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::trust::{
    Authority, AuthorityConfig, AuthorityEvent, Enrollment, JoinMode, TrustState,
};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A site without a passphrase, joined in `mode`.
fn coded_site(tag: &str, mode: JoinMode) -> Site {
    let s = site(tag, None);
    s.authority.set_join_mode(mode).unwrap();
    s
}

/// [`coded_site`] with its authority's state in `dir`: opened again after a restart.
fn site_in(dir: &Path, mode: JoinMode) -> (Authority, DiscoveryServer, String) {
    let s = open_site(dir, None);
    s.0.set_join_mode(mode).unwrap();
    s
}

/// A device as the IDN Node runs it, its state in `dir`: its person confirms comparison
/// codes (a typed code needs no such step).
fn device(server: &str, dir: &Path, name: &str) -> Arc<Enrollment> {
    Enrollment::start_confirming(device_config(server, &dir.join(name), name, None), None).unwrap()
}

/// Waits until the device has heard the site's join mode (its first hello).
fn knows_mode(e: &Enrollment) {
    wait_for("the site's join mode", Duration::from_secs(10), || {
        e.site_join_mode().is_some()
    });
}

fn enrolled(s: &TrustState) -> bool {
    matches!(s, TrustState::Enrolled { connected: true })
}

#[test]
fn code_or_approval_a_good_code_joins_at_once_and_none_waits_for_approval() {
    let site = coded_site("jc-or", JoinMode::CodeOrApproval);
    let (a, server) = (&site.authority, &site.server);
    // No pairing window open, no approval: the code alone.
    let c = a.issue_join_code("op").unwrap();
    assert_eq!(c.code.len(), 6);
    let d = device(server, &site.dir, "OR-A (App)");
    knows_mode(&d);
    assert_eq!(d.site_join_mode(), Some(JoinMode::CodeOrApproval));
    // Typed as people see it, with the space.
    let shown = format!("{} {}", &c.code[..3], &c.code[3..]);
    d.join_with_code(&shown).unwrap();
    let st = wait_state(&d, 15, enrolled);
    assert!(enrolled(&st), "{st:?}");
    assert!(
        wait_event(
            a,
            5,
            |e| matches!(e, AuthorityEvent::CodeUsed { id, .. } if *id == c.id)
        )
        .is_some()
    );
    assert!(a.join_codes().is_empty(), "used once, gone");

    // Without a code: the comparison code and approval, as before.
    let b = device(server, &site.dir, "OR-B (App)");
    let st = wait_state(&b, 10, |s| matches!(s, TrustState::WaitingForPairing));
    assert_eq!(st, TrustState::WaitingForPairing);
    a.open_pairing(Duration::from_secs(60));
    b.retry_now();
    let st = wait_state(&b, 10, |s| matches!(s, TrustState::Pending { .. }));
    let TrustState::Pending { code } = st else {
        panic!("{st:?}")
    };
    let req = a
        .pending()
        .into_iter()
        .find(|r| r.name == "OR-B (App)")
        .unwrap();
    assert_eq!(req.code, code);
    assert!(!req.by_code);
}

#[test]
fn code_and_approval_needs_both() {
    let site = coded_site("jc-and", JoinMode::CodeAndApproval);
    let (a, server) = (&site.authority, &site.server);
    // No code: told so (even with the window open).
    a.open_pairing(Duration::from_secs(60));
    let d = device(server, &site.dir, "AND (App)");
    let st = wait_state(&d, 10, |s| matches!(s, TrustState::CodeNeeded));
    assert_eq!(st, TrustState::CodeNeeded);
    a.close_pairing();
    // A good code, then an operator's approval (no window needed).
    let c = a.issue_join_code("op").unwrap();
    d.join_with_code(&c.code).unwrap();
    let st = wait_state(&d, 15, |s| matches!(s, TrustState::CodeAccepted));
    assert_eq!(st, TrustState::CodeAccepted);
    let Some(AuthorityEvent::JoinRequested(req)) = wait_event(
        a,
        5,
        |e| matches!(e, AuthorityEvent::JoinRequested(r) if r.name == "AND (App)"),
    ) else {
        panic!("no request")
    };
    assert!(req.by_code);
    assert!(req.code.is_empty());
    std::thread::sleep(Duration::from_millis(500));
    assert!(!enrolled(&d.state()), "not before approval");
    // Approved under the operator's name: the certificate,
    // the authority's record and the device all carry it.
    a.approve_as(req.id, Some("Booth")).unwrap();
    let st = wait_state(&d, 15, enrolled);
    assert!(enrolled(&st), "{st:?}");
    assert_eq!(d.name_in_site().as_deref(), Some("Booth"));
    let rec = a
        .devices()
        .into_iter()
        .find(|r| r.fingerprint == d.fingerprint());
    assert_eq!(rec.map(|r| r.name).as_deref(), Some("Booth"));
}

#[test]
fn wrong_codes_count_and_burn_after_five() {
    let site = coded_site("jc-wrong", JoinMode::CodeOrApproval);
    let (a, server) = (&site.authority, &site.server);
    let c = a.issue_join_code("op").unwrap();
    let wrong = format!("{:06}", (c.code.parse::<u32>().unwrap() + 1) % 1_000_000);
    let d = device(server, &site.dir, "WRONG (App)");
    knows_mode(&d);
    for left in (0..5u32).rev() {
        d.join_with_code(&wrong).unwrap();
        let st = wait_state(&d, 15, |s| matches!(s, TrustState::WrongCode { .. }));
        assert_eq!(st, TrustState::WrongCode { left }, "try {}", 5 - left);
        assert!(
            wait_event(
                a,
                5,
                |e| matches!(e, AuthorityEvent::CodeWrong { left: l, .. } if *l == left)
            )
            .is_some()
        );
    }
    assert!(
        wait_event(
            a,
            5,
            |e| matches!(e, AuthorityEvent::CodeBurned { id, replaced: false } if *id == c.id)
        )
        .is_some()
    );
    assert!(a.join_codes().is_empty());
    // Burned: the right code no longer joins.
    d.join_with_code(&c.code).unwrap();
    let st = wait_state(&d, 15, |s| matches!(s, TrustState::CodeExpired));
    assert_eq!(st, TrustState::CodeExpired);
    assert!(!enrolled(&d.state()));
}

#[test]
fn a_code_is_single_use() {
    let site = coded_site("jc-once", JoinMode::CodeOrApproval);
    let (a, server) = (&site.authority, &site.server);
    let c = a.issue_join_code("op").unwrap();
    let first = device(server, &site.dir, "ONCE-1 (App)");
    knows_mode(&first);
    first.join_with_code(&c.code).unwrap();
    assert!(enrolled(&wait_state(&first, 15, enrolled)));
    let second = device(server, &site.dir, "ONCE-2 (App)");
    knows_mode(&second);
    second.join_with_code(&c.code).unwrap();
    let st = wait_state(&second, 15, |s| matches!(s, TrustState::CodeExpired));
    assert_eq!(st, TrustState::CodeExpired);
    assert_eq!(a.devices().len(), 1);
}

#[test]
fn codes_survive_a_restart_and_expire() {
    let t = TempDir::new("jc-restart");
    let dir = t.join("restart");
    let c = {
        let (a, ds, _) = site_in(&dir, JoinMode::CodeOrApproval);
        let c = a.issue_join_code("op").unwrap();
        drop(ds);
        c
    };
    // Opened again: the code waits still, the mode too.
    let (a, _ds, server) = site_in(&dir, JoinMode::CodeOrApproval);
    let listed = a.join_codes();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].id, listed[0].expires), (c.id, c.expires));
    let d = device(&server, &t, "RESTART (App)");
    knows_mode(&d);
    d.join_with_code(&c.code).unwrap();
    assert!(enrolled(&wait_state(&d, 15, enrolled)));

    // A code whose 10 minutes are over (its file says so): refused as expired.
    let dir2 = t.join("expired");
    {
        let (a, ds, _) = site_in(&dir2, JoinMode::CodeOrApproval);
        a.issue_join_code("op").unwrap();
        drop(ds);
    }
    let f = dir2.join("join_codes.tsv");
    let tsv = std::fs::read_to_string(&f).unwrap();
    let mut cols: Vec<&str> = tsv.trim_end().split('\t').collect();
    let past = (idn_common::epoch::secs() - 1).to_string();
    cols[2] = &past;
    std::fs::write(&f, cols.join("\t") + "\n").unwrap();
    let code = String::from_utf8(idn_common::hex::unhex(cols[1]).unwrap()).unwrap();
    let (a2, _ds2, server2) = site_in(&dir2, JoinMode::CodeOrApproval);
    assert!(a2.join_codes().is_empty());
    let d2 = device(&server2, &t, "LATE (App)");
    knows_mode(&d2);
    d2.join_with_code(&code).unwrap();
    let st = wait_state(&d2, 15, |s| matches!(s, TrustState::CodeExpired));
    assert_eq!(st, TrustState::CodeExpired);
}

#[test]
fn a_site_in_approval_mode_takes_no_codes() {
    let site = coded_site("jc-approval", JoinMode::Approval);
    let (a, server) = (&site.authority, &site.server);
    let c = a.issue_join_code("op").unwrap();
    a.open_pairing(Duration::from_secs(60));
    let d = device(server, &site.dir, "APPROVAL (App)");
    knows_mode(&d);
    assert_eq!(d.site_join_mode(), Some(JoinMode::Approval));
    d.join_with_code(&c.code).unwrap();
    // The usual join: a comparison code to approve; the issued code untouched.
    let st = wait_state(&d, 15, |s| matches!(s, TrustState::Pending { .. }));
    assert!(matches!(st, TrustState::Pending { .. }), "{st:?}");
    assert_eq!(a.join_codes().len(), 1);
    assert_eq!(a.join_codes()[0].tries_left, 5);
}

/// A rogue authority (its own CA, its own code) the device was pointed at: the code from
/// the real site matches none of its runs, so the device sends no confirmation, pins
/// nothing, and the rogue learns only that its guess was wrong.
#[test]
fn a_rogue_authority_cannot_complete_a_code_join() {
    let real = coded_site("jc-real", JoinMode::CodeOrApproval);
    let rogue = coded_site("jc-rogue", JoinMode::CodeOrApproval);
    let good = real.authority.issue_join_code("op").unwrap();
    rogue.authority.issue_join_code("attacker").unwrap(); // its own guess
    let name = "VICTIM (App)";
    let d = device(&rogue.server, &rogue.dir, name);
    knows_mode(&d);
    d.join_with_code(&good.code).unwrap();
    let st = wait_state(&d, 15, |s| {
        matches!(
            s,
            TrustState::WrongCode { .. } | TrustState::Enrolled { .. }
        )
    });
    assert!(matches!(st, TrustState::WrongCode { .. }), "{st:?}");
    assert!(d.creds().is_none());
    assert!(
        !rogue.dir.join(name).join("ca.crt").exists(),
        "no CA pinned"
    );
}

/// A fake authority speaking the wire protocol (its CA made in `dir`): what the device
/// sends back.
fn fake_authority(dir: &Path, runs: usize) -> (String, std::thread::JoinHandle<Option<String>>) {
    let throwaway = Authority::open(AuthorityConfig {
        state_dir: dir.join(format!("fake-ca-{runs}")),
        passphrase: None,
    })
    .unwrap();
    let ca = idn_open_protocol::trust::proto::b64(&throwaway.ca_der());
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = l.local_addr().unwrap().to_string();
    let t = std::thread::spawn(move || {
        let read_msg = |s: &mut std::net::TcpStream| -> Option<String> {
            let mut out = Vec::new();
            let mut b = [0u8; 1];
            loop {
                if s.read(&mut b).ok()? == 0 {
                    return None;
                }
                if b[0] == 0 {
                    return String::from_utf8(out).ok();
                }
                out.push(b[0]);
            }
        };
        // Each try of the device's: joins without a code are told to wait.
        loop {
            let (mut s, _) = l.accept().ok()?;
            s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
            let hello = read_msg(&mut s)?;
            assert!(hello.contains(r#"version="2""#), "{hello}");
            s.write_all(
                format!(r#"<idn_trust_ok version="2" ca="{ca}" join="code_or_approval"/>"#)
                    .as_bytes(),
            )
            .ok()?;
            s.write_all(&[0]).ok()?;
            let join = read_msg(&mut s)?;
            if !join.contains("pake=") {
                s.write_all(b"<idn_join_status state=\"closed\"/>\0").ok()?;
                continue;
            }
            // Runs with random messages (a rogue guessing): never the device's code.
            let mut ch = String::from(
                r#"<idn_code_challenge na="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=">"#,
            );
            for _ in 0..runs {
                let mut y = vec![0x42u8];
                y.extend(idn_open_protocol::random::secure_random::<32>());
                ch += &format!(
                    r#"<run y="{}" confirm="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="/>"#,
                    idn_open_protocol::trust::proto::b64(&y)
                );
            }
            ch += "</idn_code_challenge>";
            s.write_all(ch.as_bytes()).ok()?;
            s.write_all(&[0]).ok()?;
            return read_msg(&mut s);
        }
    });
    (server, t)
}

#[test]
fn the_device_confirms_nothing_to_wrong_runs_and_refuses_too_many() {
    let dir = TempDir::new("jc-fake");
    // One wrong run: an empty confirmation, nothing usable.
    let (server, t) = fake_authority(&dir, 1);
    let d = device(&server, &dir, "FAKE-1 (App)");
    knows_mode(&d);
    d.join_with_code("123456").unwrap();
    let reply = t.join().unwrap().unwrap_or_default();
    assert!(reply.starts_with("<idn_code_confirm"), "{reply}");
    assert!(!reply.contains("confirm="), "{reply}");
    drop(d);
    // Five runs (more than a site holds codes: five guesses at once): refused, nothing
    // sent back.
    let (server, t) = fake_authority(&dir, 5);
    let d = device(&server, &dir, "FAKE-5 (App)");
    knows_mode(&d);
    d.join_with_code("123456").unwrap();
    assert_eq!(t.join().unwrap(), None);
    let st = wait_state(&d, 10, |s| matches!(s, TrustState::Error(_)));
    assert!(
        matches!(&st, TrustState::Error(x) if x.contains("too many runs")),
        "{st:?}"
    );
}
