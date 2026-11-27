//! SpeedHQ (SHQ) video payload decoder (spec chapter 3), new and legacy formats.

mod ac_table;
mod bits;
pub mod encode;
#[cfg(feature = "gpu")]
pub mod gpu_encode;
/// SpeedHQ's inverse DCT on the GPU, from coefficients (§E.27).
#[cfg(feature = "gpu")]
pub mod gpu_idct;
mod idct;
mod vlc;

pub use idct::basis_table as idct_basis;
pub(crate) use idct::set_machine_precision;
pub use idct::{Precision, idct_precision, machine_precision, set_idct_precision};

use bits::BitReader;
use std::fmt;
use vlc::{AcSym, INTRA_QUANT, ZIGZAG, ac_lut, dc_chroma_lut, dc_luma_lut};

mod block;
mod canvas;
mod coefs;
mod legacy;
mod new_format;
mod scratch;
pub use block::*;
use canvas::*;
pub use coefs::*;
use legacy::*;
use new_format::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// A code or field extends past its row chunk or slice (§3.12.1 #4, #11).
    Overrun,
    /// Twelve `0` bits where an AC code is expected (#1).
    BadAcCode,
    /// AC scan index above 63 (#2).
    ScanOverrun,
    /// A skip run past the end of the MB row (#3).
    SkipOverrun,
    /// Row chunk or slice sizes inconsistent with the payload (#4–#6).
    BadSize,
    /// Payload too short for its geometry (#12).
    Truncated,
    /// First byte not a valid format/quality (#7).
    BadFirstByte,
    /// FourCC not supported (#8).
    BadFourcc,
    /// Field id invalid for this format (#9).
    BadFieldId,
    /// Zero width or height (#10).
    BadDimensions,
    /// Above the receiver's implementation limit (#13).
    TooLarge,
    /// Valid but unverified layout this decoder does not handle (two-field legacy with
    /// odd height).
    Unsupported,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for DecodeError {}

/// A picture's layout: chroma sampling, and whether there is an alpha plane.
///
/// Plane sizes for a `w` × `h` picture: Y and A are `w` × `h`; Cb and Cr are
/// `ceil(w/2)` × `h` for 4:2:2, `ceil(w/2)` × `ceil(h/2)` for 4:2:0, and `w` × `h` for
/// 4:4:4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fourcc {
    /// 4:2:0, no alpha.
    Shq0,
    /// 4:2:2, no alpha: by far the most common.
    Shq2,
    /// 4:4:4, no alpha.
    Shq4,
    /// 4:2:2 with alpha: graphics for keying.
    Shq7,
    /// 4:4:4 with alpha.
    Shq9,
}

/// Blocks of a macroblock as (plane, x offset, y offset).
type BlockLayout = &'static [(usize, usize, usize)];

impl Fourcc {
    /// The layout for a FourCC code such as `*b"SHQ2"`, if known.
    pub fn from_bytes(b: [u8; 4]) -> Option<Self> {
        match &b {
            b"SHQ0" => Some(Fourcc::Shq0),
            b"SHQ2" => Some(Fourcc::Shq2),
            b"SHQ4" => Some(Fourcc::Shq4),
            b"SHQ7" => Some(Fourcc::Shq7),
            b"SHQ9" => Some(Fourcc::Shq9),
            _ => None,
        }
    }

    /// Pictures with this layout have a fourth (alpha) plane.
    pub fn has_alpha(self) -> bool {
        matches!(self, Fourcc::Shq7 | Fourcc::Shq9)
    }

    /// Chroma samples per MB (width, height).
    fn chroma_mb(self) -> (usize, usize) {
        match self {
            Fourcc::Shq0 => (8, 8),
            Fourcc::Shq2 | Fourcc::Shq7 => (8, 16),
            Fourcc::Shq4 | Fourcc::Shq9 => (16, 16),
        }
    }

