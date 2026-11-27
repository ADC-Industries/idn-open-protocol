//! Lab script for the Output API: one output stepping through format changes while
//! sending audio on its own and metadata, for checking against NDI receivers.
//! Run: lab_output [--name NAME] [--phase-seconds N]
//!
//! Phases (N seconds each, default 15):
//! 1. 1920x1080 29.97 fps, opaque, moving picture
//! 2. alpha on: left half opaque, right half transparent, a half-transparent bar
//! 3. 60 fps (frames sent at 60 fps)
//! 4. 1280x720, still 60 fps, alpha on
//! 5. back to 1920x1080 29.97 fps opaque
//!
//! Throughout: a 440 Hz tone (left) / 880 Hz (right), -12 dBFS, 48 kHz stereo, sent with
//! `send_audio` in 10 ms blocks; `<idn_lab_meta n="k" phase="p"/>` once a second. Prints
//! phase changes, receiver count and metadata received.

use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Premultiplied RGBA: a moving gradient; with `alpha`, right half transparent and a
/// half-transparent bar across the lower third.
fn picture(w: usize, h: usize, n: usize, alpha: bool) -> Arc<[u8]> {
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let xs = (x + 6 * n) % w;
            let mut px = [
                (xs * 255 / w) as u8,
                (y * 255 / h) as u8,
                ((n * 4) % 256) as u8,
                255u8,
            ];
            if alpha {
                let a: u32 = if y > h * 2 / 3 && y < h * 5 / 6 {
                    128
                } else if x >= w / 2 {
                    0
                } else {
                    255
                };
                for c in &mut px[..3] {
                    *c = (*c as u32 * a / 255) as u8;
                }
                px[3] = a as u8;
            }
            v[4 * (y * w + x)..4 * (y * w + x) + 4].copy_from_slice(&px);
        }
    }
    v.into()
}

/// (label, size, frame rate, alpha).
type Phase = (&'static str, (usize, usize), (u32, u32), bool);

fn main() {
    let mut name = "Lab Output".to_owned();
    let mut phase_s = 15u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next().expect("--name NAME"),
            "--phase-seconds" => {
                phase_s = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--phase-seconds N")
            }
            _ => {
                eprintln!("usage: lab_output [--name NAME] [--phase-seconds N]");
                std::process::exit(2);
            }
        }
    }
    let publisher = Publisher::new(PublisherConfig::default()).expect("publisher");
    let out = Arc::new(Output::new(&publisher, OutputConfig::new(&name)).expect("output"));
    println!(
        "{} on TCP port {} (idn-open-protocol {})",
        out.name(),
        out.port(),
        idn_open_protocol::VERSION
    );

    let stop = Arc::new(AtomicBool::new(false));
    let audio = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            let (rate, block) = (48_000u32, 480usize);
            let mut t = 0u64;
            let start = Instant::now();
            let mut sent = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let tone = |f: f64| -> Vec<f32> {
                    (0..block)
                        .map(|i| {
                            (0.25
                                * (2.0 * std::f64::consts::PI * f * (t + i as u64) as f64
                                    / rate as f64)
                                    .sin()) as f32
                        })
                        .collect()
                };
                let b = AudioBlock {
                    sample_rate: rate,
                    channels: vec![tone(440.0), tone(880.0)],
                };
                out.send_audio(b, None).expect("audio");
                t += block as u64;
                sent += 1;
                let due = start + Duration::from_millis(10 * sent as u64);
                if let Some(d) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        })
    };

    let phases: [Phase; 5] = [
        (
            "1: 1920x1080 29.97 opaque",
            (1920, 1080),
            (30000, 1001),
            false,
        ),
        (
            "2: 1920x1080 29.97 alpha",
            (1920, 1080),
            (30000, 1001),
            true,
        ),
        ("3: 1920x1080 60 alpha", (1920, 1080), (60, 1), true),
        ("4: 1280x720 60 alpha", (1280, 720), (60, 1), true),
        (
            "5: 1920x1080 29.97 opaque",
            (1920, 1080),
            (30000, 1001),
            false,
        ),
    ];
    // Every phase's 30 pictures are rendered before starting, so that nothing but the
    // output's own work happens at a phase change (rendering between phases stalled
    // video by 0.1–0.35 s in lab L2).
    let rendered: Vec<Vec<Arc<[u8]>>> = phases
        .iter()
        .map(|&(_, (w, h), _, alpha)| (0..30).map(|n| picture(w, h, n, alpha)).collect())
        .collect();
    let t0 = Instant::now();
    let mut k = 0u64;
    for ((label, (w, h), rate, alpha), pics) in phases.into_iter().zip(rendered) {
        out.set_alpha(alpha);
        out.set_frame_rate(rate);
        println!("[{:6.1} s] phase {label}", t0.elapsed().as_secs_f64());
        let tick = Duration::from_secs_f64(rate.1 as f64 / rate.0 as f64);
        let (start, mut next, mut n) = (Instant::now(), Instant::now(), 0usize);
        let mut last_meta = Instant::now() - Duration::from_secs(1);
        while start.elapsed() < Duration::from_secs(phase_s) {
            let mode = if alpha {
                AlphaMode::Premultiplied
            } else {
                AlphaMode::Opaque
            };
            out.send(VideoFrame::rgba(w, h, pics[n % 30].clone(), mode))
                .expect("video");
            n += 1;
            if last_meta.elapsed() >= Duration::from_secs(1) {
                last_meta = Instant::now();
                let p = &label[..1];
                out.send_metadata(&format!(r#"<idn_lab_meta n="{k}" phase="{p}"/>"#))
                    .expect("metadata");
                k += 1;
                for (who, m) in out.recv_metadata() {
                    println!(
                        "[{:6.1} s] metadata from {who}: {m}",
                        t0.elapsed().as_secs_f64()
                    );
                }
                let st = out.stats();
                println!(
                    "[{:6.1} s] receivers {}, frames submitted {}, replaced {}",
                    t0.elapsed().as_secs_f64(),
                    out.receivers(),
                    st.submitted,
                    st.replaced
                );
            }
            next += tick;
            match next.checked_duration_since(Instant::now()) {
                Some(d) => std::thread::sleep(d),
                None => next = Instant::now(),
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    audio.join().unwrap();
    println!("[{:6.1} s] done", t0.elapsed().as_secs_f64());
}
