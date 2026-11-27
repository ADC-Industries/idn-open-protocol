//! 4K (3840x2160) per-frame cost of each stage, sender and receiver, wall time and CPU,
//! on continuously changing content. The work pool's size is `IDN_WORKER_THREADS`
//! (default: the physical cores, at most 6). Also the receive throughput with two pictures
//! decoding at once, as inputs do by default (`InputConfig::parallel_decode`).
//! Run: cargo run --release --example bench_4k
use idn_open_protocol::color::{AlphaMode, ColorConfig, planes_to_rgba_into, rgba_to_planes};
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use idn_open_protocol::shq::{Fourcc, Limits, VideoParams};
use std::time::Instant;

/// (Unix: this process's CPU time; elsewhere none measured.)
#[cfg(not(unix))]
fn cpu() -> f64 {
    0.0
}

#[cfg(unix)]
fn cpu() -> f64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: fills a timespec of our own.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) };
    t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9
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
            v[i..i + 4].copy_from_slice(&[r, g, b, (128 + (x % 128)) as u8]);
        }
    }
    v
}

/// Runs `f` `reps` times: (wall ms, CPU ms) per run.
fn time(reps: usize, mut f: impl FnMut(usize)) -> (f64, f64) {
    f(0); // warm-up
    let (w0, c0) = (Instant::now(), cpu());
    for i in 0..reps {
        f(i);
    }
    (
        w0.elapsed().as_secs_f64() * 1e3 / reps as f64,
        (cpu() - c0) * 1e3 / reps as f64,
    )
}

fn main() {
    let (w, h) = (3840, 2160);
    let frames: Vec<Vec<u8>> = (0..6).map(|n| frame(w, h, n)).collect();
    let cfg = ColorConfig::default();
    let reps = 20;
    let threads = std::env::var("IDN_WORKER_THREADS").unwrap_or_else(|_| "default".into());
    println!("3840x2160, work pool: {threads}");
    for (fourcc, alpha, label) in [
        (Fourcc::Shq2, AlphaMode::Opaque, "SHQ2 (4:2:2)"),
        (
            Fourcc::Shq7,
            AlphaMode::Premultiplied,
            "SHQ7 (4:2:2 + alpha)",
        ),
    ] {
        let mut planes = Vec::new();
        let (wc, cc) = time(reps, |i| {
            planes = rgba_to_planes(&frames[i % 6], w, h, w * 4, fourcc, alpha, &cfg);
        });
        let mut bytes = Vec::new();
        let (we, ce) = time(reps, |_| {
            bytes = encode(
                &EncodeInput {
                    fourcc,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions {
                    quality: 92,
                    ..Default::default()
                },
            );
        });
        let vp = VideoParams {
            fourcc: match fourcc {
                Fourcc::Shq7 => *b"SHQ7",
                _ => *b"SHQ2",
            },
            width: w as u32,
            height: h as u32,
            field_id: 0,
        };
        let mut pic = None;
        let (wd, cd) = time(reps, |_| {
            pic = Some(
                idn_open_protocol::shq::decode(&vp, &bytes, &Limits::default()).expect("decodes"),
            );
        });
        let p = pic.expect("decoded");
        let mut buf = Vec::new();
        let (wr, cr) = time(reps, |_| {
            planes_to_rgba_into(&p.planes, w, h, alpha, &cfg, &mut buf);
            std::hint::black_box(&buf);
        });
        // Two pictures at once, each decoded and converted on the pool.
        let t = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    let mut buf = Vec::new();
                    for _ in 0..reps / 2 {
                        let p = idn_open_protocol::shq::decode(&vp, &bytes, &Limits::default())
                            .expect("decodes");
                        planes_to_rgba_into(&p.planes, w, h, alpha, &cfg, &mut buf);
                        std::hint::black_box(&buf);
                    }
                });
            }
        });
        let two = reps as f64 / t.elapsed().as_secs_f64();
        // Half size, for a receiver showing it at 1080p (NDI sources ignore size requests).
        let mut half = None;
        let (wh, ch) = time(reps, |_| {
            half = Some(
                idn_open_protocol::shq::decode_half(&vp, &bytes, &Limits::default())
                    .expect("decodes"),
            );
        });
        let hp = half.expect("decoded");
        let (whr, chr) = time(reps, |_| {
            planes_to_rgba_into(&hp.planes, hp.width, hp.height, alpha, &cfg, &mut buf);
            std::hint::black_box(&buf);
        });
        println!(
            "{label}: send: convert {wc:.1} ms ({cc:.1} CPU) + encode {we:.1} ms ({ce:.1} CPU) = {:.1} ms; \
receive: decode {wd:.1} ms ({cd:.1} CPU) + to RGBA {wr:.1} ms ({cr:.1} CPU) = {:.1} ms \
({:.0} fps one at a time, {two:.0} fps two at once); half size: {wh:.1} ms ({ch:.1} CPU) \
+ to RGBA {whr:.1} ms ({chr:.1} CPU); {:.1} MB/frame",
            wc + we,
            wd + wr,
            1000.0 / (wd + wr),
            bytes.len() as f64 / 1e6
        );
    }
}
