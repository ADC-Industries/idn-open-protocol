//! SDR pictures made HDR (roadmap R.24 stage 1): an 8-bit BT.709 Y'CbCr
//! 4:2:2 picture (a switcher's program, its graphics and SDR cameras) as a 16-bit BT.2100
//! one, PQ or HLG, as BT.2408 maps SDR into HDR: SDR white at 203 nits (HLG: 75 % of the
//! signal), BT.709's colours kept inside BT.2020 (converted in linear light, nothing made
//! more saturated).
//!
//! Each pixel: Y'CbCr → R'G'B' (BT.709, limited range) → linear (BT.1886, γ 2.4) → BT.2020
//! primaries → light (`sdr_white_nits` for 1.0) → PQ's inverse EOTF, or HLG's inverse OOTF
//! (a 1000 nit display, γ 1.2) and OETF → Y'CbCr (BT.2020 non-constant luminance) → 16-bit
//! limited range (the P216 convention: an 8-bit `v` is `v << 8`). Chroma: the pair's two
//! pixels each made HDR, their Cb and Cr averaged.
//!
//! Fast: tables instead of powers. R' to linear is a 4096-entry table; linear to the
//! output signal a 16384-entry one over the square root of the linear value (PQ is steep
//! near black: the square root spaces the entries there), both read at the nearest entry.
//! 1080p on the lab host: PQ 70 ms on one thread, 20 ms on four; HLG 102 / 28 ms. So on
//! the CPU an HDR program is for up to 1080p30; the GPU compositor makes it in a shader. Within a
//! 10-bit step of [`to_hdr_exact`] (the test says how far).

use crate::color::Transfer;
use crate::ext::ColorInfo;

/// BT.709 → BT.2020 primaries, linear light (BT.2087), and BT.2020's luma weights
/// (`transfer`'s, as f32).
const M709_2020: [[f32; 3]; 3] = {
    let m = super::transfer::M709_2020;
    [
        [m[0][0] as f32, m[0][1] as f32, m[0][2] as f32],
        [m[1][0] as f32, m[1][1] as f32, m[1][2] as f32],
        [m[2][0] as f32, m[2][1] as f32, m[2][2] as f32],
    ]
};
const KR: f32 = super::transfer::KR_2020 as f32;
const KB: f32 = super::transfer::KB_2020 as f32;
/// The HLG display the mapping assumes (BT.2408: 1000 nits) and its system gamma.
const HLG_PEAK: f64 = 1000.0;
const HLG_GAMMA: f64 = 1.2;
const N: usize = 4096;
/// The output tables' size (over the square root of light): read without interpolating.
const NO: usize = 16384;

/// The `ndi_color_info` an HDR program says (§E.7): BT.2100, 10 bits.
pub fn hdr_color_info(transfer: Transfer) -> ColorInfo {
    ColorInfo {
        matrix: "bt_2020".into(),
        range: "limited".into(),
        primaries: "bt_2020".into(),
        transfer: match transfer {
            Transfer::Hlg => "bt_2100_hlg",
            _ => "bt_2100_pq",
        }
        .into(),
        bits: 10,
        alpha: "straight".into(),
        siting: "pair-average".into(),
    }
}

pub(crate) use super::transfer::{hlg_oetf, pq_inverse_eotf};

/// One pixel exactly (f64), for tests and as the reference: 8-bit Y'CbCr (BT.709,
/// limited) → 16-bit Y'CbCr (BT.2020, limited, the HDR transfer). Chroma unaveraged.
pub fn to_hdr_exact(y: u8, cb: u8, cr: u8, transfer: Transfer, white_nits: f64) -> [u16; 3] {
    let (yn, b, r) = (
        (y as f64 - 16.0) / 219.0,
        (cb as f64 - 128.0) / 224.0,
        (cr as f64 - 128.0) / 224.0,
    );
    let rgb = [
        yn + 1.5748 * r,
        yn - 0.187324 * b - 0.468124 * r,
        yn + 1.8556 * b,
    ]
    .map(|v| v.clamp(0.0, 1.0).powf(2.4));
    let lin: [f64; 3] =
        std::array::from_fn(|i| (0..3).map(|j| M709_2020[i][j] as f64 * rgb[j]).sum::<f64>());
    let out = signal(lin, transfer, white_nits);
    ycbcr16(out.map(|v| v as f32))
}

