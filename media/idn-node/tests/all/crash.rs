//! Crash reports and logs reach the Site Manager (idn-node.md §N.16). A test program of
//! its own: crash reports wait in a store of the whole process, handed to whichever Node
//! a program of the process connects to first, so another test's Node must not be there.

use crate::common;
use common::free_port;

use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::node::Node;
use idn_open_protocol::trust::{Authority, AuthorityConfig, AuthorityEvent};
use idn_open_protocol::xml::Element;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/icrash-{}-{name}.sock", std::process::id())
}

struct Kill(Child, String);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

/// §N.16: a program's crash report reaches the Site Manager through its Node, and the Site
/// Manager collects the machine's logs (the Node's own and the program's). §N.18: so does
/// an alert, kept by the Node until the machine is in the site.
#[test]
fn crash_reports_and_logs_reach_the_site_manager() {
    let adir = TempDir::new("auth-logs");
    let authority = Authority::open(AuthorityConfig {
        state_dir: adir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    // A program of this machine with a log, and a crash (a thread's panic, through the
    // crash hook).
    // (Programs keep theirs in a `logs` folder: the Node takes logs from no other.)
    let prog = TempDir::new("prog");
    let pdir = prog.join("logs");
    idn_open_protocol::diag::init_in("crashy", &pdir);
    let _ = std::thread::spawn(|| panic!("the test's own crash")).join();
    assert_eq!(
        idn_open_protocol::diag::crash_reports().len(),
        1,
        "a crash report"
    );
    let (endpoint, state) = (sock("node-logs"), TempDir::new("node-logs"));
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
        Node::connect_to(&endpoint, "crashy").is_ok()
    });
    let node = Node::connect_to(&endpoint, "crashy").expect("the Node starts");
    // Handed on to the Node as the program connected.
    assert!(
        idn_open_protocol::diag::crash_reports().is_empty(),
        "handed to the Node"
    );
    // An alert before the machine is in a site: the Node keeps it.
    node.alert("warning", "YouTube: the upload can't keep up")
        .unwrap();
    // Typed events (§N.41): the same one three times (a source coming and going), and one
    // about another input. The Site Manager's log gets each once.
    for _ in 0..3 {
        node.event(
            "warning",
            "lost",
            "Cam 1 lost",
            &serde_json::json!({"input": 1}),
        )
        .unwrap();
    }
    node.event(
        "warning",
        "lost",
        "Cam 2 lost",
        &serde_json::json!({"input": 2}),
    )
    .unwrap();
    // The Node joins the site.
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
    // This machine's person confirms the code too (§N.11).
    // (No confirming on the machine since 2026-11-18, §N.11.4: approved is in.)
    wait_for("the Node in the site", Duration::from_secs(15), || {
        node.enrollment().creds().is_some()
    });
    // The crash report arrives by itself.
    let report = || authority.crash_reports().into_iter().find(|r| r.0 == fp);
    wait_for(
        "the crash report at the Site Manager",
        Duration::from_secs(15),
        || report().is_some(),
    );
    let report = report().unwrap();
    assert!(report.1.starts_with("crash-crashy-"), "{report:?}");
    // The alert too, with the program's name.
    let alert = || {
        authority
            .alerts()
            .into_iter()
            .find(|a| a.1 == fp && a.4.starts_with("YouTube"))
    };
    wait_for(
        "the alert at the Site Manager",
        Duration::from_secs(10),
        || alert().is_some(),
    );
    let alert = alert().unwrap();
    assert_eq!(
        (alert.2.as_str(), alert.3.as_str(), alert.4.as_str()),
        ("crashy", "warning", "YouTube: the upload can't keep up")
    );
    assert!(alert.0 > 1_700_000_000, "a time: {alert:?}");
    // The typed events: once per input, with their fields and the program's name.
    let lost = || -> Vec<_> {
        authority
            .alert_entries()
            .into_iter()
            .filter(|a| a.fingerprint == fp && a.event == "lost")
            .collect()
    };
    wait_for(
        "the typed events at the Site Manager",
        Duration::from_secs(10),
        || lost().len() >= 2,
    );
    let events = lost();
    std::thread::sleep(Duration::from_millis(500));
    let again = authority
        .alert_entries()
        .into_iter()
        .filter(|a| a.fingerprint == fp && a.event == "lost")
        .count();
    assert_eq!(again, 2, "each once: {events:?}");
    let d: serde_json::Value = serde_json::from_str(&events[0].detail).unwrap();
    assert_eq!(d["app"], "crashy");
    assert!(d["input"] == 1 || d["input"] == 2, "{d}");
    let text = authority.crash_report(&fp, &report.1).unwrap();
    assert!(text.contains("what: the test's own crash"), "{text}");
    assert!(text.contains("backtrace:"));
    // The machine's logs, asked for.
    let snap = authority
        .collect_logs(&fp, Duration::from_secs(10))
        .unwrap();
    let snaps = authority.log_snapshots();
    let (_, _, files) = snaps
        .iter()
        .find(|s| s.0 == fp && s.1 == snap)
        .expect("the snapshot");
    let all: String = files
        .iter()
        .map(|(f, _)| authority.log_file(&fp, &snap, f).unwrap())
        .collect();
    assert!(all.contains("crashy"), "the program's log: {files:?}");
    assert!(all.contains("idn-node"), "the Node's own log: {files:?}");
    assert!(
        all.contains("crashed: the test's own crash"),
        "the crash in the program's log"
    );
    // Names that leave their folder are refused.
    assert!(authority.crash_report(&fp, "../x").is_err());
    assert!(authority.log_file(&fp, "..", "x").is_err());
}
