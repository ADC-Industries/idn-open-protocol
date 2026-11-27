use super::*;
use crate::shq::{Limits, VideoParams, decode};

#[test]
fn malformed_input_is_refused_up_front() {
    let plane = |w: usize, h: usize, len: usize| Plane {
        width: w,
        height: h,
        data: vec![0; len],
    };
    let run = |w: usize, h: usize, planes: Vec<Plane>| {
        std::panic::catch_unwind(move || {
            encode(
                &EncodeInput {
                    fourcc: Fourcc::Shq2,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions::default(),
            )
        })
    };
    let good = || vec![plane(16, 16, 256), plane(8, 16, 128), plane(8, 16, 128)];
    assert!(run(16, 16, good()).is_ok());
    let msg = |r: std::thread::Result<Vec<u8>>| {
        let e = r.unwrap_err();
        e.downcast_ref::<String>().cloned().unwrap_or_default()
    };
    assert!(msg(run(70_000, 16, good())).contains("outside 1..=65535"));
    let mut short = good();
    short[1].data.pop();
    assert!(msg(run(16, 16, short)).contains("plane 1"));
    assert!(msg(run(16, 16, good()[..2].to_vec())).contains("planes for"));
}

#[test]
fn long_zero_runs_are_escaped() {
    // A level after every possible run, including the longest (62).
    for pos in 1..64 {
        let mut lv = [0i32; 64];
        lv[SCAN[pos] as usize] = if pos % 2 == 0 { 3 } else { -1 };
        let mut arena = SymArena::default();
        let s = ac_symbols(&lv, &mut arena);
        assert_eq!(s.n, 1);
        let run = (pos - 1) as u32;
        let word = arena.words[0];
        if run >= 32 {
            assert_eq!(word >> 24, 24, "run {run} must be an escape");
            assert_eq!((word >> 6) & 63, run);
        }
    }
}

#[test]
fn vector_dct_quant_matches_the_portable_one() {
    let q = QTable::new(3);
    let mut seed = 777u32;
    for _ in 0..500 {
        let s: [u8; 64] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 24) as u8
        });
        let rows: [[f32; 8]; 8] =
            std::array::from_fn(|y| std::array::from_fn(|x| s[8 * y + x] as f32));
        let (dc0, lv0, a0) = dct_quant_rows(&rows, &q);
        let (dc1, lv1, a1) = dct_quant_u8(&s, &q);
        assert!((dc0 - dc1).abs() < 1e-3 && a0 == a1);
        // Float evaluation order may differ: allow a level off by one at a rounding tie.
        assert!(
            lv0.iter().zip(&lv1).all(|(a, b)| (a - b).abs() <= 1),
            "{lv0:?} vs {lv1:?}"
        );
    }
}

#[test]
fn aan_dct_matches_the_exact_dct() {
    let mut seed = 12345u32;
    for _ in 0..200 {
        let s: [f32; 64] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 24) as f32
        });
        let exact = fdct_f(&s);
        let rows: [[f32; 8]; 8] = std::array::from_fn(|y| std::array::from_fn(|x| s[8 * y + x]));
        let fast = fdct_aan(&rows);
        for k in 0..64 {
            let got = fast[tr(k)] * aan_scale()[k];
            assert!(
                (got - exact[k]).abs() < 2e-3,
                "k={k}: {got} vs {}",
                exact[k]
            );
        }
    }
}

/// 16-bit SHQ2 planes from a luma function in 8-bit units (chroma mid-grey).
fn planes16(w: usize, h: usize, luma: impl Fn(usize, usize) -> f64) -> Vec<Plane16> {
    let y = Plane16 {
        width: w,
        height: h,
        data: (0..w * h)
            .map(|i| (luma(i % w, i / w) * 256.0).round() as u16)
            .collect(),
    };
    let c = Plane16 {
        width: w / 2,
        height: h,
        data: vec![128 << 8; w / 2 * h],
    };
    vec![y, c.clone(), c]
}

