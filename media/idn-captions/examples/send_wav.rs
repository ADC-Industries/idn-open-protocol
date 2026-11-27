//! Plays a 16 kHz WAV (16-bit) as an IDN sound source in real time, then quiet:
//! `send_wav FILE PORT SECONDS` (for trying captions).

use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let b = std::fs::read(&a[1]).unwrap();
    let data = b.windows(4).position(|w| w == b"data").unwrap() + 8;
    let s16: Vec<f32> = b[data..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    // 16 kHz to 48 kHz, each sample three times.
    let s48: Vec<f32> = s16.iter().flat_map(|s| [*s; 3]).collect();
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(
        &p,
        OutputConfig {
            port: a[2].parse().unwrap(),
            ..OutputConfig::new("Speech")
        },
    )
    .unwrap();
    let end = Instant::now() + Duration::from_secs(a[3].parse().unwrap());
    let start = Instant::now();
    let mut k = 0usize;
    while Instant::now() < end {
        let block: Vec<f32> = (0..960)
            .map(|i| s48.get(k + i).copied().unwrap_or(0.0))
            .collect();
        k += 960;
        let _ = out.send_audio(
            AudioBlock {
                sample_rate: 48000,
                channels: vec![block.clone(), block],
            },
            None,
        );
        let due = start + Duration::from_millis((k / 48) as u64);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}
