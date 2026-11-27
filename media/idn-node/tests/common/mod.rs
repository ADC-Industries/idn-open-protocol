//! Helpers shared by integration tests.
#![allow(dead_code)] // (each test file uses some of these)

use idn_open_protocol::node::Node;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A TCP port free on this machine now.
#[allow(unused_imports)] // (not every test file of the crate binds a port)
pub use idn_common::testing::test_port as free_port;

/// The Node process, stopped however the test ends (a failed assertion included).
pub struct Kill(Child, String);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

/// The Node started on `endpoint` with its state in `state`, once it answers there.
pub fn start(endpoint: &str, state: &Path) -> Kill {
    // A free port can be taken by another test's Node before this one binds it: that
    // Node exits, and a new port is tried (three times).
    for _ in 0..3 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
            .env("IDN_CLOCK", "off") // no PTP on the host network from tests
            .args(["run", "--user", "--state"])
            .arg(state)
            .args([
                "--endpoint",
                endpoint,
                "--panel-port",
                &free_port().to_string(),
            ])
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && child.try_wait().ok().flatten().is_none() {
            if Node::connect_to(endpoint, "probe").is_ok() {
                return Kill(child, endpoint.to_owned());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("the Node at {endpoint} didn't start");
}
