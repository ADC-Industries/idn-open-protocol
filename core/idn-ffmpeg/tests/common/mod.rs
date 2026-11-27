//! Helpers shared by integration tests.

/// A TCP port free on this machine now.
#[allow(unused_imports)] // (not every test file of the crate binds a port)
pub use idn_common::testing::test_port as free_port;
