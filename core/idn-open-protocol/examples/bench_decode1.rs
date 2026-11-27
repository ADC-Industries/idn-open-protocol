//! Decode cost of a camera-like 1080p SHQ2 payload on one thread: ms a picture (CPU and
//! wall alike), and a checksum of the picture, so that a change to the decoder can be
//! shown to give the same samples.   bench_decode1 [PICTURES] [--dense]
//! (`--dense`: a detailed picture, noise across the whole range, as the costliest content.)
use idn_open_protocol::color::{AlphaMode, ColorConfig, rgba_to_planes};
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use idn_open_protocol::shq::{Fourcc, Limits, VideoParams, decode};
fn main() {
    let (w, h) = (1920usize, 1080usize);
    let dense = std::env::args().any(|a| a == "--dense");
    let mut seed = 7u64;
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let (x, y) = (i % w, i / w);
            let n = if dense {
                (seed % 161) as i32 - 80
            } else {
                (seed % 9) as i32 - 4
            };
            let base =
                80 + ((x / 37 + y / 23) % 6) as i32 * 20 + ((x as f32 / 40.0).sin() * 25.0) as i32;
            [
                (base + n).clamp(0, 255) as u8,
                (base - 10 + n).clamp(0, 255) as u8,
                (base - 30 + n).clamp(0, 255) as u8,
                255,
            ]
        })
        .collect();
    let planes = rgba_to_planes(
        &rgba,
        w,
        h,
        w * 4,
        Fourcc::Shq2,
        AlphaMode::Opaque,
        &ColorConfig::default(),
    );
    let payload = encode(
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
    let vp = VideoParams {
        fourcc: *b"SHQ2",
        width: w as u32,
        height: h as u32,
        field_id: 0,
    };
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);
    idn_open_protocol::shq::force_workers(1);
    let first = decode(&vp, &payload, &Limits::default()).unwrap();
    let sum: u64 = first
        .planes
        .iter()
        .flat_map(|p| p.data.iter())
        .enumerate()
        .map(|(i, &b)| (i as u64 % 251 + 1) * b as u64)
        .sum();
    let t = std::time::Instant::now();
    for _ in 0..n {
        let p = decode(&vp, &payload, &Limits::default()).unwrap();
        std::hint::black_box(&p);
    }
    println!(
        "decode {} 1080p Q92 ({} kB) on 1 thread: {:.3} ms a picture, checksum {sum}",
        if dense { "dense" } else { "camera" },
        payload.len() / 1000,
        t.elapsed().as_secs_f64() * 1e3 / n as f64
    );
}