/// Linear BT.2020 light (1.0 = SDR white) → the HDR signal, each channel 0–1.
fn signal(lin: [f64; 3], transfer: Transfer, white: f64) -> [f64; 3] {
    match transfer {
        Transfer::Hlg => {
            // Display light relative to the HLG display's peak; its luminance undoes the
            // OOTF's gain (Y_s = Y_d^(1/γ)).
            let f = lin.map(|v| v * white / HLG_PEAK);
            let yd = KR as f64 * f[0] + (1.0 - KR as f64 - KB as f64) * f[1] + KB as f64 * f[2];
            let k = if yd > 0.0 {
                yd.powf((1.0 - HLG_GAMMA) / HLG_GAMMA)
            } else {
                0.0
            };
            f.map(|v| hlg_oetf(v * k))
        }
        _ => lin.map(|v| pq_inverse_eotf(v * white)),
    }
}

/// R'G'B' (BT.2020, 0–1) → 16-bit limited Y'CbCr.
fn ycbcr16(e: [f32; 3]) -> [u16; 3] {
    let y = KR * e[0] + (1.0 - KR - KB) * e[1] + KB * e[2];
    let cb = (e[2] - y) / (2.0 * (1.0 - KB));
    let cr = (e[0] - y) / (2.0 * (1.0 - KR));
    let q =
        |v: f32, lo: f32, span: f32| ((lo + span * v) * 256.0).round().clamp(0.0, 65535.0) as u16;
    [q(y, 16.0, 219.0), q(cb, 128.0, 224.0), q(cr, 128.0, 224.0)]
}

/// The tables for one transfer and white.
pub struct ToHdr {
    transfer: Transfer,
    /// R' (0–1 in N − 1 steps) → linear.
    lin: Vec<f32>,
    /// PQ: √(linear) (0–1, ×(N − 1)) → signal. HLG: √(E) over 0–`hlg_max` → signal.
    out: Vec<f32>,
    /// HLG: √(Y_d) (0–1 relative to the display) → the inverse OOTF's gain.
    gain: Vec<f32>,
    /// HLG: light relative to the display peak for SDR white, and the largest E.
    hlg_scale: f32,
    hlg_max: f32,
}

impl ToHdr {
    /// The tables for `transfer` (PQ or HLG; anything else is taken as PQ), SDR white at
    /// `white_nits` (BT.2408: 203).
    pub fn new(transfer: Transfer, white_nits: f32) -> ToHdr {
        let white = white_nits.max(1.0) as f64;
        let step = |i: usize| i as f64 / (N - 1) as f64;
        let lin = (0..N).map(|i| step(i).powf(2.4) as f32).collect();
        let (out, gain, hlg_scale, hlg_max) = match transfer {
            Transfer::Hlg => {
                let scale = white / HLG_PEAK;
                // E's largest: SDR white's own light at the smallest luminance it comes
                // with (pure blue): its gain is the largest.
                let gain_at = |yd: f64| {
                    if yd > 0.0 {
                        yd.powf((1.0 - HLG_GAMMA) / HLG_GAMMA)
                    } else {
                        0.0
                    }
                };
                let max = (scale * gain_at(scale * KB as f64)).min(1.0);
                let so = |i: usize| i as f64 / (NO - 1) as f64;
                let out = (0..NO)
                    .map(|i| hlg_oetf((so(i) * so(i)) * max) as f32)
                    .collect();
                let gain = (0..N).map(|i| gain_at(step(i) * step(i)) as f32).collect();
                (out, gain, scale as f32, max as f32)
            }
            _ => (
                (0..NO)
                    .map(|i| {
                        let v = i as f64 / (NO - 1) as f64;
                        pq_inverse_eotf(v * v * white) as f32
                    })
                    .collect(),
                Vec::new(),
                0.0,
                0.0,
            ),
        };
        ToHdr {
            transfer,
            lin,
            out,
            gain,
            hlg_scale,
            hlg_max,
        }
    }

    /// The HDR transfer made.
    pub fn transfer(&self) -> Transfer {
        self.transfer
    }

    /// The nearest of a table's entries over 0–1 (no interpolation).
    #[inline]
    fn nearest(t: &[f32], x: f32) -> f32 {
        let n = t.len() - 1;
        // (`as usize` saturates: below 0 is 0.)
        t[((x * n as f32 + 0.5) as usize).min(n)]
    }

    #[inline]
    fn lookup(t: &[f32], x: f32) -> f32 {
        let f = x.clamp(0.0, 1.0) * (N - 1) as f32;
        let i = (f as usize).min(N - 2);
        let w = f - i as f32;
        t[i] + (t[i + 1] - t[i]) * w
    }

