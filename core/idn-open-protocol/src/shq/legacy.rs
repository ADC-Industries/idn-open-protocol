//! Legacy format (§3.11).

use super::*;

pub(super) fn decode_legacy(
    fourcc: Fourcc,
    w: usize,
    h: usize,
    field_id: u32,
    quality: u8,
    payload: &[u8],
    track_ac: bool,
) -> Result<(Picture, Vec<Plane>), DecodeError> {
    if payload.len() < 4 {
        return Err(DecodeError::Truncated);
    }
    let offset = u24(&payload[1..]);
    let len = payload.len();
    let one_field = matches!(field_id, 1 | 2) || offset == 4 || offset == len - 4;
    let finish = |planes, short_dc| Picture {
        fourcc,
        width: w,
        height: h,
        format: Format::Legacy,
        quality,
        short_dc,
        high_precision: false,
        planes,
        precise: None,
        coefficients: None,
    };
    if one_field {
        legacy_slices(&payload[4..])?;
        let mut canvas = Canvas::new(fourcc, w, h, track_ac);
        decode_legacy_field(&payload[4..], &mut canvas, quality, w, h)?;
        let (planes, masks) = canvas.finish();
        return Ok((finish(planes, false), masks));
    }
    // Two fields in one payload (§3.11.2, unverified): even frame lines, then odd.
    if offset < 16 || offset.saturating_add(12) > len {
        return Err(DecodeError::BadSize);
    }
    if !h.is_multiple_of(2) {
        return Err(DecodeError::Unsupported);
    }
    let fh = h / 2;
    legacy_slices(&payload[4..offset])?;
    legacy_slices(&payload[offset..])?;
    let mut fields = Vec::new();
    for data in [&payload[4..offset], &payload[offset..]] {
        let mut canvas = Canvas::new(fourcc, w, fh, track_ac);
        decode_legacy_field(data, &mut canvas, quality, w, fh)?;
        fields.push(canvas.finish());
    }
    let weave = |a: &[Plane], b: &[Plane]| -> Vec<Plane> {
        a.iter()
            .zip(b)
            .map(|(a, b)| {
                let mut data = Vec::with_capacity(a.data.len() * 2);
                for y in 0..a.height {
                    data.extend_from_slice(&a.data[y * a.width..(y + 1) * a.width]);
                    data.extend_from_slice(&b.data[y * b.width..(y + 1) * b.width]);
                }
                Plane {
                    width: a.width,
                    height: a.height * 2,
                    data,
                }
            })
            .collect()
    };
    let planes = weave(&fields[0].0, &fields[1].0);
    let masks = weave(&fields[0].1, &fields[1].1);
    Ok((finish(planes, false), masks))
}

/// The four slices of a legacy field (§3.11.2), checked before anything is allocated
/// for the picture (§3.12.1 #12).
pub(super) fn legacy_slices(data: &[u8]) -> Result<[&[u8]; 4], DecodeError> {
    if data.len() < 12 {
        return Err(DecodeError::Truncated);
    }
    let mut slices = [&data[..0]; 4];
    let mut pos = 0;
    for s in &mut slices {
        if data.len() - pos < 3 {
            return Err(DecodeError::BadSize);
        }
        let size = u24(&data[pos..]);
        if size < 3 || size > data.len() - pos {
            return Err(DecodeError::BadSize);
        }
        *s = &data[pos + 3..pos + size];
        pos += size;
    }
    Ok(slices)
}

pub(super) fn decode_legacy_field(
    data: &[u8],
    canvas: &mut Canvas,
    quality: u8,
    w: usize,
    h: usize,
) -> Result<(), DecodeError> {
    let slices = legacy_slices(data)?;
    let rows = h.div_ceil(16);
    let regular_cols = w / 16;
    let decode_slice = |s: usize, canvas: &mut Canvas| -> Result<(), DecodeError> {
        let mut ctx = BlockCtx::new(quality, false);
        let mut r = BitReader::new(slices[s]);
        for row in (s..rows).step_by(4) {
            ctx.reset();
            for m in 0..regular_cols {
                decode_legacy_mb(&mut r, canvas, &mut ctx, m, row)?;
            }
        }
        if s == 3 && !w.is_multiple_of(16) {
            for row in 0..rows {
                ctx.reset();
                decode_legacy_mb(&mut r, canvas, &mut ctx, regular_cols, row)?;
            }
        }
        Ok(())
    };
    if worker_count(w, rows) == 1 {
        return (0..4).try_for_each(|s| decode_slice(s, canvas));
    }
    // The four slices are independent: decode each into its own canvas, then take each
    // slice's rows (and slice 3's border column) from it.
    let (fourcc, track_ac) = (canvas.fourcc, canvas.ac.is_some());
    let parts: Vec<Result<Canvas, DecodeError>> = crate::pool::par_map(4, |s| {
        let mut c = Canvas::new(fourcc, w, h, track_ac);
        decode_slice(s, &mut c).map(|_| c)
    });
    let parts = parts.into_iter().collect::<Result<Vec<_>, _>>()?;
    for row in 0..rows {
        canvas.blit_rows(&parts[row % 4], row, row, 1);
    }
    if !w.is_multiple_of(16) {
        canvas.blit_col(&parts[3], regular_cols);
    }
    Ok(())
}

pub(super) fn decode_legacy_mb(
    r: &mut BitReader,
    canvas: &mut Canvas,
    ctx: &mut BlockCtx,
    m: usize,
    row: usize,
) -> Result<(), DecodeError> {
    let (colour, alpha) = canvas.fourcc.blocks();
    let mut buf = [0u8; 64];
    for &(pl, dx, dy) in colour.iter().chain(alpha) {
        let ac = ctx.block(r, pl, BlockKind::Full, &mut buf)?;
        let (x0, y0) = canvas.mb_origin(pl, m, row);
        canvas.put(pl, x0 + dx, y0 + dy, &buf, ac);
    }
    Ok(())
}
