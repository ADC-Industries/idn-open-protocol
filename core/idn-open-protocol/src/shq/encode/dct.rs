//! The forward DCT and the quantiser: the scalar path, the reference DCT for tests, and
//! the choice between the SIMD paths.

use super::*;
use crate::shq::vlc::{INTRA_QUANT, ZIGZAG};
use std::sync::OnceLock;

/// Forward DCT basis (the transpose of the decoder's, §3.9.2): the reference the fast
/// DCT is tested against.
#[cfg(test)]
fn basis() -> &'static [[f32; 8]; 8] {
    static B: OnceLock<[[f32; 8]; 8]> = OnceLock::new();
    B.get_or_init(|| {
        let mut b = [[0.0; 8]; 8];
        for (u, row) in b.iter_mut().enumerate() {
            for (x, v) in row.iter_mut().enumerate() {
                let c = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                *v = (c / 2.0 * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos())
                    as f32;
            }
        }
        b // b[u][x]
    })
}

/// The exact separable 8×8 DCT (raster `8·v + u`).
#[cfg(test)]
pub(super) fn fdct_f(s: &[f32; 64]) -> [f32; 64] {
    let b = basis();
    let mut tmp = [[0.0f32; 8]; 8]; // [y][u]
    for y in 0..8 {
        let row: [f32; 8] = std::array::from_fn(|x| s[8 * y + x]);
        for u in 0..8 {
            let mut acc = 0.0;
            for x in 0..8 {
                acc += b[u][x] * row[x];
            }
            tmp[y][u] = acc;
        }
    }
    let mut f = [0.0f32; 64];
    for v in 0..8 {
        for u in 0..8 {
            let mut acc = 0.0;
            for y in 0..8 {
                acc += b[v][y] * tmp[y][u];
            }
            f[8 * v + u] = acc;
        }
    }
    f
}

/// The AAN 8-point DCT (as in the JPEG float DCT), on eight columns at once: `d[i]` is
/// row `i`, each lane a column. Outputs are the true DCT scaled per coefficient (see
/// [`aan_scale`]); the scale is folded into the quantiser. About 5 multiplies and 29 adds
/// per 8 points instead of 64 multiply-adds, and the lanes vectorise.
#[inline(always)]
fn aan8(d: &[[f32; 8]; 8]) -> [[f32; 8]; 8] {
    let mut o = [[0.0f32; 8]; 8];
    for x in 0..8 {
        let (d0, d1, d2, d3, d4, d5, d6, d7) = (
            d[0][x], d[1][x], d[2][x], d[3][x], d[4][x], d[5][x], d[6][x], d[7][x],
        );
        let (t0, t7) = (d0 + d7, d0 - d7);
        let (t1, t6) = (d1 + d6, d1 - d6);
        let (t2, t5) = (d2 + d5, d2 - d5);
        let (t3, t4) = (d3 + d4, d3 - d4);
        // Even part.
        let (t10, t13) = (t0 + t3, t0 - t3);
        let (t11, t12) = (t1 + t2, t1 - t2);
        o[0][x] = t10 + t11;
        o[4][x] = t10 - t11;
        let z1 = (t12 + t13) * std::f32::consts::FRAC_1_SQRT_2;
        o[2][x] = t13 + z1;
        o[6][x] = t13 - z1;
        // Odd part.
        let (u10, u11, u12) = (t4 + t5, t5 + t6, t6 + t7);
        let z5 = (u10 - u12) * 0.382_683_43;
        let z2 = 0.541_196_1 * u10 + z5;
        let z4 = 1.306_563 * u12 + z5;
        let z3 = u11 * std::f32::consts::FRAC_1_SQRT_2;
        let (z11, z13) = (t7 + z3, t7 - z3);
        o[5][x] = z13 + z2;
        o[3][x] = z13 - z2;
        o[1][x] = z11 + z4;
        o[7][x] = z11 - z4;
    }
    o
}

fn transpose(m: &[[f32; 8]; 8]) -> [[f32; 8]; 8] {
    std::array::from_fn(|i| std::array::from_fn(|j| m[j][i]))
}

/// Per-coefficient factor turning AAN outputs into the true DCT of [`fdct_f`]:
/// `1 / (8 · a[v] · a[u])`, `a[0] = 1`, `a[k] = √2 · cos(kπ/16)`.
pub(super) fn aan_scale() -> &'static [f32; 64] {
    static S: OnceLock<[f32; 64]> = OnceLock::new();
    S.get_or_init(|| {
        let a = |k: usize| {
            if k == 0 {
                1.0
            } else {
                std::f64::consts::SQRT_2 * (k as f64 * std::f64::consts::PI / 16.0).cos()
            }
        };
        std::array::from_fn(|i| (1.0 / (8.0 * a(i / 8) * a(i % 8))) as f32)
    })
}

/// Transposes a raster index: `8·v + u` ↔ `8·u + v`.
pub(super) const fn tr(k: usize) -> usize {
    (k % 8) * 8 + k / 8
}

