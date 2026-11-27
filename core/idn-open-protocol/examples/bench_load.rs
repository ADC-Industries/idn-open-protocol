//! Stall hunt: several 1080p30 outputs under CPU load, each watched by a receiver in this
//! process. Reports, per output, the longest gap between received frames and the
//! end-to-end latency (send call to decoded picture). Drop frames, never stall.
//! Run: cargo run --release --example bench_load [outputs] [seconds] [hog threads]
//! TRANSPORT=tcp|udp|quic selects the receivers' transport (default quic).

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent, Transport};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn now_100ns() -> i64 {
    (idn_common::epoch::now()
        .as_nanos()
        / 100) as i64
}

fn frame(w: usize, h: usize, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = 4 * (y * w + x);
            let (xs, ys) = (x + 3 * n, y + n);
            let r = ((xs * 255) / w) as u8;
            let g = (((ys / 8) % 2) * 160 + ((xs ^ ys) & 31)) as u8;
            let b = (((x as f32 / 40.0 + n as f32 / 10.0).sin() * 100.0) + 128.0) as u8;
            v[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    v
}

fn main() {
    let arg = |i: usize, d: u64| {
        std::env::args()
            .nth(i)
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    let (outputs, secs, hogs) = (arg(1, 3) as usize, arg(2, 20), arg(3, 0) as usize);
    let transport = match std::env::var("TRANSPORT").as_deref() {
        Ok("tcp") => Transport::Tcp,
        Ok("udp") => Transport::Udp,
        _ => Transport::Quic,
    };
    let (w, h) = (1920, 1080);
    let frames: Arc<Vec<Arc<[u8]>>> = Arc::new((0..30).map(|n| frame(w, h, n).into()).collect());
    let stop = Arc::new(AtomicBool::new(false));
    // CPU hogs, as a busy render server would have.
    let hog_threads: Vec<_> = (0..hogs)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..100_000 {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    }
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let mut threads = Vec::new();
    let mut results = Vec::new();
    for o in 0..outputs {
        let out =
            Arc::new(Output::new(&publisher, OutputConfig::new(&format!("Load {o}"))).unwrap());
        let input = Input::open(InputConfig {
            transports: vec![transport],
            ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
        })
        .unwrap();
        let (frames, stop2) = (frames.clone(), stop.clone());
        let feed_out = out.clone();
        threads.push(std::thread::spawn(move || {
            let tick = Duration::from_secs_f64(1.0 / 30.0);
            let mut next = Instant::now();
            let mut n = o * 7;
            while !stop2.load(Ordering::Relaxed) {
                let mut f = VideoFrame::rgba(w, h, frames[n % 30].clone(), AlphaMode::Opaque);
                f.timestamp = Some(now_100ns());
                let _ = feed_out.send(f);
                n += 1;
                next += tick;
                match next.checked_duration_since(Instant::now()) {
                    Some(d) => std::thread::sleep(d),
                    None => next = Instant::now(),
                }
            }
        }));
        let stop3 = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        results.push((out, rx));
        threads.push(std::thread::spawn(move || {
            let (mut gaps, mut lat) = (Vec::new(), Vec::new());
            let mut last: Option<Instant> = None;
            let warm = Instant::now() + Duration::from_secs(3);
            while !stop3.load(Ordering::Relaxed) {
                if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(100)) {
                    let now = Instant::now();
                    if now > warm {
                        if let Some(l) = last {
                            gaps.push((now - l).as_secs_f64() * 1e3);
                        }
                        lat.push((now_100ns() - v.header.timestamp) as f64 / 1e4);
                    }
                    last = Some(now);
                }
            }
            let _ = tx.send((gaps, lat, input.stats()));
        }));
    }
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    for t in threads.into_iter().chain(hog_threads) {
        let _ = t.join();
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v.get(((v.len() as f64 - 1.0) * p) as usize)
            .copied()
            .unwrap_or(f64::NAN)
    };
    for (i, (out, rx)) in results.into_iter().enumerate() {
        let (mut gaps, mut lat, st) = rx.recv().unwrap();
        let n = lat.len();
        println!(
            "output {i}: {n} frames ({:.1} fps), gap max {:.0} ms, latency p50 {:.0} / p99 {:.0} / max {:.0} ms, replaced {}, reconnects {}, dropped {}, late {}",
            n as f64 / (secs as f64 - 3.0),
            pct(&mut gaps, 1.0),
            pct(&mut lat, 0.5),
            pct(&mut lat, 0.99),
            pct(&mut lat, 1.0),
            out.stats().replaced,
            st.reconnects,
            st.dropped,
            st.late
        );
    }
}
