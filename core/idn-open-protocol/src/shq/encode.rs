//! SpeedHQ encoder (spec chapter 3): new format (§3.3–§3.10) and legacy format (§3.11),
//! following the sender rules of §3.12.3.

use super::{Fourcc, Plane, Plane16};

/// The same DCT and quantisation as [`sse`] on whole 8-lane rows, for processors with AVX2
/// (chosen at run time; others keep `sse`): one pass down the columns, an 8 × 8 transpose,
/// one along the rows. Each lane does the same operations in the same order as there, so
/// the levels are the same, bit for bit (tested).
#[cfg(target_arch = "x86_64")]
mod avx2;
mod bitstream;
mod blocks;
mod dct;
/// The AAN DCT and the quantiser with SSE2 (part of every x86-64): each row of a block
/// is two 4-lane vectors, so both 1-D passes run on all columns at once.
#[cfg(target_arch = "x86_64")]
mod sse;

use bitstream::*;
use blocks::*;
use dct::*;

/// Picture to encode: planes Y, Cb, Cr (and A for alpha FourCCs) in the FourCC's
/// sampling (§3.2.3), 8-bit, cropped (no padding).
pub struct EncodeInput<'a> {
    pub fourcc: Fourcc,
    pub width: usize,
    pub height: usize,
    pub planes: &'a [Plane],
    /// The same picture with more than 8 bits (P216 convention), for high-precision
    /// sources (§3.9.5, §E.7). When set, coefficients come from these samples, DC keeps
    /// its 1/8-sample fraction, short DC is not used and the wrapper bit is set. `planes`
    /// must still hold the 8-bit rounding (used for the legacy format).
    pub precise: Option<&'a [Plane16]>,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    /// Quality 1..=99 (new format) or 50..=99 (legacy).
    pub quality: u8,
    /// Write the new format (receiver advertised `shq_skip_block`) or the legacy format.
    pub new_format: bool,
    /// Wrapper "high-precision source" bit (new format only).
    pub high_precision: bool,
    /// Allow short DC (§3.6.3): only when the receiver advertised `shq_short_dc` (§3.1.1).
    /// Applied at Q ≤ 86 only.
    pub short_dc: bool,
    /// Use skip runs, REPEAT/DCONLY blocks and mode 0 where cheaper (new format only).
    /// Off = every MB coded in mode 1 with all blocks FULL.
    pub compact: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        EncodeOptions {
            quality: 98,
            new_format: true,
            high_precision: false,
            short_dc: false,
            compact: true,
        }
    }
}

/// The preconditions of [`encode`].
fn check_input(inp: &EncodeInput) {
    let n = if inp.fourcc.has_alpha() { 4 } else { 3 };
    assert_eq!(inp.planes.len(), n, "planes for {:?}", inp.fourcc);
    assert!(
        (1..=u16::MAX as usize).contains(&inp.width)
            && (1..=u16::MAX as usize).contains(&inp.height),
        "picture size {}x{} outside 1..=65535",
        inp.width,
        inp.height
    );
    let ok = |w: usize, h: usize, len: usize| {
        w > 0 && h > 0 && w.checked_mul(h).is_some_and(|a| len >= a)
    };
    for (i, p) in inp.planes.iter().enumerate() {
        assert!(
            ok(p.width, p.height, p.data.len()),
            "plane {i}: {} samples for {}x{}",
            p.data.len(),
            p.width,
            p.height
        );
    }
    if let Some(pp) = inp.precise {
        assert!(
            pp.len() >= n,
            "precise planes: {} for {:?}",
            pp.len(),
            inp.fourcc
        );
        for (i, p) in pp.iter().enumerate() {
            assert!(
                ok(p.width, p.height, p.data.len()),
                "precise plane {i}: {} samples for {}x{}",
                p.data.len(),
                p.width,
                p.height
            );
        }
    }
}

/// `f(0..n)` in order: on the shared worker pool when the work is large enough.
fn parallel_map(n: usize, width: usize, f: &(dyn Fn(usize) -> Vec<u8> + Sync)) -> Vec<Vec<u8>> {
    if width * n * 16 < 320 * 240 || n < 2 {
        return (0..n).map(f).collect();
    }
    crate::pool::par_map(n, f)
}

