//! Colour settings: how RGBA relates to the Y'CbCr pictures carried on the network.
//!
//! You only need this module for [`AlphaMode`] (telling an output whether your RGBA is
//! opaque, straight or premultiplied, and asking an input for RGBA in the form your
//! renderer wants) and, rarely, [`ColorConfig`].
//!
//! Conversion details: limited-range BT.709 by default, correctly rounded. Chroma is
//! averaged over each horizontal pair of pixels. Alpha travels straight and full range;
//! premultiplied input is un-premultiplied before sending, and received pictures can be
//! premultiplied again on output.

use crate::shq::{Fourcc, Plane};
use idn_common::sync::LockOk as _;

/// Colour matrix for R′G′B′ ↔ Y′CbCr conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    /// ITU-R BT.601 (standard definition).
    Bt601,
    /// ITU-R BT.709 (HD; the default, and what NDI senders and receivers use).
    Bt709,
    /// ITU-R BT.2020 / BT.2100 non-constant luminance (UHD and HDR).
    Bt2020,
}

impl Matrix {
    /// (Kr, Kb) luma coefficients.
    pub fn kr_kb(self) -> (f64, f64) {
        match self {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// The matrix an `ndi_color_info` / [`ColorInfo`](crate::ext::ColorInfo) name
    /// stands for (`bt_601`, `bt_709`, `bt_2020`, `bt_2100`), if known.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "bt_601" => Some(Matrix::Bt601),
            "bt_709" => Some(Matrix::Bt709),
            "bt_2020" | "bt_2100" => Some(Matrix::Bt2020),
            _ => None,
        }
    }
}

/// Transfer function of the source's samples (`ndi_color_info transfer`, §3.10.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    /// SDR gamma (`bt_601`, `bt_709`, `bt_2020`): the default; R′G′B′ pass through to the
    /// sRGB-encoded RGBA output unchanged.
    #[default]
    Sdr,
    /// ITU-R BT.2100 PQ (`bt_2100_pq`): absolute nits, tone-mapped to SDR on conversion.
    Pq,
    /// ITU-R BT.2100 HLG (`bt_2100_hlg`): relative, shown as on a 1000 nit display.
    Hlg,
}

impl Transfer {
    /// The transfer an `ndi_color_info` / [`ColorInfo`](crate::ext::ColorInfo) name
    /// stands for; unknown names are SDR.
    pub fn from_name(name: &str) -> Self {
        match name {
            "bt_2100_pq" => Transfer::Pq,
            "bt_2100_hlg" => Transfer::Hlg,
            _ => Transfer::Sdr,
        }
    }
}

/// Colour primaries of the source (`ndi_color_info primaries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Primaries {
    /// BT.709 / sRGB (also used for `bt_601`): the output's own, no conversion.
    #[default]
    Bt709,
    /// BT.2020 / BT.2100 wide gamut: converted to BT.709 in linear light when an HDR
    /// transfer is tone-mapped (SDR BT.2020 content is passed through).
    Bt2020,
}

impl Primaries {
    /// The primaries an `ndi_color_info` name stands for; unknown names are BT.709.
    pub fn from_name(name: &str) -> Self {
        match name {
            "bt_2020" | "bt_2100" => Primaries::Bt2020,
            _ => Primaries::Bt709,
        }
    }
}

/// How the colour channels of an RGBA buffer relate to its alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    /// Alpha is ignored (always 255).
    Opaque,
    /// Colour is independent of alpha.
    Straight,
    /// Colour has been multiplied by alpha.
    Premultiplied,
}

/// How RGBA is converted to and from the Y'CbCr carried on the network.
///
/// The default (BT.709, limited range, straight alpha on the wire) matches NDI
/// senders and receivers. Other values are for IDN-to-IDN set-ups that need them.
#[derive(Debug, Clone, Copy)]
pub struct ColorConfig {
    /// The colour matrix.
    pub matrix: Matrix,
    /// Limited range (Y 16–235, Cb/Cr 16–240, the default) or full range (0–255).
    pub limited: bool,
    /// The alpha convention on the network. NDI uses straight alpha; keep the default.
    pub wire_alpha: AlphaMode,
    /// The source's transfer function. An input takes it from the source's colour
    /// metadata when that says PQ or HLG; the RGBA output is then tone-mapped to SDR.
    pub transfer: Transfer,
    /// The source's primaries, likewise from its colour metadata.
    pub primaries: Primaries,
    /// Tone-mapping of PQ/HLG sources: this many nits become RGB white; anything brighter
    /// clips. 203 is the reference white of BT.2408. A Windows desktop in HDR mode shows
    /// SDR content at its "SDR content brightness" setting, often 80 nits; a screen
    /// capture of it comes out dim unless this matches.
    pub sdr_white_nits: f32,
}

impl Default for ColorConfig {
    fn default() -> Self {
        ColorConfig {
            matrix: Matrix::Bt709,
            limited: true,
            wire_alpha: AlphaMode::Straight,
            transfer: Transfer::Sdr,
            primaries: Primaries::Bt709,
            sdr_white_nits: 203.0,
        }
    }
}

/// Fixed-point (×65536) coefficients: Y, Cb, Cr rows, each [r, g, b], plus offsets.
pub(crate) struct Coeffs {
    pub(crate) y: [i32; 3],
    pub(crate) cb: [i32; 3],
    pub(crate) cr: [i32; 3],
    pub(crate) y_off: i32,
}

pub(crate) fn coeffs(c: &ColorConfig) -> Coeffs {
    let (kr, kb) = c.matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    let (ys, cs, yo) = if c.limited {
        (219.0 / 255.0, 224.0 / 255.0, 16)
    } else {
        (1.0, 1.0, 0)
    };
    let f = |v: f64| (v * 65536.0).round() as i32;
    Coeffs {
        y: [f(kr * ys), f(kg * ys), f(kb * ys)],
        cb: [
            f(-kr / (2.0 * (1.0 - kb)) * cs),
            f(-kg / (2.0 * (1.0 - kb)) * cs),
            f(0.5 * cs),
        ],
        cr: [
            f(0.5 * cs),
            f(-kg / (2.0 * (1.0 - kr)) * cs),
            f(-kb / (2.0 * (1.0 - kr)) * cs),
        ],
        y_off: yo,
    }
}

/// `(255 << 16) / a`, for un-premultiplying without a division per pixel.
fn recip_table() -> &'static [u32; 256] {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        std::array::from_fn(|a| {
            if a == 0 {
                0
            } else {
                ((255u32 << 16) + a as u32 / 2) / a as u32
            }
        })
    })
}

