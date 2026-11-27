//! Tests of the device side's trust connections and tracked sessions.

use super::*;

#[test]
fn trust_connections_notice_a_vanished_authority() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let s = trust_socket(l.local_addr().unwrap()).unwrap();
    assert!(socket2::SockRef::from(&s).keepalive().unwrap());
    assert_eq!(
        s.write_timeout().unwrap(),
        Some(crate::transport::WRITE_TIMEOUT)
    );
}

#[test]
fn checking_a_tracked_session_never_disturbs_its_reader() {
    check_never_disturbs(session_alive);
}

/// A blocking reader takes 2000 bytes trickling in while `alive` checks a dup of its
/// socket over and over: none of its reads may fail, and a closed peer is noticed.
fn check_never_disturbs(alive: fn(&TcpStream) -> bool) {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut peer = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (live, _) = l.accept().unwrap();
    let tracked = live.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        let mut live = live;
        let mut b = [0u8; 2000];
        live.read_exact(&mut b)
    });
    let writer = std::thread::spawn(move || {
        for i in 0..2000u32 {
            peer.write_all(&[i as u8]).unwrap();
            std::thread::sleep(Duration::from_micros(200));
        }
        peer
    });
    while !reader.is_finished() {
        assert!(alive(&tracked));
    }
    reader
        .join()
        .unwrap()
        .expect("the session's reads never fail");
    drop(writer.join().unwrap());
    let end = std::time::Instant::now() + Duration::from_secs(2);
    while alive(&tracked) && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(&tracked), "a closed session is forgotten");
}
