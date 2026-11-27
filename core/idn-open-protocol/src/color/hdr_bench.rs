use super::*;

/// `cargo test --release --lib hdr_bench -- --ignored --nocapture`: the HDR
/// conversion's time per 2560x1600 frame on the pool, against the SDR one.
#[test]
#[ignore]
fn time_per_frame() {
    let (w, h) = (2560usize, 1600usize);
    let (cw, ch) = (w / 2, h / 2);
    let planes = vec![
        Plane {
            width: w,
            height: h,
            data: (0..w * h).map(|i| (i % 251) as u8).collect(),
        },
        Plane {
            width: cw,
            height: ch,
            data: (0..cw * ch).map(|i| (i % 97 + 80) as u8).collect(),
        },
        Plane {
            width: cw,
            height: ch,
            data: (0..cw * ch).map(|i| (i % 89 + 90) as u8).collect(),
        },
    ];
    for (name, cfg) in [
        ("sdr", ColorConfig::default()),
        (
            "pq",
            ColorConfig {
                transfer: Transfer::Pq,
                primaries: Primaries::Bt2020,
                matrix: Matrix::Bt2020,
                sdr_white_nits: 80.0,
                ..Default::default()
            },
        ),
        (
            "hlg",
            ColorConfig {
                transfer: Transfer::Hlg,
                primaries: Primaries::Bt2020,
                matrix: Matrix::Bt2020,
                ..Default::default()
            },
        ),
    ] {
        let _ = planes_to_rgba(&planes, w, h, AlphaMode::Opaque, &cfg);
        let t = std::time::Instant::now();
        for _ in 0..20 {
            std::hint::black_box(planes_to_rgba(&planes, w, h, AlphaMode::Opaque, &cfg));
        }
        println!(
            "{name}: {:.2} ms per frame ({} pool threads)",
            t.elapsed().as_secs_f64() * 1e3 / 20.0,
            crate::pool::threads()
        );
    }
}
