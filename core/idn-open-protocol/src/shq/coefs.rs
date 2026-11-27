//! Coefficients for reconstruction elsewhere (the switcher's GPU, perf queue 2b).

use idn_common::sync::LockOk as _;
use super::*;

/// A picture's blocks as the inverse DCT's input, from [`decode_coefficients`]: the
/// bitstream read and dequantised on the CPU (the part that doesn't run well on a GPU),
/// the inverse DCT and the writing of samples left to whoever reconstructs it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoefPicture {
    pub fourcc: Fourcc,
    /// The picture's size in pixels.
    pub width: usize,
    pub height: usize,
    /// Y, Cb, Cr, and A for layouts with alpha.
    pub planes: Vec<CoefPlane>,
}

/// One plane's blocks. Block `i = by · bw + bx` (the 8 × 8 block at (8·bx, 8·by)) is
/// three words, `blocks[3i..3i + 3]`: `start`, then a 64-bit mask (low word first) of its
/// non-zero coefficients by raster index `8·v + u`. Their dequantised values (§3.8) are
/// `values[start..]`, in increasing index. With a mask of 0 it is flat: `start` is its
/// sample value. The plane is `width` × `height` samples; blocks at its right and bottom
/// edges are partly outside it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoefPlane {
    pub width: usize,
    pub height: usize,
    pub bw: usize,
    pub bh: usize,
    pub blocks: Vec<u32>,
    pub values: Vec<i16>,
}

/// A band of MB rows' blocks being read (see [`CoefPlane`]): each plane's block rows, its
/// width in blocks, and the band's own values (starts relative to them, made absolute
/// when the bands are joined).
pub(super) struct CoefBand<'a> {
    pub(super) fourcc: Fourcc,
    pub(super) blocks: Vec<(&'a mut [u32], usize)>,
    pub(super) values: Vec<Vec<i16>>,
}

impl CoefBand<'_> {
    #[inline(always)]
    pub(super) fn slot(
        &mut self,
        pl: usize,
        (m, row, dx, dy): (usize, usize, usize, usize),
    ) -> &mut [u32] {
        let (x0, y0) = mb_origin(self.fourcc, pl, m, row);
        let (b, bw) = &mut self.blocks[pl];
        let i = 3 * ((y0 + dy) / 8 * *bw + (x0 + dx) / 8);
        &mut b[i..i + 3]
    }
}

impl RowSink for CoefBand<'_> {
    fn fourcc(&self) -> Fourcc {
        self.fourcc
    }

    fn fill(&mut self, ctx: &BlockCtx, pl: usize, at: (usize, usize, usize, usize)) {
        let v = ctx.fill(ctx.pred[pl]) as u32;
        self.slot(pl, at).copy_from_slice(&[v, 0, 0]);
    }

    fn coded(
        &mut self,
        ctx: &mut BlockCtx,
        r: &mut BitReader,
        kind: BlockKind,
        pl: usize,
        at: (usize, usize, usize, usize),
    ) -> Result<(), DecodeError> {
        let mut f = [0i32; 64];
        let words = if ctx.read(r, pl, kind, &mut f)? {
            // The mask first, then the non-zero values by its set bits, lowest (raster
            // order) first: a block has a handful, and gathering all 64 one after another
            // was half the picture's decode time.
            let mask = nonzero_mask(&f);
            let c = &mut self.values[pl];
            let start = c.len();
            let mut m = mask;
            while m != 0 {
                c.push(f[m.trailing_zeros() as usize] as i16);
                m &= m - 1;
            }
            // (All zero after dequantisation: a flat block of 0, what the inverse DCT
            // gives.)
            [
                start as u32 * u32::from(mask != 0),
                mask as u32,
                (mask >> 32) as u32,
            ]
        } else {
            [ctx.fill(ctx.pred[pl]) as u32, 0, 0]
        };
        self.slot(pl, at).copy_from_slice(&words);
        Ok(())
    }
}

/// Bit `k` set where `f[k]` isn't zero. On x86-64 four at a time with SSE2's compare and
/// movemask (the plain loop became variable 64-bit shifts, the hottest code of a
/// coefficient decode).
#[inline]
pub(super) fn nonzero_mask(f: &[i32; 64]) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        let mut m = 0u64;
        // SAFETY: SSE2 is part of x86-64; 16 unaligned loads of four i32 within `f`.
        unsafe {
            let zero = _mm_setzero_si128();
            for (i, q) in f.as_chunks::<4>().0.iter().enumerate() {
                let v = _mm_loadu_si128(q.as_ptr().cast());
                let z = _mm_movemask_ps(_mm_castsi128_ps(_mm_cmpeq_epi32(v, zero))) as u64;
                m |= (!z & 0xf) << (4 * i);
            }
        }
        m
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut m = 0u64;
        for (i, &v) in f.iter().enumerate() {
            m |= u64::from(v != 0) << i;
        }
        m
    }
}

