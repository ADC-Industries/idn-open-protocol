//! A minute of 48 kHz sound through a lane's resampler, filtered and passed through:
//! the time each takes (`cargo run --release --example bench_through [channels]`).
use idn_open_protocol::audio::resample::Rate;
use std::time::Instant;

fn main() {
    let ch: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2);
    let chunk = 240;
    let block: Vec<Vec<f32>> = (0..ch)
        .map(|c| (0..chunk).map(|i| ((i + c) as f32 * 0.01).sin()).collect())
        .collect();
    for through in [false, true] {
        let mut rate = Rate::new(48_000, 48_000, ch, chunk, 0.003).unwrap();
        rate.allow_through(through);
        rate.set_through(through);
        let start = Instant::now();
        let mut frames = 0;
        for _ in 0..48_000 * 60 / chunk {
            rate.push(&block);
            rate.pull(|_, n| frames += n).unwrap();
        }
        let took = start.elapsed().as_secs_f64();
        println!(
            "{ch} channels, {}: {:.1} ms a minute of sound ({:.2}% of a core), {frames} frames",
            if through {
                "passed through"
            } else {
                "filtered"
            },
            took * 1000.0,
            took / 60.0 * 100.0
        );
    }
}
