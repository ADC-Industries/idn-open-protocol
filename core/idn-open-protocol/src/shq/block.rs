//! Block decoding shared by both formats.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    Full,
    DcOnly,
    Repeat,
}

pub(super) struct BlockCtx {
    pub(super) short_dc: bool,
    /// Dequantisation multipliers `M[k]`, raster order (§3.8).
    pub(super) mul: [i32; 64],
    pub(super) pred: [i32; 4],
    /// Also reconstruct with 3 fraction bits into `hi` (§3.9.5).
    pub(super) precise: bool,
    pub(super) hi: [u16; 64],
    /// Reconstruct 4 × 4 blocks for a half-size picture ([`idct::idct_half`]).
    pub(super) half: bool,
}

impl BlockCtx {
    pub(super) fn new(quality: u8, short_dc: bool) -> Self {
        let s = 100 - quality as i32;
        let mut mul = [0; 64];
        for k in 1..64 {
            mul[k] = INTRA_QUANT[k] as i32 * s;
        }
        mul[0] = if short_dc { 128 } else { 16 };
        BlockCtx {
            short_dc,
            mul,
            pred: [0; 4],
            precise: false,
            hi: [0; 64],
            half: false,
        }
    }

    pub(super) fn reset(&mut self) {
        self.pred = [if self.short_dc { 128 } else { 1024 }; 4];
    }

    /// The 3-fraction-bit fill of a DC-only block: the predictor itself (§3.9.5).
    pub(super) fn fill_hi(&self, x: i32) -> u16 {
        if self.short_dc {
            (x.clamp(0, 255) as u16) << 8
        } else {
            (x.clamp(0, 2047) as u16) << 5
        }
    }

    pub(super) fn fill(&self, x: i32) -> u8 {
        if self.short_dc {
            x.clamp(0, 255) as u8
        } else {
            // Negative predictors are implementation-defined (§3.9.1); use 0.
            (x.max(0) >> 3).min(255) as u8
        }
    }

    /// Reads one block of component `comp`: its DC predictor updated and, if it has AC,
    /// its dequantised coefficients into `f` (raster, all zero on entry), the inverse DCT's input. Returns
    /// whether it had AC (without, it is filled with its predictor).
    #[inline(always)]
    pub(super) fn read(
        &mut self,
        r: &mut BitReader,
        comp: usize,
        kind: BlockKind,
        f: &mut [i32; 64],
    ) -> Result<bool, DecodeError> {
        if kind != BlockKind::Repeat {
            let diff = read_dc_diff(r, comp)?;
            self.pred[comp] = (self.pred[comp] - diff) as i16 as i32;
        }
        let d = self.pred[comp];
        // Dequantised as read, into `f` (all zero on entry): only the coefficients
        // present cost anything (multiplying all 64 of every block was a tenth of a
        // decode).
        let has_ac = kind == BlockKind::Full && read_ac(r, f, &self.mul)?;
        if !has_ac {
            return Ok(false);
        }
        f[0] = ((d * self.mul[0]) as i16 as i32) >> 4;
        Ok(true)
    }

    /// Decodes one block of component `comp` into samples; returns whether it had AC.
    pub(super) fn block(
        &mut self,
        r: &mut BitReader,
        comp: usize,
        kind: BlockKind,
        out: &mut [u8; 64],
    ) -> Result<bool, DecodeError> {
        let mut f = [0i32; 64];
        if !self.read(r, comp, kind, &mut f)? {
            let d = self.pred[comp];
            out.fill(self.fill(d));
            if self.precise {
                let v = self.fill_hi(d);
                self.hi.fill(v);
            }
            return Ok(false);
        }
        if self.half {
            idct::idct_half(&f, out);
        } else if self.precise {
            idct::idct_8x8_hi(&f, out, &mut self.hi);
        } else {
            idct::idct_8x8(&f, out);
        }
        Ok(true)
    }
}

pub(super) fn read_dc_diff(r: &mut BitReader, comp: usize) -> Result<i32, DecodeError> {
    let lut = if comp == 1 || comp == 2 {
        dc_chroma_lut()
    } else {
        dc_luma_lut()
    };
    let e = lut.get(r.peek(10));
    r.consume(e.len as u32)?;
    let s = e.sym as u32;
    if s == 0 {
        return Ok(0);
    }
    let v = r.read(s)? as i32;
    Ok(if v >> (s - 1) & 1 == 1 {
        v
    } else {
        v - ((1 << s) - 1)
    })
}

