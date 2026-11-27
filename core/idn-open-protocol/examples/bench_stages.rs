//! CPU per 1080p frame for each sender stage, on continuously changing content.
use idn_open_protocol::color::{AlphaMode, ColorConfig, rgba_to_planes};
use idn_open_protocol::shq::Fourcc;
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};

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
            v[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    v
}

fn main() {
    let (w, h) = (1920, 1080);
    let frames: Vec<Vec<u8>> = (0..10).map(|n| frame(w, h, n)).collect();
    let cfg = ColorConfig::default();
    let reps = 30;
    let t = cpu();
    let mut planes = Vec::new();
    for i in 0..reps {
        planes = rgba_to_planes(
            &frames[i % 10],
            w,
            h,
            w * 4,
            Fourcc::Shq2,
            AlphaMode::Premultiplied,
            &cfg,
        );
    }
    println!(
        "convert (premultiplied): {:.1} ms CPU/frame",
        (cpu() - t) / reps as f64 * 1e3
    );
    let t = cpu();
    let mut bytes = 0;
    for _ in 0..reps {
        let e = encode(
            &EncodeInput {
                fourcc: Fourcc::Shq2,
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
        bytes = e.len();
    }
    println!(
        "encode Q92: {:.1} ms CPU/frame ({} kB)",
        (cpu() - t) / reps as f64 * 1e3,
        bytes / 1000
    );
    for q in [10u8, 50, 80] {
        let t = cpu();
        let mut b = 0;
        for _ in 0..reps {
            b = encode(
                &EncodeInput {
                    fourcc: Fourcc::Shq2,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions {
                    quality: q,
                    ..Default::default()
                },
            )
            .len();
        }
        println!(
            "encode Q{q}: {:.1} ms CPU/frame ({} kB)",
            (cpu() - t) / reps as f64 * 1e3,
            b / 1000
        );
    }
    let e = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
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
    let t = cpu();
    for _ in 0..reps {
        let f = idn_open_protocol::frame::Frame {
            kind: 7,
            scrambled: true,
            version: 0,
            ext: vec![0; 140],
            payload: e.clone(),
        };
        std::hint::black_box(f.encode());
    }
    println!(
        "frame + scramble + copy: {:.1} ms CPU/frame",
        (cpu() - t) / reps as f64 * 1e3
    );
}