    /// One pixel: 8-bit Y'CbCr → the HDR R'G'B' (0–1).
    #[inline]
    fn pixel(&self, y: u8, cb: f32, cr: f32) -> [f32; 3] {
        let yn = (y as f32 - 16.0) * (1.0 / 219.0);
        let rgb = [
            yn + 1.5748 * cr,
            yn - 0.187_324 * cb - 0.468_124 * cr,
            yn + 1.8556 * cb,
        ]
        .map(|v| Self::nearest(&self.lin, v));
        let lin: [f32; 3] = std::array::from_fn(|i| {
            M709_2020[i][0] * rgb[0] + M709_2020[i][1] * rgb[1] + M709_2020[i][2] * rgb[2]
        });
        match self.transfer {
            Transfer::Hlg => {
                let f = lin.map(|v| v * self.hlg_scale);
                let yd = KR * f[0] + (1.0 - KR - KB) * f[1] + KB * f[2];
                let k = Self::lookup(&self.gain, yd.max(0.0).sqrt());
                f.map(|v| Self::nearest(&self.out, (v * k / self.hlg_max).max(0.0).sqrt()))
            }
            _ => lin.map(|v| Self::nearest(&self.out, v.max(0.0).sqrt())),
        }
    }

    /// Rows `rows` of a 4:2:2 picture (`y` w × h, `cb` and `cr` w/2 × h) into `out`, a P216
    /// buffer of the whole picture ([`p216_len`]): Y first, then interleaved Cb, Cr rows.
    pub fn rows(
        &self,
        (y, cb, cr): (&[u8], &[u8], &[u8]),
        w: usize,
        h: usize,
        rows: std::ops::Range<usize>,
        out: &mut [u16],
    ) {
        let cw = w.div_ceil(2);
        let (ys, cs) = out.split_at_mut(w * h);
        for row in rows {
            let (yr, cbr, crr) = (
                &y[row * w..row * w + w],
                &cb[row * cw..row * cw + cw],
                &cr[row * cw..row * cw + cw],
            );
            let (yo, co) = (&mut ys[row * w..row * w + w], &mut cs[row * w..row * w + w]);
            for k in 0..cw {
                let (b, r) = (
                    (cbr[k] as f32 - 128.0) * (1.0 / 224.0),
                    (crr[k] as f32 - 128.0) * (1.0 / 224.0),
                );
                let x = 2 * k;
                let a = ycbcr16(self.pixel(yr[x], b, r));
                yo[x] = a[0];
                let (mut sb, mut sr) = (a[1] as u32, a[2] as u32);
                if x + 1 < w {
                    let c = ycbcr16(self.pixel(yr[x + 1], b, r));
                    yo[x + 1] = c[0];
                    sb += c[1] as u32;
                    sr += c[2] as u32;
                    sb = sb.div_ceil(2);
                    sr = sr.div_ceil(2);
                }
                co[x] = sb as u16;
                if x + 1 < w {
                    co[x + 1] = sr as u16;
                }
            }
        }
    }

    /// The whole picture as P216, its rows split over `threads`.
    pub fn picture(
        &self,
        y: &[u8],
        cb: &[u8],
        cr: &[u8],
        w: usize,
        h: usize,
        threads: usize,
    ) -> Vec<u16> {
        let mut out = vec![0u16; p216_len(w, h)];
        let threads = threads.clamp(1, 16).min(h.max(1));
        if threads == 1 {
            self.rows((y, cb, cr), w, h, 0..h, &mut out);
            return out;
        }
        // Each thread its band of rows, written through its own slices of both halves.
        let per = h.div_ceil(threads);
        let (ys, cs) = out.split_at_mut(w * h);
        std::thread::scope(|s| {
            for ((yb, cbnd), t) in ys.chunks_mut(per * w).zip(cs.chunks_mut(per * w)).zip(0..) {
                let start = t * per;
                let rows = yb.len() / w.max(1);
                s.spawn(move || {
                    let mut band = vec![0u16; p216_len(w, rows)];
                    // Rows start..start+rows, as a picture of their own.
                    let cw = w.div_ceil(2);
                    let (yy, bb, rr) = (
                        &y[start * w..(start + rows) * w],
                        &cb[start * cw..(start + rows) * cw],
                        &cr[start * cw..(start + rows) * cw],
                    );
                    self.rows((yy, bb, rr), w, rows, 0..rows, &mut band);
                    yb.copy_from_slice(&band[..rows * w]);
                    cbnd.copy_from_slice(&band[rows * w..]);
                });
            }
        });
        out
    }
}

