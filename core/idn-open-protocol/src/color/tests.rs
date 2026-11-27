use super::*;

/// A flat limited-range grey picture: R′ = G′ = B′ = `v` (0–1) as Y′, neutral chroma.
fn flat(v: f64, w: usize, h: usize) -> Vec<Plane> {
    let y = (16.0 + 219.0 * v).round() as u8;
    vec![
        Plane {
            width: w,
            height: h,
            data: vec![y; w * h],
        },
        Plane {
            width: w.div_ceil(2),
            height: h.div_ceil(2),
            data: vec![128; w.div_ceil(2) * h.div_ceil(2)],
        },
        Plane {
            width: w.div_ceil(2),
            height: h.div_ceil(2),
            data: vec![128; w.div_ceil(2) * h.div_ceil(2)],
        },
    ]
}

/// SMPTE ST 2084 inverse EOTF: nits → PQ signal.
fn pq(nits: f64) -> f64 {
    let (m1, m2) = (2610.0 / 16384.0, 2523.0 / 4096.0 * 128.0);
    let (c1, c2, c3) = (
        3424.0 / 4096.0,
        2413.0 / 4096.0 * 32.0,
        2392.0 / 4096.0 * 32.0,
    );
    let y = (nits / 10000.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

#[test]
fn pq_white_lands_on_white_at_the_configured_sdr_level() {
    let hdr = |white: f32| ColorConfig {
        transfer: Transfer::Pq,
        primaries: Primaries::Bt2020,
        matrix: Matrix::Bt2020,
        sdr_white_nits: white,
        ..Default::default()
    };
    // An HDR desktop at 80 nits SDR white: PQ(80) ≈ Y′ 121 on the wire.
    let planes = flat(pq(80.0), 8, 2);
    assert!(
        (120..=123).contains(&planes[0].data[0]),
        "PQ(80 nits) ≈ Y′ 121"
    );
    let rgb = &planes_to_rgba(&planes, 8, 2, AlphaMode::Opaque, &hdr(80.0))[..3];
    assert!(
        rgb.iter().all(|&c| c >= 253),
        "80 nit white at 80 nit SDR: {rgb:?}"
    );
    let rgb = &planes_to_rgba(&planes, 8, 2, AlphaMode::Opaque, &hdr(203.0))[..3];
    // (80/203) linear is sRGB 0.66.
    assert!(
        rgb.iter().all(|&c| (165..=171).contains(&c)),
        "at 203 nit SDR: {rgb:?}"
    );
    let planes = flat(pq(203.0), 8, 2);
    let rgb = &planes_to_rgba(&planes, 8, 2, AlphaMode::Opaque, &hdr(203.0))[..3];
    assert!(rgb.iter().all(|&c| c >= 253), "203 nit white: {rgb:?}");
    // Brighter than white clips, black stays black.
    let rgb = &planes_to_rgba(
        &flat(pq(1000.0), 8, 2),
        8,
        2,
        AlphaMode::Opaque,
        &hdr(203.0),
    )[..3];
    assert_eq!(rgb, [255, 255, 255]);
    let rgb = &planes_to_rgba(&flat(0.0, 8, 2), 8, 2, AlphaMode::Opaque, &hdr(203.0))[..3];
    assert_eq!(rgb, [0, 0, 0]);
}

#[test]
fn hlg_reference_white_is_75_percent() {
    // BT.2408: HLG signal 0.75 is reference white, 203 nits on a 1000 nit display.
    let cfg = ColorConfig {
        transfer: Transfer::Hlg,
        primaries: Primaries::Bt2020,
        matrix: Matrix::Bt2020,
        ..Default::default()
    };
    let rgb = &planes_to_rgba(&flat(0.75, 8, 2), 8, 2, AlphaMode::Opaque, &cfg)[..3];
    assert!(rgb.iter().all(|&c| (250..=255).contains(&c)), "{rgb:?}");
    let rgb = &planes_to_rgba(&flat(0.5, 8, 2), 8, 2, AlphaMode::Opaque, &cfg)[..3];
    assert!(
        rgb.iter().all(|&c| (120..=180).contains(&c)),
        "mid grey: {rgb:?}"
    );
}

/// The straightforward per-pixel conversion the fast one must match exactly.
fn planes_to_rgba_reference(
    planes: &[Plane],
    width: usize,
    height: usize,
    out_alpha: AlphaMode,
    cfg: &ColorConfig,
) -> Vec<u8> {
    let (kr, kb) = cfg.matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    let (ys, cs, yo) = if cfg.limited {
        (255.0 / 219.0, 255.0 / 224.0, 16.0)
    } else {
        (1.0, 1.0, 0.0)
    };
    let f = |v: f64| (v * 65536.0).round() as i32;
    let (ky, kcr_r, kcb_g, kcr_g, kcb_b) = (
        f(ys),
        f(2.0 * (1.0 - kr) * cs),
        f(-2.0 * (1.0 - kb) * kb / kg * cs),
        f(-2.0 * (1.0 - kr) * kr / kg * cs),
        f(2.0 * (1.0 - kb) * cs),
    );
    let wire_premultiplied = cfg.wire_alpha == AlphaMode::Premultiplied;
    let (yp, cbp, crp) = (&planes[0], &planes[1], &planes[2]);
    let (sx, sy) = (
        width.div_ceil(cbp.width.max(1)),
        height.div_ceil(cbp.height.max(1)),
    );
    let mut out = vec![0u8; width * height * 4];
    let clamp = |v: i32| ((v + 32768) >> 16).clamp(0, 255) as u8;
    crate::pool::for_each(out.chunks_mut(width * 4 * 32).collect(), |ci, chunk| {
        for (ry, row) in chunk.chunks_mut(width * 4).enumerate() {
            let y = ci * 32 + ry;
            for x in 0..width {
                let yv = (yp.data[y * yp.width + x] as i32 - yo as i32) * ky;
                let ci2 = (y / sy).min(cbp.height - 1) * cbp.width + (x / sx).min(cbp.width - 1);
                let (cb, cr) = (cbp.data[ci2] as i32 - 128, crp.data[ci2] as i32 - 128);
                let mut px = [
                    clamp(yv + kcr_r * cr),
                    clamp(yv + kcb_g * cb + kcr_g * cr),
                    clamp(yv + kcb_b * cb),
                    255,
                ];
                // Opaque output: alpha is 255 whatever the source sends (colour as sent).
                if let Some(ap) = planes.get(3).filter(|_| out_alpha != AlphaMode::Opaque) {
                    px[3] = ap.data[y * ap.width + x];
                    let a = px[3] as u32;
                    match (wire_premultiplied, out_alpha) {
                        (false, AlphaMode::Premultiplied) => {
                            for c in &mut px[..3] {
                                *c = ((*c as u32 * a + 127) / 255) as u8;
                            }
                        }
                        (true, AlphaMode::Straight) => {
                            for c in &mut px[..3] {
                                // Fully transparent: no colour to recover.
                                *c = (*c as u32 * 255 + a / 2)
                                    .checked_div(a)
                                    .map_or(0, |v| v.min(255) as u8);
                            }
                        }
                        _ => {}
                    }
                }
                row[4 * x..4 * x + 4].copy_from_slice(&px);
            }
        }
    });
    out
}

#[test]
fn fast_conversion_matches_the_reference() {
    let mut seed = 3u64;
    let mut rnd = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect()
    };
    let plane = |w: usize, h: usize, data: Vec<u8>| Plane {
        width: w,
        height: h,
        data,
    };
    for (w, h) in [
        (1usize, 1usize),
        (2, 2),
        (3, 5),
        (64, 33),
        (321, 181),
        (1920, 37),
    ] {
        // 4:2:2, 4:2:0 and 4:4:4 chroma, with alpha.
        for (cw, ch) in [(w.div_ceil(2), h), (w.div_ceil(2), h.div_ceil(2)), (w, h)] {
            let planes = vec![
                plane(w, h, rnd(w * h)),
                plane(cw, ch, rnd(cw * ch)),
                plane(cw, ch, rnd(cw * ch)),
                plane(w, h, rnd(w * h)),
            ];
            for limited in [true, false] {
                for wire in [AlphaMode::Straight, AlphaMode::Premultiplied] {
                    for out in [
                        AlphaMode::Straight,
                        AlphaMode::Premultiplied,
                        AlphaMode::Opaque,
                    ] {
                        let cfg = ColorConfig {
                            limited,
                            wire_alpha: wire,
                            ..ColorConfig::default()
                        };
                        assert_eq!(
                            planes_to_rgba(&planes, w, h, out, &cfg),
                            planes_to_rgba_reference(&planes, w, h, out, &cfg),
                            "{w}x{h} chroma {cw}x{ch} limited {limited} {wire:?}->{out:?}"
                        );
                    }
                }
            }
        }
    }
}

fn px(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8, u8) {
    let p = rgba_to_planes(
        &[r, g, b, a, r, g, b, a],
        2,
        1,
        8,
        Fourcc::Shq7,
        AlphaMode::Straight,
        &ColorConfig::default(),
    );
    (p[0].data[0], p[1].data[0], p[2].data[0], p[3].data[0])
}

#[test]
fn bt709_limited_reference_colours() {
    assert_eq!(px(255, 255, 255, 255), (235, 128, 128, 255));
    assert_eq!(px(0, 0, 0, 255), (16, 128, 128, 255));
    // BT.709 limited red: Y 63, Cb 102, Cr 240.
    assert_eq!(px(255, 0, 0, 255), (63, 102, 240, 255));
    assert_eq!(px(0, 255, 0, 255), (173, 42, 26, 255));
    assert_eq!(px(0, 0, 255, 255), (32, 240, 118, 255));
}

#[test]
fn premultiplied_input_to_straight_wire() {
    // Half-transparent red, premultiplied: (128, 0, 0, 128) → straight red.
    let p = rgba_to_planes(
        &[128, 0, 0, 128, 128, 0, 0, 128],
        2,
        1,
        8,
        Fourcc::Shq7,
        AlphaMode::Premultiplied,
        &ColorConfig::default(),
    );
    assert_eq!(
        (p[0].data[0], p[1].data[0], p[2].data[0], p[3].data[0]),
        (63, 102, 240, 128)
    );
}

#[test]
fn vector_conversion_matches_the_scalar_one() {
    let (w, h) = (61, 40); // AVX2: 48 pixels, SSE2: 8, scalar: 5
    let mut seed = 99u32;
    let mut rgba: Vec<u8> = (0..w * h * 4)
        .map(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 24) as u8
        })
        .collect();
    // Some fully transparent and fully opaque pixels, and premultiplied-valid ones.
    for (i, px) in rgba.chunks_mut(4).enumerate() {
        match i % 5 {
            0 => px[3] = 0,
            1 => px[3] = 255,
            2 => {
                let a = px[3];
                px.iter_mut()
                    .take(3)
                    .for_each(|c| *c = (*c as u16 * a as u16 / 255) as u8);
            }
            _ => {}
        }
    }
    for cfg in [
        ColorConfig::default(),
        ColorConfig {
            limited: false,
            matrix: Matrix::Bt601,
            ..Default::default()
        },
    ] {
        let k = coeffs(&cfg);
        let lim = if cfg.limited {
            (16, 235, 16, 240)
        } else {
            (0, 255, 0, 255)
        };
        for row in 0..h {
            let line = &rgba[row * w * 4..(row + 1) * w * 4];
            let cw = w.div_ceil(2);
            let (mut y0, mut b0, mut r0) = (vec![0; w], vec![0; cw], vec![0; cw]);
            let (mut y1, mut b1, mut r1) = (vec![0; w], vec![0; cw], vec![0; cw]);
            convert_row::<1>(line, w, &k, lim, &mut y0, &mut b0, &mut r0, None, false);
            convert_pixels::<1>(line, w, &k, lim, &mut y1, &mut b1, &mut r1);
            assert_eq!((&y0, &b0, &r0), (&y1, &b1, &r1));
            convert_row::<0>(line, w, &k, lim, &mut y0, &mut b0, &mut r0, None, false);
            convert_pixels::<0>(line, w, &k, lim, &mut y1, &mut b1, &mut r1);
            assert_eq!((&y0, &b0, &r0), (&y1, &b1, &r1));
        }
    }
}

