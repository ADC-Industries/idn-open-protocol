//! New format (§3.3–§3.10).

use idn_common::sync::LockOk as _;
use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_new(
    fourcc: Fourcc,
    w: usize,
    h: usize,
    quality: u8,
    payload: &[u8],
    track_ac: bool,
    precise: bool,
    shift: usize,
) -> Result<(Picture, Vec<Plane>), DecodeError> {
    let precise = precise && shift == 0;
    let (cols, rows) = (w.div_ceil(16), h.div_ceil(16));
    if payload.len() < 8 || (payload.len() - 8) / 4 < rows {
        return Err(DecodeError::Truncated);
    }
    let short_dc = payload[6] & 3 != 0;
    let high_precision = payload[6] & 4 != 0;
    let chunks = row_chunks(payload, rows)?;
    // Decodes rows `start..` into a band of the canvas starting there.
    let decode_band = |start: usize, band: &mut Band| -> Result<(), DecodeError> {
        let mut ctx = BlockCtx::new(quality, short_dc);
        ctx.precise = precise;
        ctx.half = shift == 1;
        let n = band.planes[0].0.len() / (band.planes[0].1 * (16 >> shift));
        if band.ac.is_none() && band.hi.is_none() {
            // Each MB row decoded into a small scratch row (in this core's cache, so its
            // blocks' short rows don't miss), then streamed into the picture in one pass:
            // writing blocks straight into the picture needed its lines cleared first to be
            // cache-warm, writing the picture twice (with the writes, 60%
            // of a 2160p receive).
            let mut scratch: Vec<Vec<u8>> = band
                .planes
                .iter()
                .map(|(d, w)| vec![0u8; d.len() / n.max(1) / (*w).max(1) * *w])
                .collect();
            for (i, chunk) in chunks[start..start + n].iter().enumerate() {
                let mut row = Band {
                    fourcc: band.fourcc,
                    shift: band.shift,
                    planes: scratch
                        .iter_mut()
                        .zip(&band.planes)
                        .map(|(s, (_, w))| (&mut s[..], *w))
                        .collect(),
                    ac: None,
                    hi: None,
                };
                decode_new_row(&mut BitReader::new(chunk), &mut row, &mut ctx, 0, cols)?;
                for (s, (d, _)) in scratch.iter().zip(band.planes.iter_mut()) {
                    stream_into(&mut d[i * s.len()..(i + 1) * s.len()], s);
                }
            }
            stream_done();
            return Ok(());
        }
        for (i, chunk) in chunks[start..start + n].iter().enumerate() {
            band.warm_row(i, n);
            decode_new_row(&mut BitReader::new(chunk), band, &mut ctx, i, cols)?;
        }
        Ok(())
    };
    // One canvas for the picture, its bands of MB rows decoded in parallel straight into
    // it: where its padded width is the picture's (a multiple of 16), the planes are the
    // picture's, its padding rows cut off, with no copy (each band used to have a canvas
    // of its own, copied into the picture after).
    let workers = worker_count(w, rows);
    let per = rows.div_ceil(workers);
    let mut canvas = Canvas::scaled(fourcc, w, h, track_ac, precise, shift, true);
    {
        let bands: Vec<std::sync::Mutex<Option<Band>>> = canvas
            .bands(per)
            .into_iter()
            .map(|b| std::sync::Mutex::new(Some(b)))
            .collect();
        let results: Vec<Result<(), DecodeError>> = if bands.len() == 1 {
            vec![decode_band(
                0,
                bands[0].lock_ok().as_mut().expect("one band"),
            )]
        } else {
            crate::pool::par_map(bands.len(), |i| {
                let mut band = bands[i].lock_ok().take().expect("each band once");
                decode_band(i * per, &mut band)
            })
        };
        results.into_iter().collect::<Result<Vec<_>, _>>()?;
    }
    let (planes, masks, precise) = canvas.finish_hi();
    let pic = Picture {
        fourcc,
        width: w.div_ceil(1 << shift),
        height: h.div_ceil(1 << shift),
        format: Format::New,
        quality,
        short_dc,
        high_precision,
        planes,
        precise,
        coefficients: None,
    };
    Ok((pic, masks))
}