/// A P216 buffer's length for `w` × `h`.
pub fn p216_len(w: usize, h: usize) -> usize {
    2 * w * h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BT.2408's anchors: SDR white at 203 nits is 58 % PQ and 75 % HLG; black is code 64
    /// (10-bit) in both.
    #[test]
    fn white_and_black_where_bt2408_puts_them() {
        let ten = |v: u16| v as f64 / 64.0;
        let pct = |v: u16| (ten(v) - 64.0) / 876.0;
        let w = to_hdr_exact(235, 128, 128, Transfer::Pq, 203.0);
        assert!((pct(w[0]) - 0.58).abs() < 0.01, "PQ white {}", pct(w[0]));
        assert_eq!((w[1] >> 8, w[2] >> 8), (128, 128), "white has no colour");
        let w = to_hdr_exact(235, 128, 128, Transfer::Hlg, 203.0);
        assert!((pct(w[0]) - 0.75).abs() < 0.01, "HLG white {}", pct(w[0]));
        for t in [Transfer::Pq, Transfer::Hlg] {
            assert_eq!(to_hdr_exact(16, 128, 128, t, 203.0)[0], 16 << 8);
        }
    }

    /// The tables against the exact sums, over every 8-bit Y and a grid of colours:
    /// within a 10-bit step (64 in 16 bits) for Y, Cb and Cr.
    #[test]
    fn tables_match_the_exact_conversion() {
        for t in [Transfer::Pq, Transfer::Hlg] {
            let fast = ToHdr::new(t, 203.0);
            let mut worst = 0i32;
            for y in 16..=235u8 {
                for cb in (16..=240u8).step_by(14) {
                    for cr in (16..=240u8).step_by(14) {
                        let e = to_hdr_exact(y, cb, cr, t, 203.0);
                        let f = ycbcr16(fast.pixel(
                            y,
                            (cb as f32 - 128.0) / 224.0,
                            (cr as f32 - 128.0) / 224.0,
                        ));
                        for k in 0..3 {
                            worst = worst.max((e[k] as i32 - f[k] as i32).abs());
                        }
                    }
                }
            }
            assert!(worst <= 64, "{t:?}: worst {worst} (16-bit)");
        }
    }

    /// A picture: P216's layout, both halves, threads or not the same.
    #[test]
    fn picture_layout_and_threads() {
        let (w, h) = (64usize, 37usize);
        let y: Vec<u8> = (0..w * h).map(|i| (16 + i % 220) as u8).collect();
        let cb: Vec<u8> = (0..w / 2 * h).map(|i| (16 + (i * 7) % 225) as u8).collect();
        let cr: Vec<u8> = (0..w / 2 * h)
            .map(|i| (16 + (i * 13) % 225) as u8)
            .collect();
        let t = ToHdr::new(Transfer::Pq, 203.0);
        let one = t.picture(&y, &cb, &cr, w, h, 1);
        assert_eq!(one.len(), p216_len(w, h));
        assert_eq!(one, t.picture(&y, &cb, &cr, w, h, 5));
        // Row 3's second pair: its Cb at 2 × (pair) in the chroma half.
        let e = to_hdr_exact(
            y[3 * w + 2],
            cb[3 * w / 2 + 1],
            cr[3 * w / 2 + 1],
            Transfer::Pq,
            203.0,
        );
        assert!((one[3 * w + 2] as i32 - e[0] as i32).abs() <= 64);
        let c = &one[w * h..];
        assert!((c[3 * w + 2] as i32 - e[1] as i32).abs() <= 128);
    }

    /// The conversion's speed at 1080p (cargo test --release -p idn-open-protocol --lib
    /// to_hdr_speed -- --ignored --nocapture).
    #[test]
    #[ignore = "timing"]
    fn to_hdr_speed() {
        let (w, h) = (1920usize, 1080usize);
        let y: Vec<u8> = (0..w * h).map(|i| (16 + i % 220) as u8).collect();
        let c: Vec<u8> = (0..w / 2 * h).map(|i| (16 + i % 225) as u8).collect();
        for tr in [Transfer::Pq, Transfer::Hlg] {
            let t = ToHdr::new(tr, 203.0);
            for threads in [1, 4] {
                let s = std::time::Instant::now();
                for _ in 0..10 {
                    std::hint::black_box(t.picture(&y, &c, &c, w, h, threads));
                }
                println!(
                    "{tr:?} 1080p, {threads} thread(s): {:.1} ms",
                    s.elapsed().as_secs_f64() * 100.0
                );
            }
        }
    }
}