#[test]
fn roundtrip_rgba_through_ycbcr() {
    // Primaries and greys survive RGB → Y′CbCr → RGB within ±2 (8-bit limited range).
    let cols: [[u8; 4]; 6] = [
        [255, 255, 255, 255],
        [0, 0, 0, 255],
        [255, 0, 0, 200],
        [0, 255, 0, 128],
        [0, 0, 255, 0],
        [128, 128, 128, 255],
    ];
    let rgba: Vec<u8> = cols.iter().flat_map(|c| [*c, *c]).flatten().collect();
    let p = rgba_to_planes(
        &rgba,
        12,
        1,
        48,
        Fourcc::Shq7,
        AlphaMode::Straight,
        &ColorConfig::default(),
    );
    let back = planes_to_rgba(&p, 12, 1, AlphaMode::Straight, &ColorConfig::default());
    for (a, b) in rgba.iter().zip(&back) {
        assert!((*a as i32 - *b as i32).abs() <= 2, "{rgba:?}\n{back:?}");
    }
    // Premultiplied output scales colour by alpha.
    let pm = planes_to_rgba(&p, 12, 1, AlphaMode::Premultiplied, &ColorConfig::default());
    // Pixel 6 is green at A = 128: premultiplied ≈ (0, 128, 0, 128).
    let g = &pm[24..28];
    assert!(
        g[0] <= 1 && (g[1] as i32 - 128).abs() <= 1 && g[2] <= 1 && g[3] == 128,
        "{g:?}"
    );
}

