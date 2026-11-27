//! A source that can't be reached from the start: why is logged (for hosts that only
//! show the log, as OBS does), once a reason, however often it retries.

use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[test]
fn why_a_source_cant_be_reached_is_logged_once() {
    // A port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let rx = Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
    let end = Instant::now() + Duration::from_secs(7);
    let (mut lost, mut logged) = (0, Vec::new());
    while Instant::now() < end {
        match rx.recv(Duration::from_millis(50)) {
            Some(InputEvent::State(s)) if format!("{s:?}").starts_with("Lost") => lost += 1,
            Some(InputEvent::Log(t)) if t.starts_with("not connected") => logged.push(t),
            _ => {}
        }
    }
    assert!(lost >= 2, "retried {lost} times");
    assert_eq!(logged.len(), 1, "{logged:?}");
}
