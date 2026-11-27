//! Canvas: padded planes covering whole MBs, with bounded block writes (§3.12.4 #3).

use super::*;

pub(super) struct Canvas {
    pub(super) fourcc: Fourcc,
    pub(super) planes: Vec<Plane>,
    /// Cropped (visible) size of each plane.
    pub(super) crop: Vec<(usize, usize)>,
    pub(super) ac: Option<Vec<Vec<u8>>>,
    /// Samples with 3 fraction bits (§3.9.5), when decoding precisely.
    pub(super) hi: Option<Vec<Vec<u16>>>,
    /// 1 for a half-size picture ([`decode_half`]): blocks are 4 × 4, positions halved.
    pub(super) shift: usize,
}

impl Canvas {
    pub(super) fn new(fourcc: Fourcc, w: usize, h: usize, track_ac: bool) -> Self {
        Self::with_precise(fourcc, w, h, track_ac, false)
    }

    /// The visible size of each plane of a `w` × `h` picture.
    pub(super) fn crop_dims(fourcc: Fourcc, w: usize, h: usize) -> Vec<(usize, usize)> {
        let chroma_crop = match fourcc {
            Fourcc::Shq0 => (w.div_ceil(2), h.div_ceil(2)),
            Fourcc::Shq2 | Fourcc::Shq7 => (w.div_ceil(2), h),
            Fourcc::Shq4 | Fourcc::Shq9 => (w, h),
        };
        let mut crop = vec![(w, h), chroma_crop, chroma_crop];
        if fourcc.has_alpha() {
            crop.push((w, h));
        }
        crop
    }

    pub(super) fn with_precise(
        fourcc: Fourcc,
        w: usize,
        h: usize,
        track_ac: bool,
        precise: bool,
    ) -> Self {
        Self::scaled(fourcc, w, h, track_ac, precise, 0, false)
    }

    /// A canvas for a `w` × `h` picture decoded at full size (`shift` 0) or half size
    /// (`shift` 1, without AC masks or fraction bits). `overwritten`: every block of it
    /// is written (the new format writes every MB of every row, skipped ones too), so
    /// it needn't start zeroed.
    pub(super) fn scaled(
        fourcc: Fourcc,
        w: usize,
        h: usize,
        track_ac: bool,
        precise: bool,
        shift: usize,
        overwritten: bool,
    ) -> Self {
        let (cols, rows) = (w.div_ceil(16), h.div_ceil(16));
        let (cmw, cmh) = fourcc.chroma_mb();
        let luma = ((16 * cols) >> shift, (16 * rows) >> shift);
        let chroma = ((cmw * cols) >> shift, (cmh * rows) >> shift);
        let mut dims = vec![luma, chroma, chroma];
        if fourcc.has_alpha() {
            dims.push(luma);
        }
        let crop = Self::crop_dims(fourcc, w.div_ceil(1 << shift), h.div_ceil(1 << shift));
        // An overwritten canvas (the new format) is as wide as the picture: the blocks
        // of the last MB column write only their visible samples, so the planes are the
        // picture's whatever its width (padded to whole MBs, a width not a multiple of 16
        // was copied out of the canvas into a fresh picture, 105,000 page faults a second
        // at 3838×2160). Its height stays whole MBs, the padding rows cut off after.
        if overwritten {
            for (d, c) in dims.iter_mut().zip(&crop) {
                d.0 = c.0;
            }
        }
        let (track_ac, precise) = (track_ac && shift == 0, precise && shift == 0);
        let planes: Vec<Plane> = dims
            .iter()
            .map(|&(pw, ph)| Plane {
                width: pw,
                height: ph,
                data: scratch::BYTES.take(pw * ph, overwritten),
            })
            .collect();
        let ac = track_ac.then(|| {
            planes
                .iter()
                .map(|p| scratch::BYTES.take(p.data.len(), overwritten))
                .collect()
        });
        let hi = precise.then(|| {
            planes
                .iter()
                .map(|p| scratch::WORDS.take(p.data.len(), overwritten))
                .collect()
        });
        Canvas {
            fourcc,
            planes,
            crop,
            ac,
            hi,
            shift,
        }
    }

    /// Origin of MB (m, r) in plane `pl`.
    pub(super) fn mb_origin(&self, pl: usize, m: usize, r: usize) -> (usize, usize) {
        if pl == 1 || pl == 2 {
            let (cw, ch) = self.fourcc.chroma_mb();
            (cw * m, ch * r)
        } else {
            (16 * m, 16 * r)
        }
    }

    pub(super) fn put(&mut self, pl: usize, x0: usize, y0: usize, block: &[u8; 64], has_ac: bool) {
        self.put_hi(pl, x0, y0, block, has_ac, None);
    }

