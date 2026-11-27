//! 8×8 inverse DCT of [H262] Annex A, evaluated exactly in double precision, followed by
//! rounding half away from zero and clamping to 0..255 (spec §3.9.2); or, when asked, in
//! single precision (faster: a sample one step off now and then).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// The arithmetic of the inverse DCT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Precision {
    /// The specification's (§3.9.2): every receiver makes the same samples.
    F64,
    /// Faster (eight samples an instruction on the CPU, not four; GPUs without f64): a
    /// sample one step off now and then (about 3 in 10 000, never more than one step).
    F32,
}

impl Precision {
    /// As the machine's video settings write it (§N.13): `exact` or `fast`.
    pub fn setting_name(self) -> &'static str {
        match self {
            Precision::F64 => "exact",
            Precision::F32 => "fast",
        }
    }

    /// From a setting's name (`exact`, `fast`; also `f64`, `f32`).
    pub fn from_setting(s: &str) -> Option<Precision> {
        match s {
            "exact" | "f64" => Some(Precision::F64),
            "fast" | "f32" => Some(Precision::F32),
            _ => None,
        }
    }
}

/// This program's choice and the machine's (0: none, 1: f64, 2: f32).
static PROGRAM: AtomicU8 = AtomicU8::new(0);
static MACHINE: AtomicU8 = AtomicU8::new(0);

fn code(p: Option<Precision>) -> u8 {
    match p {
        None => 0,
        Some(Precision::F64) => 1,
        Some(Precision::F32) => 2,
    }
}

fn of_code(c: u8) -> Option<Precision> {
    match c {
        1 => Some(Precision::F64),
        2 => Some(Precision::F32),
        _ => None,
    }
}

/// The CPU's SpeedHQ inverse DCT in `p` from now on, for this program; `None` hands the
/// choice back: the environment (`IDN_CPU_IDCT=f32` or `f64`), then the machine's video
/// settings (`precision`, §N.13), else f64.
pub fn set_idct_precision(p: Option<Precision>) {
    PROGRAM.store(code(p), Ordering::Relaxed);
}

/// The machine's choice, as the IDN Node gives it.
pub(crate) fn set_machine_precision(p: Option<Precision>) {
    MACHINE.store(code(p), Ordering::Relaxed);
}

/// The machine's choice alone (`None`: not set), for the GPU's reconstructions, which
/// have switches of their own.
pub fn machine_precision() -> Option<Precision> {
    of_code(MACHINE.load(Ordering::Relaxed))
}

/// What the CPU's inverse DCT uses now (see [`set_idct_precision`]).
pub fn idct_precision() -> Precision {
    static ENV: OnceLock<Option<Precision>> = OnceLock::new();
    of_code(PROGRAM.load(Ordering::Relaxed))
        .or(*ENV.get_or_init(|| {
            std::env::var("IDN_CPU_IDCT")
                .ok()
                .and_then(|v| Precision::from_setting(v.trim()))
        }))
        .or_else(machine_precision)
        .unwrap_or(Precision::F64)
}

/// `BASIS[x][u] = C(u)/2 · cos((2x+1)uπ/16)`, with `C(0) = 1/√2`, `C(u) = 1` otherwise.
fn basis() -> &'static [[f64; 8]; 8] {
    static B: OnceLock<[[f64; 8]; 8]> = OnceLock::new();
    B.get_or_init(|| {
        let mut b = [[0.0; 8]; 8];
        for (x, row) in b.iter_mut().enumerate() {
            for (u, v) in row.iter_mut().enumerate() {
                let c = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                *v = c / 2.0 * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        b
    })
}

/// `BASIS` transposed: `BASIS_T[u][x] = BASIS[x][u]`, so that the row pass runs along `x`.
fn basis_t() -> &'static [[f64; 8]; 8] {
    static B: OnceLock<[[f64; 8]; 8]> = OnceLock::new();
    B.get_or_init(|| {
        let b = basis();
        std::array::from_fn(|u| std::array::from_fn(|x| b[x][u]))
    })
}

/// The basis as reconstructions elsewhere (the switcher's GPU) take it, to make the same
/// sums: `BASIS[y][v]` at `8·y + v`, then `BASIS_T[u][x]` at `64 + 8·u + x`.
pub fn basis_table() -> [f64; 128] {
    let (b, bt) = (basis(), basis_t());
    std::array::from_fn(|i| {
        if i < 64 {
            b[i / 8][i % 8]
        } else {
            bt[(i - 64) / 8][i % 8]
        }
    })
}

