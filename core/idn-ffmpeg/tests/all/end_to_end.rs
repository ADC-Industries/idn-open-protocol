//! An output with the GPU encoder and inputs with the FFmpeg decoder, over loopback
//! (spec §E.18.6): the input that prefers compressed video gets it, at about the target
//! bit rate, and shows the same pictures as a SpeedHQ input. Skipped without a GPU.

use crate::common;
use common::free_port;

use idn_common::sync::LockOk as _;
use idn_ffmpeg::Ffmpeg;
use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::codec::{CodecInput, CodecOutput, CodecUse, EncoderFactory};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::LinkConfig;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const W: usize = 1280;
const H: usize = 720;

/// A photo-like slide (a fixed grain texture) with a moving bar; frame `n` stamped
/// `n + 1`.
fn frame(n: u32) -> VideoFrame {
    let mut px = vec![0u8; W * H * 4];
    let bar = (n as usize * 16) % W;
    let mut seed = 0x2545_f491u32;
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) * 4;
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let grain = (seed % 48) as usize;
            let v = if (bar..bar + 64).contains(&x) {
                230
            } else {
                (30 + x * 120 / W + y * 40 / H + grain) as u8
            };
            px[i..i + 4].copy_from_slice(&[v, v / 2 + 60, 255 - v, 255]);
        }
    }
    let mut f = VideoFrame::rgba(W, H, px.into(), AlphaMode::Opaque);
    f.timestamp = Some(n as i64 + 1);
    f
}

type Seen = Arc<Mutex<BTreeMap<i64, ([u8; 4], Vec<u8>, Instant)>>>;

// On a machine with a GPU encoder, an unoptimised build can't keep the 30 fps this checks
// (about 110 of 180 pictures); without one the test skips itself anyway.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "needs an optimised build: cargo test --release"
)]
fn a_remote_room_input_gets_h264_from_the_gpu() {
    let ff = Arc::new(Ffmpeg::probe());
    if EncoderFactory::codecs(&*ff).is_empty() {
        println!("no hardware encoder here: skipped");
        return;
    }
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (30, 1),
                codec: Some(CodecOutput::new(ff.clone())),
                ..OutputConfig::new("Remote room")
            },
        )
        .unwrap(),
    );
    let frames: Vec<VideoFrame> = (0..30).map(frame).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let sent: Arc<Mutex<BTreeMap<i64, Instant>>> = Arc::default();
    let feeder = {
        let (o, s, sent) = (out.clone(), stop.clone(), sent.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut n = 0u32;
            while !s.load(Ordering::Relaxed) {
                let mut f = frames[n as usize % frames.len()].clone();
                f.timestamp = Some(n as i64 + 1);
                sent.lock_ok().insert(n as i64 + 1, Instant::now());
                let _ = o.send(f);
                n += 1;
                let due = start + Duration::from_secs_f64(n as f64 / 30.0);
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        })
    };
    let open = |use_: Option<CodecUse>| {
        Arc::new(
            Input::open(InputConfig {
                link: Some(LinkConfig::ephemeral()),
                codec: use_.map(|u| CodecInput::new(ff.clone(), u)),
                // A remote room: over the network, not this machine's shared memory
                // (which carries the pictures uncompressed, whatever the codec asked).
                machine: false,
                ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
            })
            .unwrap(),
        )
    };
    let inputs = [open(Some(CodecUse::Prefer)), open(None)];
    let watchers: Vec<(Seen, _)> = inputs
        .iter()
        .map(|i| {
            let (i, s) = (i.clone(), stop.clone());
            let seen: Seen = Arc::default();
            let s2 = seen.clone();
            let t = std::thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    if let Some(InputEvent::Video(v)) = i.recv(Duration::from_millis(20)) {
                        let y = v.picture.planes[0].data.clone();
                        s2.lock_ok()
                            .insert(v.header.timestamp, (v.header.fourcc, y, Instant::now()));
                    }
                }
            });
            (seen, t)
        })
        .collect();
    std::thread::sleep(Duration::from_secs(6));
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    let seen: Vec<_> = watchers
        .into_iter()
        .map(|(s, t)| {
            t.join().unwrap();
            s.lock_ok().clone()
        })
        .collect();

    let (h264, shq) = (&seen[0], &seen[1]);
    assert!(h264.len() > 150, "{} compressed pictures", h264.len());
    assert!(h264.values().rev().take(30).all(|v| &v.0 == b"H264"));
    assert!(shq.values().rev().take(30).all(|v| &v.0 == b"SHQ2"));
    let st = inputs[0].stats();
    assert_eq!(st.video_errors, 0, "{st:?}");
    // About the 8 Mbit/s for 1080p, scaled to 720p (3.6), against SpeedHQ's tens.
    let rate = |i: &Input| i.stats().video_bytes as f64 * 8.0 / 6.0 / 1e6;
    let (r_h264, r_shq) = (rate(&inputs[0]), rate(&inputs[1]));
    // The same pictures as SpeedHQ shows (luma PSNR), and the latency added.
    let sent = sent.lock_ok();
    let (mut worst_psnr, mut lat_h, mut lat_s, mut n) = (f64::MAX, 0.0, 0.0, 0);
    for (ts, (_, y, at)) in h264.iter().skip(30) {
        if let Some((_, y2, at2)) = shq.get(ts) {
            let mse: f64 = y
                .iter()
                .zip(y2)
                .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
                .sum::<f64>()
                / y.len() as f64;
            worst_psnr = worst_psnr.min(10.0 * (65025.0 / mse.max(1e-9)).log10());
            lat_h += (*at - sent[ts]).as_secs_f64();
            lat_s += (*at2 - sent[ts]).as_secs_f64();
            n += 1;
        }
    }
    println!(
        "H.264 {r_h264:.1} Mbit/s vs SpeedHQ {r_shq:.1}; luma PSNR vs SpeedHQ >= {worst_psnr:.1} dB over {n} pictures; latency {:.1} ms vs {:.1} ms",
        lat_h * 1000.0 / n as f64,
        lat_s * 1000.0 / n as f64
    );
    assert!(n > 100);
    assert!(r_h264 > 1.5 && r_h264 < 7.0, "{r_h264:.1} Mbit/s");
    assert!(r_h264 * 3.0 < r_shq);
    assert!(worst_psnr > 30.0, "{worst_psnr:.1} dB");
}
