//! A source that vanishes without closing its connections (power loss, cable pull) must
//! be noticed, so the input reconnects instead of freezing on the last picture.

use idn_open_protocol::control::parse_element;
use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent};
use idn_open_protocol::quic::client::Client;
use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use crate::common;

#[test]
fn input_reconnects_when_quic_media_goes_silent() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
    // A scripted sender: accept, read up to ndi_enabled_streams, connect QUIC media,
    // then go silent while keeping every socket open.
    let (mut s, _) = listener.accept().unwrap();
    let mut rx = s.try_clone().unwrap();
    s.write_all(
        &Frame::text(
            kind::CONTROL,
            0,
            r#"<ndi_version text="3" video="7" audio="5"/>"#,
        )
        .encode(),
    )
    .unwrap();
    let enabled = loop {
        let f = read_frame(&mut rx, &Limits::default()).unwrap().unwrap();
        let e = parse_element(f.text_payload().unwrap()).unwrap();
        if e.name == "ndi_enabled_streams" {
            break e;
        }
    };
    let qport: u16 = enabled.attr("video_quic_port_v1").unwrap().parse().unwrap();
    let vid: u32 = enabled.attr("video_quic_id_v1").unwrap().parse().unwrap();
    // Through a relay that can go silent, as a pulled cable would.
    let (relay, cut) = common::udp_relay(format!("127.0.0.1:{qport}").parse().unwrap());
    let mut c = Client::connect(relay, Duration::from_secs(3)).unwrap();
    c.open_media_stream(0, vid);
    for _ in 0..20 {
        c.poll_once().unwrap();
    }
    // Silence from here on: no QUIC packets, no control messages, nothing closed.
    cut.store(true, std::sync::atomic::Ordering::Relaxed);
    let t0 = Instant::now();
    listener.set_nonblocking(true).unwrap();
    let mut lost = None;
    let mut reconnected = false;
    while t0.elapsed() < Duration::from_secs(45) && !reconnected {
        if let Some(InputEvent::State(ConnectionState::Lost(why))) =
            input.recv(Duration::from_millis(100))
        {
            lost.get_or_insert((why, t0.elapsed()));
        }
        reconnected = listener.accept().is_ok();
    }
    let (why, after) = lost.expect("the input reported the source lost");
    assert!(why.contains("QUIC media"), "{why}");
    assert!(after < Duration::from_secs(35), "noticed after {after:?}");
    assert!(reconnected, "the input tried to connect again");
    drop(s);
    drop(c);
}
