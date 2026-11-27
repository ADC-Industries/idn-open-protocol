//! Sender CPU for one Output, 1080p30, continuously changing content (premultiplied RGBA),
//! with one receiver in a separate process. Prints this process's CPU use (THREADS=1: per
//! thread). TRANSPORT selects the receiver's transport, CONTENT=camera the picture.
//! Run: cargo run --release --example bench_output [seconds]

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cpu_seconds() -> f64 {
    // utime + stime of this process, in clock ticks (fields 14 and 15 of /proc/self/stat).
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    let rest = &s[s.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // (Linux reads /proc; the tick rate elsewhere doesn't matter.)
    #[cfg(unix)]
    // SAFETY: no pointers.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    #[cfg(not(unix))]
    let ticks = 100.0;
    (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / ticks
}

/// CPU seconds per thread name (threads of the same name summed).
fn thread_cpu() -> std::collections::BTreeMap<String, f64> {
    // (Linux reads /proc; the tick rate elsewhere doesn't matter.)
    #[cfg(unix)]
    // SAFETY: no pointers.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    #[cfg(not(unix))]
    let ticks = 100.0;
    let mut m = std::collections::BTreeMap::new();
    for e in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let Ok(s) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let name = s[s.find('(').unwrap() + 1..s.rfind(')').unwrap()]
            .trim_end_matches(char::is_numeric)
            .to_owned();
        let f: Vec<&str> = s[s.rfind(')').unwrap() + 2..].split_whitespace().collect();
        *m.entry(name).or_insert(0.0) +=
            (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / ticks;
    }
    m
}

/// A moving picture with gradients, edges and fine texture, different every frame.
/// Camera-like: soft texture and gradients, moving, with fresh sensor noise every frame.
fn camera(w: usize, h: usize, n: usize) -> Vec<u8> {
    let mut seed = 7u64 + n as u64;
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let noise = (seed % 7) as i32 - 3;
            let xs = x + 4 * n;
            let base = 70
                + ((xs / 37 + y / 23) % 6) as i32 * 18
                + ((xs as f32 / 50.0).sin() * 25.0) as i32
                + (y / 12) as i32;
            let i = 4 * (y * w + x);
            v[i..i + 4].copy_from_slice(&[
                (base + noise).clamp(0, 255) as u8,
                (base - 10 + noise).clamp(0, 255) as u8,
                (base - 25 + noise).clamp(0, 255) as u8,
                255,
            ]);
        }
    }
    v
}

fn frame(w: usize, h: usize, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = 4 * (y * w + x);
            let xs = x + 3 * n;
            let ys = y + n;
            let r = ((xs * 255) / w) as u8;
            let g = (((ys / 8) % 2) * 160 + ((xs ^ ys) & 31)) as u8;
            let b = (((x as f32 / 40.0 + n as f32 / 10.0).sin() * 100.0) + 128.0) as u8;
            v[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    v
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let (w, h) = (1920, 1080);
    // Pre-render 30 different frames (the renderer's cost isn't ours).
    // CONTENT=camera for camera-like pictures; the default is dense synthetic detail
    // (about 240 Mbit/s at quality 92, a worst case).
    let render = if std::env::var("CONTENT").is_ok_and(|c| c == "camera") {
        camera
    } else {
        frame
    };
    let frames: Vec<Arc<[u8]>> = (0..30).map(|n| render(w, h, n).into()).collect();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(
        &publisher,
        OutputConfig {
            quality: 92,
            ..OutputConfig::new("Bench")
        },
    )
    .unwrap();
    let port = out.port();
    let idn = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/idn");
    let mut child = std::process::Command::new(idn)
        .args([
            "recv",
            &format!("127.0.0.1:{port}"),
            "--seconds",
            &(secs + 3).to_string(),
            "--max-frames",
            "0",
            "--out",
            "/tmp/idn-bench-out",
            "--transport",
            &std::env::var("TRANSPORT").unwrap_or_else(|_| "quic".into()),
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("build idn first: cargo build --release -p idn-cli");
    // Warm up until the receiver is connected.
    let t0 = Instant::now();
    let mut n = 0usize;
    let tick = Duration::from_secs_f64(1.0 / 30.0);
    let mut next = Instant::now();
    let mut measure: Option<(Instant, f64, u64)> = None;
    let mut threads0 = Default::default();
    while t0.elapsed() < Duration::from_secs(secs + 2) {
        let _ = out.send(VideoFrame::rgba(
            w,
            h,
            frames[n % 30].clone(),
            AlphaMode::Premultiplied,
        ));
        n += 1;
        if measure.is_none() && t0.elapsed() > Duration::from_secs(2) {
            measure = Some((Instant::now(), cpu_seconds(), out.stats().submitted));
            threads0 = thread_cpu();
        }
        next += tick;
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    let (t, c, s0) = measure.unwrap();
    let dt = t.elapsed().as_secs_f64();
    let st = out.stats();
    println!(
        "sender CPU: {:.0}% of one core over {:.1} s ({} receivers, {} frames, {} replaced)",
        (cpu_seconds() - c) / dt * 100.0,
        dt,
        out.receivers(),
        st.submitted - s0,
        st.replaced
    );
    if std::env::var_os("THREADS").is_some() {
        for (name, c) in thread_cpu() {
            let d = c - threads0.get(&name).copied().unwrap_or(0.0);
            if d > 0.0 {
                println!("  {name:<20} {:.0}%", d / dt * 100.0);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