fn precise_roundtrip(
    p16: &[Plane16],
    w: usize,
    h: usize,
    opt: EncodeOptions,
) -> (Vec<u8>, crate::shq::Picture) {
    let p8: Vec<Plane> = p16.iter().map(Plane16::to_8bit).collect();
    let payload = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
            width: w,
            height: h,
            planes: &p8,
            precise: Some(p16),
        },
        &opt,
    );
    let vp = VideoParams {
        fourcc: *b"SHQ2",
        width: w as u32,
        height: h as u32,
        field_id: 0,
    };
    let pic = crate::shq::decode_precise(&vp, &payload, &Limits::default()).unwrap();
    (payload, pic)
}

/// §3.9.5: flat stripes Y' = 100 + k/8 arrive exactly with 3 fraction
/// bits, even when short DC is allowed (it must not be used); the wrapper bit is set.
#[test]
fn precise_flat_stripes_are_exact() {
    let (w, h) = (128, 32);
    let p16 = planes16(w, h, |x, _| 100.0 + (x / 16) as f64 / 8.0);
    let (payload, pic) = precise_roundtrip(
        &p16,
        w,
        h,
        EncodeOptions {
            quality: 80,
            short_dc: true,
            ..Default::default()
        },
    );
    assert_eq!(payload[6] & 7, 4, "high-precision bit, no short DC");
    assert!(pic.high_precision);
    let y = &pic.precise.as_ref().unwrap()[0];
    for x in 0..w {
        assert_eq!(y.data[x], ((800 + x / 16) as u16) << 5, "x={x}");
    }
    // 8-bit output truncates as NDI's does: 100 everywhere.
    assert!(pic.planes[0].data.iter().all(|&v| v == 100));
}

/// Coefficients from precise samples: a gentle ramp is reconstructed closer than from
/// the same ramp rounded to 8 bits first (lab: 0.23 vs 0.29 steps RMS).
#[test]
fn precise_ramp_beats_8bit_input() {
    let (w, h) = (256, 32);
    let ramp = |x: usize, _y: usize| 60.0 + x as f64 * 0.37;
    let p16 = planes16(w, h, ramp);
    let opt = EncodeOptions {
        quality: 95,
        ..Default::default()
    };
    let (_, pic) = precise_roundtrip(&p16, w, h, opt);
    let rms = |plane: &Plane16| {
        let se: f64 = plane
            .data
            .iter()
            .enumerate()
            .map(|(i, &v)| (v as f64 / 256.0 - ramp(i % w, i / w)).powi(2))
            .sum();
        (se / plane.data.len() as f64).sqrt()
    };
    let precise = rms(&pic.precise.as_ref().unwrap()[0]);
    // The same ramp rounded to 8 bits, encoded normally, decoded with fraction bits.
    let p8: Vec<Plane> = p16.iter().map(Plane16::to_8bit).collect();
    let payload = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
            width: w,
            height: h,
            planes: &p8,
            precise: None,
        },
        &opt,
    );
    let vp = VideoParams {
        fourcc: *b"SHQ2",
        width: w as u32,
        height: h as u32,
        field_id: 0,
    };
    let rounded = rms(
        &crate::shq::decode_precise(&vp, &payload, &Limits::default())
            .unwrap()
            .precise
            .unwrap()[0],
    );
    assert!(
        precise < rounded && precise < 0.27,
        "precise {precise:.3} vs 8-bit-rounded {rounded:.3}"
    );
}

fn planes(fourcc: Fourcc, w: usize, h: usize, f: impl Fn(usize, usize, usize) -> u8) -> Vec<Plane> {
    let (cw, ch) = match fourcc {
        Fourcc::Shq0 => (w.div_ceil(2), h.div_ceil(2)),
        Fourcc::Shq2 | Fourcc::Shq7 => (w.div_ceil(2), h),
        _ => (w, h),
    };
    let mut v = vec![(w, h), (cw, ch), (cw, ch)];
    if fourcc.has_alpha() {
        v.push((w, h));
    }
    v.iter()
        .enumerate()
        .map(|(pl, &(pw, ph))| Plane {
            width: pw,
            height: ph,
            data: (0..pw * ph).map(|i| f(pl, i % pw, i / pw)).collect(),
        })
        .collect()
}