/// Transforms dequantised coefficients `f` (raster, `8·v + u`) to 8-bit samples (raster,
/// `8·y + x`).
pub fn idct_8x8(f: &[i32; 64], out: &mut [u8; 64]) {
    dispatch(f, out, None);
}

/// As [`idct_8x8`], also giving each sample with 3 fraction bits, `round(8·s)` clamped to
/// 0..2047 and shifted left by 5 (P216 convention, §3.9.5).
pub fn idct_8x8_hi(f: &[i32; 64], out: &mut [u8; 64], hi: &mut [u16; 64]) {
    dispatch(f, out, Some(hi));
}

/// The same arithmetic compiled for AVX2 where the CPU has it (four samples per
/// instruction), else for the baseline CPU. Every sample's sum is formed in the same
/// order either way, multiplications and additions rounded separately (Rust never fuses
/// them), so the results are identical.
#[inline]
fn dispatch(f: &[i32; 64], out: &mut [u8; 64], hi: Option<&mut [u16; 64]>) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // In f32 when asked (not the 3-fraction-bit samples: those are for precision).
        if hi.is_none() && idct_precision() == Precision::F32 {
            // SAFETY: the CPU has AVX2.
            return unsafe { idct_avx2_f32(f, out) };
        }
        // SAFETY: the CPU has AVX2.
        return unsafe { idct_avx2(f, out, hi) };
    }
    idct_inner(f, out, hi)
}

/// A basis and its transpose in f32.
type BasisF32 = ([[f32; 8]; 8], [[f32; 8]; 8]);

/// The basis in f32, as [`basis`] and [`basis_t`].
fn basis_f32() -> &'static BasisF32 {
    static T: OnceLock<BasisF32> = OnceLock::new();
    T.get_or_init(|| {
        let (b, bt) = (basis(), basis_t());
        let mut t = ([[0f32; 8]; 8], [[0f32; 8]; 8]);
        for i in 0..8 {
            for j in 0..8 {
                t.0[i][j] = b[i][j] as f32;
                t.1[i][j] = bt[i][j] as f32;
            }
        }
        t
    })
}

/// [`idct_avx2`] in f32 ([`Precision::F32`]): a row of eight samples an instruction, the
/// same pruning of zero rows and coefficients; rounded half up after the clamp (the
/// same as half away from zero there, as the samples are then not negative).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn idct_avx2_f32(f: &[i32; 64], out: &mut [u8; 64]) {
    use std::arch::x86_64::*;
    let (b, bt) = basis_f32();
    let zero = _mm256_setzero_ps();
    let mut tmp = [zero; 8];
    let mut rows = [0usize; 8];
    let mut nrows = 0;
    for v in 0..8 {
        let fr = &f[8 * v..8 * v + 8];
        // SAFETY: 8 i32 are 32 readable bytes; unaligned loads.
        let r = unsafe { _mm256_loadu_si256(fr.as_ptr().cast()) };
        if _mm256_testz_si256(r, r) == 1 {
            continue;
        }
        rows[nrows] = v;
        nrows += 1;
        let mut acc = zero;
        for u in 0..8 {
            if fr[u] != 0 {
                let c = _mm256_set1_ps(fr[u] as f32);
                // SAFETY: a row of 8 f32.
                let k = unsafe { _mm256_loadu_ps(bt[u].as_ptr()) };
                acc = _mm256_add_ps(acc, _mm256_mul_ps(k, c));
            }
        }
        tmp[v] = acc;
    }
    let (half, max8) = (_mm256_set1_ps(0.5), _mm256_set1_ps(255.0));
    for y in 0..8 {
        let mut s = zero;
        for &v in &rows[..nrows] {
            s = _mm256_add_ps(s, _mm256_mul_ps(_mm256_set1_ps(b[y][v]), tmp[v]));
        }
        let v = _mm256_min_ps(_mm256_max_ps(s, zero), max8);
        let i = _mm256_cvttps_epi32(_mm256_add_ps(v, half));
        let w = _mm_packs_epi32(_mm256_castsi256_si128(i), _mm256_extracti128_si256::<1>(i));
        // SAFETY: 8 bytes at row y of a 64-byte block.
        unsafe { _mm_storel_epi64(out[8 * y..].as_mut_ptr().cast(), _mm_packus_epi16(w, w)) };
    }
}

/// Nonzero coefficients from which a row is taken whole (every coefficient, no
/// per-coefficient choice).
#[cfg(target_arch = "x86_64")]
const DENSE_ROW: u32 = 4;