/// Where a row's blocks go: reconstructed into a picture's planes ([`Band`]), or kept
/// as coefficients for the GPU ([`CoefBand`]).
pub(super) trait RowSink {
    fn fourcc(&self) -> Fourcc;
    /// A block of a skipped MB: its predictor, flat.
    fn fill(&mut self, ctx: &BlockCtx, pl: usize, at: (usize, usize, usize, usize));
    /// A coded block, read from `r`.
    fn coded(
        &mut self,
        ctx: &mut BlockCtx,
        r: &mut BitReader,
        kind: BlockKind,
        pl: usize,
        at: (usize, usize, usize, usize),
    ) -> Result<(), DecodeError>;
}

/// The MB `m` of row `r`'s origin in plane `pl` (in samples, before any half-size shift).
pub(super) fn mb_origin(fourcc: Fourcc, pl: usize, m: usize, r: usize) -> (usize, usize) {
    if pl == 1 || pl == 2 {
        let (cw, ch) = fourcc.chroma_mb();
        (cw * m, ch * r)
    } else {
        (16 * m, 16 * r)
    }
}

impl RowSink for Band<'_> {
    fn fourcc(&self) -> Fourcc {
        self.fourcc
    }

    #[inline(always)]
    fn fill(&mut self, ctx: &BlockCtx, pl: usize, (m, row, dx, dy): (usize, usize, usize, usize)) {
        let buf = [ctx.fill(ctx.pred[pl]); 64];
        let hi = ctx.precise.then(|| [ctx.fill_hi(ctx.pred[pl]); 64]);
        let (x0, y0) = self.mb_origin(pl, m, row);
        self.put_hi(pl, x0 + dx, y0 + dy, &buf, false, hi.as_ref());
    }

    #[inline(always)]
    fn coded(
        &mut self,
        ctx: &mut BlockCtx,
        r: &mut BitReader,
        kind: BlockKind,
        pl: usize,
        (m, row, dx, dy): (usize, usize, usize, usize),
    ) -> Result<(), DecodeError> {
        let mut buf = [0u8; 64];
        let ac = ctx.block(r, pl, kind, &mut buf)?;
        let (x0, y0) = self.mb_origin(pl, m, row);
        self.put_hi(
            pl,
            x0 + dx,
            y0 + dy,
            &buf,
            ac,
            ctx.precise.then_some(&ctx.hi),
        );
        Ok(())
    }
}

pub(super) fn decode_new_row<S: RowSink>(
    r: &mut BitReader,
    canvas: &mut S,
    ctx: &mut BlockCtx,
    row: usize,
    cols: usize,
) -> Result<(), DecodeError> {
    let (colour, alpha) = canvas.fourcc().blocks();
    ctx.reset();
    let mut m = 0;
    loop {
        let run = read_skip_run(r, cols - m)?;
        for mb in m..m + run {
            for &(pl, dx, dy) in colour.iter().chain(alpha) {
                canvas.fill(ctx, pl, (mb, row, dx, dy));
            }
        }
        m += run;
        if m == cols {
            return Ok(());
        }
        let all_full = r.bit()?;
        let kind_for = |r: &mut BitReader| -> Result<BlockKind, DecodeError> {
            if all_full {
                return Ok(BlockKind::Full);
            }
            Ok(match (r.bit()?, r.peek(1)) {
                (false, _) => BlockKind::Full,
                (true, b) => {
                    r.consume(1)?;
                    if b == 0 {
                        BlockKind::Repeat
                    } else {
                        BlockKind::DcOnly
                    }
                }
            })
        };
        for &(pl, dx, dy) in colour {
            let kind = kind_for(r)?;
            canvas.coded(ctx, r, kind, pl, (m, row, dx, dy))?;
        }
        if !alpha.is_empty() {
            let alpha_repeat = r.bit()?;
            for &(pl, dx, dy) in alpha {
                let kind = if alpha_repeat {
                    BlockKind::Repeat
                } else {
                    kind_for(r)?
                };
                canvas.coded(ctx, r, kind, pl, (m, row, dx, dy))?;
            }
        }
        m += 1;
        if m == cols {
            return Ok(());
        }
    }
}

/// Reads a `SKIPRUN` (§3.5.2), rejecting runs longer than `remaining`.
pub(super) fn read_skip_run(r: &mut BitReader, remaining: usize) -> Result<usize, DecodeError> {
    let mut total = 0;
    loop {
        let mut k = 0;
        while k < 5 && !r.bit()? {
            k += 1;
        }
        if k == 5 {
            total += 31;
            if total > remaining {
                return Err(DecodeError::SkipOverrun);
            }
            continue;
        }
        total += (1 << k) - 1 + r.read(k)? as usize;
        if total > remaining {
            return Err(DecodeError::SkipOverrun);
        }
        return Ok(total);
    }
}