fn psnr(a: &[Plane], b: &[Plane]) -> f64 {
    let (mut se, mut n) = (0f64, 0usize);
    for (x, y) in a.iter().zip(b) {
        for (p, q) in x.data.iter().zip(&y.data) {
            se += (*p as f64 - *q as f64).powi(2);
            n += 1;
        }
    }
    if se == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64.powi(2) * n as f64 / se).log10()
    }
}

fn roundtrip(fourcc: Fourcc, w: usize, h: usize, opt: EncodeOptions) -> f64 {
    let p = planes(fourcc, w, h, |pl, x, y| match pl {
        0 => ((x * 3 + y * 2) % 256) as u8,
        1 => (128 + (x as i32 - 20).clamp(-100, 100)) as u8,
        2 => (y * 5 % 256) as u8,
        _ => {
            if (x / 8 + y / 8) % 2 == 0 {
                255
            } else {
                40
            }
        }
    });
    let payload = encode(
        &EncodeInput {
            fourcc,
            width: w,
            height: h,
            planes: &p,
            precise: None,
        },
        &opt,
    );
    let cc = match fourcc {
        Fourcc::Shq0 => *b"SHQ0",
        Fourcc::Shq2 => *b"SHQ2",
        Fourcc::Shq7 => *b"SHQ7",
        Fourcc::Shq4 => *b"SHQ4",
        Fourcc::Shq9 => *b"SHQ9",
    };
    let pic = decode(
        &VideoParams {
            fourcc: cc,
            width: w as u32,
            height: h as u32,
            field_id: 0,
        },
        &payload,
        &Limits::default(),
    )
    .unwrap();
    psnr(&pic.planes, &p)
}

#[test]
fn parallel_output_is_identical() {
    // Large enough to be split over threads; compare with a picture encoded row by
    // row through the serial threshold (narrow width forces one worker).
    let p = planes(Fourcc::Shq2, 640, 480, |pl, x, y| {
        ((x * (pl + 1) + y * 3) % 251) as u8
    });
    let inp = EncodeInput {
        fourcc: Fourcc::Shq2,
        width: 640,
        height: 480,
        planes: &p,
        precise: None,
    };
    for new_format in [true, false] {
        let par = encode(
            &inp,
            &EncodeOptions {
                new_format,
                ..Default::default()
            },
        );
        let pic = decode(
            &VideoParams {
                fourcc: *b"SHQ2",
                width: 640,
                height: 480,
                field_id: 0,
            },
            &par,
            &Limits::default(),
        )
        .unwrap();
        assert!(psnr(&pic.planes, &p) > 40.0);
        // Same bytes as encoding each 16-line stripe on its own would give, row for row.
        if new_format {
            let stripe = |r: usize| -> Vec<Plane> {
                p.iter()
                    .map(|pl| Plane {
                        width: pl.width,
                        height: 16,
                        data: pl.data[r * 16 * pl.width..(r + 1) * 16 * pl.width].to_vec(),
                    })
                    .collect()
            };
            let mut pos = 8;
            for r in 0..30 {
                let sp = stripe(r);
                let one = encode(
                    &EncodeInput {
                        fourcc: Fourcc::Shq2,
                        width: 640,
                        height: 16,
                        planes: &sp,
                        precise: None,
                    },
                    &EncodeOptions::default(),
                );
                let chunk = &one[8..one.len() - 4];
                assert_eq!(&par[pos..pos + chunk.len()], chunk, "row {r}");
                pos += chunk.len();
            }
        }
    }
}

fn decode_with(
    fourcc: Fourcc,
    w: usize,
    h: usize,
    p: &[Plane],
    opt: EncodeOptions,
) -> (Vec<Plane>, usize) {
    let payload = encode(
        &EncodeInput {
            fourcc,
            width: w,
            height: h,
            planes: p,
            precise: None,
        },
        &opt,
    );
    let cc = match fourcc {
        Fourcc::Shq0 => *b"SHQ0",
        Fourcc::Shq2 => *b"SHQ2",
        Fourcc::Shq7 => *b"SHQ7",
        Fourcc::Shq4 => *b"SHQ4",
        Fourcc::Shq9 => *b"SHQ9",
    };
    let pic = decode(
        &VideoParams {
            fourcc: cc,
            width: w as u32,
            height: h as u32,
            field_id: 0,
        },
        &payload,
        &Limits::default(),
    )
    .unwrap();
    (pic.planes, payload.len())
}