#[test]
fn geometry_and_420() {
    let (w, h) = (5usize, 3usize);
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| [(i * 17) as u8, 50, 200, 255])
        .collect();
    let p = rgba_to_planes(
        &rgba,
        w,
        h,
        w * 4,
        Fourcc::Shq0,
        AlphaMode::Opaque,
        &ColorConfig::default(),
    );
    assert_eq!(
        (p[0].width, p[0].height, p[1].width, p[1].height),
        (5, 3, 3, 2)
    );
    let p = rgba_to_planes(
        &rgba,
        w,
        h,
        w * 4,
        Fourcc::Shq2,
        AlphaMode::Opaque,
        &ColorConfig::default(),
    );
    assert_eq!((p[1].width, p[1].height), (3, 3));
    assert_eq!(p.len(), 3);
}

#[test]
fn opaque_output_ignores_the_alpha_plane() {
    let rgba = [10u8, 200, 30, 77].repeat(4);
    let p = rgba_to_planes(
        &rgba,
        2,
        2,
        8,
        Fourcc::Shq7,
        AlphaMode::Straight,
        &ColorConfig::default(),
    );
    let out = planes_to_rgba(&p, 2, 2, AlphaMode::Opaque, &ColorConfig::default());
    assert!(out.chunks(4).all(|px| px[3] == 255), "{out:?}");
    let straight = planes_to_rgba(&p, 2, 2, AlphaMode::Straight, &ColorConfig::default());
    assert!(straight.chunks(4).all(|px| px[3] == 77));
}

