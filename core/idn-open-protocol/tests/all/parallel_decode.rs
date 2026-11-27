//! Decoding two pictures at once (`InputConfig::parallel_decode`): pictures still arrive
//! in order, every one of them when late ones are not skipped, also while the setting is
//! switched off and on again.

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::time::{Duration, Instant};

// 4K at 50 fps: an unoptimised build encodes a picture in about 350 ms, so the sender
// skips most of them and the check means nothing there.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "4K at 50 fps needs an optimised build: cargo test --release"
)]
fn pictures_arrive_in_order_and_all_of_them() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let mut cfg = OutputConfig::new("Parallel");
    cfg.frame_rate = (50, 1);
    let out = Output::new(&publisher, cfg).unwrap();

    let mut icfg = InputConfig::new(&format!("127.0.0.1:{}", out.port()));
    icfg.rgba = Some(AlphaMode::Straight);
    icfg.skip_late = false;
    icfg.parallel_decode = Some(true);
    let input = Input::open(icfg).unwrap();

    // 4K at 50 fps: a picture can take longer to decode than the 20 ms between them, so
    // two are decoding at once now and then.
    let (w, h) = (3840, 2160);
    let frames: Vec<std::sync::Arc<[u8]>> = (0..4u8)
        .map(|n| {
            (0..w * h)
                .flat_map(|i| {
                    [
                        (i % 251) as u8 ^ n.wrapping_mul(31),
                        n.wrapping_mul(29),
                        90,
                        255,
                    ]
                })
                .collect::<Vec<u8>>()
                .into()
        })
        .collect();
    let (mut got, mut last_ts, mut connected) = (0usize, i64::MIN, false);
    let start = Instant::now();
    let mut sent = 0usize;
    let mut next_send = Instant::now();
    // Sending for 5 s, then the pictures still decoding taken in, until none has come for
    // 2 s (a loaded machine was still decoding at 6 s: they counted as lost).
    let mut last_got = Instant::now();
    while start.elapsed() < Duration::from_secs(6)
        || (last_got.elapsed() < Duration::from_secs(2)
            && start.elapsed() < Duration::from_secs(15))
    {
        let t = start.elapsed();
        // Off for a while in the middle, to switch with pictures decoding on the side.
        if t > Duration::from_millis(2500) && t < Duration::from_millis(2600) {
            input.set_parallel_decode(false);
        }
        if t > Duration::from_millis(3500) && t < Duration::from_millis(3600) {
            input.set_parallel_decode(true);
        }
        if connected && Instant::now() >= next_send && t < Duration::from_secs(5) {
            let f = VideoFrame::rgba(w, h, frames[sent % frames.len()].clone(), AlphaMode::Opaque);
            if out.send(f).is_ok() {
                sent += 1;
            }
            next_send += Duration::from_millis(20);
        }
        while let Some(ev) = input.recv(Duration::from_millis(2)) {
            match ev {
                InputEvent::State(idn_open_protocol::input::ConnectionState::Connected(_)) => {
                    connected = true;
                    next_send = Instant::now();
                }
                InputEvent::Video(v) => {
                    assert!(
                        v.header.timestamp > last_ts,
                        "out of order: {} after {last_ts} via {} (got {got})",
                        v.header.timestamp,
                        v.via
                    );
                    last_ts = v.header.timestamp;
                    let rgba = v.rgba.expect("rgba asked for");
                    assert_eq!(rgba.len(), w * h * 4);
                    input.recycle_rgba(rgba);
                    got += 1;
                    last_got = Instant::now();
                }
                _ => {}
            }
        }
    }
    let st = input.stats();
    println!("sent {sent}, got {got}");
    assert!(sent > 150, "sent {sent}");
    // The output may skip frames under load (it reports how many); the input skips
    // none (skip_late off). As it starts, the input moves to this machine's ring: a
    // picture still on its way over the connection then, older than one from the ring,
    // goes (never back in time); on a loaded machine a few do.
    let sender_skipped = st
        .sender_report
        .as_ref()
        .map_or(0, |r| r.v_skipped as usize);
    assert!(
        got + sender_skipped + 8 >= sent,
        "sent {sent}, got {got}, stats {st:?}"
    );
    assert_eq!(st.late, 0, "{st:?}");
}