    /// Colour then alpha blocks of one MB, in bitstream order (§3.4.1): (plane, dx, dy)
    /// relative to the MB's origin in that plane.
    fn blocks(self) -> (BlockLayout, BlockLayout) {
        const Y: [(usize, usize, usize); 4] = [(0, 0, 0), (0, 8, 0), (0, 0, 8), (0, 8, 8)];
        const A: [(usize, usize, usize); 4] = [(3, 0, 0), (3, 8, 0), (3, 0, 8), (3, 8, 8)];
        const C420: [(usize, usize, usize); 6] = [Y[0], Y[1], Y[2], Y[3], (1, 0, 0), (2, 0, 0)];
        const C422: [(usize, usize, usize); 8] = [
            Y[0],
            Y[1],
            Y[2],
            Y[3],
            (1, 0, 0),
            (2, 0, 0),
            (1, 0, 8),
            (2, 0, 8),
        ];
        const C444: [(usize, usize, usize); 12] = [
            Y[0],
            Y[1],
            Y[2],
            Y[3],
            (1, 0, 0),
            (2, 0, 0),
            (1, 0, 8),
            (2, 0, 8),
            (1, 8, 0),
            (2, 8, 0),
            (1, 8, 8),
            (2, 8, 8),
        ];
        let colour: &[_] = match self {
            Fourcc::Shq0 => &C420,
            Fourcc::Shq2 | Fourcc::Shq7 => &C422,
            Fourcc::Shq4 | Fourcc::Shq9 => &C444,
        };
        (colour, if self.has_alpha() { &A } else { &[] })
    }
}

/// The fields of the video extension header (§2.4) that the decoder needs.
#[derive(Debug, Clone, Copy)]
pub struct VideoParams {
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    pub field_id: u32,
}

/// Implementation limits on picture size (§3.2.3, §3.12.1 #13).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_width: 8192,
            max_height: 8192,
            max_pixels: 8192 * 4320,
        }
    }
}

/// The SpeedHQ bitstream variant a picture was coded in. For information only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The current format (all NDI 5+ and IDN senders).
    New,
    /// The older format, from very old senders.
    Legacy,
}

/// One 8-bit plane of a picture: samples row by row, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// Samples per row.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// `width * height` samples. Y'CbCr are limited range (Y 16–235, Cb/Cr 16–240, 128
    /// = no colour); alpha is 0–255.
    pub data: Vec<u8>,
}

/// One 16-bit plane: samples row by row, no padding. An 8-bit value `v` is `v << 8`
/// (the P216 convention), so the low byte carries the extra precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane16 {
    /// Samples per row.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// `width * height` samples.
    pub data: Vec<u16>,
}

impl Picture {
    /// Hands the picture's memory back to the decoder for its next picture (optional):
    /// see [`Plane::recycle`]. Call it when done with a received picture.
    /// A copy whose planes come from the memory pictures hand back ([`Plane::buffer`]),
    /// not fresh pages: for a picture kept and delivered both (a changed-regions
    /// reference).
    pub fn pooled_clone(&self) -> Picture {
        let mut p = self.clone_without_planes();
        p.planes = self
            .planes
            .iter()
            .map(|pl| {
                let mut data = Plane::buffer(pl.data.len());
                data.copy_from_slice(&pl.data);
                Plane {
                    width: pl.width,
                    height: pl.height,
                    data,
                }
            })
            .collect();
        p
    }

    /// Everything but the planes (left empty).
    fn clone_without_planes(&self) -> Picture {
        Picture {
            planes: Vec::new(),
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            format: self.format,
            quality: self.quality,
            short_dc: self.short_dc,
            high_precision: self.high_precision,
            precise: self.precise.clone(),
            coefficients: self.coefficients.clone(),
        }
    }

    pub fn recycle(self) {
        for p in self.planes {
            p.recycle();
        }
        // A picture delivered as coefficients: theirs too, when nothing else holds them.
        if let Some(c) = self
            .coefficients
            .and_then(|c| std::sync::Arc::try_unwrap(c).ok())
        {
            c.recycle();
        }
    }
}

impl CoefPicture {
    /// Hands its block tables and values back for the next picture's (optional): a 2160p
    /// picture's were several MB allocated afresh, and faulted in, every time.
    pub fn recycle(self) {
        for p in self.planes {
            scratch::BLOCKS.give(p.blocks);
            scratch::VALUES.give(p.values);
        }
    }
}