/// [`idct_inner`] with four samples per instruction: lane `x` of each vector does exactly
/// the scalar code's operations for sample `x`, in the same order.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn idct_avx2(f: &[i32; 64], out: &mut [u8; 64], hi: Option<&mut [u16; 64]>) {
    use std::arch::x86_64::*;
    let (b, bt) = (basis(), basis_t());
    let zero = _mm256_setzero_pd();
    let mut tmp = [[zero; 2]; 8];
    let mut rows = [0usize; 8];
    let mut nrows = 0;
    for v in 0..8 {
        let fr = &f[8 * v..8 * v + 8];
        // SAFETY: 8 i32 are 32 readable bytes; unaligned loads.
        let r = unsafe { _mm256_loadu_si256(fr.as_ptr().cast()) };
        if _mm256_testz_si256(r, r) == 1 {
            continue;
        }
        rows[nrows] = v;
        nrows += 1;
        let (mut lo, mut up) = (zero, zero);
        // The row's nonzero coefficients, in order: their bits (a branch on each
        // coefficient was unpredictable on detailed pictures). A dense row takes every
        // coefficient: a zero one adds ±0, which leaves a sum as it is (its sign of zero
        // aside, which rounding and the clamp make no matter), so the samples are the same.
        let nz = !(_mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpeq_epi32(
            r,
            _mm256_setzero_si256(),
        ))) as u32)
            & 0xff;
        let mut term = |u: usize| {
            let c = _mm256_set1_pd(fr[u] as f64);
            // SAFETY: each half of a row of 8 f64.
            let (k0, k1) = unsafe {
                (
                    _mm256_loadu_pd(bt[u].as_ptr()),
                    _mm256_loadu_pd(bt[u][4..].as_ptr()),
                )
            };
            lo = _mm256_add_pd(lo, _mm256_mul_pd(k0, c));
            up = _mm256_add_pd(up, _mm256_mul_pd(k1, c));
        };
        if nz.count_ones() >= DENSE_ROW {
            for u in 0..8 {
                term(u);
            }
        } else {
            let mut m = nz;
            while m != 0 {
                term(m.trailing_zeros() as usize);
                m &= m - 1;
            }
        }
        tmp[v] = [lo, up];
    }
    // round_clamped on four lanes; the result is exact in f64, so the second conversion
    // only changes its type.
    let (half, one) = (_mm256_set1_pd(0.5), _mm256_set1_pd(1.0));
    let round = |s: __m256d, max: __m256d| -> __m128i {
        let v = _mm256_min_pd(_mm256_max_pd(s, zero), max);
        let i = _mm256_cvtepi32_pd(_mm256_cvttpd_epi32(v));
        let up = _mm256_and_pd(_mm256_cmp_pd::<_CMP_GE_OQ>(_mm256_sub_pd(v, i), half), one);
        _mm256_cvttpd_epi32(_mm256_add_pd(i, up))
    };
    let (max8, max11, eight) = (
        _mm256_set1_pd(255.0),
        _mm256_set1_pd(2047.0),
        _mm256_set1_pd(8.0),
    );
    let mut hi = hi;
    for y in 0..8 {
        let (mut s0, mut s1) = (zero, zero);
        for &v in &rows[..nrows] {
            let k = _mm256_set1_pd(b[y][v]);
            s0 = _mm256_add_pd(s0, _mm256_mul_pd(k, tmp[v][0]));
            s1 = _mm256_add_pd(s1, _mm256_mul_pd(k, tmp[v][1]));
        }
        let words = _mm_packs_epi32(round(s0, max8), round(s1, max8));
        // SAFETY: 8 bytes at row y of a 64-byte block.
        unsafe {
            _mm_storel_epi64(
                out[8 * y..].as_mut_ptr().cast(),
                _mm_packus_epi16(words, words),
            )
        };
        if let Some(h) = hi.as_deref_mut() {
            let w = _mm_packs_epi32(
                round(_mm256_mul_pd(eight, s0), max11),
                round(_mm256_mul_pd(eight, s1), max11),
            );
            // SAFETY: 8 u16 at row y of a 64-sample block.
            unsafe { _mm_storeu_si128(h[8 * y..].as_mut_ptr().cast(), _mm_slli_epi16::<5>(w)) };
        }
    }
}

