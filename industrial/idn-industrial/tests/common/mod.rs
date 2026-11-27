//! Helpers shared by integration tests.

use std::time::Duration;

/// A TCP port free on this machine now.
pub fn free_tcp() -> u16 {
    idn_common::testing::test_port()
}

/// A UDP port free on this machine now.
pub fn free_udp() -> u16 {
    idn_common::testing::test_port()
}

/// A test's own folder, removed when it is dropped (at the test's end).
pub fn dir(name: &str) -> idn_common::testing::TempDir {
    idn_common::testing::TempDir::new(&format!("industrial-{name}"))
}

/// Waits up to `secs` seconds for `f`; fails the test naming `what`.
pub fn wait(what: &str, secs: u64, f: impl FnMut() -> bool) {
    idn_common::testing::wait_for(what, Duration::from_secs(secs), f)
}