/// Pixel colour as it goes on the wire. MODE: 0 = as is, 1 = un-premultiply,
/// 2 = premultiply.
#[inline(always)]
fn wire_rgb<const MODE: u8>(p: &[u8], recip: &[u32; 256]) -> (i32, i32, i32) {
    let (r, g, b, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
    match MODE {
        1 if a < 255 => {
            let k = recip[a as usize];
            let un = |c: u32| ((c * k + 32768) >> 16).min(255) as i32;
            (un(r), un(g), un(b))
        }
        2 => {
            let pm = |c: u32| ((c * a + 127) / 255) as i32;
            (pm(r), pm(g), pm(b))
        }
        _ => (r as i32, g as i32, b as i32),
    }
}

/// One row: luma (and alpha) per pixel, chroma per horizontal pair from the pair's
/// average colour (the same as averaging the two chroma values, before clamping).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn convert_row<const MODE: u8>(
    line: &[u8],
    width: usize,
    k: &Coeffs,
    lim: (i32, i32, i32, i32),
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    a: Option<&mut [u8]>,
    keep_alpha: bool,
) {
    // Sixteen pixels at a time with AVX2, then eight with SSE2 (same results), the rest one
    // pair at a time.
    #[cfg(target_arch = "x86_64")]
    let done = if MODE == 2 {
        0
    } else {
        let wide = if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has AVX2.
            unsafe { sse::convert_avx2(line, width / 16, MODE == 1, k, lim, y, cb, cr) }
        } else {
            0
        };
        let h = wide / 2;
        wide + sse::convert(
            &line[4 * wide..],
            (width - wide) / 8,
            MODE == 1,
            k,
            lim,
            &mut y[wide..],
            &mut cb[h..],
            &mut cr[h..],
        )
    };
    #[cfg(not(target_arch = "x86_64"))]
    let done = 0;
    let h = done / 2;
    convert_pixels::<MODE>(
        &line[4 * done..],
        width - done,
        k,
        lim,
        &mut y[done..],
        &mut cb[h..],
        &mut cr[h..],
    );
    if let Some(a) = a {
        if keep_alpha {
            for (av, px) in a.iter_mut().zip(line.as_chunks::<4>().0) {
                *av = px[3];
            }
        } else {
            a.fill(255);
        }
    }
}

/// The colour planes of one row (or the tail of one), pixel pair by pixel pair.
#[inline(always)]
fn convert_pixels<const MODE: u8>(
    line: &[u8],
    width: usize,
    k: &Coeffs,
    lim: (i32, i32, i32, i32),
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
) {
    let recip = recip_table();
    let (ymin, ymax, cmin, cmax) = lim;
    let luma = |r: i32, g: i32, b: i32| {
        (((k.y[0] * r + k.y[1] * g + k.y[2] * b + 32768) >> 16) + k.y_off).clamp(ymin, ymax) as u8
    };
    let pairs = width / 2;
    for (((px, yy), cbv), crv) in line[..8 * pairs]
        .as_chunks::<8>()
        .0
        .iter()
        .zip(y[..2 * pairs].as_chunks_mut::<2>().0.iter_mut())
        .zip(cb[..pairs].iter_mut())
        .zip(cr[..pairs].iter_mut())
    {
        let (r0, g0, b0) = wire_rgb::<MODE>(&px[..4], recip);
        let (r1, g1, b1) = wire_rgb::<MODE>(&px[4..], recip);
        yy[0] = luma(r0, g0, b0);
        yy[1] = luma(r1, g1, b1);
        let (r, g, b) = (r0 + r1, g0 + g1, b0 + b1); // twice the average
        *cbv = (((k.cb[0] * r + k.cb[1] * g + k.cb[2] * b + 65536) >> 17) + 128).clamp(cmin, cmax)
            as u8;
        *crv = (((k.cr[0] * r + k.cr[1] * g + k.cr[2] * b + 65536) >> 17) + 128).clamp(cmin, cmax)
            as u8;
    }
    if width % 2 == 1 {
        let (r0, g0, b0) = wire_rgb::<MODE>(&line[4 * (width - 1)..4 * width], recip);
        y[width - 1] = luma(r0, g0, b0);
        cb[pairs] = (((k.cb[0] * r0 + k.cb[1] * g0 + k.cb[2] * b0 + 32768) >> 16) + 128)
            .clamp(cmin, cmax) as u8;
        cr[pairs] = (((k.cr[0] * r0 + k.cr[1] * g0 + k.cr[2] * b0 + 32768) >> 16) + 128)
            .clamp(cmin, cmax) as u8;
    }
}

/// Eight pixels at a time with SSE2 (part of every x86-64). The fixed-point arithmetic of
/// [`convert_pixels`] is done in f32 where every product and partial sum is an integer
/// below 2²⁴ (chroma rows sum to zero, so their partial sums stay small), hence exact; the
/// rounding shifts are done on integers. The results are identical.
#[cfg(target_arch = "x86_64")]
mod sse {
    use super::{Coeffs, recip_table};
    use std::arch::x86_64::*;

