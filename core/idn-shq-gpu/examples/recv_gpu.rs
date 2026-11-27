//! Receives a source as the OBS plugin does with SpeedHQ on the GPU (coefficients from
//! the input, reconstructed and read back here), or with `cpu` as before (the input's
//! own decoder): pictures a second, and the CPU a picture costs, as the OBS plugin's
//! choice measures it (the decode's, every thread's, and the read-back's) and as the
//! process used it (Linux), for checking the one against the other.
//!
//!   cargo run --release -p idn-shq-gpu --example recv_gpu -- HOST:PORT [gpu|cpu] [SECS]

use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_shq_gpu::{Gpu, Reconstructor};
use std::time::{Duration, Instant};

fn main() {
    let mut a = std::env::args().skip(1);
    let source = a.next().expect("HOST:PORT");
    let gpu = a.next().as_deref() != Some("cpu");
    let secs: u64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut recon = gpu.then(|| Gpu::shared("IDN_OBS_IDCT")).flatten().map(|g| {
        println!("GPU: {} ({:?})", g.name, g.precision);
        Reconstructor::new(g)
    });
    let input = Input::open(InputConfig {
        coefficients: recon.is_some(),
        ..InputConfig::new(&source)
    })
    .expect("input");
    // The process's CPU (Linux), from the first picture on.
    let process_cpu = || {
        let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let f: Vec<&str> = s
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        let n = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
        (n(11) + n(12)) * 10.0
    };
    let start = Instant::now();
    let (mut n, mut coefs, mut busy) = (0u64, 0u64, Duration::ZERO);
    let (mut metered, mut cpu0) = (Duration::ZERO, None);
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(100)) {
            if cpu0.is_none() {
                cpu0 = Some(process_cpu());
            } else {
                n += 1;
                metered += v.decode_cpu;
            }
            if let (Some(c), Some(r)) = (v.picture.coefficients.as_ref(), recon.as_mut()) {
                coefs += 1;
                let (t, c0) = (Instant::now(), idn_open_protocol::thread_cpu_time());
                let p = r.reconstruct(c).expect("GPU");
                busy += t.elapsed();
                metered += idn_open_protocol::thread_cpu_time().saturating_sub(c0);
                std::hint::black_box(&p.planes[0].data[0]);
                r.recycle(p);
            }
            v.picture.recycle();
        }
    }
    let process = process_cpu() - cpu0.unwrap_or(0.0);
    println!(
        "{n} pictures in {secs} s ({coefs} as coefficients, {:.1} ms each on the GPU path); \
         CPU a picture: {:.1} ms metered, {:.1} ms the process",
        busy.as_secs_f64() * 1e3 / coefs.max(1) as f64,
        metered.as_secs_f64() * 1e3 / n.max(1) as f64,
        process / n.max(1) as f64
    );
}