impl Plane {
    /// Hands this plane's memory back to the decoder for its next picture (optional):
    /// while pictures are being decoded, the next one is then written into memory already
    /// in use instead of memory allocated and zeroed afresh (a fifth of a 2160p60
    /// receiver's CPU). Elsewhere it is simply freed.
    pub fn recycle(mut self) {
        scratch::BYTES.recycle(std::mem::take(&mut self.data));
    }

    /// A buffer of `len` bytes for a plane, from the memory pictures hand back
    /// ([`Plane::recycle`]) when there is some: its bytes are whatever was there, so write
    /// every one. For code that makes pictures of its own each frame (the switcher's
    /// filters), so it too reuses memory rather than faulting in fresh pages.
    pub fn buffer(len: usize) -> Vec<u8> {
        scratch::BYTES.take(len, true)
    }
}

impl Plane16 {
    /// The 8-bit plane, rounded.
    pub fn to_8bit(&self) -> Plane {
        Plane {
            width: self.width,
            height: self.height,
            data: self
                .data
                .iter()
                .map(|&v| ((v as u32 + 128) >> 8).min(255) as u8)
                .collect(),
        }
    }
}

/// A decoded picture: planes Y, Cb, Cr, and A for layouts with alpha, sized as described
/// under [`Fourcc`].
#[derive(Debug, Clone)]
pub struct Picture {
    /// Chroma sampling and alpha.
    pub fourcc: Fourcc,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels (field height for a field).
    pub height: usize,
    /// Bitstream variant (informational).
    pub format: Format,
    /// The quality the source coded it at, 1–99.
    pub quality: u8,
    /// Coded with the more compact, slightly less precise DC mode (informational).
    pub short_dc: bool,
    /// The source had more than 8 bits per sample (10-bit or HDR); worth requesting
    /// [`precise`](Picture::precise) planes.
    pub high_precision: bool,
    /// The planes, 8 bits per sample.
    pub planes: Vec<Plane>,
    /// The same planes with 3 extra fraction bits (11 significant bits, stored as
    /// `value << 5` in 16 bits), if
    /// [`InputConfig::precise`](crate::input::InputConfig::precise) is set. Smooth
    /// areas keep this precision; fine detail is limited by the source's quality.
    pub precise: Option<Vec<Plane16>>,
    /// With [`InputConfig::coefficients`](crate::input::InputConfig::coefficients):
    /// the picture as dequantised coefficients, for reconstructing it elsewhere (a GPU),
    /// and `planes` empty. `None` for pictures decoded as usual.
    pub coefficients: Option<std::sync::Arc<CoefPicture>>,
}

/// Decodes one SHQ payload.
pub fn decode(p: &VideoParams, payload: &[u8], limits: &Limits) -> Result<Picture, DecodeError> {
    decode_inner(p, payload, limits, false, false).map(|(pic, _)| pic)
}

/// As [`decode`], also reconstructing with 3 fraction bits for outputs above 8 bits
/// (§3.9.5): [`Picture::precise`] is set for new-format payloads.
pub fn decode_precise(
    p: &VideoParams,
    payload: &[u8],
    limits: &Limits,
) -> Result<Picture, DecodeError> {
    decode_inner(p, payload, limits, false, true).map(|(pic, _)| pic)
}

/// As [`decode`], at half the width and height (rounded up) for receivers that show the
/// picture at half size or less: every coefficient is still read, but each block is
/// reconstructed as 4 × 4 from its low frequencies, which costs a fraction of the full
/// inverse DCT, and there are a quarter of the samples to convert and copy. The legacy
/// format is decoded at full size ([`Picture::width`] says which).
pub fn decode_half(
    p: &VideoParams,
    payload: &[u8],
    limits: &Limits,
) -> Result<Picture, DecodeError> {
    decode_scaled(p, payload, limits, false, false, 1).map(|(pic, _)| pic)
}

