//! Pictures' planes scaled, on the library's worker pool: area averaging (a sender
//! scaling a picture down for a receiver that asked for less) and bilinear with a crop
//! (the switcher's tiles, picture-in-pictures, virtual shots and filters). One copy of
//! each: the switcher's own started threads for every plane of every picture (up to 24
//! a picture, 78 ms of CPU for a 1080p to 4K upscale that took 9.5 ms) and ignored the
//! machine's thread setting.

use crate::shq::Plane;

/// Area-average resampling of one plane to `nw` × `nh`.
pub fn area(p: &Plane, nw: usize, nh: usize) -> Plane {
    // Whole numbers of source samples to each (a 1080p screen to IDN Remote's 320 × 180
    // preview, 2160p to 1080p): the mean of each block, summed straight. The weighted
    // passes below took 17% of a core for one 1080p30 preview.
    if nw > 0
        && nh > 0
        && p.width.is_multiple_of(nw)
        && p.height.is_multiple_of(nh)
        && (p.width / nw) * (p.height / nh) <= 1 << 16
    {
        return area_blocks(p, nw, nh);
    }
    /// Weights are in units of 1/2^16; each output's weights sum to exactly 2^16.
    const ONE: u32 = 1 << 16;
    // Per output column (or row): its first source index and how many it covers, with
    // their weights stored flat in `w`.
    struct Spans {
        first: Vec<usize>,
        start: Vec<usize>,
        w: Vec<u32>,
    }
    fn spans(n: usize, max: usize) -> Spans {
        let s = max as f64 / n as f64;
        let mut sp = Spans {
            first: Vec::with_capacity(n),
            start: Vec::with_capacity(n + 1),
            w: Vec::new(),
        };
        for i in 0..n {
            let (a, b) = (i as f64 * s, (i + 1) as f64 * s);
            let first = (a.floor() as usize).min(max - 1);
            let last = (b.ceil() as usize).min(max).max(first + 1);
            sp.first.push(first);
            sp.start.push(sp.w.len());
            let mut left = ONE;
            for k in first..last {
                let x = (((k + 1) as f64).min(b) - (k as f64).max(a)).max(0.0) / s;
                let wk = if k + 1 == last {
                    left
                } else {
                    ((x * ONE as f64).round() as u32).min(left)
                };
                left -= wk;
                sp.w.push(wk);
            }
        }
        sp.start.push(sp.w.len());
        sp
    }
    let (cols, rows) = (spans(nw, p.width), spans(nh, p.height));
    // Bands of output rows on the worker pool (a 4K picture scaled for one
    // receiver held the output to 34 fps on one thread). Each band runs the horizontal
    // pass over just the source rows it needs, so the results are those of one pass.
    const BAND: usize = 16;
    let mut d = vec![0u8; nw * nh];
    crate::pool::for_each(d.chunks_mut(nw * BAND).collect(), |bi, out| {
        let (y0, y1) = (bi * BAND, (bi * BAND + BAND).min(nh));
        let src0 = rows.first[y0];
        let src1 = (y0..y1)
            .map(|y| rows.first[y] + rows.start[y + 1] - rows.start[y])
            .max()
            .unwrap_or(src0)
            .min(p.height);
        // Horizontal pass into 16.16 fixed point (at most 255 · 2^16).
        let mut tmp = vec![0u32; nw * (src1 - src0)];
        for y in src0..src1 {
            let row = &p.data[y * p.width..(y + 1) * p.width];
            let t = &mut tmp[(y - src0) * nw..(y - src0 + 1) * nw];
            for (x, o) in t.iter_mut().enumerate() {
                let (f, ws) = (cols.first[x], &cols.w[cols.start[x]..cols.start[x + 1]]);
                *o = ws.iter().zip(&row[f..]).map(|(w, &v)| w * v as u32).sum();
            }
        }
        // Vertical pass: 32 fractional bits, rounded.
        let mut acc = vec![0u64; nw];
        for y in y0..y1 {
            let (f, ws) = (rows.first[y], &rows.w[rows.start[y]..rows.start[y + 1]]);
            acc.fill(1 << 31);
            for (k, &w) in ws.iter().enumerate() {
                let r = f + k - src0;
                let (wk, src) = (w as u64, &tmp[r * nw..(r + 1) * nw]);
                for x in 0..nw {
                    acc[x] += wk * src[x] as u64;
                }
            }
            for (o, a) in out[(y - y0) * nw..(y - y0 + 1) * nw].iter_mut().zip(&acc) {
                *o = (a >> 32).min(255) as u8;
            }
        }
    });
    Plane {
        width: nw,
        height: nh,
        data: d,
    }
}

