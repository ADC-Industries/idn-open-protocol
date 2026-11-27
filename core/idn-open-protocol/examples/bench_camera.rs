//! Single-frame cost on camera-like 1080p content (texture + sensor noise): convert + encode.
use idn_open_protocol::color::{AlphaMode, ColorConfig, rgba_to_planes};
use idn_open_protocol::shq::Fourcc;
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
fn main() {
    let (w, h) = (1920usize, 1080usize);
    let mut seed = 7u64;
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let (x, y) = (i % w, i / w);
            let n = (seed % 9) as i32 - 4;
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
    let n = 20;
    let t = std::time::Instant::now();
    let mut planes = Vec::new();
    for _ in 0..n {
        planes = rgba_to_planes(
            &rgba,
            w,
            h,
            w * 4,
            Fourcc::Shq2,
            AlphaMode::Opaque,
            &ColorConfig::default(),
        );
    }
    let conv = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    for q in [92u8, 70] {
        let t = std::time::Instant::now();
        let mut bytes = 0;
        for _ in 0..n {
            bytes = encode(
                &EncodeInput {
                    fourcc: Fourcc::Shq2,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions {
                    quality: q,
                    short_dc: true,
                    ..Default::default()
                },
            )
            .len();
        }
        println!(
            "camera Q{q}: convert {conv:.1} ms + encode {:.1} ms ({} kB) on {} threads",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64,
            bytes / 1000,
            std::thread::available_parallelism().unwrap()
        );
    }
}