/// The AVX2 rows against the scalar ones: every (colour, alpha) pair, and random
/// samples for every matrix and range at both chroma spacings.
#[cfg(target_arch = "x86_64")]
#[test]
fn avx2_rows_match_the_scalar_ones() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    // Alpha: 65536 pixels, colour c in every channel (rotated), alpha a.
    let n = 65536;
    let arow: Vec<u8> = (0..n).map(|i| (i >> 8) as u8).collect();
    let pixels: Vec<u8> = (0..n)
        .flat_map(|i| {
            let c = i as u8;
            [c, c.wrapping_add(85), c.wrapping_add(170), 0]
        })
        .collect();
    for (pm, un) in [(false, false), (true, false), (false, true)] {
        let (mut want, mut got) = (pixels.clone(), pixels.clone());
        alpha_row(&arow, pm, un, &mut want);
        // SAFETY: AVX2 is checked at the top; `arow` has `n` pixels and `got` their 4n bytes.
        unsafe { to_rgba_avx2::alpha(&arow, n / 8, pm, un, &mut got) };
        assert!(want == got, "premultiply {pm} unpremultiply {un}");
    }
    // Colour.
    let mut seed = 12345u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as u8
    };
    for matrix in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
        for limited in [true, false] {
            let cfg = ColorConfig {
                matrix,
                limited,
                ..Default::default()
            };
            for (w, h, cw) in [
                (64, 4, 32),
                (61, 3, 31),
                (64, 4, 64),
                (37, 2, 37),
                (64, 2, 16),
            ] {
                let planes = vec![
                    Plane {
                        width: w,
                        height: h,
                        data: (0..w * h).map(|_| rnd()).collect(),
                    },
                    Plane {
                        width: cw,
                        height: h,
                        data: (0..cw * h).map(|_| rnd()).collect(),
                    },
                    Plane {
                        width: cw,
                        height: h,
                        data: (0..cw * h).map(|_| rnd()).collect(),
                    },
                    Plane {
                        width: w,
                        height: h,
                        data: (0..w * h).map(|_| rnd()).collect(),
                    },
                ];
                for out_alpha in [
                    AlphaMode::Opaque,
                    AlphaMode::Straight,
                    AlphaMode::Premultiplied,
                ] {
                    let got = planes_to_rgba(&planes, w, h, out_alpha, &cfg);
                    // The scalar path for the same picture.
                    let want = scalar_to_rgba(&planes, w, h, out_alpha, &cfg);
                    assert!(
                        got == want,
                        "{matrix:?} limited {limited} {w}x{h}/{cw} {out_alpha:?}"
                    );
                }
            }
        }
    }
}