/// Reads AC symbols up to EOB into `coef` (raster order). Returns whether any AC symbol
/// was present.
/// Reads a block's AC coefficients into `f` (raster order, which must be all zero),
/// each dequantised with `mul` as it comes: `((level · mul) as i16) >> 4`.
pub(super) fn read_ac(
    r: &mut BitReader,
    f: &mut [i32; 64],
    mul: &[i32; 64],
) -> Result<bool, DecodeError> {
    let lut = ac_lut();
    let mut i = 0usize;
    let mut any = false;
    // The bits ahead in a register, `have` of them, refilled from the reader below 32: a
    // load from the payload for every code had been on the path from one code to the next.
    let (mut acc, mut have) = r.window();
    loop {
        if have < 32 {
            (acc, have) = r.window();
        }
        // One look serves the code and the sign bit after it (codes are at most 16 bits).
        let bits = acc as u32;
        let e = lut.get(bits);
        if e.len == 0 {
            return Err(DecodeError::BadAcCode);
        }
        let (run, level) = match e.sym {
            AcSym::Eob => {
                r.consume(e.len as u32)?;
                return Ok(any);
            }
            AcSym::Esc => {
                r.consume(e.len as u32)?;
                let run = r.read(6)? as usize;
                let u = r.read(12)? as i32;
                have = 0;
                (run, u - 2048)
            }
            AcSym::RunLevel(run, l) => {
                let n = e.len as u32 + 1;
                r.consume(n)?;
                acc >>= n;
                have -= n;
                let l = l as i32;
                (run as usize, if (bits >> e.len) & 1 == 1 { -l } else { l })
            }
        };
        i += run + 1;
        if i > 63 {
            return Err(DecodeError::ScanOverrun);
        }
        let z = ZIGZAG[i] as usize;
        f[z] = ((level * mul[z]) as i16 as i32) >> 4;
        any = true;
    }
}

pub(super) static FORCE_WORKERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Forces the number of decoder threads (0 = automatic). For testing the parallel paths.
#[doc(hidden)]
pub fn force_workers(n: usize) {
    FORCE_WORKERS.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Threads to use for a picture: one for small pictures, else up to the core count with at
/// least two MB rows per thread.
pub(super) fn worker_count(w: usize, rows: usize) -> usize {
    let forced = FORCE_WORKERS.load(std::sync::atomic::Ordering::Relaxed);
    if forced != 0 {
        return forced.min(rows).max(1);
    }
    if w * rows * 16 < 320 * 240 {
        return 1;
    }
    // Up to 1080p two are enough (a picture in about 1.5 ms on this machine's cores):
    // more only cost their waking, 94–99 ms of CPU a second for a 1080p30 receiver against
    // 74–78 on one or two (2026-11-03).
    if w * rows * 16 <= 1920 * 1088 {
        return crate::pool::threads().min(2);
    }
    crate::pool::threads().min(rows / 2).max(1)
}

/// Copies `src` into `dst` (the same length) with streaming stores, which don't bring
/// `dst` into the cache first (a decoded picture is written once and read by another
/// core later). Call [`stream_done`] before the picture is handed on.
pub(super) fn stream_into(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    // Up to dst's first 16-byte boundary by hand (where streaming stores are used).
    let head = if cfg!(target_arch = "x86_64") {
        dst.as_ptr().align_offset(16).min(n)
    } else {
        0
    };
    dst[..head].copy_from_slice(&src[..head]);
    let mut i = head;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SSE2 is part of x86-64; stores go to 16-byte-aligned addresses in bounds.
    unsafe {
        use std::arch::x86_64::*;
        while i + 64 <= n {
            let s = src.as_ptr().add(i) as *const __m128i;
            let d = dst.as_mut_ptr().add(i) as *mut __m128i;
            _mm_stream_si128(d, _mm_loadu_si128(s));
            _mm_stream_si128(d.add(1), _mm_loadu_si128(s.add(1)));
            _mm_stream_si128(d.add(2), _mm_loadu_si128(s.add(2)));
            _mm_stream_si128(d.add(3), _mm_loadu_si128(s.add(3)));
            i += 64;
        }
        while i + 16 <= n {
            let v = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
            _mm_stream_si128(dst.as_mut_ptr().add(i) as *mut __m128i, v);
            i += 16;
        }
    }
    dst[i..n].copy_from_slice(&src[i..n]);
}

/// Orders this thread's streaming stores before what it does next (handing the picture
/// on).
pub(super) fn stream_done() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a store fence.
    unsafe {
        std::arch::x86_64::_mm_sfence()
    };
}

/// Every MB row's chunk, located by its size (§3.3.2): rows are independent, so they can
/// then be decoded in parallel.
pub(super) fn row_chunks(payload: &[u8], rows: usize) -> Result<Vec<&[u8]>, DecodeError> {
    if payload.len() < 8 || (payload.len() - 8) / 4 < rows {
        return Err(DecodeError::Truncated);
    }
    let mut chunks = Vec::with_capacity(rows);
    let mut pos = 8;
    for _ in 0..rows {
        if payload.len() - pos < 3 {
            return Err(DecodeError::Truncated);
        }
        let size = u24(&payload[pos..]);
        if size < 4 || size > payload.len() - pos {
            return Err(DecodeError::BadSize);
        }
        chunks.push(&payload[pos + 3..pos + size]);
        pos += size;
    }
    Ok(chunks)
}

pub(super) fn u24(b: &[u8]) -> usize {
    b[0] as usize | (b[1] as usize) << 8 | (b[2] as usize) << 16
}
