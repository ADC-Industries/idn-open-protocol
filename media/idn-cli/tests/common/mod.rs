//! What the tests of the `idn` program share.
#![allow(dead_code)]

use std::process::Command;

/// The `idn` program running `tool` (`idn recv …`).
pub fn idn(tool: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_idn"));
    c.arg(tool);
    c
}

/// A TCP port free on this machine now.
#[allow(unused_imports)] // (not every test file of the crate binds a port)
pub use idn_common::testing::test_port as free_port;

/// A connection to a program just started on `port`, tried until it listens (up to 10 s:
/// a busy machine, the full run side by side, starts it slower than a fixed wait).
pub fn connect_when_up(port: u16) -> std::net::TcpStream {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return s,
            Err(e) if std::time::Instant::now() > end => panic!("port {port}: {e}"),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}