    pub(super) fn put_hi(
        &mut self,
        pl: usize,
        x0: usize,
        y0: usize,
        block: &[u8; 64],
        has_ac: bool,
        hi_block: Option<&[u16; 64]>,
    ) {
        let width = self.planes[pl].width;
        put_block(
            &mut self.planes[pl].data,
            width,
            self.ac.as_mut().map(|a| a[pl].as_mut_slice()),
            self.hi.as_mut().map(|h| h[pl].as_mut_slice()),
            self.shift,
            (x0, y0),
            block,
            has_ac,
            hi_block,
        );
    }

    /// The canvas as bands of `per` MB rows, top first, each with its own planes' rows
    /// (for decoding the bands in parallel straight into it).
    pub(super) fn bands(&mut self, per: usize) -> Vec<Band<'_>> {
        let (fourcc, shift) = (self.fourcc, self.shift);
        let heights: Vec<usize> = (0..self.planes.len())
            .map(|pl| self.mb_height(pl))
            .collect();
        let n = self.planes[0].height.div_ceil(per * heights[0]);
        let mut bands: Vec<Band<'_>> = (0..n)
            .map(|_| Band {
                fourcc,
                shift,
                planes: Vec::new(),
                ac: None,
                hi: None,
            })
            .collect();
        for (pl, p) in self.planes.iter_mut().enumerate() {
            for (b, rows) in bands
                .iter_mut()
                .zip(p.data.chunks_mut(per * heights[pl] * p.width))
            {
                b.planes.push((rows, p.width));
            }
        }
        if let Some(ac) = &mut self.ac {
            for (pl, a) in ac.iter_mut().enumerate() {
                let w = bands[0].planes[pl].1;
                for (b, rows) in bands.iter_mut().zip(a.chunks_mut(per * heights[pl] * w)) {
                    b.ac.get_or_insert_with(Vec::new).push(rows);
                }
            }
        }
        if let Some(hi) = &mut self.hi {
            for (pl, h) in hi.iter_mut().enumerate() {
                let w = bands[0].planes[pl].1;
                for (b, rows) in bands.iter_mut().zip(h.chunks_mut(per * heights[pl] * w)) {
                    b.hi.get_or_insert_with(Vec::new).push(rows);
                }
            }
        }
        bands
    }

    /// Sample lines per MB row in plane `pl`.
    pub(super) fn mb_height(&self, pl: usize) -> usize {
        (if pl == 1 || pl == 2 {
            self.fourcc.chroma_mb().1
        } else {
            16
        }) >> self.shift
    }

    /// Copies `n` MB rows of `src`, starting at its MB row `src_row`, to MB row `dst_row`.
    /// Both canvases have the same FourCC and width.
    pub(super) fn blit_rows(&mut self, src: &Canvas, dst_row: usize, src_row: usize, n: usize) {
        for pl in 0..self.planes.len() {
            let mh = self.mb_height(pl);
            let w = self.planes[pl].width;
            let (d, s, len) = (dst_row * mh * w, src_row * mh * w, n * mh * w);
            self.planes[pl].data[d..d + len].copy_from_slice(&src.planes[pl].data[s..s + len]);
            if let (Some(da), Some(sa)) = (&mut self.ac, &src.ac) {
                da[pl][d..d + len].copy_from_slice(&sa[pl][s..s + len]);
            }
            if let (Some(dh), Some(sh)) = (&mut self.hi, &src.hi) {
                dh[pl][d..d + len].copy_from_slice(&sh[pl][s..s + len]);
            }
        }
    }

    /// Copies MB column `col`, over all MB rows, from `src` (same geometry).
    pub(super) fn blit_col(&mut self, src: &Canvas, col: usize) {
        for pl in 0..self.planes.len() {
            let (x0, _) = self.mb_origin(pl, col, 0);
            let mw = self.mb_origin(pl, 1, 0).0;
            let w = self.planes[pl].width;
            for y in 0..self.planes[pl].height {
                let o = y * w + x0;
                self.planes[pl].data[o..o + mw].copy_from_slice(&src.planes[pl].data[o..o + mw]);
                if let (Some(da), Some(sa)) = (&mut self.ac, &src.ac) {
                    da[pl][o..o + mw].copy_from_slice(&sa[pl][o..o + mw]);
                }
                if let (Some(dh), Some(sh)) = (&mut self.hi, &src.hi) {
                    dh[pl][o..o + mw].copy_from_slice(&sh[pl][o..o + mw]);
                }
            }
        }
    }

    pub(super) fn finish(self) -> (Vec<Plane>, Vec<Plane>) {
        let (p, m, _) = self.finish_hi();
        (p, m)
    }

    pub(super) fn finish_hi(self) -> (Vec<Plane>, Vec<Plane>, Option<Vec<Plane16>>) {
        let crop = self.crop.clone();
        Self::finish_parts(vec![self], &crop)
    }

    /// The visible planes from canvases covering consecutive MB rows, top first, each
    /// sample copied once (or not at all: a single canvas whose padded width is the
    /// visible width just loses its padding rows).
    pub(super) fn finish_parts(
        mut parts: Vec<Canvas>,
        crop: &[(usize, usize)],
    ) -> (Vec<Plane>, Vec<Plane>, Option<Vec<Plane16>>) {
        // The parts' buffers go back to `pool` once copied out (a single part as wide as
        // the picture becomes it).
        fn join<T: Copy + Default>(
            parts: &mut [Vec<T>],
            pw: usize,
            (cw, ch): (usize, usize),
            pool: &scratch::Pool<T>,
        ) -> Vec<T> {
            if parts.len() == 1 && pw == cw {
                let mut v = std::mem::take(&mut parts[0]);
                v.truncate(cw * ch);
                return v;
            }
            let mut out = Vec::with_capacity(cw * ch);
            'rows: for d in parts.iter() {
                for row in d.chunks_exact(pw) {
                    if out.len() == cw * ch {
                        break 'rows;
                    }
                    out.extend_from_slice(&row[..cw]);
                }
            }
            for d in parts.iter_mut() {
                pool.give(std::mem::take(d));
            }
            out
        }
        let n = crop.len();
        let widths: Vec<usize> = parts[0].planes.iter().map(|p| p.width).collect();
        let take = |parts: &mut Vec<Canvas>, pl: usize| -> Vec<Vec<u8>> {
            parts
                .iter_mut()
                .map(|c| std::mem::take(&mut c.planes[pl].data))
                .collect()
        };
        let mut pics = Vec::with_capacity(n);
        for (pl, &c) in crop.iter().enumerate() {
            let mut d = take(&mut parts, pl);
            pics.push(Plane {
                width: c.0,
                height: c.1,
                data: join(&mut d, widths[pl], c, &scratch::BYTES),
            });
        }
        let masks = if parts[0].ac.is_some() {
            (0..n)
                .map(|pl| {
                    let mut d: Vec<Vec<u8>> = parts
                        .iter_mut()
                        .map(|c| std::mem::take(&mut c.ac.as_mut().unwrap()[pl]))
                        .collect();
                    Plane {
                        width: crop[pl].0,
                        height: crop[pl].1,
                        data: join(&mut d, widths[pl], crop[pl], &scratch::BYTES),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let hi = parts[0].hi.is_some().then(|| {
            (0..n)
                .map(|pl| {
                    let mut d: Vec<Vec<u16>> = parts
                        .iter_mut()
                        .map(|c| std::mem::take(&mut c.hi.as_mut().unwrap()[pl]))
                        .collect();
                    Plane16 {
                        width: crop[pl].0,
                        height: crop[pl].1,
                        data: join(&mut d, widths[pl], crop[pl], &scratch::WORDS),
                    }
                })
                .collect()
        });
        (pics, masks, hi)
    }
}

/// Writes a reconstructed block at (`x0`, `y0`) of a padded plane `width` samples wide:
/// 8 × 8, or 4 × 4 at half the position for a half-size picture (`shift` 1); its AC flag
/// into `ac` and its fraction-bit samples into `hi` when kept.
#[allow(clippy::too_many_arguments)]
pub(super) fn put_block(
    data: &mut [u8],
    width: usize,
    ac: Option<&mut [u8]>,
    hi: Option<&mut [u16]>,
    shift: usize,
    (x0, y0): (usize, usize),
    block: &[u8; 64],
    has_ac: bool,
    hi_block: Option<&[u16; 64]>,
) {
    // (A block outside the plane is caught by the slices below; working out the plane's
    // height for a check here, a division per block, was a few % of a 2160p receive.)
    // A plane as wide as the picture: a block at its right edge keeps its visible
    // columns (`n`), one past it none.
    if shift == 1 {
        // A 4 × 4 block (`block[4·y + x]`) at half the position.
        let (x0, y0) = (x0 / 2, y0 / 2);
        let n = 4.min(width.saturating_sub(x0));
        debug_assert!(
            y0 + 4 <= data.len() / width.max(1),
            "block outside padded plane"
        );
        if n == 0 {
            return;
        }
        for y in 0..4 {
            let o = (y0 + y) * width + x0;
            data[o..o + n].copy_from_slice(&block[4 * y..4 * y + n]);
        }
        return;
    }
    let n = 8.min(width.saturating_sub(x0));
    debug_assert!(
        y0 + 8 <= data.len() / width.max(1),
        "block outside padded plane"
    );
    if n == 0 {
        return;
    }
    let (mut ac, mut hi) = (ac, hi.zip(hi_block));
    if n == 8 && ac.is_none() && hi.is_none() {
        // The usual block: whole rows of a constant length, moved inline (a length known
        // only at run time made each row a memcpy call: +15% of a core at 2160p), checked
        // once for all eight (a check, a slice and a multiply a row were most of this
        // function's time, 27% of a 1080p receive).
        let start = y0 * width + x0;
        if width >= 8 && start + 7 * width + 8 <= data.len() {
            // SAFETY: the eight rows start..start+8, +width each, end at or before the end
            // of `data` (checked above); `block` has 64 bytes; they don't overlap.
            unsafe {
                let to = data.as_mut_ptr().add(start);
                for y in 0..8 {
                    std::ptr::copy_nonoverlapping(block.as_ptr().add(8 * y), to.add(y * width), 8);
                }
            }
            return;
        }
        for y in 0..8 {
            let o = (y0 + y) * width + x0;
            data[o..o + 8].copy_from_slice(&block[8 * y..8 * y + 8]);
        }
        return;
    }
    for y in 0..8 {
        let o = (y0 + y) * width + x0;
        data[o..o + n].copy_from_slice(&block[8 * y..8 * y + n]);
        if let Some(ac) = ac.as_deref_mut() {
            ac[o..o + n].fill(has_ac as u8);
        }
        if let Some((hi, b)) = hi.as_mut() {
            hi[o..o + n].copy_from_slice(&b[8 * y..8 * y + n]);
        }
    }
}

/// A band of MB rows of a [`Canvas`], borrowed: the planes' rows (with each plane's
/// width), and the AC masks' and fraction bits' when kept. A band's MB row 0 is its first.
pub(super) struct Band<'a> {
    pub(super) fourcc: Fourcc,
    pub(super) shift: usize,
    pub(super) planes: Vec<(&'a mut [u8], usize)>,
    pub(super) ac: Option<Vec<&'a mut [u8]>>,
    pub(super) hi: Option<Vec<&'a mut [u16]>>,
}

impl Band<'_> {
    /// Brings MB row `i` of `n` into this core's cache before its blocks are written: its
    /// lines cleared in one pass. Written block by block, 8 short rows a picture-width
    /// apart, without it each block's rows missed to memory: a 1080p receiver took 30% of
    /// a core instead of 21% when a whole-picture clear (lab4h's memset, kept cache-warm)
    /// was taken away.
    pub(super) fn warm_row(&mut self, i: usize, n: usize) {
        for (data, w) in &mut self.planes {
            let lines = data.len() / (*w).max(1) / n.max(1);
            let a = i * lines * *w;
            data[a..a + lines * *w].fill(0);
        }
        let widths: Vec<usize> = self.planes.iter().map(|p| p.1).collect();
        for (pl, d) in self.ac.iter_mut().flatten().enumerate() {
            let lines = d.len() / widths[pl].max(1) / n.max(1);
            let a = i * lines * widths[pl];
            d[a..a + lines * widths[pl]].fill(0);
        }
        for (pl, d) in self.hi.iter_mut().flatten().enumerate() {
            let lines = d.len() / widths[pl].max(1) / n.max(1);
            let a = i * lines * widths[pl];
            d[a..a + lines * widths[pl]].fill(0);
        }
    }

    pub(super) fn mb_origin(&self, pl: usize, m: usize, r: usize) -> (usize, usize) {
        if pl == 1 || pl == 2 {
            let (cw, ch) = self.fourcc.chroma_mb();
            (cw * m, ch * r)
        } else {
            (16 * m, 16 * r)
        }
    }

    pub(super) fn put_hi(
        &mut self,
        pl: usize,
        x0: usize,
        y0: usize,
        block: &[u8; 64],
        has_ac: bool,
        hi_block: Option<&[u16; 64]>,
    ) {
        let (data, width) = &mut self.planes[pl];
        put_block(
            data,
            *width,
            self.ac.as_mut().map(|a| &mut *a[pl]),
            self.hi.as_mut().map(|h| &mut *h[pl]),
            self.shift,
            (x0, y0),
            block,
            has_ac,
            hi_block,
        );
    }
}

impl Drop for Canvas {
    /// What is left of its buffers (not taken into a picture) goes back to the pool.
    fn drop(&mut self) {
        for mut p in self.planes.drain(..) {
            scratch::BYTES.give(std::mem::take(&mut p.data));
        }
        for v in self.ac.take().into_iter().flatten() {
            scratch::BYTES.give(v);
        }
        for v in self.hi.take().into_iter().flatten() {
            scratch::WORDS.give(v);
        }
    }
}