/// Encodes one picture (a frame, or a single field at field height).
///
/// # Panics
///
/// If the input is malformed, checked here before any work: a size of 0 or above 65535
/// (the header has 16 bits for each), the wrong number of planes (or too few precise
/// planes) for the FourCC, or a plane that is empty or whose data is shorter than its
/// width × height.
pub fn encode(inp: &EncodeInput, opt: &EncodeOptions) -> Vec<u8> {
    check_input(inp);
    let (cols, rows) = (inp.width.div_ceil(16), inp.height.div_ceil(16));
    if opt.new_format {
        let q = opt.quality.clamp(1, 99);
        let qt = QTable::new(100 - q as i32);
        let digit = match inp.fourcc {
            Fourcc::Shq0 => 0,
            Fourcc::Shq2 => 2,
            Fourcc::Shq4 => 4,
            Fourcc::Shq7 => 7,
            Fourcc::Shq9 => 9,
        };
        let mut out = vec![100 + q];
        out.extend_from_slice(&(inp.width as u16).to_le_bytes());
        out.extend_from_slice(&(inp.height as u16).to_le_bytes());
        out.push(digit << 4);
        // Short DC rounds DC to whole samples: worth it at moderate quality, a measurable
        // loss at very high quality, so used only at Q ≤ 86, as NDI 6.3 senders do (§3.6.3).
        // Short DC loses the DC fraction of precise sources (§3.9.5).
        let short = opt.short_dc && opt.compact && q <= 86 && inp.precise.is_none();
        let hp = opt.high_precision || inp.precise.is_some();
        out.push(if hp { 4 } else { 0 } | if short { 3 } else { 0 });
        out.push(0);
        // Rows are independent (§3.3.2): encode them in parallel, then join in order.
        let row = |r: usize| -> Vec<u8> {
            let mut w = BitWriter::new();
            let mut arena = SymArena::default();
            if !opt.compact {
                let mut preds = [1024i32; 4];
                for m in 0..cols {
                    w.code("1"); // SKIPRUN: run 0
                    w.put(true); // mode 1: every colour block FULL
                    write_mb(&mut w, inp, &mut preds, m, r, &qt, true, &mut arena);
                }
                return w.finish();
            }
            let mut preds = [if short { 128 } else { 1024 }; 4];
            let mut pending = 0usize;
            let (mut colour, mut alpha) = (Blocks::with_capacity(12), Blocks::with_capacity(4));
            for m in 0..cols {
                mb_blocks(
                    inp,
                    m,
                    r,
                    &qt,
                    short,
                    &mut colour,
                    &mut alpha,
                    &mut arena,
                    &preds,
                );
                // A skipped MB is filled from the predictors (§3.5.5): skip when every
                // block would be a REPEAT.
                let skippable = colour
                    .iter()
                    .chain(&alpha)
                    .all(|(pl, c)| !c.has_ac && c.d == preds[*pl]);
                if skippable {
                    pending += 1;
                    continue;
                }
                write_skiprun(&mut w, pending);
                pending = 0;
                write_mb_compact(&mut w, &mut preds, &colour, &alpha, &arena);
            }
            if pending > 0 {
                write_skiprun(&mut w, pending);
            }
            w.finish()
        };
        let coded = parallel_map(rows, inp.width, &row);
        out.reserve_exact(coded.iter().map(|r| 3 + r.len()).sum::<usize>() + 4);
        for bytes in coded {
            // A row of 16 lines is far below 16 MB at any size and quality (§3.3.2).
            let size = 3 + bytes.len();
            debug_assert!(size <= MAX_CHUNK);
            out.extend_from_slice(&(size as u32).to_le_bytes()[..3]);
            out.extend_from_slice(&bytes);
        }
        out.extend_from_slice(&[0; 4]);
        out
    } else {
        // A slice is a quarter of the picture: 4K 4:4:4:4 noise at high quality can pass
        // the 24-bit size (§3.11.2). Then lower the quality until it fits.
        let mut q = opt.quality.clamp(50, 99);
        loop {
            match encode_legacy(inp, q, rows) {
                Some(out) => return out,
                None if q > 50 => q = q.saturating_sub(10).max(50),
                None => panic!("a legacy slice over 16 MB even at Q50"),
            }
        }
    }
}

/// The largest row chunk or slice: its size field is 24 bits (§3.3.2, §3.11.2).
const MAX_CHUNK: usize = 0xff_ffff;

/// The legacy format at quality `q`; `None` if a slice doesn't fit its size field.
fn encode_legacy(inp: &EncodeInput, q: u8, rows: usize) -> Option<Vec<u8>> {
    {
        let qt = QTable::new(100 - q as i32);
        let regular = inp.width / 16;
        let slice = |s: usize| -> Vec<u8> {
            let mut w = BitWriter::new();
            let mut arena = SymArena::default();
            for r in (s..rows).step_by(4) {
                let mut preds = [1024i32; 4];
                for m in 0..regular {
                    write_mb(&mut w, inp, &mut preds, m, r, &qt, false, &mut arena);
                }
            }
            if s == 3 && !inp.width.is_multiple_of(16) {
                // Border column, one MB per row, after the regular rows (§3.11.4).
                for r in 0..rows {
                    let mut preds = [1024i32; 4];
                    write_mb(&mut w, inp, &mut preds, regular, r, &qt, false, &mut arena);
                }
            }
            w.finish()
        };
        let slices = parallel_map(4, inp.width * rows.div_ceil(4), &slice);
        if slices.iter().any(|s| 3 + s.len() > MAX_CHUNK) {
            return None;
        }
        let mut out = Vec::with_capacity(4 + slices.iter().map(|s| 3 + s.len()).sum::<usize>() + 4);
        out.extend_from_slice(&[q, 4, 0, 0]); // offset 4 (§3.11.2)
        for s in slices {
            out.extend_from_slice(&((3 + s.len()) as u32).to_le_bytes()[..3]);
            out.extend_from_slice(&s);
        }
        out.extend_from_slice(&[0; 4]); // trailer
        Some(out)
    }
}

#[cfg(test)]
mod stage_bench;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod timing;