    /// Converts the first `8 · groups` pixels of a row; returns the number of pixels done.
    /// `unpremultiply` as MODE 1 of [`super::wire_rgb`].
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub(super) fn convert(
        line: &[u8],
        groups: usize,
        unpremultiply: bool,
        k: &Coeffs,
        lim: (i32, i32, i32, i32),
        y: &mut [u8],
        cb: &mut [u8],
        cr: &mut [u8],
    ) -> usize {
        let (line, y, cb, cr) = (
            &line[..32 * groups],
            &mut y[..8 * groups],
            &mut cb[..4 * groups],
            &mut cr[..4 * groups],
        );
        let recip = recip_table();
        // SAFETY: SSE2 is part of the x86-64 baseline. Group g reads line[32g..32g + 32] and
        // writes y[8g..8g + 8], cb/cr[4g..4g + 4], all inside the slices cut above.
        unsafe {
            let f = |v: i32| _mm_set1_ps(v as f32);
            let (ky, kb, kr) = (k.y.map(f), k.cb.map(f), k.cr.map(f));
            let byte = _mm_set1_epi32(0xff);
            let chan = |p: __m128i, sh: i32| -> __m128 {
                let v = match sh {
                    0 => p,
                    8 => _mm_srli_epi32(p, 8),
                    16 => _mm_srli_epi32(p, 16),
                    _ => _mm_srli_epi32(p, 24),
                };
                _mm_cvtepi32_ps(_mm_and_si128(v, byte))
            };
            // ((c · k + 32768) >> 16).min(255): c · k < 2²⁴ unless the result clamps anyway.
            let un = |c: __m128, kf: __m128| -> __m128 {
                let q = _mm_min_ps(
                    _mm_add_ps(_mm_mul_ps(c, kf), _mm_set1_ps(32768.0)),
                    _mm_set1_ps(16_777_216.0),
                );
                let i = _mm_srai_epi32(_mm_cvttps_epi32(q), 16); // ≤ 256
                _mm_cvtepi32_ps(_mm_add_epi32(i, _mm_cmpgt_epi32(i, _mm_set1_epi32(255))))
            };
            let dot = |c: &[__m128; 3], r: __m128, g: __m128, b: __m128| {
                _mm_add_ps(
                    _mm_add_ps(_mm_mul_ps(c[0], r), _mm_mul_ps(c[1], g)),
                    _mm_mul_ps(c[2], b),
                )
            };
            let (ymin, ymax) = (_mm_set1_epi16(lim.0 as i16), _mm_set1_epi16(lim.1 as i16));
            let (cmin, cmax) = (_mm_set1_epi16(lim.2 as i16), _mm_set1_epi16(lim.3 as i16));
            let yoff = _mm_set1_epi32(k.y_off);
            for g in 0..groups {
                let src = line.as_ptr().add(32 * g);
                let (p0, p1) = (
                    _mm_loadu_si128(src.cast()),
                    _mm_loadu_si128(src.add(16).cast()),
                );
                let (mut r0, mut g0, mut b0) = (chan(p0, 0), chan(p0, 8), chan(p0, 16));
                let (mut r1, mut g1, mut b1) = (chan(p1, 0), chan(p1, 8), chan(p1, 16));
                if unpremultiply {
                    let kk: [i32; 8] =
                        std::array::from_fn(|j| recip[*src.add(4 * j + 3) as usize] as i32);
                    let k0 = _mm_cvtepi32_ps(_mm_loadu_si128(kk.as_ptr().cast()));
                    let k1 = _mm_cvtepi32_ps(_mm_loadu_si128(kk.as_ptr().add(4).cast()));
                    (r0, g0, b0) = (un(r0, k0), un(g0, k0), un(b0, k0));
                    (r1, g1, b1) = (un(r1, k1), un(g1, k1), un(b1, k1));
                }
                // Luma per pixel.
                let luma = |r, g, b| {
                    _mm_add_epi32(
                        _mm_srai_epi32(
                            _mm_cvttps_epi32(_mm_add_ps(dot(&ky, r, g, b), _mm_set1_ps(32768.0))),
                            16,
                        ),
                        yoff,
                    )
                };
                let yv = _mm_packs_epi32(luma(r0, g0, b0), luma(r1, g1, b1));
                let yv = _mm_min_epi16(_mm_max_epi16(yv, ymin), ymax);
                _mm_storel_epi64(y.as_mut_ptr().add(8 * g).cast(), _mm_packus_epi16(yv, yv));
                // Chroma from the sum of each horizontal pair.
                let pair = |a, b| {
                    _mm_add_ps(
                        _mm_shuffle_ps::<0b10_00_10_00>(a, b),
                        _mm_shuffle_ps::<0b11_01_11_01>(a, b),
                    )
                };
                let (rs, gs, bs) = (pair(r0, r1), pair(g0, g1), pair(b0, b1));
                let chroma = |c: &[__m128; 3]| {
                    _mm_add_epi32(
                        _mm_srai_epi32(
                            _mm_cvttps_epi32(_mm_add_ps(dot(c, rs, gs, bs), _mm_set1_ps(65536.0))),
                            17,
                        ),
                        _mm_set1_epi32(128),
                    )
                };
                let cv = _mm_packs_epi32(chroma(&kb), chroma(&kr));
                let cv = _mm_packus_epi16(_mm_min_epi16(_mm_max_epi16(cv, cmin), cmax), cv);
                cb.as_mut_ptr()
                    .add(4 * g)
                    .cast::<i32>()
                    .write_unaligned(_mm_cvtsi128_si32(cv));
                cr.as_mut_ptr()
                    .add(4 * g)
                    .cast::<i32>()
                    .write_unaligned(_mm_cvtsi128_si32(_mm_srli_si128::<4>(cv)));
            }
        }
        8 * groups
    }