/// Compact coding (skip runs, REPEAT, DCONLY, mode 0) reconstructs exactly what the
/// all-FULL coding does, and is never larger.
/// The AVX2 transform gives the SSE one's levels and DC, bit for bit: random blocks of
/// 8-bit samples and with fraction bits, flat ones, at several qualities.
#[cfg(target_arch = "x86_64")]
#[test]
fn avx2_transform_equals_sse() {
    if !has_avx2() {
        return;
    }
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for s_q in [1, 3, 8, 20, 60] {
        let q = QTable::new(s_q);
        for n in 0..2000 {
            let samples: [u8; 64] = if n % 50 == 0 {
                [(n % 256) as u8; 64]
            } else {
                std::array::from_fn(|_| next() as u8)
            };
            let (lo, hi) = sse::load_u8(&samples);
            let (mut a, mut b) = ([0i32; 64], [0i32; 64]);
            let ra = sse::dct_quant(lo, hi, &q, &mut a);
            // SAFETY: AVX2 is there.
            let rb = unsafe { avx2::dct_quant_u8(&samples, &q, &mut b) };
            assert_eq!(
                (ra.0.to_bits(), ra.1, a),
                (rb.0.to_bits(), rb.1, b),
                "8-bit block {n}, s_q {s_q}"
            );
            let f: [f32; 64] =
                std::array::from_fn(|i| samples[i] as f32 + (next() % 8) as f32 / 8.0);
            let (lo, hi) = sse::load_f32(&f);
            let ra = sse::dct_quant(lo, hi, &q, &mut a);
            // SAFETY: AVX2 is there.
            let rb = unsafe { avx2::dct_quant_f32(&f, &q, &mut b) };
            assert_eq!(
                (ra.0.to_bits(), ra.1, a),
                (rb.0.to_bits(), rb.1, b),
                "fraction block {n}, s_q {s_q}"
            );
        }
    }
}

#[test]
fn compact_equals_full_reconstruction() {
    // Rows of 62, 31 and 33 MBs exercise `00000 00000 1`, `00000 1` and long runs that
    // end the row; text-like detail sits in the middle; alpha has flat and busy parts.
    for (fourcc, w, h) in [
        (Fourcc::Shq2, 992, 48),
        (Fourcc::Shq2, 496, 32),
        (Fourcc::Shq7, 528, 40),
        (Fourcc::Shq0, 200, 72),
        (Fourcc::Shq2, 100, 22),
    ] {
        let p = planes(fourcc, w, h, |pl, x, y| {
            let text = x > w / 3 && x < w / 3 + 40 && y % 16 > 4 && (x * 7 + y * 3) % 11 < 4;
            match pl {
                0 => {
                    if text {
                        235
                    } else if y > h / 2 && x < w / 4 {
                        60
                    } else {
                        16
                    }
                }
                3 => {
                    if x < w / 2 {
                        255
                    } else {
                        (x * 9 % 256) as u8
                    }
                }
                _ => 128,
            }
        });
        for q in [98u8, 60] {
            let full = decode_with(
                fourcc,
                w,
                h,
                &p,
                EncodeOptions {
                    quality: q,
                    compact: false,
                    ..Default::default()
                },
            );
            let compact = decode_with(
                fourcc,
                w,
                h,
                &p,
                EncodeOptions {
                    quality: q,
                    ..Default::default()
                },
            );
            assert_eq!(compact.0, full.0, "{fourcc:?} {w}x{h} Q{q}");
            assert!(
                compact.1 < full.1,
                "{fourcc:?} {w}x{h} Q{q}: {} vs {}",
                compact.1,
                full.1
            );
            // Short DC keeps the DC to whole samples (§3.6.3): quality must stay close to
            // normal DC on the same content.
            let short = decode_with(
                fourcc,
                w,
                h,
                &p,
                EncodeOptions {
                    quality: q,
                    short_dc: true,
                    ..Default::default()
                },
            );
            let (ps, pn) = (psnr(&short.0, &p), psnr(&compact.0, &p));
            assert!(
                ps > pn - 1.0,
                "short DC {fourcc:?} Q{q}: {ps:.1} dB vs normal {pn:.1} dB"
            );
        }
    }
}

