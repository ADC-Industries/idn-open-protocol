//! An audio mixer's input lanes under load: a routed source of N channels
//! onto a 64-channel mixer (idn-mixer's inputs), pushed in 2 ms blocks and taken in
//! 2 ms blocks, as idn-mixer's router and clock do. Gives the time per second of sound
//! for pushing (resampling) and taking.
//!
//! `adaptive` makes the lanes adapt (R.30: their jitter measured, the mixer's sync group
//! published to with every push); LANES sources of CHANNELS each (default 1).
//!
//!   cargo run --release --example bench_lanes [-- CHANNELS [SECONDS [fixed|adaptive [LANES]]]]

use idn_open_protocol::audio::{LaneConfig, Mixer};
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let channels: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);
    let secs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let adaptive = args.next().is_some_and(|m| m == "adaptive");
    let lanes: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    let (rate, block, inputs) = (48_000usize, 96usize, 64usize);
    let mixer = Mixer::new(rate as u32, inputs as u16, "bench");
    let all: Vec<_> = (0..lanes)
        .map(|_| {
            mixer.lane(&LaneConfig {
                latency_ms: 10,
                max_correction_ppm: 1000.0,
                adaptive,
                ..Default::default()
            })
        })
        .collect();
    let maps: Vec<Vec<(usize, u16)>> = (0..lanes)
        .map(|l| (0..channels).map(|c| (c, ((l * channels + c) % inputs) as u16)).collect())
        .collect();
    let mut out = vec![0f32; block * inputs];
    let (mut push, mut take) = (Duration::ZERO, Duration::ZERO);
    let blocks = secs * rate / block;
    for b in 0..blocks {
        let src: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                (0..block)
                    .map(|i| {
                        let t = (b * block + i) as f32 / rate as f32;
                        0.1 * (2.0 * std::f32::consts::PI * 300.0 * (c + 1) as f32 * t).sin()
                    })
                    .collect()
            })
            .collect();
        let t0 = Instant::now();
        for (lane, map) in all.iter().zip(&maps) {
            lane.push(rate as u32, &src, map);
        }
        push += t0.elapsed();
        let t1 = Instant::now();
        mixer.fill(&mut out);
        take += t1.elapsed();
    }
    let per_s = |d: Duration| d.as_secs_f64() * 1e3 / secs as f64;
    println!(
        "{lanes} x {channels} channels onto {inputs} ({}): push {:.1} ms and take {:.1} ms per second of sound \
         ({:.1}% of a core)",
        if adaptive { "adaptive" } else { "fixed" },
        per_s(push),
        per_s(take),
        (per_s(push) + per_s(take)) / 10.0
    );
}