    /// [`convert`] sixteen pixels at a time: the same operations in 8-lane vectors.
    ///
    /// # Safety
    ///
    /// The CPU has AVX2.
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn convert_avx2(
        line: &[u8],
        groups: usize,
        unpremultiply: bool,
        k: &Coeffs,
        lim: (i32, i32, i32, i32),
        y: &mut [u8],
        cb: &mut [u8],
        cr: &mut [u8],
    ) -> usize {
        let (line, y, cb, cr) = (
            &line[..64 * groups],
            &mut y[..16 * groups],
            &mut cb[..8 * groups],
            &mut cr[..8 * groups],
        );
        let recip = recip_table();
        let f = |v: i32| _mm256_set1_ps(v as f32);
        let (ky, kb, kr) = (k.y.map(f), k.cb.map(f), k.cr.map(f));
        let byte = _mm256_set1_epi32(0xff);
        let chan = |p: __m256i, sh: i32| -> __m256 {
            let v = match sh {
                0 => p,
                8 => _mm256_srli_epi32::<8>(p),
                16 => _mm256_srli_epi32::<16>(p),
                _ => _mm256_srli_epi32::<24>(p),
            };
            _mm256_cvtepi32_ps(_mm256_and_si256(v, byte))
        };
        let un = |c: __m256, kf: __m256| -> __m256 {
            let q = _mm256_min_ps(
                _mm256_add_ps(_mm256_mul_ps(c, kf), _mm256_set1_ps(32768.0)),
                _mm256_set1_ps(16_777_216.0),
            );
            let i = _mm256_srai_epi32::<16>(_mm256_cvttps_epi32(q));
            _mm256_cvtepi32_ps(_mm256_add_epi32(
                i,
                _mm256_cmpgt_epi32(i, _mm256_set1_epi32(255)),
            ))
        };
        let dot = |c: &[__m256; 3], r: __m256, g: __m256, b: __m256| {
            _mm256_add_ps(
                _mm256_add_ps(_mm256_mul_ps(c[0], r), _mm256_mul_ps(c[1], g)),
                _mm256_mul_ps(c[2], b),
            )
        };
        let i = |v: i32| _mm256_set1_epi32(v);
        let (ymin, ymax, cmin, cmax) = (i(lim.0), i(lim.1), i(lim.2), i(lim.3));
        let yoff = i(k.y_off);
        let in_order = _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7);
        for g in 0..groups {
            // SAFETY: group g reads line[64g..64g + 64] and writes y[16g..16g + 16],
            // cb/cr[8g..8g + 8], inside the slices cut above.
            let src = unsafe { line.as_ptr().add(64 * g) };
            let (p0, p1) = unsafe {
                (
                    _mm256_loadu_si256(src.cast()),
                    _mm256_loadu_si256(src.add(32).cast()),
                )
            };
            let (mut r0, mut g0, mut b0) = (chan(p0, 0), chan(p0, 8), chan(p0, 16));
            let (mut r1, mut g1, mut b1) = (chan(p1, 0), chan(p1, 8), chan(p1, 16));
            if unpremultiply {
                let a0 = _mm256_and_si256(_mm256_srli_epi32::<24>(p0), byte);
                let a1 = _mm256_and_si256(_mm256_srli_epi32::<24>(p1), byte);
                // SAFETY: indices are bytes, below the table's 256 entries.
                let (k0, k1) = unsafe {
                    (
                        _mm256_cvtepi32_ps(_mm256_i32gather_epi32::<4>(recip.as_ptr().cast(), a0)),
                        _mm256_cvtepi32_ps(_mm256_i32gather_epi32::<4>(recip.as_ptr().cast(), a1)),
                    )
                };
                (r0, g0, b0) = (un(r0, k0), un(g0, k0), un(b0, k0));
                (r1, g1, b1) = (un(r1, k1), un(g1, k1), un(b1, k1));
            }
            let luma = |r, g, b| {
                let v = _mm256_add_epi32(
                    _mm256_srai_epi32::<16>(_mm256_cvttps_epi32(_mm256_add_ps(
                        dot(&ky, r, g, b),
                        _mm256_set1_ps(32768.0),
                    ))),
                    yoff,
                );
                _mm256_min_epi32(_mm256_max_epi32(v, ymin), ymax)
            };
            // Pixels 0-3 8-11 | 4-7 12-15 as i16, put in order, then bytes.
            let yv = _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_packs_epi32(
                luma(r0, g0, b0),
                luma(r1, g1, b1),
            ));
            let yb = _mm256_permute4x64_epi64::<0b10_00_10_00>(_mm256_packus_epi16(yv, yv));
            // SAFETY: 16 bytes at y[16g].
            unsafe {
                _mm_storeu_si128(
                    y.as_mut_ptr().add(16 * g).cast(),
                    _mm256_castsi256_si128(yb),
                )
            };
            // Pair sums: lanes hold pairs 0 1 4 5 | 2 3 6 7; reorder.
            let pair = |a, b| {
                _mm256_permutevar8x32_ps(
                    _mm256_add_ps(
                        _mm256_shuffle_ps::<0b10_00_10_00>(a, b),
                        _mm256_shuffle_ps::<0b11_01_11_01>(a, b),
                    ),
                    in_order,
                )
            };
            let (rs, gs, bs) = (pair(r0, r1), pair(g0, g1), pair(b0, b1));
            let chroma = |c: &[__m256; 3]| {
                let v = _mm256_add_epi32(
                    _mm256_srai_epi32::<17>(_mm256_cvttps_epi32(_mm256_add_ps(
                        dot(c, rs, gs, bs),
                        _mm256_set1_ps(65536.0),
                    ))),
                    _mm256_set1_epi32(128),
                );
                _mm256_min_epi32(_mm256_max_epi32(v, cmin), cmax)
            };
            // cb 0-3 cr 0-3 | cb 4-7 cr 4-7 as i16, then bytes; gather each plane's 8.
            let cv = _mm256_packs_epi32(chroma(&kb), chroma(&kr));
            let cv = _mm256_permutevar8x32_epi32(
                _mm256_packus_epi16(cv, cv),
                _mm256_setr_epi32(0, 4, 1, 5, 0, 0, 0, 0),
            );
            let lo = _mm256_castsi256_si128(cv);
            // SAFETY: 8 bytes each at cb/cr[8g].
            unsafe {
                _mm_storel_epi64(cb.as_mut_ptr().add(8 * g).cast(), lo);
                _mm_storel_epi64(cr.as_mut_ptr().add(8 * g).cast(), _mm_srli_si128::<8>(lo));
            }
        }
        16 * groups
    }
}

/// RGBA8 (tightly packed or with `stride` bytes per row) to the planes of `fourcc`
/// (SHQ2/SHQ7 4:2:2, SHQ0 4:2:0, SHQ4/SHQ9 4:4:4; alpha plane for SHQ7/SHQ9).
/// `src_alpha` describes the input.
#[doc(hidden)]
pub fn rgba_to_planes(
    rgba: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    fourcc: Fourcc,
    src_alpha: AlphaMode,
    cfg: &ColorConfig,
) -> Vec<Plane> {
    to_planes(rgba, width, height, stride, fourcc, src_alpha, cfg, false)
}

/// As [`rgba_to_planes`], for BGRA8 (a screen's BGR0): the same converter with the red
/// and blue weights swapped (swscale took 22 ms a 2160p picture on one core).
#[doc(hidden)]
pub fn bgra_to_planes(
    bgra: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    fourcc: Fourcc,
    src_alpha: AlphaMode,
    cfg: &ColorConfig,
) -> Vec<Plane> {
    to_planes(bgra, width, height, stride, fourcc, src_alpha, cfg, true)
}

