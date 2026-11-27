//! On this machine's GPU (skipped where there is none): encode and decode at 1080p,
//! measure latency and picture quality (spec §E.18.6 item 5). Needs access to the render
//! node (the `render` group).

use idn_ffmpeg::Ffmpeg;
use idn_open_protocol::codec::{Codec, DecoderFactory, EncoderConfig, EncoderFactory};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::time::{Duration, Instant};

const W: usize = 1920;
const H: usize = 1080;

/// A 4:2:2 test picture: smooth ramps, fine stripes, and a box moving with `n`.
fn picture(n: usize) -> Vec<Plane> {
    let mut y = vec![0u8; W * H];
    for r in 0..H {
        for c in 0..W {
            let mut v = 16 + (c * 200 / W) as u32;
            if (r / 120) % 2 == 1 && c % 8 < 4 {
                v = 200;
            }
            if (300..500).contains(&r) && (n * 12 % W..n * 12 % W + 200).contains(&c) {
                v = 235;
            }
            y[r * W + c] = v as u8;
        }
    }
    let cw = W / 2;
    let cb: Vec<u8> = (0..cw * H)
        .map(|i| (64 + (i % cw) * 128 / cw) as u8)
        .collect();
    let cr: Vec<u8> = (0..cw * H)
        .map(|i| (192 - (i / cw) * 128 / H) as u8)
        .collect();
    vec![
        Plane {
            width: W,
            height: H,
            data: y,
        },
        Plane {
            width: cw,
            height: H,
            data: cb,
        },
        Plane {
            width: cw,
            height: H,
            data: cr,
        },
    ]
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

#[test]
fn encode_and_decode_1080p_on_the_gpu() {
    let ff = Ffmpeg::probe();
    println!(
        "encoders: {:?}; decoders: {:?}",
        ff.encoders(),
        DecoderFactory::codecs(&ff)
    );
    if ff.encoders().is_empty() {
        println!("no hardware encoder here (or no access to the render node): skipped");
        return;
    }
    let frames: Vec<Vec<Plane>> = (0..90).map(picture).collect();
    for codec in EncoderFactory::codecs(&ff) {
        let mut enc = EncoderFactory::open(
            &ff,
            &EncoderConfig {
                codec,
                width: W,
                height: H,
                frame_rate: (30, 1),
                bitrate: 8_000_000,
                keyframe_interval: 60,
            },
        )
        .unwrap();
        let mut dec = DecoderFactory::open(&ff, codec).unwrap();
        let (mut enc_time, mut worst, mut bytes, mut keys) =
            (Duration::ZERO, Duration::ZERO, 0, vec![]);
        let (mut decoded, mut psnr_min, mut late) = (0, f64::MAX, 0);
        for (n, f) in frames.iter().enumerate() {
            let t = Instant::now();
            let out = enc
                .encode(f, Fourcc::Shq2, n as i64, n == 0 || n == 45)
                .unwrap();
            let dt = t.elapsed();
            enc_time += dt;
            worst = worst.max(dt);
            // One access unit per picture, at once (low latency, §E.18.3), after the first.
            if out.is_empty() {
                late += 1;
            }
            for e in out {
                bytes += e.data.len();
                if e.keyframe {
                    keys.push(n);
                    assert!(
                        !e.extra.is_empty(),
                        "{codec:?}: keyframe without parameter sets"
                    );
                }
                let data = if e.keyframe && codec != Codec::Av1 {
                    idn_open_protocol::hx::with_parameter_sets(
                        &e.data,
                        &e.extra,
                        codec == Codec::Hevc,
                    )
                } else {
                    e.data
                };
                if let Some(p) = dec.decode(&data, e.keyframe, n as i64).unwrap() {
                    assert_eq!((p.width, p.height), (W, H));
                    decoded += 1;
                    // Compared with the source picture it most likely is (decoders may
                    // hold one back); the best match.
                    let best = (n.saturating_sub(2)..=n)
                        .map(|k| psnr(&p.planes[0].data, &frames[k][0].data))
                        .fold(0.0, f64::max);
                    psnr_min = psnr_min.min(best);
                }
            }
        }
        let mbps = bytes as f64 * 8.0 / 3.0 / 1e6;
        println!(
            "{codec:?}: encode {:.2} ms/frame (worst {:.1} ms), {late} late, {mbps:.1} Mbit/s, keyframes at {keys:?}, decoded {decoded}, luma PSNR >= {psnr_min:.1} dB",
            enc_time.as_secs_f64() * 1000.0 / frames.len() as f64,
            worst.as_secs_f64() * 1000.0,
        );
        assert!(
            keys.contains(&0) && keys.contains(&45),
            "{codec:?}: forced keyframes {keys:?}"
        );
        assert!(
            late <= 2,
            "{codec:?}: {late} pictures without an access unit"
        );
        assert!(decoded >= 85, "{codec:?}: {decoded} decoded");
        assert!(psnr_min > 32.0, "{codec:?}: PSNR {psnr_min:.1} dB");
        assert!(mbps < 16.0, "{codec:?}: {mbps:.1} Mbit/s for 8");
    }
}
