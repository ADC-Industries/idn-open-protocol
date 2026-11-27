//! Sound from one program to another on this machine, the whole chain (the
//! smallest buffer that doesn't click): a sender making sound in blocks of N
//! samples on its own clock (as a desk or a card does) and sending each block as it is
//! made, the network path, and a receiver taking it into a lane (the buffer before a
//! desk's input or a card) that it reads N samples at a time on its own clock. A click
//! every 100 ms, made at whole tenths of a second from the start: one read out at time t
//! (from the start) was made t mod 100 ms ago (the delay is far under 100 ms): from the
//! moment its sample was made to the moment it is read out (what a card would then play,
//! its own buffer not counted).
//!
//!   cargo run --release --example audio_chain [SAMPLES...] [--pin MS,...] [--seconds S]
//!                                              [--transport tcp|quic|link]
//!
//! Per block size: with the receiver adapting (as by default, from its start guess), then
//! pinned at each `--pin` latency: the delay median / p99 / max, the lane's target and
//! measured jitter at the end, underruns (clicks and gaps), packets a second, CPU.

use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::audio::{LaneConfig, Mixer};
use idn_open_protocol::input::{Input, InputConfig, InputEvent, Transport};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
/// A click every this many samples (100 ms).
const EVERY: u64 = 4800;

fn cpu_secs() -> f64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1;
            let f: Vec<&str> = rest.split_whitespace().collect();
            Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
        })
        .unwrap_or(0.0)
}

/// Sleeps until `t` (as the desk's clock does: a timer, no spinning).
fn until(t: Instant) {
    if let Some(d) = t.checked_duration_since(Instant::now()) {
        std::thread::sleep(d);
    }
}

struct Run {
    delays_us: Vec<i64>,
    target_ms: u32,
    jitter_ms: f64,
    underruns: u64,
    blocks_in: u64,
    secs: f64,
    cpu: f64,
}

fn run(
    publisher: &Publisher,
    n: usize,
    lane: LaneConfig,
    seconds: u64,
    transport: Option<Transport>,
    label: &str,
) -> Run {
    let out =
        Arc::new(Output::new(publisher, OutputConfig::new(&format!("Chain {label}"))).unwrap());
    let mut icfg = InputConfig {
        receive_video: false,
        lossless_audio: true,
        ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
    };
    if let Some(t) = transport {
        icfg.transports = vec![t];
    }
    let input = Input::open(icfg).unwrap();
    let mixer = Mixer::new(RATE, 1, "chain");
    let lane = Arc::new(mixer.lane(&lane));
    let stop = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now() + Duration::from_millis(200);
    // The sender: sample k is made at t0 + k/RATE; a block leaves when its last is made.
    let sender = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut k: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let mut b = vec![0f32; n];
                for (j, v) in b.iter_mut().enumerate() {
                    let s = k + j as u64;
                    if s.is_multiple_of(EVERY) && s > 0 {
                        *v = 0.9;
                    }
                }
                k += n as u64;
                until(t0 + Duration::from_nanos(k * 1_000_000_000 / RATE as u64));
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: RATE,
                        channels: vec![b],
                    },
                    None,
                );
            }
        })
    };
    // The receiver: arrivals into the lane.
    let receiver = {
        let (lane, stop) = (lane.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(20)) {
                    lane.push_all(a.header.sample_rate, &a.channels);
                }
            }
        })
    };
    // The reader: N samples every N/RATE on its own clock (started a second later, so the
    // first clicks find a lane already running); a click read at sample j of a read at
    // time t was "played" at t + j/RATE.
    let delays = Arc::new(Mutex::new(Vec::<i64>::new()));
    let measuring = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader = {
        let (mixer, stop, delays, measuring, reads) = (
            mixer.clone(),
            stop.clone(),
            delays.clone(),
            measuring.clone(),
            reads.clone(),
        );
        std::thread::spawn(move || {
            let mut buf = vec![0f32; n];
            let start = t0 + Duration::from_secs(1);
            let mut r: u64 = 0;
            // (A click's first sample over 0.3; the next looked for 50 ms on: the lane's
            // resampling spreads one over a few samples.)
            let mut quiet_until: i64 = 0;
            while !stop.load(Ordering::Relaxed) {
                r += 1;
                let due = start + Duration::from_nanos(r * n as u64 * 1_000_000_000 / RATE as u64);
                until(due);
                mixer.fill(&mut buf);
                reads.fetch_add(1, Ordering::Relaxed);
                for (j, v) in buf.iter().enumerate() {
                    let played =
                        (due - t0).as_nanos() as i64 + j as i64 * 1_000_000_000 / RATE as i64;
                    if *v < 0.3 || played < quiet_until {
                        continue;
                    }
                    quiet_until = played + 50_000_000;
                    if measuring.load(Ordering::Relaxed) {
                        delays.lock().unwrap().push((played % 100_000_000) / 1000);
                    }
                }
            }
        })
    };
    // 3 s to settle (an adapting lane starts at its guess), then measured.
    std::thread::sleep(Duration::from_secs(3));
    let (s0, c0, b0) = (lane.stats(), cpu_secs(), lane.stats().blocks_in);
    let _ = s0;
    let m0 = Instant::now();
    measuring.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(seconds));
    measuring.store(false, Ordering::Relaxed);
    let secs = m0.elapsed().as_secs_f64();
    let st = lane.stats();
    let cpu = (cpu_secs() - c0) / secs;
    stop.store(true, Ordering::Relaxed);
    for h in [sender, receiver, reader] {
        let _ = h.join();
    }
    let mut d = std::mem::take(&mut *delays.lock().unwrap());
    d.sort_unstable();
    Run {
        delays_us: d,
        target_ms: st.target_ms,
        jitter_ms: st.jitter_ms,
        underruns: st.underruns,
        blocks_in: st.blocks_in - b0,
        secs,
        cpu,
    }
}

