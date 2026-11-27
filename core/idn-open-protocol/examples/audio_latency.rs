//! Sound blocks through the library on this machine: how long from `send_audio` to the
//! input handing the block over, for small blocks (the low-latency mode, R.14 stage 2).
//!
//!   cargo run --release --example audio_latency [BLOCK_MS...] [--channels N] [--seconds S]
//!                                               [--lossless] [--transport tcp|quic|link]
//!
//! Prints, per block size: blocks received, latency median / p99 / max (sender's clock to
//! arrival), the longest gap between arrivals, and the process's CPU time per second.

use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent, Transport};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn cpu_secs() -> f64 {
    // utime + stime of this process, in clock ticks (100 a second on Linux).
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1;
            let f: Vec<&str> = rest.split_whitespace().collect();
            Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
        })
        .unwrap_or(0.0)
}

fn now_100ns() -> i64 {
    idn_common::epoch::now()
        .as_nanos() as i64
        / 100
}

fn main() {
    let mut blocks = Vec::new();
    let (mut channels, mut seconds, mut lossless) = (2usize, 5u64, false);
    let mut transport: Option<Transport> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--channels" => channels = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--lossless" => lossless = true,
            "--transport" => {
                transport = Some(match args.next().unwrap().as_str() {
                    "tcp" => Transport::Tcp,
                    "quic" => Transport::Quic,
                    _ => Transport::Link,
                })
            }
            n => blocks.push(n.parse::<f64>().expect("a block length in ms")),
        }
    }
    if blocks.is_empty() {
        blocks = vec![20.0, 10.0, 5.0, 2.0, 1.0];
    }
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    println!(
        "{channels} channels, {} audio, {}",
        if lossless {
            "float (audio-f32/1)"
        } else {
            "16-bit (fowt)"
        },
        transport.map_or("default transport (IDN link)".to_owned(), |t| format!(
            "{t:?}"
        ))
    );
    for ms in blocks {
        let out =
            Arc::new(Output::new(&publisher, OutputConfig::new(&format!("Lat{ms}"))).unwrap());
        let mut icfg = InputConfig {
            receive_video: false,
            lossless_audio: lossless,
            ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
        };
        if let Some(t) = transport {
            icfg.transports = vec![t];
        }
        let input = Input::open(icfg).unwrap();
        let n = (48_000.0 * ms / 1000.0).round() as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = {
            let (out, stop) = (out.clone(), stop.clone());
            std::thread::spawn(move || {
                let period = Duration::from_secs_f64(n as f64 / 48_000.0);
                let mut next = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let chans = (0..channels)
                        .map(|c| vec![(c as f32 + 1.0) / 100.0; n])
                        .collect();
                    let _ = out.send_audio(
                        AudioBlock {
                            sample_rate: 48_000,
                            channels: chans,
                        },
                        None,
                    );
                    next += period;
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(d);
                    }
                }
            })
        };
        // Wait for sound, then measure.
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end
            && !matches!(
                input.recv(Duration::from_millis(50)),
                Some(InputEvent::Audio(_))
            )
        {}
        let (t0, c0) = (Instant::now(), cpu_secs());
        let mut lat_us: Vec<i64> = Vec::new();
        let (mut last, mut gap) = (Instant::now(), Duration::ZERO);
        while t0.elapsed() < Duration::from_secs(seconds) {
            if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(50)) {
                let now = Instant::now();
                gap = gap.max(now - last);
                last = now;
                lat_us.push((now_100ns() - a.header.timestamp) / 10);
            }
        }
        let cpu = (cpu_secs() - c0) / t0.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        feeder.join().unwrap();
        lat_us.sort_unstable();
        let q = |p: f64| {
            lat_us
                .get(((lat_us.len().max(1) - 1) as f64 * p) as usize)
                .copied()
                .unwrap_or(0) as f64
                / 1000.0
        };
        println!(
            "{ms:>5} ms blocks: {:>6} received ({:.0}/s), latency median {:.2} / p99 {:.2} / max {:.2} ms, longest gap {:.1} ms, CPU {:.1}% of a core",
            lat_us.len(),
            lat_us.len() as f64 / seconds as f64,
            q(0.5),
            q(0.99),
            q(1.0),
            gap.as_secs_f64() * 1000.0,
            cpu * 100.0
        );
    }
}