#[test]
fn skiprun_codes() {
    let bits = |n: usize| {
        let mut w = BitWriter::new();
        write_skiprun(&mut w, n);
        let nbits = w.bit_len();
        let bytes = w.finish();
        (0..nbits)
            .map(|i| {
                if bytes[i / 8] >> (i % 8) & 1 == 1 {
                    '1'
                } else {
                    '0'
                }
            })
            .collect::<String>()
    };
    assert_eq!(bits(0), "1");
    assert_eq!(bits(4), "00110"); // §3.3.4: 001 then v = 1 as "10"
    assert_eq!(bits(31), "000001");
    assert_eq!(bits(62), "00000000001");
    assert_eq!(bits(64), "0000000000011"); // §3.5.2 example: 00000 00000 011 (+ v bit)
    assert_eq!(bits(30), "000011111");
}

#[test]
fn flat_picture_is_exact() {
    let p = planes(Fourcc::Shq2, 64, 16, |_, _, _| 77);
    let payload = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
            width: 64,
            height: 16,
            planes: &p,
            precise: None,
        },
        &EncodeOptions::default(),
    );
    let pic = decode(
        &VideoParams {
            fourcc: *b"SHQ2",
            width: 64,
            height: 16,
            field_id: 0,
        },
        &payload,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(pic.planes, p);
    assert_eq!(&payload[..8], &[198, 64, 0, 16, 0, 0x20, 0, 0]);
}

#[test]
fn roundtrips_all_formats_and_geometries() {
    for (fourcc, w, h) in [
        (Fourcc::Shq2, 64, 16),
        (Fourcc::Shq0, 128, 64),
        (Fourcc::Shq7, 72, 40),
        (Fourcc::Shq2, 100, 22),
        (Fourcc::Shq4, 48, 32),
        (Fourcc::Shq9, 40, 24),
    ] {
        for new_format in [true, false] {
            if !new_format
                && fourcc != Fourcc::Shq2
                && fourcc != Fourcc::Shq7
                && fourcc != Fourcc::Shq0
            {
                continue;
            }
            for q in [98u8, 70, 52] {
                let db = roundtrip(
                    fourcc,
                    w,
                    h,
                    EncodeOptions {
                        quality: q,
                        new_format,
                        ..Default::default()
                    },
                );
                let want = match q {
                    98 => 45.0,
                    70 => 32.0,
                    _ => 28.0,
                };
                assert!(
                    db > want,
                    "{fourcc:?} {w}x{h} new={new_format} Q{q}: {db:.1} dB"
                );
            }
        }
    }
}

#[test]
fn vector_v2_picture_reencodes_close() {
    // Re-encode the decoded v2 picture: the decoder must accept our bitstream and give
    // back nearly the same samples.
    let d = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/shq/v2-new-shq2-ramp-64x16.planes.yuv"
    );
    let raw = std::fs::read(d).unwrap();
    let p = vec![
        Plane {
            width: 64,
            height: 16,
            data: raw[..1024].to_vec(),
        },
        Plane {
            width: 32,
            height: 16,
            data: raw[1024..1536].to_vec(),
        },
        Plane {
            width: 32,
            height: 16,
            data: raw[1536..].to_vec(),
        },
    ];
    let payload = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
            width: 64,
            height: 16,
            planes: &p,
            precise: None,
        },
        &EncodeOptions::default(),
    );
    let pic = decode(
        &VideoParams {
            fourcc: *b"SHQ2",
            width: 64,
            height: 16,
            field_id: 0,
        },
        &payload,
        &Limits::default(),
    )
    .unwrap();
    assert!(psnr(&pic.planes, &p) > 50.0);
}
