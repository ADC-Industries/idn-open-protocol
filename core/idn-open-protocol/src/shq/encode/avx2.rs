//! The AVX2 DCT and quantiser.

use super::QTable;
use std::arch::x86_64::*;

// SAFETY (every function): only called after `is_x86_feature_detected!("avx2")`.

#[inline]
#[target_feature(enable = "avx2")]
fn aan8(d: [__m256; 8]) -> [__m256; 8] {
    let add = |a, b| _mm256_add_ps(a, b);
    let sub = |a, b| _mm256_sub_ps(a, b);
    let mul = |a, b| _mm256_mul_ps(a, b);
    let k = |v| _mm256_set1_ps(v);
    let (t0, t7) = (add(d[0], d[7]), sub(d[0], d[7]));
    let (t1, t6) = (add(d[1], d[6]), sub(d[1], d[6]));
    let (t2, t5) = (add(d[2], d[5]), sub(d[2], d[5]));
    let (t3, t4) = (add(d[3], d[4]), sub(d[3], d[4]));
    let (t10, t13) = (add(t0, t3), sub(t0, t3));
    let (t11, t12) = (add(t1, t2), sub(t1, t2));
    let z1 = mul(add(t12, t13), k(std::f32::consts::FRAC_1_SQRT_2));
    let (u10, u11, u12) = (add(t4, t5), add(t5, t6), add(t6, t7));
    let z5 = mul(sub(u10, u12), k(0.382_683_43));
    let z2 = add(mul(k(0.541_196_1), u10), z5);
    let z4 = add(mul(k(1.306_563), u12), z5);
    let z3 = mul(u11, k(std::f32::consts::FRAC_1_SQRT_2));
    let (z11, z13) = (add(t7, z3), sub(t7, z3));
    [
        add(t10, t11),
        add(z11, z4),
        add(t13, z1),
        sub(z13, z2),
        sub(t10, t11),
        add(z13, z2),
        sub(t13, z1),
        sub(z11, z4),
    ]
}

/// Rows become columns.
#[inline]
#[target_feature(enable = "avx2")]
fn transpose(r: [__m256; 8]) -> [__m256; 8] {
    let (t0, t1) = (
        _mm256_unpacklo_ps(r[0], r[1]),
        _mm256_unpackhi_ps(r[0], r[1]),
    );
    let (t2, t3) = (
        _mm256_unpacklo_ps(r[2], r[3]),
        _mm256_unpackhi_ps(r[2], r[3]),
    );
    let (t4, t5) = (
        _mm256_unpacklo_ps(r[4], r[5]),
        _mm256_unpackhi_ps(r[4], r[5]),
    );
    let (t6, t7) = (
        _mm256_unpacklo_ps(r[6], r[7]),
        _mm256_unpackhi_ps(r[6], r[7]),
    );
    let s0 = _mm256_shuffle_ps::<0x44>(t0, t2);
    let s1 = _mm256_shuffle_ps::<0xee>(t0, t2);
    let s2 = _mm256_shuffle_ps::<0x44>(t1, t3);
    let s3 = _mm256_shuffle_ps::<0xee>(t1, t3);
    let s4 = _mm256_shuffle_ps::<0x44>(t4, t6);
    let s5 = _mm256_shuffle_ps::<0xee>(t4, t6);
    let s6 = _mm256_shuffle_ps::<0x44>(t5, t7);
    let s7 = _mm256_shuffle_ps::<0xee>(t5, t7);
    [
        _mm256_permute2f128_ps::<0x20>(s0, s4),
        _mm256_permute2f128_ps::<0x20>(s1, s5),
        _mm256_permute2f128_ps::<0x20>(s2, s6),
        _mm256_permute2f128_ps::<0x20>(s3, s7),
        _mm256_permute2f128_ps::<0x31>(s0, s4),
        _mm256_permute2f128_ps::<0x31>(s1, s5),
        _mm256_permute2f128_ps::<0x31>(s2, s6),
        _mm256_permute2f128_ps::<0x31>(s3, s7),
    ]
}

/// AAN DCT and quantisation of one block of rows (`rows[y]` lanes x): levels into `lv`
/// (index 8·u + v, as [`super::sse::dct_quant`]); the AAN-scaled DC and whether any AC
/// level is non-zero.
#[inline]
#[target_feature(enable = "avx2")]
fn dct_quant(rows: [__m256; 8], q: &QTable, lv: &mut [i32; 64]) -> (f32, bool) {
    // Down the columns (lanes x, vector v), then along the rows (lanes v, vector u).
    let o = aan8(transpose(aan8(rows)));
    let abs = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fff_ffff));
    let half = _mm256_set1_ps(0.5);
    let mut any = _mm256_setzero_si256();
    for (u, v) in o.iter().enumerate() {
        let k = 8 * u;
        // k + 8 ≤ 64 for both tables and for `lv`.
        // SAFETY: AVX2 is this function's feature; 8 floats at `k` ≤ 56 are inside both tables.
        let (inv, lim) = unsafe {
            (
                _mm256_loadu_ps(q.inv_aan.as_ptr().add(k)),
                _mm256_loadu_ps(q.lim_f.as_ptr().add(k)),
            )
        };
        // Round half away from zero, as `sse`.
        let x = _mm256_mul_ps(*v, inv);
        let c = _mm256_cvttps_epi32(_mm256_min_ps(
            _mm256_add_ps(_mm256_and_ps(x, abs), half),
            lim,
        ));
        let sign = _mm256_srai_epi32::<31>(_mm256_castps_si256(x));
        let c = _mm256_sub_epi32(_mm256_xor_si256(c, sign), sign);
        // SAFETY: 8 lanes at `k` ≤ 56 are inside `lv`.
        unsafe { _mm256_storeu_si256(lv.as_mut_ptr().add(k).cast(), c) };
        any = _mm256_or_si256(any, c); // lane 0 of u = 0 is 0: its factor and limit are 0
    }
    (_mm256_cvtss_f32(o[0]), _mm256_testz_si256(any, any) == 0)
}

/// [`dct_quant`] of 8-bit samples.
#[target_feature(enable = "avx2")]
pub(super) fn dct_quant_u8(s: &[u8; 64], q: &QTable, lv: &mut [i32; 64]) -> (f32, bool) {
    let rows: [__m256; 8] = std::array::from_fn(|y| {
        // Bytes 8y..8y+8 are inside the 64-byte array.
        // SAFETY: AVX2 is this function's feature, and the read is the 8 bytes noted above.
        let r = unsafe { _mm_loadl_epi64(s.as_ptr().add(8 * y).cast()) };
        _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(r))
    });
    dct_quant(rows, q, lv)
}

/// [`dct_quant`] of samples with fraction bits.
#[target_feature(enable = "avx2")]
pub(super) fn dct_quant_f32(s: &[f32; 64], q: &QTable, lv: &mut [i32; 64]) -> (f32, bool) {
    // Floats 8y..8y+8 are inside the array.
    // SAFETY: AVX2 is this function's feature, and each read is the 8 floats noted above.
    let rows: [__m256; 8] =
        std::array::from_fn(|y| unsafe { _mm256_loadu_ps(s.as_ptr().add(8 * y)) });
    dct_quant(rows, q, lv)
}
