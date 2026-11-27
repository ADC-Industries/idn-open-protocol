//! Encodes 4:2:2 pictures on several GPU encoders at once, each on its own thread as the
//! gateway's are, and reports each one's pictures per second and time per picture: does
//! the GPU keep up with a program and its ISOs?
//! Run: cargo run --release -p idn-ffmpeg --example encode_bench -- WxH [WxH ...]
//!      [--seconds N] [--own-device (each encoder its own VA-API device)]
use idn_open_protocol::codec::{Codec, EncoderConfig, EncoderFactory};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(|_| {});
    let args: Vec<String> = std::env::args().skip(1).collect();
    let own = args.iter().any(|a| a == "--own-device");
    let secs: u64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let sizes: Vec<(usize, usize)> = args
        .iter()
        .filter_map(|a| a.split_once('x'))
        .filter_map(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .collect();
    let shared = std::sync::Arc::new(idn_ffmpeg::Ffmpeg::probe());
    let start = Instant::now() + Duration::from_millis(500);
    let threads: Vec<_> = sizes
        .into_iter()
        .map(|(w, h)| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let ff = if own {
                    std::sync::Arc::new(idn_ffmpeg::Ffmpeg::probe())
                } else {
                    shared
                };
                let mut e = ff
                    .open(&EncoderConfig {
                        codec: Codec::H264,
                        width: w,
                        height: h,
                        frame_rate: (30, 1),
                        bitrate: 6_000_000,
                        keyframe_interval: 60,
                    })
                    .expect("encoder");
                let p = |w: usize, h: usize, v: u8| Plane {
                    width: w,
                    height: h,
                    data: (0..w * h)
                        .map(|i| v.wrapping_add((i % 251) as u8))
                        .collect(),
                };
                let planes = vec![p(w, h, 64), p(w / 2, h, 128), p(w / 2, h, 128)];
                std::thread::sleep(start.saturating_duration_since(Instant::now()));
                let (t0, mut n) = (Instant::now(), 0u32);
                while t0.elapsed() < Duration::from_secs(secs) {
                    e.encode(&planes, Fourcc::Shq2, n as i64, n == 0).unwrap();
                    n += 1;
                }
                let s = t0.elapsed().as_secs_f64();
                println!(
                    "{w}x{h}: {:.1} pictures/s, {:.1} ms each",
                    n as f64 / s,
                    s * 1e3 / n as f64
                );
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}
