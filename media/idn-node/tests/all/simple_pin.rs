//! The panels' simple-mode PIN through the IDN Node and the Site Manager (idn-node.md
//! §N.20). On its own (its own process): another test's crash report, the process's,
//! would go to this test's Node as its program connects (`diag` is process-wide).

use crate::common;
use common::free_port;

use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::node::Node;
use idn_open_protocol::trust::{Authority, AuthorityConfig, AuthorityEvent};
use idn_open_protocol::xml::Element;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// This machine's person confirms the site's code too (§N.11).
fn confirm_code(node: &Node) {
    // (No confirming on the machine since 2026-11-18, §N.11.4: approved is in.)
    wait_for("the Node in the site", Duration::from_secs(15), || {
        node.enrollment().creds().is_some()
    });
}

fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/ipin-{}-{name}.sock", std::process::id())
}

struct Kill(Child, String);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

/// §N.20: the panels' simple-mode PIN, set by the Site Manager and held there (the panels
/// can't change it, only use it), then released to the panels.
#[test]
fn the_site_holds_the_simple_mode_pin() {
    let adir = TempDir::new("auth-pin");
    let authority = Authority::open(AuthorityConfig {
        state_dir: adir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let endpoint = sock("node-pin");
    // Not looking for a site by itself: another test here announces one, which a fresh
    // Node joins when it is the only one it finds.
    let state = TempDir::new("node-pin");
    std::fs::write(
        state.join("node.xml"),
        r#"<idn_node name="PIN-TEST" auto_site="false" welcomed="true"/>"#,
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_CLOCK", "off")
        .args(["run", "--user", "--state"])
        .arg(state.path())
        .args([
            "--endpoint",
            &endpoint,
            "--panel-port",
            &free_port().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _kill = Kill(child, endpoint.clone());
    wait_for("the Node to start", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, "idn-switcher").is_ok()
    });
    let node = Node::connect_to(&endpoint, "idn-switcher").expect("the Node starts");
    // No PIN: just the button; the panel may set one.
    assert_eq!(node.panel_lock().unwrap(), (false, false));
    node.panel_unlock("").unwrap();
    node.set_panel_pin(Some("1357")).unwrap();
    assert!(node.panel_unlock("0000").is_err());
    node.panel_unlock("1357").unwrap();
    // Into the site.
    node.request(Element::new("set").with_attr("server", &format!("127.0.0.1:{}", ds.port())))
        .unwrap();
    authority.open_pairing(Duration::from_secs(60));
    let fp = node.enrollment().fingerprint();
    let end = Instant::now() + Duration::from_secs(15);
    let req = loop {
        if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_millis(200))
            && r.fingerprint == fp
        {
            break r;
        }
        assert!(Instant::now() < end, "no join request");
    };
    authority.approve(req.id).unwrap();
    confirm_code(&node);
    // (Enrolled, the machine connects again as a member: a few seconds.)
    let set = |attrs: &[(&str, &str)]| {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            match authority.set_node(&fp, attrs, Duration::from_secs(5)) {
                Ok(r) if r.attr("ok") == Some("true") => return,
                r => assert!(Instant::now() < end, "set_node: {r:?}"),
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    // The Site Manager sets its PIN and keeps it.
    let hash = idn_open_protocol::web::hash_password("2468").unwrap();
    set(&[("panel_pin", &hash), ("panel_site_only", "true")]);
    assert_eq!(node.panel_lock().unwrap(), (true, true));
    let e = node.set_panel_pin(Some("9999")).unwrap_err();
    assert!(e.to_string().contains("Site Manager"), "{e}");
    assert!(node.set_panel_pin(None).is_err(), "nor wipe it");
    assert!(node.panel_unlock("1357").is_err(), "the old PIN is gone");
    node.panel_unlock("2468").unwrap();
    // The machine's own `idn-node set` can't either.
    let r = node.request(Element::new("set").with_attr("panel_pin", "1111"));
    assert!(r.is_err(), "{r:?}");
    // Its report says so (never the hash).
    let report = || {
        authority
            .node_reports()
            .into_iter()
            .find(|r| r.fingerprint == fp)
            .map(|r| r.report)
    };
    wait_for(
        "the report to show the site's PIN",
        Duration::from_secs(10),
        || {
            report()
                .and_then(|r| r.child("panel").cloned())
                .is_some_and(|p| {
                    p.attr("pin") == Some("true") && p.attr("site_only") == Some("true")
                })
        },
    );
    assert!(
        !report().unwrap().to_xml().contains("$argon2"),
        "the hash in the report"
    );
    // Released: the panels may change it again; wiped: just the button.
    set(&[("panel_site_only", "false")]);
    node.set_panel_pin(None).unwrap();
    assert_eq!(node.panel_lock().unwrap(), (false, false));
}