#[allow(clippy::too_many_arguments)]
fn to_planes(
    rgba: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    fourcc: Fourcc,
    src_alpha: AlphaMode,
    cfg: &ColorConfig,
    bgr: bool,
) -> Vec<Plane> {
    assert!(stride >= width * 4 && rgba.len() >= stride * (height - 1) + width * 4);
    if let Some(as_422) = match fourcc {
        Fourcc::Shq4 => Some(Fourcc::Shq2),
        Fourcc::Shq9 => Some(Fourcc::Shq7),
        _ => None,
    } {
        return to_444(rgba, width, height, stride, as_422, src_alpha, cfg, bgr);
    }
    let mut k = coeffs(cfg);
    if bgr {
        for row in [&mut k.y, &mut k.cb, &mut k.cr] {
            row.swap(0, 2);
        }
    }
    let cw = width.div_ceil(2);
    let with_alpha = fourcc.has_alpha();
    // Opaque outputs drop alpha and show premultiplied colour as is.
    let mode: u8 = match (with_alpha, src_alpha, cfg.wire_alpha) {
        (true, AlphaMode::Premultiplied, AlphaMode::Straight) => 1,
        (true, AlphaMode::Straight, AlphaMode::Premultiplied) => 2,
        _ => 0,
    };
    let keep_alpha = src_alpha != AlphaMode::Opaque;
    let lim = if cfg.limited {
        (16, 235, 16, 240)
    } else {
        (0, 255, 0, 255)
    };
    let mut y = vec![0u8; width * height];
    let mut cb = vec![0u8; cw * height];
    let mut cr = vec![0u8; cw * height];
    let mut a = if with_alpha {
        vec![0u8; width * height]
    } else {
        Vec::new()
    };
    const ROWS: usize = 32;
    let mut a_chunks = a.chunks_mut(width * ROWS);
    let chunks: Vec<_> = y
        .chunks_mut(width * ROWS)
        .zip(cb.chunks_mut(cw * ROWS))
        .zip(cr.chunks_mut(cw * ROWS))
        .map(|((ych, cbch), crch)| {
            (
                ych,
                cbch,
                crch,
                if with_alpha { a_chunks.next() } else { None },
            )
        })
        .collect();
    crate::pool::for_each(chunks, |ci, (ych, cbch, crch, mut ach)| {
        for (ry, yrow) in ych.chunks_mut(width).enumerate() {
            let row = ci * ROWS + ry;
            let line = &rgba[row * stride..row * stride + width * 4];
            let (b, r) = (
                &mut cbch[ry * cw..(ry + 1) * cw],
                &mut crch[ry * cw..(ry + 1) * cw],
            );
            let arow = ach
                .as_deref_mut()
                .map(|a| &mut a[ry * width..(ry + 1) * width]);
            match mode {
                1 => convert_row::<1>(line, width, &k, lim, yrow, b, r, arow, keep_alpha),
                2 => convert_row::<2>(line, width, &k, lim, yrow, b, r, arow, keep_alpha),
                _ => convert_row::<0>(line, width, &k, lim, yrow, b, r, arow, keep_alpha),
            }
        }
    });
    let (cb, cr, ch) = if fourcc == Fourcc::Shq0 {
        // 4:2:0: average vertical pairs of 4:2:2 chroma rows.
        let ch = height.div_ceil(2);
        // Row pair by row pair (not a division and a remainder per sample), into memory
        // pictures hand back.
        let half = |c: &[u8]| -> Vec<u8> {
            let mut out = Plane::buffer(ch * cw);
            for (j, o) in out.chunks_exact_mut(cw).enumerate() {
                let (r0, r1) = (2 * j, (2 * j + 1).min(height - 1));
                let (a, b) = (&c[r0 * cw..][..cw], &c[r1 * cw..][..cw]);
                for ((o, &a), &b) in o.iter_mut().zip(a).zip(b) {
                    *o = (a as u16 + b as u16).div_ceil(2) as u8;
                }
            }
            out
        };
        (half(&cb), half(&cr), ch)
    } else {
        (cb, cr, height)
    };
    let mut planes = vec![
        Plane {
            width,
            height,
            data: y,
        },
        Plane {
            width: cw,
            height: ch,
            data: cb,
        },
        Plane {
            width: cw,
            height: ch,
            data: cr,
        },
    ];
    if with_alpha {
        planes.push(Plane {
            width,
            height,
            data: a,
        });
    }
    planes
}

/// 4:4:4 through the 4:2:2 converter: with every pixel doubled horizontally, each chroma
/// pair is one source pixel, so chroma is per pixel (the pair average of a pixel with
/// itself is exactly its own value); every other luma and alpha sample is kept.
#[allow(clippy::too_many_arguments)]
fn to_444(
    rgba: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    as_422: Fourcc,
    src_alpha: AlphaMode,
    cfg: &ColorConfig,
    bgr: bool,
) -> Vec<Plane> {
    let mut doubled = Vec::with_capacity(width * height * 8);
    for y in 0..height {
        for px in rgba[y * stride..y * stride + width * 4].as_chunks::<4>().0 {
            doubled.extend_from_slice(px);
            doubled.extend_from_slice(px);
        }
    }
    let mut planes = to_planes(
        &doubled,
        2 * width,
        height,
        8 * width,
        as_422,
        src_alpha,
        cfg,
        bgr,
    );
    for i in [0, 3] {
        if let Some(p) = planes.get_mut(i) {
            p.data = p.data.iter().step_by(2).copied().collect();
            p.width = width;
        }
    }
    planes
}

/// Y′CbCr planes (any of the SHQ samplings, with optional alpha plane) to RGBA8,
/// tightly packed. `out_alpha` chooses straight or premultiplied output; with
/// [`AlphaMode::Opaque`], or without an alpha plane, every pixel is opaque. `cfg` describes the planes as sent (matrix, range,
/// and whether the colour on the wire is premultiplied).
#[doc(hidden)]
pub fn planes_to_rgba(
    planes: &[Plane],
    width: usize,
    height: usize,
    out_alpha: AlphaMode,
    cfg: &ColorConfig,
) -> Vec<u8> {
    let mut out = Vec::new();
    planes_to_rgba_into(planes, width, height, out_alpha, cfg, &mut out);
    out
}

/// [`planes_to_rgba`] into a buffer kept from an earlier picture: a 4K picture's 33 MB
/// is above what the system allocator keeps for reuse, so a new buffer costs about 4 ms
/// of page faults (Linux, glibc) each time.
#[doc(hidden)]
pub fn planes_to_rgba_into(
    planes: &[Plane],
    width: usize,
    height: usize,
    out_alpha: AlphaMode,
    cfg: &ColorConfig,
    out: &mut Vec<u8>,
) {
    if cfg.transfer != Transfer::Sdr {
        planes_to_rgba_hdr(planes, width, height, out_alpha, cfg, out);
        return;
    }
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
    // Every byte is written below: a buffer of the right size is used as it is.
    if out.len() != width * height * 4 {
        out.clear();
        out.resize(width * height * 4, 0);
    }
    // Opaque output: alpha is 255 whatever the source sends (colour as sent).
    let alpha = planes.get(3).filter(|_| out_alpha != AlphaMode::Opaque);
    let (premultiply, unpremultiply) = match (wire_premultiplied, out_alpha) {
        (false, AlphaMode::Premultiplied) => (true, false),
        (true, AlphaMode::Straight) => (false, true),
        _ => (false, false),
    };
    let k = ToRgb {
        ky,
        kcr_r,
        kcb_g,
        kcr_g,
        kcb_b,
        yo: yo as i32,
    };
    let cw = cbp.width.max(1);
    #[cfg(target_arch = "x86_64")]
    let avx2 = std::arch::is_x86_feature_detected!("avx2");
    crate::pool::for_each(out.chunks_mut(width * 4 * 32).collect(), |ci, chunk| {
        for (ry, row) in chunk.chunks_mut(width * 4).enumerate() {
            let y = ci * 32 + ry;
            let yrow = &yp.data[y * yp.width..][..width];
            let cy = (y / sy).min(cbp.height - 1);
            let (cbrow, crrow) = (&cbp.data[cy * cw..][..cw], &crp.data[cy * cw..][..cw]);
            let arow = alpha.map(|ap| &ap.data[y * ap.width..][..width]);
            // Eight pixels at a time with AVX2 (same results) where each has its own
            // chroma sample position (`x / sx` inside the chroma row), the rest one by one.
            let mut done = 0;
            #[cfg(target_arch = "x86_64")]
            if avx2 && (sx == 1 || sx == 2) {
                let groups = width.min(sx * cw) / 8;
                // SAFETY: the CPU has AVX2; the rows hold `8 · groups` pixels (and their
                // chroma samples, `x / sx` < `cw`).
                unsafe { to_rgba_avx2::colour(yrow, cbrow, crrow, sx, groups, &k, row) };
                done = 8 * groups;
            }
            colour_row(yrow, cbrow, crrow, sx, done, &k, row);
            if let Some(arow) = arow {
                let mut done = 0;
                #[cfg(target_arch = "x86_64")]
                if avx2 {
                    // SAFETY: as above, `width / 8` groups of pixels and alpha samples.
                    unsafe {
                        to_rgba_avx2::alpha(arow, width / 8, premultiply, unpremultiply, row)
                    };
                    done = 8 * (width / 8);
                }
                alpha_row(
                    &arow[done..],
                    premultiply,
                    unpremultiply,
                    &mut row[4 * done..],
                );
            }
        }
    });
}