/// Reads one SHQ payload as far as its dequantised coefficients (see [`CoefPicture`]).
/// New-format payloads only (legacy ones give [`DecodeError::Unsupported`]: decode them
/// with [`decode`]).
pub fn decode_coefficients(
    p: &VideoParams,
    payload: &[u8],
    limits: &Limits,
) -> Result<CoefPicture, DecodeError> {
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
    if !(101..=199).contains(&b0) {
        return Err(if (50..=99).contains(&b0) {
            DecodeError::Unsupported
        } else {
            DecodeError::BadFirstByte
        });
    }
    if p.field_id == 3 {
        return Err(DecodeError::BadFieldId);
    }
    let quality = b0 - 100;
    let (cols, rows) = (w.div_ceil(16), h.div_ceil(16));
    let chunks = row_chunks(payload, rows)?;
    let short_dc = payload[6] & 3 != 0;
    // Each plane's blocks, covering whole MBs; block rows per MB row.
    let (cmw, cmh) = fourcc.chroma_mb();
    let mb_rows = |pl: usize| if pl == 1 || pl == 2 { cmh / 8 } else { 2 };
    let mut planes: Vec<CoefPlane> = Canvas::crop_dims(fourcc, w, h)
        .iter()
        .enumerate()
        .map(|(pl, &(pw, ph))| {
            let mw = if pl == 1 || pl == 2 { cmw } else { 16 };
            let (bw, bh) = (mw * cols / 8, mb_rows(pl) * rows);
            CoefPlane {
                width: pw,
                height: ph,
                bw,
                bh,
                // Every block's words are written (each MB is read or filled).
                blocks: scratch::BLOCKS.take(3 * bw * bh, true),
                values: Vec::new(),
            }
        })
        .collect();
    let per = rows.div_ceil(worker_count(w, rows));
    let bands: Vec<std::sync::Mutex<Option<CoefBand>>> = {
        let mut bands: Vec<CoefBand> = (0..rows.div_ceil(per))
            .map(|_| CoefBand {
                fourcc,
                blocks: Vec::new(),
                values: Vec::new(),
            })
            .collect();
        for (pl, p) in planes.iter_mut().enumerate() {
            let band_words = 3 * per * mb_rows(pl) * p.bw;
            for (b, rows) in bands.iter_mut().zip(p.blocks.chunks_mut(band_words)) {
                b.blocks.push((rows, p.bw));
                b.values.push(Vec::new());
            }
        }
        bands
            .into_iter()
            .map(|b| std::sync::Mutex::new(Some(b)))
            .collect()
    };
    let read_band = |i: usize| -> Result<Vec<Vec<i16>>, DecodeError> {
        let mut band = bands[i].lock_ok().take().expect("each band once");
        let mut ctx = BlockCtx::new(quality, short_dc);
        let start = i * per;
        // Room for every value the band's bits can hold (a coefficient takes at least two
        // bits): reserved, not touched, so it costs no memory it doesn't use, and the
        // values aren't moved as they grow (that was a copy and fresh pages each time).
        let bits: usize = chunks[start..(start + per).min(rows)]
            .iter()
            .map(|c| c.len() * 8)
            .sum();
        for v in band.values.iter_mut() {
            *v = scratch::VALUES.take_empty(bits / 2);
        }
        for (k, chunk) in chunks[start..(start + per).min(rows)].iter().enumerate() {
            decode_new_row(&mut BitReader::new(chunk), &mut band, &mut ctx, k, cols)?;
        }
        Ok(band.values)
    };
    let values: Vec<Vec<Vec<i16>>> = if bands.len() == 1 {
        vec![read_band(0)?]
    } else {
        crate::pool::par_map(bands.len(), read_band)
            .into_iter()
            .collect::<Result<_, _>>()?
    };
    // Joined: each band's starts moved past the bands before it.
    for (pl, p) in planes.iter_mut().enumerate() {
        p.values = scratch::VALUES.take_empty(values.iter().map(|b| b[pl].len()).sum());
        let band_words = 3 * per * mb_rows(pl) * p.bw;
        for (b, rows) in values.iter().zip(p.blocks.chunks_mut(band_words)) {
            let base = p.values.len() as u32;
            if base > 0 {
                for w in rows
                    .as_chunks_mut::<3>()
                    .0
                    .iter_mut()
                    .filter(|w| w[1] | w[2] != 0)
                {
                    w[0] += base;
                }
            }
            p.values.extend_from_slice(&b[pl]);
        }
    }
    // The bands' own values, joined: kept for the next picture's bands.
    for v in values.into_iter().flatten() {
        scratch::VALUES.give(v);
    }
    Ok(CoefPicture {
        fourcc,
        width: w,
        height: h,
        planes,
    })
}

