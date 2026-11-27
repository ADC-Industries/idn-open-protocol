//! RGBA → SHQ2/SHQ7 planes, then compact encode, at 1080p.
use idn_open_protocol::color::{AlphaMode, ColorConfig, rgba_to_planes};
use idn_open_protocol::shq::Fourcc;
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
fn main() {
    let (w, h) = (1920usize, 1080usize);
    // A lyrics-like frame: dark background, a band of text, translucent lower third.
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let text =
                (300..1600).contains(&x) && (400..640).contains(&y) && (x / 3 + y / 5) % 9 < 3;
            if text {
                [255, 255, 255, 255]
            } else if y > 860 {
                [20, 40, 120, 180]
            } else {
                [10, 10, 30, 255]
            }
        })
        .collect();
    for fc in [Fourcc::Shq2, Fourcc::Shq7] {
        let n = 30;
        let t = std::time::Instant::now();
        let mut planes = Vec::new();
        for _ in 0..n {
            planes = rgba_to_planes(
                &rgba,
                w,
                h,
                w * 4,
                fc,
                AlphaMode::Premultiplied,
                &ColorConfig::default(),
            );
        }
        let conv = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = std::time::Instant::now();
        let mut bytes = 0;
        for _ in 0..n {
            bytes = encode(
                &EncodeInput {
                    fourcc: fc,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions {
                    quality: 92,
                    ..Default::default()
                },
            )
            .len();
        }
        let enc = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!(
            "{fc:?}: convert {conv:.1} ms + encode {enc:.1} ms per frame ({} kB) on {} threads",
            bytes / 1000,
            std::thread::available_parallelism().unwrap()
        );
    }
}