#[inline(always)]
fn idct_inner(f: &[i32; 64], out: &mut [u8; 64], mut hi: Option<&mut [u16; 64]>) {
    let (b, bt) = (basis(), basis_t());
    // Rows first: tmp[v][x] = Σ_u BASIS[x][u] · F[v][u]. All-zero rows are skipped, and
    // zero coefficients add nothing: leaving out an exact +0.0 term never changes a double
    // sum, so the result is identical to the full evaluation.
    let mut tmp = [[0.0f64; 8]; 8];
    let mut rows = [0usize; 8];
    let mut nrows = 0;
    for v in 0..8 {
        let fr = &f[8 * v..8 * v + 8];
        if fr.iter().all(|&c| c == 0) {
            continue;
        }
        rows[nrows] = v;
        nrows += 1;
        let t = &mut tmp[v];
        for u in 0..8 {
            if fr[u] != 0 {
                let c = fr[u] as f64;
                let k = &bt[u];
                for x in 0..8 {
                    t[x] += k[x] * c;
                }
            }
        }
    }
    for y in 0..8 {
        let mut s = [0.0f64; 8];
        for &v in &rows[..nrows] {
            let k = b[y][v];
            let t = &tmp[v];
            for x in 0..8 {
                s[x] += k * t[x];
            }
        }
        let o: &mut [u8; 8] = (&mut out[8 * y..8 * y + 8]).try_into().expect("8 samples");
        for x in 0..8 {
            o[x] = round_clamped(s[x], 255.0) as u8;
        }
        if let Some(h) = hi.as_deref_mut() {
            let o: &mut [u16; 8] = (&mut h[8 * y..8 * y + 8]).try_into().expect("8 samples");
            for x in 0..8 {
                o[x] = (round_clamped(8.0 * s[x], 2047.0) as u16) << 5;
            }
        }
    }
}

/// `HALF[x][u] = C(u)/2 · cos(uπ/16) · cos((2x+1)uπ/8)`, x, u < 4: the average of samples
/// 2x and 2x + 1 of the 8-point inverse DCT, from its four lowest frequencies.
fn half_basis() -> &'static [[f64; 4]; 4] {
    static B: OnceLock<[[f64; 4]; 4]> = OnceLock::new();
    B.get_or_init(|| {
        let pi = std::f64::consts::PI;
        std::array::from_fn(|x| {
            std::array::from_fn(|u| {
                let c = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                c / 2.0
                    * (u as f64 * pi / 16.0).cos()
                    * ((2 * x + 1) as f64 * u as f64 * pi / 8.0).cos()
            })
        })
    })
}

/// A half-size picture of the block: each sample is the average of a 2 × 2 square of the
/// full inverse DCT, leaving out frequencies 4 to 7 (a box filter with a little extra
/// smoothing). For receivers that show the picture at half size or less; not part of
/// the specification's reconstruction. `out[4·y + x]`, x, y < 4.
pub fn idct_half(f: &[i32; 64], out: &mut [u8; 64]) {
    let k = half_basis();
    let mut tmp = [[0.0f64; 4]; 4];
    for v in 0..4 {
        for u in 0..4 {
            let c = f[8 * v + u];
            if c != 0 {
                for x in 0..4 {
                    tmp[v][x] += k[x][u] * c as f64;
                }
            }
        }
    }
    for y in 0..4 {
        for x in 0..4 {
            let s = (0..4).fold(0.0, |s, v| s + k[y][v] * tmp[v][x]);
            out[4 * y + x] = round_clamped(s, 255.0) as u8;
        }
    }
}