/// [`area`] where each output sample is the mean of an `fx` × `fy` block of the source.
fn area_blocks(p: &Plane, nw: usize, nh: usize) -> Plane {
    let (fx, fy) = (p.width / nw, p.height / nh);
    let n = (fx * fy) as u32;
    let mut d = vec![0u8; nw * nh];
    const BAND: usize = 16;
    crate::pool::for_each(d.chunks_mut(nw * BAND).collect(), |bi, out| {
        // The block's rows added down first, whole rows at a time (which vectorises), then
        // each block's columns across once: a few bytes at a time across every row, a
        // 640-wide proxy of a 2160p program cost the switcher more than a core (lab 349).
        let mut col = vec![0u32; nw * fx];
        for (r, o) in out.chunks_mut(nw).enumerate() {
            let y = bi * BAND + r;
            col.fill(0);
            for sy in y * fy..(y + 1) * fy {
                let row = &p.data[sy * p.width..sy * p.width + nw * fx];
                for (c, &v) in col.iter_mut().zip(row) {
                    *c += v as u32;
                }
            }
            for (o, block) in o.iter_mut().zip(col.chunks_exact(fx)) {
                let s: u32 = block.iter().sum();
                *o = ((s + n / 2) / n) as u8;
            }
        }
    });
    Plane {
        width: nw,
        height: nh,
        data: d,
    }
}

/// Taps for one axis: for each output position, the first source index and the weight
/// (0–256) of the next.
fn taps(n_out: usize, n_src: usize, f0: f64, f1: f64) -> Vec<(usize, u32)> {
    let start = f0 * n_src as f64;
    let step = (f1 - f0) * n_src as f64 / n_out.max(1) as f64;
    (0..n_out)
        .map(|i| {
            let f = (start + (i as f64 + 0.5) * step - 0.5).clamp(0.0, (n_src - 1) as f64);
            (f.floor() as usize, ((f - f.floor()) * 256.0).round() as u32)
        })
        .collect()
}

/// The part `crop` (x0, y0, x1, y1 as fractions) of a `sw` × `sh` plane scaled to `dw` ×
/// `dh`, bilinear with 8-bit weights. Each source row is blended across once per band of
/// output rows and kept while the rows below use it (an upscale reads each twice).
pub fn bilinear(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    crop: (f64, f64, f64, f64),
) -> Vec<u8> {
    bilinear_planes(&[(src, sw, sh, dw, dh)], crop)
        .pop()
        .expect("one plane")
}