/// [`planes_to_rgba`] with the scalar rows only.
#[cfg(target_arch = "x86_64")]
fn scalar_to_rgba(
    planes: &[Plane],
    width: usize,
    height: usize,
    out_alpha: AlphaMode,
    cfg: &ColorConfig,
) -> Vec<u8> {
    let (kr, kb) = cfg.matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    let (ys, cs, yo) = if cfg.limited {
        (255.0 / 219.0, 255.0 / 224.0, 16)
    } else {
        (1.0, 1.0, 0)
    };
    let f = |v: f64| (v * 65536.0).round() as i32;
    let k = ToRgb {
        ky: f(ys),
        kcr_r: f(2.0 * (1.0 - kr) * cs),
        kcb_g: f(-2.0 * (1.0 - kb) * kb / kg * cs),
        kcr_g: f(-2.0 * (1.0 - kr) * kr / kg * cs),
        kcb_b: f(2.0 * (1.0 - kb) * cs),
        yo,
    };
    let (pm, un) = match (cfg.wire_alpha == AlphaMode::Premultiplied, out_alpha) {
        (false, AlphaMode::Premultiplied) => (true, false),
        (true, AlphaMode::Straight) => (false, true),
        _ => (false, false),
    };
    let (yp, cbp) = (&planes[0], &planes[1]);
    let (sx, sy) = (width.div_ceil(cbp.width), height.div_ceil(cbp.height));
    let mut out = vec![0u8; width * height * 4];
    for (y, row) in out.chunks_mut(width * 4).enumerate() {
        let cy = (y / sy).min(cbp.height - 1);
        let cw = cbp.width;
        colour_row(
            &yp.data[y * width..][..width],
            &cbp.data[cy * cw..][..cw],
            &planes[2].data[cy * cw..][..cw],
            sx,
            0,
            &k,
            row,
        );
        if out_alpha != AlphaMode::Opaque {
            alpha_row(&planes[3].data[y * width..][..width], pm, un, row);
        }
    }
    out
}

#[test]
fn four_four_four_keeps_chroma_per_pixel() {
    // Red then blue: 4:2:2 would average them; 4:4:4 keeps each.
    let rgba = [255u8, 0, 0, 255, 0, 0, 255, 255];
    let cfg = ColorConfig::default();
    let p = rgba_to_planes(&rgba, 2, 1, 8, Fourcc::Shq4, AlphaMode::Opaque, &cfg);
    assert_eq!(p.len(), 3);
    assert_eq!((p[1].width, p[1].height), (2, 1));
    assert_eq!((p[0].data[0], p[1].data[0], p[2].data[0]), (63, 102, 240));
    assert_eq!((p[0].data[1], p[1].data[1], p[2].data[1]), (32, 240, 118));
    let a = rgba_to_planes(&rgba, 2, 1, 8, Fourcc::Shq9, AlphaMode::Straight, &cfg);
    assert_eq!(
        (a.len(), a[3].width, a[3].data.as_slice()),
        (4, 2, &[255, 255][..])
    );
    // And the round trip back to RGBA.
    let back = planes_to_rgba(&p, 2, 1, AlphaMode::Opaque, &cfg);
    assert!(
        back.iter()
            .zip(&rgba)
            .all(|(a, b)| (*a as i32 - *b as i32).abs() <= 2)
    );
}
