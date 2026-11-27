//! The SSE2 DCT and quantiser.

// SAFETY (every `unsafe` block here): SSE2 is part of the x86-64 baseline, so the
// intrinsics are always available; pointer offsets are noted where they occur.
use super::QTable;
use std::arch::x86_64::*;

#[inline(always)]
fn aan8(d: [__m128; 8]) -> [__m128; 8] {
    // SAFETY: SSE2 is part of x86-64; register arithmetic only.
    let add = |a, b| unsafe { _mm_add_ps(a, b) };
    let sub = |a, b| unsafe { _mm_sub_ps(a, b) };
    let mul = |a, b| unsafe { _mm_mul_ps(a, b) };
    let k = |v| unsafe { _mm_set1_ps(v) };
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

#[inline(always)]
fn tr4(a: __m128, b: __m128, c: __m128, d: __m128) -> [__m128; 4] {
    // SAFETY: SSE2 is part of x86-64; register arithmetic only.
    unsafe {
        let (t0, t1) = (_mm_unpacklo_ps(a, b), _mm_unpacklo_ps(c, d));
        let (t2, t3) = (_mm_unpackhi_ps(a, b), _mm_unpackhi_ps(c, d));
        [
            _mm_movelh_ps(t0, t1),
            _mm_movehl_ps(t1, t0),
            _mm_movelh_ps(t2, t3),
            _mm_movehl_ps(t3, t2),
        ]
    }
}

/// Rows of 8-bit samples as (columns 0–3, columns 4–7).
#[inline(always)]
pub(super) fn load_u8(s: &[u8; 64]) -> ([__m128; 8], [__m128; 8]) {
    // SAFETY: SSE2 is part of x86-64; each row's 8 bytes are inside the 64-byte array.
    unsafe {
        let z = _mm_setzero_si128();
        let mut lo = [_mm_setzero_ps(); 8];
        let mut hi = lo;
        for y in 0..8 {
            // Bytes 8y..8y+8 are inside the 64-byte array.
            let r = _mm_loadl_epi64(s.as_ptr().add(8 * y).cast());
            let w = _mm_unpacklo_epi8(r, z);
            lo[y] = _mm_cvtepi32_ps(_mm_unpacklo_epi16(w, z));
            hi[y] = _mm_cvtepi32_ps(_mm_unpackhi_epi16(w, z));
        }
        (lo, hi)
    }
}

#[inline(always)]
pub(super) fn load_f32(s: &[f32; 64]) -> ([__m128; 8], [__m128; 8]) {
    // SAFETY: SSE2 is part of x86-64; each row's 8 floats are inside the array.
    unsafe {
        let mut lo = [_mm_setzero_ps(); 8];
        let mut hi = lo;
        for y in 0..8 {
            // Floats 8y..8y+8 are inside the array.
            lo[y] = _mm_loadu_ps(s.as_ptr().add(8 * y));
            hi[y] = _mm_loadu_ps(s.as_ptr().add(8 * y + 4));
        }
        (lo, hi)
    }
}

/// AAN DCT and quantisation of one block: levels into `lv` (transposed layout, index 0
/// left at 0); returns the AAN-scaled DC and whether any AC level is non-zero.
#[inline(always)]
pub(super) fn dct_quant(
    lo: [__m128; 8],
    hi: [__m128; 8],
    q: &QTable,
    lv: &mut [i32; 64],
) -> (f32, bool) {
    // SAFETY: SSE2 is part of x86-64; the 4 lanes at `k` are within the 64 of both tables and
    // `lv` (noted below).
    unsafe {
        let (cl, ch) = (aan8(lo), aan8(hi));
        // Transpose by 4×4 blocks: rows of the result are columns of the source.
        let [a0, a1, a2, a3] = tr4(cl[0], cl[1], cl[2], cl[3]);
        let [b0, b1, b2, b3] = tr4(ch[0], ch[1], ch[2], ch[3]);
        let [c0, c1, c2, c3] = tr4(cl[4], cl[5], cl[6], cl[7]);
        let [d0, d1, d2, d3] = tr4(ch[4], ch[5], ch[6], ch[7]);
        // ol[u] holds coefficients (v = 0..3, u), oh[u] (v = 4..7, u): index 8·u + v.
        let ol = aan8([a0, a1, a2, a3, b0, b1, b2, b3]);
        let oh = aan8([c0, c1, c2, c3, d0, d1, d2, d3]);
        let abs = _mm_castsi128_ps(_mm_set1_epi32(0x7fff_ffff));
        let half = _mm_set1_ps(0.5);
        let mut any = _mm_setzero_si128();
        for u in 0..8 {
            for (h, v) in [(0, ol[u]), (4, oh[u])] {
                let k = 8 * u + h;
                // k + 4 ≤ 64 for both tables and for `lv`.
                let (inv, lim) = (
                    _mm_loadu_ps(q.inv_aan.as_ptr().add(k)),
                    _mm_loadu_ps(q.lim_f.as_ptr().add(k)),
                );
                // Round half away from zero: |x| + 0.5 clamped to the limit, truncated,
                // then the sign of x put back.
                let x = _mm_mul_ps(v, inv);
                let c = _mm_cvttps_epi32(_mm_min_ps(_mm_add_ps(_mm_and_ps(x, abs), half), lim));
                let sign = _mm_srai_epi32(_mm_castps_si128(x), 31);
                let c = _mm_sub_epi32(_mm_xor_si128(c, sign), sign);
                _mm_storeu_si128(lv.as_mut_ptr().add(k).cast(), c);
                any = _mm_or_si128(any, c); // lane 0 is 0: its factor and limit are 0
            }
        }
        let zero = _mm_cmpeq_epi32(any, _mm_setzero_si128());
        (_mm_cvtss_f32(ol[0]), _mm_movemask_epi8(zero) != 0xffff)
    }
}
