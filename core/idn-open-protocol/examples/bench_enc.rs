//! Encoder throughput at 3840x2160 SHQ0 and 1920x1080 SHQ2.
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use idn_open_protocol::shq::{Fourcc, Plane};
fn main() {
    for (fc, w, h, cw, ch) in [
        (Fourcc::Shq0, 3840, 2160, 1920, 1080),
        (Fourcc::Shq2, 1920, 1080, 960, 1080),
    ] {
        let pl = |w: usize, h: usize| Plane {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| ((i % w) * 7 / 40 + (i / w) % 3) as u8)
                .collect(),
        };
        let planes = vec![pl(w, h), pl(cw, ch), pl(cw, ch)];
        let inp = EncodeInput {
            fourcc: fc,
            width: w,
            height: h,
            planes: &planes,
            precise: None,
        };
        encode(&inp, &EncodeOptions::default());
        let t = std::time::Instant::now();
        for _ in 0..10 {
            encode(
                &inp,
                &EncodeOptions {
                    quality: 92,
                    ..Default::default()
                },
            );
        }
        println!(
            "{fc:?} {w}x{h}: {:.1} ms/frame",
            t.elapsed().as_secs_f64() * 100.0
        );
    }
}