/// As [`decode`], also returning per-sample masks: 1 where the sample came from a block
/// with AC (§3.13 `.acmask`). For conformance testing.
#[doc(hidden)]
pub fn decode_with_ac_mask(
    p: &VideoParams,
    payload: &[u8],
    limits: &Limits,
) -> Result<(Picture, Vec<Plane>), DecodeError> {
    decode_inner(p, payload, limits, true, false)
}

fn decode_inner(
    p: &VideoParams,
    payload: &[u8],
    limits: &Limits,
    track_ac: bool,
    precise: bool,
) -> Result<(Picture, Vec<Plane>), DecodeError> {
    decode_scaled(p, payload, limits, track_ac, precise, 0)
}

fn decode_scaled(
    p: &VideoParams,
    payload: &[u8],
    limits: &Limits,
    track_ac: bool,
    precise: bool,
    shift: usize,
) -> Result<(Picture, Vec<Plane>), DecodeError> {
    let fourcc = Fourcc::from_bytes(p.fourcc).ok_or(DecodeError::BadFourcc)?;
    if p.field_id > 3 {
        return Err(DecodeError::BadFieldId);
    }
    if p.width == 0 || p.height == 0 {
        return Err(DecodeError::BadDimensions);
    }
    if p.width > limits.max_width
        || p.height > limits.max_height
        || p.width as u64 * p.height as u64 > limits.max_pixels
    {
        return Err(DecodeError::TooLarge);
    }
    let (w, h) = (p.width as usize, p.height as usize);
    let b0 = *payload.first().ok_or(DecodeError::Truncated)?;
    match b0 {
        101..=199 => {
            if p.field_id == 3 {
                return Err(DecodeError::BadFieldId);
            }
            decode_new(fourcc, w, h, b0 - 100, payload, track_ac, precise, shift)
        }
        50..=99 => decode_legacy(fourcc, w, h, p.field_id, b0, payload, track_ac),
        _ => Err(DecodeError::BadFirstByte),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Coefficients read on the CPU and reconstructed from them give exactly what
    /// `decode` gives: every layout, sizes not a multiple of 16, both DC modes, flat and
    /// detailed areas.
    #[test]
    fn nonzero_masks_match_the_plain_loop() {
        let mut seed = 0x9e37_79b9u32;
        for _ in 0..2000 {
            let mut f = [0i32; 64];
            for v in f.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *v = if seed.is_multiple_of(5) {
                    (seed % 41) as i32 - 20
                } else {
                    0
                };
            }
            let plain = f
                .iter()
                .enumerate()
                .fold(0u64, |m, (k, &v)| m | u64::from(v != 0) << k);
            assert_eq!(nonzero_mask(&f), plain);
        }
        assert_eq!(nonzero_mask(&[0; 64]), 0);
        assert_eq!(nonzero_mask(&[-1; 64]), u64::MAX);
    }

    #[test]
    fn coefficients_reconstruct_to_the_decoded_picture() {
        use encode::{EncodeInput, EncodeOptions, encode};
        for (fourcc, code) in [
            (Fourcc::Shq0, *b"SHQ0"),
            (Fourcc::Shq2, *b"SHQ2"),
            (Fourcc::Shq4, *b"SHQ4"),
            (Fourcc::Shq7, *b"SHQ7"),
            (Fourcc::Shq9, *b"SHQ9"),
        ] {
            for (w, h, short_dc) in [(64usize, 48usize, false), (70, 37, true), (35, 18, false)] {
                let mut seed = 0x9e37_79b9u32 ^ (w * h) as u32;
                let planes: Vec<Plane> = Canvas::crop_dims(fourcc, w, h)
                    .into_iter()
                    .map(|(pw, ph)| Plane {
                        width: pw,
                        height: ph,
                        data: (0..pw * ph)
                            .map(|i| {
                                seed ^= seed << 13;
                                seed ^= seed >> 17;
                                seed ^= seed << 5;
                                let (x, y) = (i % pw, i / pw);
                                // Flat on the left, detail on the right.
                                if x < pw / 3 {
                                    90
                                } else {
                                    (x * 7 + y * 3) as u8 ^ (seed as u8 & 31)
                                }
                            })
                            .collect(),
                    })
                    .collect();
                let payload = encode(
                    &EncodeInput {
                        fourcc,
                        width: w,
                        height: h,
                        planes: &planes,
                        precise: None,
                    },
                    &EncodeOptions {
                        quality: 85,
                        short_dc,
                        ..Default::default()
                    },
                );
                let vp = VideoParams {
                    fourcc: code,
                    width: w as u32,
                    height: h as u32,
                    field_id: 0,
                };
                let pic = decode(&vp, &payload, &Limits::default()).unwrap();
                let c = decode_coefficients(&vp, &payload, &Limits::default()).unwrap();
                let back = reconstruct(&c);
                assert_eq!(back.planes.len(), pic.planes.len());
                for (a, b) in back.planes.iter().zip(&pic.planes) {
                    assert_eq!(
                        (a.width, a.height),
                        (b.width, b.height),
                        "{fourcc:?} {w}x{h}"
                    );
                    assert!(a.data == b.data, "{fourcc:?} {w}x{h} short_dc {short_dc}");
                }
                // At half size: what the half-size decode gives.
                let half = decode_half(&vp, &payload, &Limits::default()).unwrap();
                let small = reconstruct_half(&c);
                assert_eq!((small.width, small.height), (half.width, half.height));
                for (a, b) in small.planes.iter().zip(&half.planes) {
                    assert_eq!((a.width, a.height), (b.width, b.height), "half {fourcc:?}");
                    assert!(a.data == b.data, "half {fourcc:?} {w}x{h}");
                }
            }
        }
    }

    /// A half-size decode is close to the full decode averaged over 2 × 2 squares, for
    /// every layout, odd sizes, and one or several threads.
    /// Canvases taken from the pool aren't zeroed: every sample must be written. Decoded
    /// once over buffers holding one pattern and once over another, a picture (every
    /// layout, odd sizes, fraction bits, AC masks, one and four threads, half size)
    /// comes out the same.
    #[test]
    fn every_sample_is_written_over_reused_buffers() {
        use crate::color::{AlphaMode, ColorConfig, rgba_to_planes};
        use crate::shq::encode::{EncodeInput, EncodeOptions, encode};
        for (fourcc, name, w, h) in [
            (Fourcc::Shq2, *b"SHQ2", 333usize, 179usize),
            (Fourcc::Shq0, *b"SHQ0", 321, 183),
            (Fourcc::Shq7, *b"SHQ7", 257, 145),
            (Fourcc::Shq4, *b"SHQ4", 201, 121),
            (Fourcc::Shq9, *b"SHQ9", 199, 97),
            // Widths a multiple of 16: the canvas's planes are the picture's (no copy).
            (Fourcc::Shq2, *b"SHQ2", 320, 184),
            (Fourcc::Shq7, *b"SHQ7", 256, 136),
            (Fourcc::Shq0, *b"SHQ0", 160, 90),
        ] {
            let rgba: Vec<u8> = (0..w * h)
                .flat_map(|i| {
                    let (x, y) = ((i % w) as f32, (i / w) as f32);
                    let v = |f: f32| (128.0 + 100.0 * f.sin()) as u8;
                    // A flat band too, so skipped and DC-only blocks occur.
                    if y < 40.0 {
                        [60, 60, 60, 255]
                    } else {
                        [v(x / 9.0), v(y / 7.0), v((x + y) / 13.0), v(x / 31.0)]
                    }
                })
                .collect();
            let planes = rgba_to_planes(
                &rgba,
                w,
                h,
                w * 4,
                fourcc,
                AlphaMode::Straight,
                &ColorConfig::default(),
            );
            let bytes = encode(
                &EncodeInput {
                    fourcc,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions {
                    quality: 70,
                    ..Default::default()
                },
            );
            let p = VideoParams {
                fourcc: name,
                width: w as u32,
                height: h as u32,
                field_id: 0,
            };
            let decode_all = |fill: u8| {
                scratch::BYTES.poison(fill, 24, 1 << 20);
                scratch::WORDS.poison(u16::from(fill) * 257, 8, 1 << 20);
                let mut out = Vec::new();
                for workers in [1, 4] {
                    force_workers(workers);
                    let pic = decode_precise(&p, &bytes, &Limits::default()).unwrap();
                    let (masked, masks) =
                        decode_with_ac_mask(&p, &bytes, &Limits::default()).unwrap();
                    let half = decode_half(&p, &bytes, &Limits::default()).unwrap();
                    out.push((pic.planes, pic.precise, masked.planes, masks, half.planes));
                }
                force_workers(0);
                out
            };
            assert!(
                decode_all(0x5a) == decode_all(0xa5),
                "{name:?}: a sample left unwritten"
            );
        }
    }

    #[test]
    fn half_size_decoding_is_the_full_picture_averaged() {
        use crate::color::{AlphaMode, ColorConfig, rgba_to_planes};
        use crate::shq::encode::{EncodeInput, EncodeOptions, encode};
        for (fourcc, name, w, h) in [
            (Fourcc::Shq2, *b"SHQ2", 640usize, 360usize),
            (Fourcc::Shq2, *b"SHQ2", 333, 179),
            (Fourcc::Shq0, *b"SHQ0", 320, 182),
            (Fourcc::Shq7, *b"SHQ7", 256, 144),
            (Fourcc::Shq4, *b"SHQ4", 200, 120),
        ] {
            let rgba: Vec<u8> = (0..w * h)
                .flat_map(|i| {
                    let (x, y) = ((i % w) as f32, (i / w) as f32);
                    let v = |f: f32| (128.0 + 100.0 * f.sin()) as u8;
                    [v(x / 9.0), v(y / 7.0), v((x + y) / 13.0), v(x / 31.0)]
                })
                .collect();
            let planes = rgba_to_planes(
                &rgba,
                w,
                h,
                w * 4,
                fourcc,
                AlphaMode::Straight,
                &ColorConfig::default(),
            );
            let bytes = encode(
                &EncodeInput {
                    fourcc,
                    width: w,
                    height: h,
                    planes: &planes,
                    precise: None,
                },
                &EncodeOptions {
                    quality: 90,
                    ..Default::default()
                },
            );
            let p = VideoParams {
                fourcc: name,
                width: w as u32,
                height: h as u32,
                field_id: 0,
            };
            for workers in [1, 4] {
                force_workers(workers);
                let full = decode(&p, &bytes, &Limits::default()).unwrap();
                let half = decode_half(&p, &bytes, &Limits::default()).unwrap();
                force_workers(0);
                assert_eq!((half.width, half.height), (w.div_ceil(2), h.div_ceil(2)));
                assert_eq!(half.planes.len(), full.planes.len());
                for (f, hp) in full.planes.iter().zip(&half.planes) {
                    assert_eq!(
                        (hp.width, hp.height),
                        (f.width.div_ceil(2), f.height.div_ceil(2)),
                        "{name:?}"
                    );
                    let (mut sum, mut worst, mut n) = (0u64, 0u32, 0u64);
                    for y in 0..f.height / 2 {
                        for x in 0..f.width / 2 {
                            let at = |dx, dy| f.data[(2 * y + dy) * f.width + 2 * x + dx] as u32;
                            let avg = (at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4;
                            let d = avg.abs_diff(hp.data[y * hp.width + x] as u32);
                            sum += d as u64;
                            worst = worst.max(d);
                            n += 1;
                        }
                    }
                    let mean = sum as f64 / n as f64;
                    assert!(
                        mean < 1.5 && worst < 24,
                        "{name:?} {w}x{h}: mean {mean:.2}, worst {worst}"
                    );
                }
            }
        }
    }

    #[test]
    fn short_two_field_legacy_payloads_are_errors_not_panics() {
        let p = VideoParams {
            fourcc: *b"SHQ2",
            width: 16,
            height: 16,
            field_id: 0,
        };
        // Legacy Q80, field offset 16 in an 8-byte payload: two fields claimed, no room.
        for len in 4..12 {
            let mut payload = vec![80u8, 16, 0, 0];
            payload.resize(len, 0);
            let r = std::panic::catch_unwind(|| decode(&p, &payload, &Limits::default()));
            assert!(matches!(r, Ok(Err(_))), "len {len}: {r:?}");
        }
    }
}