/// [`bilinear`] for several planes of one picture (`(src, sw, sh, dw, dh)` each), their
/// bands all on the pool at once.
pub fn bilinear_planes(
    planes: &[(&[u8], usize, usize, usize, usize)],
    crop: (f64, f64, f64, f64),
) -> Vec<Vec<u8>> {
    struct Plan<'a> {
        src: &'a [u8],
        sw: usize,
        sh: usize,
        dw: usize,
        xt: Vec<(usize, u32)>,
        yt: Vec<(usize, u32)>,
        band: usize,
    }
    let mut outs: Vec<Vec<u8>> = planes.iter().map(|p| vec![0u8; p.3 * p.4]).collect();
    let plans: Vec<Option<Plan>> = planes
        .iter()
        .map(|&(src, sw, sh, dw, dh)| {
            (sw > 0 && sh > 0 && dw > 0 && dh > 0 && src.len() >= sw * sh).then(|| Plan {
                src,
                sw,
                sh,
                dw,
                xt: taps(dw, sw, crop.0, crop.2),
                yt: taps(dh, sh, crop.1, crop.3),
                // About 64k samples a band: a thumbnail is one, a 4K plane about 130.
                band: (65_536 / dw).clamp(1, dh),
            })
        })
        .collect();
    let mut jobs = Vec::new();
    for (pi, (out, plan)) in outs.iter_mut().zip(&plans).enumerate() {
        if let Some(pl) = plan {
            jobs.extend(
                out.chunks_mut(pl.band * pl.dw)
                    .enumerate()
                    .map(|(bi, rows)| (pi, bi, rows)),
            );
        }
    }
    crate::pool::for_each(jobs, |_, (pi, bi, rows)| {
        let Some(p) = &plans[pi] else { return };
        let (sw, dw) = (p.sw, p.dw);
        let across = |y: usize, row: &mut [u16]| {
            let r = &p.src[y * sw..y * sw + sw];
            for (o, &(xa, wx)) in row.iter_mut().zip(&p.xt) {
                let xb = (xa + 1).min(sw - 1);
                debug_assert!(xa < sw);
                // SAFETY: `taps` clamps every index below the source's width, `sw`.
                let (va, vb) = unsafe { (*r.get_unchecked(xa), *r.get_unchecked(xb)) };
                *o = (va as u32 * (256 - wx) + vb as u32 * wx) as u16;
            }
        };
        // The two source rows blended across, by index.
        let mut held: [(usize, Vec<u16>); 2] =
            [(usize::MAX, vec![0; dw]), (usize::MAX, vec![0; dw])];
        for (r, line) in rows.chunks_mut(dw).enumerate() {
            let (ya, wy) = p.yt[bi * p.band + r];
            let yb = (ya + 1).min(p.sh - 1);
            // Row `ya` in the first slot: the second holds it after a step down a row.
            if held[0].0 != ya {
                if held[1].0 == ya {
                    held.swap(0, 1);
                } else {
                    across(ya, &mut held[0].1);
                    held[0].0 = ya;
                }
            }
            // At the bottom edge both rows are the last.
            if yb != ya && held[1].0 != yb {
                across(yb, &mut held[1].1);
                held[1].0 = yb;
            }
            let (a, b) = (&held[0].1, if yb == ya { &held[0].1 } else { &held[1].1 });
            let (wa, wb) = (256 - wy, wy);
            for ((o, &t), &u) in line.iter_mut().zip(a).zip(b) {
                *o = ((t as u32 * wa + u as u32 * wb + 32768) >> 16) as u8;
            }
        }
    });
    outs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Today's bilinear, sample by sample: the shared one must give the same bytes.
    fn reference(
        src: &[u8],
        sw: usize,
        sh: usize,
        dw: usize,
        dh: usize,
        c: (f64, f64, f64, f64),
    ) -> Vec<u8> {
        let (xt, yt) = (taps(dw, sw, c.0, c.2), taps(dh, sh, c.1, c.3));
        let mut out = Vec::with_capacity(dw * dh);
        for &(ya, wy) in &yt {
            let yb = (ya + 1).min(sh - 1);
            for &(xa, wx) in &xt {
                let xb = (xa + 1).min(sw - 1);
                let p = |y: usize, x: usize| src[y * sw + x] as u32;
                let top = p(ya, xa) * (256 - wx) + p(ya, xb) * wx;
                let bot = p(yb, xa) * (256 - wx) + p(yb, xb) * wx;
                out.push(((top * (256 - wy) + bot * wy + 32768) >> 16) as u8);
            }
        }
        out
    }

    #[test]
    fn bilinear_matches_the_sample_by_sample_blend() {
        let (sw, sh) = (97, 61);
        let src: Vec<u8> = (0..sw * sh).map(|i| (i * 37 % 251) as u8).collect();
        for (dw, dh, c) in [
            (300, 200, (0.0, 0.0, 1.0, 1.0)),
            (40, 30, (0.0, 0.0, 1.0, 1.0)),
            (97, 61, (0.0, 0.0, 1.0, 1.0)),
            (128, 900, (0.25, 0.1, 0.75, 0.6)),
            (1, 1, (0.0, 0.0, 1.0, 1.0)),
        ] {
            assert_eq!(
                bilinear(&src, sw, sh, dw, dh, c),
                reference(&src, sw, sh, dw, dh, c),
                "{dw}x{dh} {c:?}"
            );
        }
    }

    /// Whole-number factors: each sample the rounded mean of its block, as summed one
    /// block at a time.
    #[test]
    fn area_blocks_give_each_blocks_mean() {
        let mut seed = 1u32;
        for (w, h, nw, nh) in [
            (96, 54, 16, 9),
            (3840, 64, 640, 32),
            (1920, 48, 640, 16),
            (50, 30, 7, 4),
        ] {
            let data: Vec<u8> = (0..w * h)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (seed >> 16) as u8
                })
                .collect();
            let p = Plane {
                width: w,
                height: h,
                data,
            };
            let got = area_blocks(&p, nw, nh);
            let (fx, fy) = (w / nw, h / nh);
            let n = (fx * fy) as u32;
            for y in 0..nh {
                for x in 0..nw {
                    let s: u32 = (0..fy)
                        .flat_map(|j| (0..fx).map(move |i| (x * fx + i, y * fy + j)))
                        .map(|(sx, sy)| p.data[sy * w + sx] as u32)
                        .sum();
                    assert_eq!(
                        got.data[y * nw + x],
                        ((s + n / 2) / n) as u8,
                        "{w}x{h} → {nw}x{nh} at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn area_averages_areas() {
        let p = Plane {
            width: 4,
            height: 2,
            data: vec![0, 10, 20, 30, 40, 50, 60, 70],
        };
        // 2:1 both ways: the mean of each 2x2 block.
        assert_eq!(area(&p, 2, 1).data, [25, 45]);
        // 4 → 3 columns: thirds overlap (0·¾+10·¼ = 2.5 → 3, 10·½+20·½ = 15, ...).
        let row = Plane {
            width: 4,
            height: 1,
            data: vec![0, 10, 20, 30],
        };
        assert_eq!(area(&row, 3, 1).data, [3, 15, 28]);
        let flat = Plane {
            width: 1920,
            height: 1080,
            data: vec![200; 1920 * 1080],
        };
        assert!(area(&flat, 640, 360).data.iter().all(|&v| v == 200));
    }

    /// Whole-number ratios take the blocks' means: the same as the weighted passes, to
    /// within their rounding.
    #[test]
    fn whole_ratios_are_block_means() {
        let (w, h) = (96, 54);
        let p = Plane {
            width: w,
            height: h,
            data: (0..w * h).map(|i| (i * 37 % 251) as u8).collect(),
        };
        for (nw, nh) in [(48, 27), (16, 9), (32, 18), (96, 54)] {
            let fast = area(&p, nw, nh);
            let (fx, fy) = (w / nw, h / nh);
            for y in 0..nh {
                for x in 0..nw {
                    let mut s = 0u32;
                    for sy in y * fy..(y + 1) * fy {
                        for sx in x * fx..(x + 1) * fx {
                            s += p.data[sy * w + sx] as u32;
                        }
                    }
                    let n = (fx * fy) as f64;
                    let want = (s as f64 / n).round();
                    let got = fast.data[y * nw + x] as f64;
                    assert!(
                        (got - want).abs() <= 0.5,
                        "{nw}x{nh} ({x},{y}): {got} {want}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod speed {
    /// The area scaler on a 1080p luma plane, to an exact quarter and to a multiview
    /// tile's size (`--release --ignored --nocapture`).
    #[test]
    #[ignore]
    fn area_speed() {
        let p = crate::shq::Plane { width: 1920, height: 1080, data: (0..1920 * 1080).map(|i| (i % 251) as u8).collect() };
        for (w, h) in [(480usize, 270usize), (476, 268), (640, 360), (636, 358)] {
            let t = std::time::Instant::now();
            for _ in 0..30 {
                std::hint::black_box(super::area(&p, w, h));
            }
            eprintln!("1080p luma -> {w}x{h}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 30.0);
        }
    }
}