/// A [`CoefPicture`] reconstructed at half size on the CPU (each 8 × 8 block as 4 × 4, its
/// four lowest frequencies, as [`decode_half`] does): for showing it small (the switcher's
/// multiview) at an eighth of a full reconstruction's cost.
pub fn reconstruct_half(c: &CoefPicture) -> Picture {
    let planes = c
        .planes
        .iter()
        .map(|p| {
            let (hw, hh) = (p.width.div_ceil(2), p.height.div_ceil(2));
            let mut data = vec![0u8; hw * hh];
            for by in 0..p.bh {
                for bx in 0..p.bw {
                    let b = &p.blocks[3 * (by * p.bw + bx)..][..3];
                    let mask = b[1] as u64 | (b[2] as u64) << 32;
                    let mut out = [0u8; 64];
                    if mask == 0 {
                        out.fill(b[0] as u8);
                    } else {
                        let mut f = [0i32; 64];
                        let mut next = b[0] as usize;
                        for (k, v) in f.iter_mut().enumerate() {
                            if mask >> k & 1 == 1 {
                                *v = p.values[next] as i32;
                                next += 1;
                            }
                        }
                        idct::idct_half(&f, &mut out);
                    }
                    for y in 0..4 {
                        let (py, px) = (4 * by + y, 4 * bx);
                        if py >= hh || px >= hw {
                            continue;
                        }
                        let n = 4.min(hw - px);
                        data[py * hw + px..py * hw + px + n]
                            .copy_from_slice(&out[4 * y..4 * y + n]);
                    }
                }
            }
            Plane {
                width: hw,
                height: hh,
                data,
            }
        })
        .collect();
    Picture {
        fourcc: c.fourcc,
        width: c.width.div_ceil(2),
        height: c.height.div_ceil(2),
        format: Format::New,
        quality: 0,
        short_dc: false,
        high_precision: false,
        planes,
        precise: None,
        coefficients: None,
    }
}

impl Picture {
    /// A picture delivered as coefficients ([`Picture::coefficients`]), its planes empty.
    pub fn from_coefficients(c: CoefPicture) -> Picture {
        Picture {
            fourcc: c.fourcc,
            width: c.width,
            height: c.height,
            format: Format::New,
            quality: 0,
            short_dc: false,
            high_precision: false,
            planes: Vec::new(),
            precise: None,
            coefficients: Some(std::sync::Arc::new(c)),
        }
    }
}

/// The samples of a [`CoefPicture`] reconstructed on the CPU, the reference the GPU's
/// reconstruction is held to: the same planes [`decode`] gives. Large pictures in bands of
/// block rows on the worker pool (one thread took about a frame's time at 2160p, so a
/// switcher's virtual shot of a 4K camera kept only every other frame; not threads of its
/// own per band per plane per picture), the planes from the scratch pool.
pub fn reconstruct(c: &CoefPicture) -> Picture {
    let bands = if c.width * c.height > 1920 * 1080 {
        crate::pool::threads().clamp(1, 8)
    } else {
        1
    };
    let mut planes: Vec<Plane> = c
        .planes
        .iter()
        .map(|p| Plane {
            width: p.width,
            height: p.height,
            // Every visible sample is written by `reconstruct_row`: not zeroed first (about
            // 16 MB of memset a 2160p 4:2:2 picture).
            data: scratch::BYTES.take(p.width * p.height, true),
        })
        .collect();
    // Each band writes its own block rows of its plane.
    let mut work = Vec::new();
    for (p, out) in c.planes.iter().zip(planes.iter_mut()) {
        if p.width == 0 || p.height == 0 {
            continue;
        }
        let per = p.bh.div_ceil(bands).max(1);
        for (band, rows) in out.data.chunks_mut(8 * per * p.width).enumerate() {
            work.push((p, band, per, rows));
        }
    }
    crate::pool::for_each(work, |_, (p, band, per, rows)| {
        // Block rows below the plane (padded) have nothing to write.
        for by in band * per..((band + 1) * per).min(p.bh).min(p.height.div_ceil(8)) {
            reconstruct_row(p, by, &mut rows[8 * (by - band * per) * p.width..]);
        }
    });
    Picture {
        fourcc: c.fourcc,
        width: c.width,
        height: c.height,
        format: Format::New,
        quality: 0,
        short_dc: false,
        high_precision: false,
        planes,
        precise: None,
        coefficients: None,
    }
}

/// One row of 8 × 8 blocks of plane `p` reconstructed into `rows` (its rows from the
/// block row's top).
pub(super) fn reconstruct_row(p: &CoefPlane, by: usize, rows: &mut [u8]) {
    for bx in 0..p.bw {
        let b = &p.blocks[3 * (by * p.bw + bx)..][..3];
        let mask = b[1] as u64 | (b[2] as u64) << 32;
        let mut out = [0u8; 64];
        if mask == 0 {
            out.fill(b[0] as u8);
        } else {
            let mut f = [0i32; 64];
            let mut next = b[0] as usize;
            for (k, v) in f.iter_mut().enumerate() {
                if mask >> k & 1 == 1 {
                    *v = p.values[next] as i32;
                    next += 1;
                }
            }
            idct::idct_8x8(&f, &mut out);
        }
        for y in 0..8 {
            let (py, px) = (8 * by + y, 8 * bx);
            if py >= p.height || px >= p.width {
                continue;
            }
            let n = 8.min(p.width - px);
            rows[y * p.width + px..y * p.width + px + n].copy_from_slice(&out[8 * y..8 * y + n]);
        }
    }
}