/// The zigzag scan over the transposed layout the DCT produces (scan position → index).
pub(super) const SCAN: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        t[i] = tr(ZIGZAG[i] as usize) as u8;
        i += 1;
    }
    t
};

/// AAN-scaled 2-D DCT, transposed: coefficient `8·v + u` at index `8·u + v` (the final
/// transpose is folded into the quantiser tables and the scan). Multiply by
/// [`aan_scale`] (at the raster index) for the true DCT.
// The scalar path: used off x86-64 (which has the SIMD one), tested everywhere.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
#[inline(always)]
pub(super) fn fdct_aan(s: &[[f32; 8]; 8]) -> [f32; 64] {
    let cols = aan8(s); // along y, per column
    let r = aan8(&transpose(&cols)); // along x
    let mut f = [0.0f32; 64];
    for u in 0..8 {
        f[8 * u..8 * u + 8].copy_from_slice(&r[u]);
    }
    f
}
/// AVX2 is there (checked once).
#[cfg(target_arch = "x86_64")]
pub(super) fn has_avx2() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}
/// Quantises AAN coefficients (transposed layout) to levels, index 0 left at 0; returns
/// whether any AC level is non-zero. Rounds half away from zero.
// The scalar path: used off x86-64 (which has the SIMD one), tested everywhere.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
#[inline(always)]
fn quant_levels(f: &[f32; 64], q: &QTable) -> ([i32; 64], bool) {
    let mut lv = [0i32; 64];
    let mut any = 0i32;
    for k in 0..64 {
        let x = f[k] * q.inv_aan[k];
        // Clamp in floating point, then a plain truncating conversion.
        let m = (x.abs() + 0.5).min(q.lim_f[k]);
        // SAFETY: `m` is finite (finite samples) and within 0..=2047 after the min.
        let c = unsafe { m.to_int_unchecked::<i32>() };
        let c = if x < 0.0 { -c } else { c };
        lv[k] = c;
        any |= c; // lane 0 is 0: its factor and limit are 0
    }
    (lv, any != 0)
}

/// Per-quality quantiser tables, in the DCT's transposed layout.
pub(crate) struct QTable {
    /// `16 / M[k]` with the AAN output scale folded in: AAN coefficient → level directly.
    pub(super) inv_aan: [f32; 64],
    /// The |level| limit of each AC position, as a float for clamping before conversion.
    pub(super) lim_f: [f32; 64],
}

impl QTable {
    pub(super) fn new(s_q: i32) -> Self {
        let mut inv_aan = [0.0; 64];
        let mut lim_f = [0.0; 64];
        for k in 1..64 {
            let m = INTRA_QUANT[k] as i32 * s_q;
            inv_aan[tr(k)] = 16.0 / m as f32 * aan_scale()[k];
            // Keep |c·M| ≤ 32767 (§3.8) and levels within −2048..2047.
            lim_f[tr(k)] = (32767 / m).min(2047) as f32;
        }
        QTable { inv_aan, lim_f }
    }
}

/// DCT and quantisation of one block: the true DC coefficient (8 × the mean), the levels
/// (transposed layout) and whether any AC level is non-zero.
#[inline(always)]
pub(super) fn dct_quant_u8(samples: &[u8; 64], q: &QTable) -> (f32, [i32; 64], bool) {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            let mut lv = [0i32; 64];
            // SAFETY: AVX2 is there.
            let (dc, any) = unsafe { avx2::dct_quant_u8(samples, q, &mut lv) };
            return (dc * aan_scale()[0], lv, any);
        }
        let (lo, hi) = sse::load_u8(samples);
        let mut lv = [0i32; 64];
        let (dc, any) = sse::dct_quant(lo, hi, q, &mut lv);
        (dc * aan_scale()[0], lv, any)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let rows: [[f32; 8]; 8] =
            std::array::from_fn(|y| std::array::from_fn(|x| samples[8 * y + x] as f32));
        dct_quant_rows(&rows, q)
    }
}

#[inline(always)]
pub(super) fn dct_quant_f32(samples: &[f32; 64], q: &QTable) -> (f32, [i32; 64], bool) {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            let mut lv = [0i32; 64];
            // SAFETY: AVX2 is there.
            let (dc, any) = unsafe { avx2::dct_quant_f32(samples, q, &mut lv) };
            return (dc * aan_scale()[0], lv, any);
        }
        let (lo, hi) = sse::load_f32(samples);
        let mut lv = [0i32; 64];
        let (dc, any) = sse::dct_quant(lo, hi, q, &mut lv);
        (dc * aan_scale()[0], lv, any)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let rows: [[f32; 8]; 8] =
            std::array::from_fn(|y| std::array::from_fn(|x| samples[8 * y + x]));
        dct_quant_rows(&rows, q)
    }
}

// The scalar path: used off x86-64 (which has the SIMD one), tested everywhere.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
#[inline(always)]
pub(super) fn dct_quant_rows(rows: &[[f32; 8]; 8], q: &QTable) -> (f32, [i32; 64], bool) {
    let f = fdct_aan(rows);
    let (lv, any) = quant_levels(&f, q);
    (f[0] * aan_scale()[0], lv, any)
}