/// Fixed-point (×65536) Y′CbCr to RGB terms of [`planes_to_rgba`].
struct ToRgb {
    ky: i32,
    kcr_r: i32,
    kcb_g: i32,
    kcr_g: i32,
    kcb_b: i32,
    yo: i32,
}

/// One row's colour from pixel `x_start` (a multiple of `sx`) on, alpha 255. Each chroma
/// sample's terms once, for the `sx` pixels it covers (the last one also for any pixels
/// past the chroma plane's width).
fn colour_row(
    yrow: &[u8],
    cbrow: &[u8],
    crrow: &[u8],
    sx: usize,
    x_start: usize,
    k: &ToRgb,
    row: &mut [u8],
) {
    let (width, cw) = (yrow.len(), cbrow.len());
    let clamp = |v: i32| ((v + 32768) >> 16).clamp(0, 255) as u8;
    let mut x0 = x_start;
    for cx in x_start / sx..cw {
        if x0 >= width {
            break;
        }
        let x1 = if cx + 1 == cw {
            width
        } else {
            (x0 + sx).min(width)
        };
        let (cb, cr) = (cbrow[cx] as i32 - 128, crrow[cx] as i32 - 128);
        let (rc, gc, bc) = (k.kcr_r * cr, k.kcb_g * cb + k.kcr_g * cr, k.kcb_b * cb);
        let pixels = row[4 * x0..4 * x1].as_chunks_mut::<4>().0;
        for (px, &yv) in pixels.iter_mut().zip(&yrow[x0..x1]) {
            let yv = (yv as i32 - k.yo) * k.ky;
            *px = [clamp(yv + rc), clamp(yv + gc), clamp(yv + bc), 255];
        }
        x0 = x1;
    }
}

