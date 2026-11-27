//! Each video filter's time on one picture, at 1080p and 2160p (§S.17):
//! `cargo run --release -p idn-switcher --example filter_bench`.

use idn_switcher::filter::{Chain, items_from_json};
use idn_switcher::frame::{Frame, Ycc};
use serde_json::json;
use std::time::Instant;

fn main() {
    // A 33-point identity LUT, as a camera LUT is sized.
    let lut = std::env::temp_dir().join("filter_bench.cube");
    let mut text = String::from("LUT_3D_SIZE 33\n");
    for b in 0..33 {
        for g in 0..33 {
            for r in 0..33 {
                text += &format!(
                    "{} {} {}\n",
                    r as f32 / 32.0,
                    g as f32 / 32.0,
                    b as f32 / 32.0
                );
            }
        }
    }
    std::fs::write(&lut, text).unwrap();
    let cases = [
        ("chroma key", json!([{"type": "chroma_key"}])),
        ("colour key", json!([{"type": "colour_key"}])),
        (
            "luma key",
            json!([{"type": "luma_key", "min": 0.1, "min_smooth": 0.05}]),
        ),
        (
            "correct",
            json!([{"type": "correct", "contrast": 0.2, "saturation": 1.2, "hue": 5}]),
        ),
        ("lut", json!([{"type": "lut", "file": lut}])),
        ("sharpen", json!([{"type": "sharpen"}])),
        ("scroll", json!([{"type": "scroll", "x": 0.1}])),
        (
            "key + correct + lut",
            json!([{"type": "chroma_key"}, {"type": "correct", "contrast": 0.1}, {"type": "lut", "file": lut}]),
        ),
    ];
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        // A green screen with a grey person in the middle.
        let mut f = Frame::solid(w, h, Ycc::from_rgb(20, 180, 40));
        for y in h / 4..h * 3 / 4 {
            for x in w / 3..w * 2 / 3 {
                f.y[y * w + x] = 120;
            }
        }
        for (name, v) in &cases {
            let t0 = Instant::now();
            let c = Chain::new(items_from_json(v).unwrap(), None).unwrap();
            let build = t0.elapsed();
            let n = 20;
            let t0 = Instant::now();
            for i in 0..n {
                std::hint::black_box(c.apply(&f, i as f64 / 30.0));
            }
            println!(
                "{w}x{h} {name:22} {:6.2} ms a picture (built in {:.0} ms)",
                t0.elapsed().as_secs_f64() * 1000.0 / n as f64,
                build.as_secs_f64() * 1000.0
            );
        }
    }
    let _ = std::fs::remove_file(lut);
}