/// `v.round().clamp(0.0, max)` (half away from zero), without `f64::round`: that is a
/// library call per sample on baseline x86-64. Clamping first gives the same result, and
/// for `0 ≤ v ≤ max < 2^31` the integer part and the fraction are exact.
#[inline(always)]
fn round_clamped(v: f64, max: f64) -> i32 {
    let v = v.max(0.0).min(max);
    let i = v as i32;
    i + i32::from(v - i as f64 >= 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_f64_round_exactly() {
        let edge = [
            -1e9,
            -0.5,
            -0.49999999999999994,
            -0.0,
            0.0,
            0.49999999999999994,
            0.5,
            0.5000000000000001,
            1.5,
            2.5,
            254.49999999999997,
            254.5,
            255.4,
            255.5,
            1e9,
        ];
        let mut seed = 7u64;
        let random = (0..200_000).map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64 * 2200.0 - 100.0
        });
        let halves = (-20..2100).map(|k| k as f64 + 0.5);
        for v in edge.into_iter().chain(random).chain(halves) {
            for max in [255.0, 2047.0] {
                assert_eq!(
                    round_clamped(v, max),
                    v.round().clamp(0.0, max) as i32,
                    "{v:?}"
                );
            }
        }
    }

    /// The transform as first written (rows along `u` with `BASIS[x][u]`), for checking
    /// that the restructured and AVX2 versions give the same samples.
    fn reference(f: &[i32; 64]) -> ([u8; 64], [u16; 64]) {
        let b = basis();
        let mut tmp = [[0.0f64; 8]; 8];
        let mut rows = Vec::new();
        for v in 0..8 {
            if f[8 * v..8 * v + 8].iter().all(|&c| c == 0) {
                continue;
            }
            rows.push(v);
            for u in 0..8 {
                if f[8 * v + u] != 0 {
                    for x in 0..8 {
                        tmp[v][x] += b[x][u] * f[8 * v + u] as f64;
                    }
                }
            }
        }
        let (mut out, mut hi) = ([0u8; 64], [0u16; 64]);
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0f64;
                for &v in &rows {
                    s += b[y][v] * tmp[v][x];
                }
                out[8 * y + x] = s.round().clamp(0.0, 255.0) as u8;
                hi[8 * y + x] = ((8.0 * s).round().clamp(0.0, 2047.0) as u16) << 5;
            }
        }
        (out, hi)
    }

    #[test]
    fn every_version_gives_the_same_samples() {
        let mut seed = 0x9e37_79b9u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for n in 0..300_000 {
            let mut f = [0i32; 64];
            // DC around mid grey, then a few to all coefficients, small and large.
            f[0] = (rnd() % 2048) as i32 - 256;
            let count = [1, 3, 8, 20, 64][n % 5];
            let range = [16, 200, 2048][n % 3];
            for _ in 0..count {
                f[(rnd() % 64) as usize] = (rnd() % (2 * range)) as i32 - range as i32;
            }
            let (want, want_hi) = reference(&f);
            let (mut out, mut hi) = ([0u8; 64], [0u16; 64]);
            idct_8x8_hi(&f, &mut out, &mut hi);
            assert_eq!((out, hi), (want, want_hi), "{f:?}");
            let mut base = [0u8; 64];
            idct_inner(&f, &mut base, None);
            assert_eq!(base, want, "{f:?}");
        }
    }

    /// In f32 (where the CPU has AVX2): a sample at most one step from f64's, and rarely.
    #[test]
    fn f32_is_at_most_one_step_off() {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            let mut seed = 0x1234_5678u32;
            let mut rnd = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            let (mut off, mut total) = (0usize, 0usize);
            for n in 0..100_000 {
                let mut f = [0i32; 64];
                f[0] = (rnd() % 2048) as i32 - 256;
                let count = [1, 3, 8, 20, 64][n % 5];
                let range = [16, 200, 2048][n % 3];
                for _ in 0..count {
                    f[(rnd() % 64) as usize] = (rnd() % (2 * range)) as i32 - range as i32;
                }
                let (want, _) = reference(&f);
                let mut got = [0u8; 64];
                // SAFETY: the CPU has AVX2.
                unsafe { idct_avx2_f32(&f, &mut got) };
                for (a, b) in got.iter().zip(&want) {
                    assert!(a.abs_diff(*b) <= 1, "{f:?}");
                    off += (a != b) as usize;
                }
                total += 64;
            }
            assert!(off * 100 < total, "{off} of {total} off");
        }
    }

    #[test]
    fn dc_only_mid_grey() {
        // §3.9.2: F[0] = 1024 alone reconstructs to 128.
        let mut f = [0; 64];
        f[0] = 1024;
        let mut out = [0; 64];
        idct_8x8(&f, &mut out);
        assert!(out.iter().all(|&s| s == 128));
    }
}

#[cfg(test)]
mod bench {
    /// `cargo test --release --lib idct::bench -- --ignored --nocapture`: the inverse DCT's
    /// cost per 4K 4:2:2 picture (259 200 blocks) on one thread.
    #[test]
    #[ignore]
    fn per_4k_picture() {
        let blocks = 3840 * 2160 * 2 / 64;
        let mut seed = 1u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for (label, nonzero) in [
            ("sparse (6 coefficients)", 6usize),
            ("dense (40 coefficients)", 40),
        ] {
            let set: Vec<[i32; 64]> = (0..256)
                .map(|_| {
                    let mut f = [0i32; 64];
                    f[0] = 800;
                    for _ in 0..nonzero {
                        f[(rnd() % 64) as usize] = (rnd() % 200) as i32 - 100;
                    }
                    f
                })
                .collect();
            let mut out = [0u8; 64];
            let t = std::time::Instant::now();
            for i in 0..blocks {
                super::idct_8x8(&set[i % 256], &mut out);
                std::hint::black_box(&out);
            }
            println!(
                "{label}: {:.1} ms per 4K picture (one thread)",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}