/// Puts the alpha samples into RGBA pixels, premultiplying or un-premultiplying colour.
fn alpha_row(arow: &[u8], premultiply: bool, unpremultiply: bool, row: &mut [u8]) {
    for (px, &a) in row.as_chunks_mut::<4>().0.iter_mut().zip(arow) {
        px[3] = a;
        let a = a as u32;
        if premultiply {
            for c in &mut px[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        } else if unpremultiply {
            for c in &mut px[..3] {
                // Fully transparent: no colour to recover.
                *c = (*c as u32 * 255 + a / 2)
                    .checked_div(a)
                    .map_or(0, |v| v.min(255) as u8);
            }
        }
    }
}

/// [`colour_row`] and [`alpha_row`] eight pixels at a time, with identical results: the
/// same integer arithmetic in 32-bit lanes; `x / 255` as `(x + 1 + (x >> 8)) >> 8` (exact
/// for `x` < 65536) and the un-premultiplying division in f32 (exact here, see the test).
#[cfg(target_arch = "x86_64")]
mod to_rgba_avx2 {
    use super::ToRgb;
    use std::arch::x86_64::*;

    /// Pixels `0..8 · groups` of a row; chroma sample `x / sx` for pixel `x` (sx 1 or 2).
    ///
    /// # Safety
    ///
    /// AVX2; `yrow` and `row` hold `8 · groups` pixels and the chroma rows their samples.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn colour(
        yrow: &[u8],
        cbrow: &[u8],
        crrow: &[u8],
        sx: usize,
        groups: usize,
        k: &ToRgb,
        row: &mut [u8],
    ) {
        let s = |v: i32| _mm256_set1_epi32(v);
        let (ky, kcr_r, kcb_g, kcr_g, kcb_b) =
            (s(k.ky), s(k.kcr_r), s(k.kcb_g), s(k.kcr_g), s(k.kcb_b));
        let (yo, c128, half, zero, max) = (s(k.yo), s(128), s(32768), s(0), s(255));
        let pairs = _mm256_setr_epi32(0, 0, 1, 1, 2, 2, 3, 3);
        let opaque = s(-0x0100_0000);
        let clamp = |v: __m256i| {
            _mm256_min_epi32(
                _mm256_max_epi32(_mm256_srai_epi32::<16>(_mm256_add_epi32(v, half)), zero),
                max,
            )
        };
        // Bytes at `p` as 8 lanes: 8 samples, or 4 samples each for two pixels.
        // SAFETY: AVX2 is this function's feature; every caller passes a pointer with 8 bytes (4
        // when `dup`) left in its row (below).
        let load = |p: *const u8, dup: bool| unsafe {
            if dup {
                let v = _mm_cvtsi32_si128(p.cast::<i32>().read_unaligned());
                _mm256_permutevar8x32_epi32(_mm256_cvtepu8_epi32(v), pairs)
            } else {
                _mm256_cvtepu8_epi32(_mm_loadl_epi64(p.cast()))
            }
        };
        let dup = sx == 2;
        for g in 0..groups {
            let x = 8 * g;
            // SAFETY: pixels x..x + 8 and their chroma samples are inside the rows.
            let (yv, cb, cr) = unsafe {
                (
                    load(yrow.as_ptr().add(x), false),
                    load(cbrow.as_ptr().add(x / sx), dup),
                    load(crrow.as_ptr().add(x / sx), dup),
                )
            };
            let yv = _mm256_mullo_epi32(_mm256_sub_epi32(yv, yo), ky);
            let (cb, cr) = (_mm256_sub_epi32(cb, c128), _mm256_sub_epi32(cr, c128));
            let r = clamp(_mm256_add_epi32(yv, _mm256_mullo_epi32(kcr_r, cr)));
            let gc = _mm256_add_epi32(_mm256_mullo_epi32(kcb_g, cb), _mm256_mullo_epi32(kcr_g, cr));
            let g_ = clamp(_mm256_add_epi32(yv, gc));
            let b = clamp(_mm256_add_epi32(yv, _mm256_mullo_epi32(kcb_b, cb)));
            let px = _mm256_or_si256(
                _mm256_or_si256(r, _mm256_slli_epi32::<8>(g_)),
                _mm256_or_si256(_mm256_slli_epi32::<16>(b), opaque),
            );
            // SAFETY: 32 bytes of pixels x..x + 8.
            unsafe { _mm256_storeu_si256(row.as_mut_ptr().add(4 * x).cast(), px) };
        }
    }

    /// [`super::alpha_row`] for pixels `0..8 · groups`.
    ///
    /// # Safety
    ///
    /// AVX2; `arow` and `row` hold `8 · groups` pixels.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn alpha(
        arow: &[u8],
        groups: usize,
        premultiply: bool,
        unpremultiply: bool,
        row: &mut [u8],
    ) {
        let s = |v: i32| _mm256_set1_epi32(v);
        let (byte, zero) = (s(0xff), s(0));
        for g in 0..groups {
            let x = 8 * g;
            // SAFETY: pixels and alpha samples x..x + 8.
            let (px, a) = unsafe {
                (
                    _mm256_loadu_si256(row.as_ptr().add(4 * x).cast()),
                    _mm256_cvtepu8_epi32(_mm_loadl_epi64(arow.as_ptr().add(x).cast())),
                )
            };
            let chan = |sh: i32| match sh {
                0 => _mm256_and_si256(px, byte),
                8 => _mm256_and_si256(_mm256_srli_epi32::<8>(px), byte),
                _ => _mm256_and_si256(_mm256_srli_epi32::<16>(px), byte),
            };
            let (r, g_, b) = (chan(0), chan(8), chan(16));
            let (r, g_, b) = if premultiply {
                let pm = |c: __m256i| {
                    let v = _mm256_add_epi32(_mm256_mullo_epi32(c, a), s(127));
                    _mm256_srli_epi32::<8>(_mm256_add_epi32(
                        _mm256_add_epi32(v, s(1)),
                        _mm256_srli_epi32::<8>(v),
                    ))
                };
                (pm(r), pm(g_), pm(b))
            } else if unpremultiply {
                let af = _mm256_cvtepi32_ps(a);
                let transparent = _mm256_cmpeq_epi32(a, zero);
                let un = |c: __m256i| {
                    let n =
                        _mm256_add_epi32(_mm256_mullo_epi32(c, s(255)), _mm256_srli_epi32::<1>(a));
                    let q = _mm256_cvttps_epi32(_mm256_div_ps(_mm256_cvtepi32_ps(n), af));
                    _mm256_andnot_si256(transparent, _mm256_min_epi32(q, s(255)))
                };
                (un(r), un(g_), un(b))
            } else {
                (r, g_, b)
            };
            let px = _mm256_or_si256(
                _mm256_or_si256(r, _mm256_slli_epi32::<8>(g_)),
                _mm256_or_si256(_mm256_slli_epi32::<16>(b), _mm256_slli_epi32::<24>(a)),
            );
            // SAFETY: as above.
            unsafe { _mm256_storeu_si256(row.as_mut_ptr().add(4 * x).cast(), px) };
        }
    }
}

/// Tables for tone-mapping one HDR transfer at one SDR white level, in fixed point.
struct ToneMap {
    /// R′ (10-bit index) → linear light relative to SDR white, 16.16 (1.0 = 65536),
    /// capped at 8.0 (anything brighter clips to white whatever the primaries do, bar
    /// pixels far outside any display's gamut): PQ. For HLG: the inverse OETF,
    /// scene-linear 0–1 in 16.16; the OOTF follows per pixel.
    eotf: Vec<i32>,
    /// HLG: Y_s (12-bit index over 0–1) → OOTF gain Y_s^(γ−1) × L_w / white, 8.8.
    hlg_gain: Vec<i32>,
    /// Linear 0–1 (12-bit index, 4097 entries) → sRGB-encoded 8-bit.
    oetf: Vec<u8>,
}

use transfer::{hlg_inverse_oetf, pq_eotf};