fn main() {
    let (mut sizes, mut pins, mut seconds) = (Vec::new(), Vec::new(), 20u64);
    let mut transport = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--pin" => {
                pins = args
                    .next()
                    .unwrap()
                    .split(',')
                    .map(|v| v.parse::<u32>().unwrap())
                    .collect()
            }
            "--transport" => {
                transport = Some(match args.next().unwrap().as_str() {
                    "tcp" => Transport::Tcp,
                    "quic" => Transport::Quic,
                    _ => Transport::Link,
                })
            }
            v => sizes.push(v.parse::<usize>().expect("a block size in samples")),
        }
    }
    if sizes.is_empty() {
        sizes = vec![32, 64, 128, 480];
    }
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    println!(
        "mono, lossless, {}; {seconds} s measured after 3 s",
        transport.map_or("default transport".to_owned(), |t| format!("{t:?}"))
    );
    for &n in &sizes {
        let mut configs: Vec<(String, LaneConfig)> = vec![(
            "adapting".into(),
            LaneConfig {
                latency_ms: 10,
                adaptive: true,
                ..Default::default()
            },
        )];
        for &p in &pins {
            configs.push((
                format!("pinned {p} ms"),
                LaneConfig {
                    latency_ms: p,
                    ..Default::default()
                },
            ));
        }
        for (name, cfg) in configs {
            let r = run(
                &publisher,
                n,
                cfg,
                seconds,
                transport,
                &format!("{n} {name}"),
            );
            let q = |p: f64| {
                r.delays_us
                    .get(((r.delays_us.len().max(1) - 1) as f64 * p) as usize)
                    .copied()
                    .unwrap_or(0) as f64
                    / 1000.0
            };
            println!(
                "{n:>4} samples ({:.2} ms), {name:<14}: delay median {:.2} / p99 {:.2} / max {:.2} ms ({} clicks), target {} ms, jitter {:.2} ms, underruns {}, {:.0} packets/s, CPU {:.1}%",
                n as f64 * 1000.0 / RATE as f64,
                q(0.5),
                q(0.99),
                q(1.0),
                r.delays_us.len(),
                r.target_ms,
                r.jitter_ms,
                r.underruns,
                r.blocks_in as f64 / r.secs,
                r.cpu * 100.0
            );
        }
    }
}
