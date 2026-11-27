//! The site clock through the Node (spec/extensions/idn-clock.md §C.3): the `idn-node`
//! program in user mode on test ports runs PTP, alone it becomes the master, and a
//! program connected to it gets site time; switched off, the program gets the machine's
//! own clock instead (R.30). A file of its own: the clock a program heard is per process.

use crate::common;
use common::free_port;

use idn_common::testing::TempDir;
use idn_open_protocol::node::{Node, clock};
use idn_open_protocol::xml::Element;
use std::process::{Child, Command};
use std::time::Duration;

/// A socket path short enough for `sun_path`.
fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/iclock-{}-{name}.sock", std::process::id())
}

/// The Node process, stopped however the test ends (a failed assertion included).
struct Kill(Child, String);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

fn wait_for(secs: u64, what: &str, pred: impl Fn() -> bool) {
    assert!(
        idn_common::testing::wait_until(Duration::from_secs(secs), pred),
        "timed out waiting for {what}: {:?}",
        clock::status()
    );
}

/// A test port: a thousand up in a release build, so a parallel full run's debug and
/// release clocks don't hear each other (bug run K8-8).
/// A test's port: debug and release apart, and each test process's by its own offset
/// (as the library's PTP tests: two runs at once heard each other's clocks).
fn port(p: u16) -> u16 {
    let base = if cfg!(debug_assertions) { p } else { p + 1000 };
    base + (std::process::id() % 40) as u16 * 2
}

#[test]
fn a_lone_node_is_master_and_its_programs_get_site_time() {
    let (endpoint, state) = (sock("c"), TempDir::new("state"));
    let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        // Test ports, which no site uses (and no master from elsewhere on them).
        .env(
            "IDN_CLOCK_PORTS",
            format!("{},{}", port(26419), port(26420)),
        )
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
    idn_common::testing::wait_for("the Node", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, "clock test").is_ok()
    });
    let node = Node::connect_to(&endpoint, "clock test").unwrap();
    assert!(
        clock::status().is_some(),
        "the clock arrives with the connection"
    );
    // Master once the announce timeout has passed with nobody else.
    wait_for(20, "master", || clock::site_now().is_some());
    let c = clock::status().unwrap();
    assert_eq!(c.state, "master");
    assert_eq!(
        clock::grandmaster().as_deref(),
        Some(c.grandmaster.as_str())
    );
    // A master's site time is the system's time as TAI.
    let tai = idn_common::epoch::now().as_nanos() as i128 + 37_000_000_000;
    let diff = clock::site_now().unwrap() - tai;
    assert!(
        diff.abs() < 50_000_000,
        "site time {diff} ns from the system's TAI"
    );
    // The status shows it too, with the test ports.
    let st = node.status().unwrap();
    let ce = st
        .children_named("clock")
        .next()
        .expect("clock in the status");
    assert_eq!(
        ce.attr("ports"),
        Some(format!("{}/{}", port(26419), port(26420)).as_str())
    );
    // PTP off: programs here get this machine's own clock instead (R.30 step 5), still
    // the system's time as TAI, its grandmaster the machine's.
    let r = node
        .request(Element::new("set").with_attr("clock", "off"))
        .unwrap();
    assert_eq!(r.name, "ok", "{}", r.to_xml());
    wait_for(5, "the machine's clock", || {
        clock::status().is_some_and(|c| c.state == "machine")
    });
    let gm = clock::grandmaster().expect("a usable clock");
    assert!(gm.starts_with("machine-"), "{gm}");
    let tai = idn_common::epoch::now().as_nanos() as i128 + 37_000_000_000;
    let diff = clock::site_now().unwrap() - tai;
    assert!(
        diff.abs() < 5_000_000,
        "machine time {diff} ns from the system's TAI"
    );
}