impl ToneMap {
    fn new(transfer: Transfer, white_nits: f32) -> Self {
        let white = white_nits.max(1.0) as f64;
        let fix = |x: f64| (x * 65536.0).round().min(8.0 * 65536.0) as i32;
        let eotf = (0..1024)
            .map(|i| {
                let v = i as f64 / 1023.0;
                match transfer {
                    Transfer::Pq => fix(pq_eotf(v) / white),
                    _ => fix(hlg_inverse_oetf(v)),
                }
            })
            .collect();
        // HLG OOTF for a 1000 nit display (BT.2100: γ = 1.2): L = L_w · Y_s^(γ−1) · E.
        let hlg_gain = (0..4096)
            .map(|i| (1000.0 * (i as f64 / 4095.0).powf(0.2) / white * 256.0).round() as i32)
            .collect();
        let oetf = (0..=4096)
            .map(|i| {
                let l = i as f64 / 4096.0;
                let v = if l <= 0.0031308 {
                    12.92 * l
                } else {
                    1.055 * l.powf(1.0 / 2.4) - 0.055
                };
                (v * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();
        ToneMap {
            eotf,
            hlg_gain,
            oetf,
        }
    }

    /// The tables for `transfer` at `white_nits`, built once and shared.
    fn get(transfer: Transfer, white_nits: f32) -> std::sync::Arc<ToneMap> {
        use std::sync::{Arc, Mutex};
        static CACHE: Mutex<Vec<(Transfer, u32, Arc<ToneMap>)>> = Mutex::new(Vec::new());
        let key = white_nits.to_bits();
        let mut cache = CACHE.lock_ok();
        if let Some((_, _, t)) = cache.iter().find(|(tr, w, _)| *tr == transfer && *w == key) {
            return t.clone();
        }
        let t = Arc::new(ToneMap::new(transfer, white_nits));
        if cache.len() >= 4 {
            cache.remove(0);
        }
        cache.push((transfer, key, t.clone()));
        t
    }

    /// R′ in 16.16 (1.0 = 65536) → linear, 16.16.
    #[inline(always)]
    fn linear(&self, v: i32) -> i32 {
        self.eotf[(v >> 6).clamp(0, 1023) as usize]
    }

    /// Linear 16.16 → sRGB 8-bit, clipping at 1.0.
    #[inline(always)]
    fn encode(&self, l: i32) -> u8 {
        self.oetf[(l.clamp(0, 65536) >> 4) as usize]
    }
}

/// BT.2020 → BT.709 primaries in linear light (rows: R, G, B out), 2.11 fixed point
/// (inputs up to 8.0 in 16.16 keep the products within i32).
const BT2020_TO_709: [[i32; 3]; 3] = [[3401, -1204, -149], [-255, 2320, -17], [-37, -206, 2291]];

/// The HDR version of [`planes_to_rgba`]: Y′CbCr → R′G′B′ with the configured matrix,
/// through the PQ or HLG transfer to linear light, scaled so `sdr_white_nits` is 1.0,
/// BT.2020 primaries converted to BT.709, clipped, and sRGB-encoded. 8-bit planes.
fn planes_to_rgba_hdr(
    planes: &[Plane],
    width: usize,
    height: usize,
    out_alpha: AlphaMode,
    cfg: &ColorConfig,
    out: &mut Vec<u8>,
) {
    let tm = ToneMap::get(cfg.transfer, cfg.sdr_white_nits);
    let (kr, kb) = cfg.matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    // R′G′B′ in 16.16 with 1.0 = 65536 (the SDR path's scale is 255.0 = 65536·255).
    let (ys, cs, yo) = if cfg.limited {
        (1.0 / 219.0, 1.0 / 224.0, 16)
    } else {
        (1.0 / 255.0, 1.0 / 255.0, 0)
    };
    let f = |v: f64| (v * 65536.0).round() as i32;
    let (ky, kcr_r, kcb_g, kcr_g, kcb_b) = (
        f(ys),
        f(2.0 * (1.0 - kr) * cs),
        f(-2.0 * (1.0 - kb) * kb / kg * cs),
        f(-2.0 * (1.0 - kr) * kr / kg * cs),
        f(2.0 * (1.0 - kb) * cs),
    );
    let hlg = cfg.transfer == Transfer::Hlg;
    let wide = cfg.primaries == Primaries::Bt2020;
    let wire_premultiplied = cfg.wire_alpha == AlphaMode::Premultiplied;
    let (yp, cbp, crp) = (&planes[0], &planes[1], &planes[2]);
    let (sx, sy) = (
        width.div_ceil(cbp.width.max(1)),
        height.div_ceil(cbp.height.max(1)),
    );
    // Every byte is written below (as in the SDR path): a buffer of the right size is
    // used as it is.
    if out.len() != width * height * 4 {
        out.clear();
        out.resize(width * height * 4, 0);
    }
    let alpha = planes.get(3).filter(|_| out_alpha != AlphaMode::Opaque);
    let (premultiply, unpremultiply) = match (wire_premultiplied, out_alpha) {
        (false, AlphaMode::Premultiplied) => (true, false),
        (true, AlphaMode::Straight) => (false, true),
        _ => (false, false),
    };
    let cw = cbp.width.max(1);
    let tm = &*tm;
    crate::pool::for_each(out.chunks_mut(width * 4 * 32).collect(), |ci, chunk| {
        for (ry, row) in chunk.chunks_mut(width * 4).enumerate() {
            let y = ci * 32 + ry;
            let yrow = &yp.data[y * yp.width..][..width];
            let cy = (y / sy).min(cbp.height - 1);
            let (cbrow, crrow) = (&cbp.data[cy * cw..][..cw], &crp.data[cy * cw..][..cw]);
            let mut x0 = 0;
            for cx in 0..cw {
                if x0 >= width {
                    break;
                }
                let x1 = if cx + 1 == cw {
                    width
                } else {
                    (x0 + sx).min(width)
                };
                let (cb, cr) = (cbrow[cx] as i32 - 128, crrow[cx] as i32 - 128);
                let (rc, gc, bc) = (kcr_r * cr, kcb_g * cb + kcr_g * cr, kcb_b * cb);
                let pixels = row[4 * x0..4 * x1].as_chunks_mut::<4>().0;
                for (px, &yv) in pixels.iter_mut().zip(&yrow[x0..x1]) {
                    let yv = (yv as i32 - yo) * ky;
                    let (mut r, mut g, mut b) =
                        (tm.linear(yv + rc), tm.linear(yv + gc), tm.linear(yv + bc));
                    if hlg {
                        // Scene-linear → display: the OOTF on the BT.2020 luminance
                        // (0.2627, 0.6780, 0.0593 in 0.8), gain in 8.8; E ≤ 1.0 here.
                        let ys = (67 * r + 174 * g + 15 * b) >> 8;
                        let gain = tm.hlg_gain[(ys.clamp(0, 65535) >> 4) as usize];
                        r = ((r * gain) >> 8).min(8 << 16);
                        g = ((g * gain) >> 8).min(8 << 16);
                        b = ((b * gain) >> 8).min(8 << 16);
                    }
                    if wide {
                        let m = &BT2020_TO_709;
                        (r, g, b) = (
                            (m[0][0] * r + m[0][1] * g + m[0][2] * b) >> 11,
                            (m[1][0] * r + m[1][1] * g + m[1][2] * b) >> 11,
                            (m[2][0] * r + m[2][1] * g + m[2][2] * b) >> 11,
                        );
                    }
                    *px = [tm.encode(r), tm.encode(g), tm.encode(b), 255];
                }
                x0 = x1;
            }
            if let Some(ap) = alpha {
                let arow = &ap.data[y * ap.width..][..width];
                for (px, &a) in row.as_chunks_mut::<4>().0.iter_mut().zip(arow) {
                    px[3] = a;
                    let a = a as u32;
                    if premultiply {
                        for c in &mut px[..3] {
                            *c = ((*c as u32 * a + 127) / 255) as u8;
                        }
                    } else if unpremultiply {
                        for c in &mut px[..3] {
                            *c = (*c as u32 * 255 + a / 2)
                                .checked_div(a)
                                .map_or(0, |v| v.min(255) as u8);
                        }
                    }
                }
            }
        }
    });
}

/// SDR pictures made HDR (BT.2408 mapping, PQ or HLG): an HDR switcher program (R.24).
pub mod to_hdr;
pub mod transfer;

#[cfg(test)]
mod hdr_bench;
#[cfg(test)]
mod tests;
