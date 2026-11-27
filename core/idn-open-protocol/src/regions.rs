//! Changed regions only (`regions/1`, spec/extensions/idn-extensions.md §E.15): a static
//! slide with a clock in one corner costs the clock's area per frame, not the picture.
//!
//! The sender compares each picture with the one it last sent that receiver in 16×16
//! blocks and sends the changed rectangles, each its own SpeedHQ picture; the receiver
//! pastes them into its reference picture.

use crate::shq::{Plane, Plane16};
use std::time::Duration;

/// The token a sender with the option on lists in `x_idn_ext` (§E.15.1).
pub const TOKEN: &str = "regions/1";
/// The video FourCC of a region update (§E.15.2).
pub const FOURCC: [u8; 4] = *b"IDNR";
/// A whole frame at least this often (§E.15.3).
pub const REFRESH: Duration = Duration::from_secs(2);
/// A receiver without a usable reference asks for a whole frame (§E.15.4).
pub const REFRESH_REQUEST: &str = "<x_idn_refresh/>";
const BLOCK: usize = 16;

/// A rectangle in luma samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// The changed blocks of a picture, merged into rectangles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub rects: Vec<Rect>,
    /// Changed 16×16 blocks, and blocks in all.
    pub changed: usize,
    pub blocks: usize,
}

impl Diff {
    /// More than half the picture changed: a whole frame is cheaper (§E.15.3).
    pub fn mostly_changed(&self) -> bool {
        self.changed * 2 > self.blocks
    }
}

/// A plane's subsampling relative to a `w` × `h` picture: divisors 1 or 2.
fn sub(pw: usize, ph: usize, w: usize, h: usize) -> (usize, usize) {
    (if pw < w { 2 } else { 1 }, if ph < h { 2 } else { 1 })
}

/// The part of a plane covering luma rectangle `r`: (x0, y0, width, height).
fn span(pw: usize, ph: usize, w: usize, h: usize, r: Rect) -> (usize, usize, usize, usize) {
    let (sx, sy) = sub(pw, ph, w, h);
    let (x0, y0) = (r.x / sx, r.y / sy);
    let x1 = (r.x + r.w).div_ceil(sx).min(pw);
    let y1 = (r.y + r.h).div_ceil(sy).min(ph);
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

fn block_changed(old: &[Plane], new: &[Plane], w: usize, h: usize, bx: usize, by: usize) -> bool {
    let r = Rect {
        x: bx * BLOCK,
        y: by * BLOCK,
        w: BLOCK.min(w - bx * BLOCK),
        h: BLOCK.min(h - by * BLOCK),
    };
    old.iter().zip(new).any(|(a, b)| {
        let (x0, y0, sw, sh) = span(a.width, a.height, w, h, r);
        (y0..y0 + sh).any(|y| {
            let i = y * a.width + x0;
            a.data[i..i + sw] != b.data[i..i + sw]
        })
    })
}

/// Compares `new` with `old` (the same layout) in 16×16 blocks of a `w` × `h` picture:
/// runs of changed blocks in each block row, joined with identical runs below (§E.15.3).
/// `None` when the layouts differ.
pub fn changed(old: &[Plane], new: &[Plane], w: usize, h: usize) -> Option<Diff> {
    let same = old.len() == new.len()
        && old.iter().zip(new).all(|(a, b)| {
            (a.width, a.height) == (b.width, b.height)
                && a.data.len() == a.width * a.height
                && b.data.len() == b.width * b.height
                && a.width <= w
                && a.height <= h
        });
    if !same || w == 0 || h == 0 {
        return None;
    }
    let (cols, rows) = (w.div_ceil(BLOCK), h.div_ceil(BLOCK));
    let mut changed = 0;
    // Open rectangles (in blocks) ending at the previous block row.
    let mut open: Vec<Rect> = Vec::new();
    let mut done = Vec::new();
    for by in 0..rows {
        let mut next = Vec::new();
        // Each block compared once; runs of changed blocks become rectangles.
        let mut bx = 0;
        while bx < cols {
            if !block_changed(old, new, w, h, bx, by) {
                bx += 1;
                continue;
            }
            let start = bx;
            bx += 1;
            while bx < cols && block_changed(old, new, w, h, bx, by) {
                bx += 1;
            }
            changed += bx - start;
            let run = (start, bx - start);
            match open.iter().position(|r| (r.x, r.w) == run) {
                Some(i) => {
                    let mut r = open.swap_remove(i);
                    r.h += 1;
                    next.push(r);
                }
                None => next.push(Rect {
                    x: run.0,
                    y: by,
                    w: run.1,
                    h: 1,
                }),
            }
        }
        done.append(&mut open);
        open = next;
    }
    done.append(&mut open);
    done.sort_by_key(|r| (r.y, r.x));
    // More rectangles than the count's 16 bits hold (a checkerboard of changes above 8K):
    // the whole picture as one (the count wrapped to 0).
    if done.len() > u16::MAX as usize {
        done = vec![Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        }];
        changed = cols * rows;
    }
    let rects = done
        .into_iter()
        .map(|r| {
            let (x, y) = (r.x * BLOCK, r.y * BLOCK);
            Rect {
                x,
                y,
                w: ((r.x + r.w) * BLOCK).min(w) - x,
                h: ((r.y + r.h) * BLOCK).min(h) - y,
            }
        })
        .collect();
    Some(Diff {
        rects,
        changed,
        blocks: cols * rows,
    })
}

/// The planes of rectangle `r` of a `w` × `h` picture, chroma keeping its subsampling.
pub fn crop(planes: &[Plane], w: usize, h: usize, r: Rect) -> Vec<Plane> {
    planes
        .iter()
        .map(|p| {
            let (x0, y0, cw, ch) = span(p.width, p.height, w, h, r);
            let mut data = Vec::with_capacity(cw * ch);
            for y in y0..y0 + ch {
                data.extend_from_slice(&p.data[y * p.width + x0..y * p.width + x0 + cw]);
            }
            Plane {
                width: cw,
                height: ch,
                data,
            }
        })
        .collect()
}

/// A region update's payload (§E.15.2): the regions' coding, then each rectangle with
/// its SpeedHQ payload.
pub fn payload(base: [u8; 4], regions: &[(Rect, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + regions.iter().map(|(_, p)| 12 + p.len()).sum::<usize>());
    out.extend_from_slice(&base);
    out.extend_from_slice(&(regions.len() as u16).to_le_bytes());
    for (r, p) in regions {
        for v in [r.x, r.y, r.w, r.h] {
            out.extend_from_slice(&(v as u16).to_le_bytes());
        }
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(p);
    }
    out
}

/// A parsed region update.
#[derive(Debug)]
pub struct Update<'a> {
    /// The regions' FourCC: the picture's format.
    pub base: [u8; 4],
    pub regions: Vec<(Rect, &'a [u8])>,
}

impl<'a> Update<'a> {
    /// Parses a region update for a `w` × `h` picture. Rectangles start on the 16-sample
    /// grid, lie inside the picture and are multiples of 16 except at its edges.
    pub fn parse(p: &'a [u8], w: usize, h: usize) -> Result<Self, &'static str> {
        let mut rest = p;
        let mut take = |n: usize| -> Result<&'a [u8], &'static str> {
            if rest.len() < n {
                return Err("truncated");
            }
            let (a, b) = rest.split_at(n);
            rest = b;
            Ok(a)
        };
        let base: [u8; 4] = take(4)?.try_into().unwrap();
        let n = u16::from_le_bytes(take(2)?.try_into().unwrap());
        // Each region takes at least 12 bytes: the count can't claim more than that.
        let mut regions = Vec::with_capacity((n as usize).min(p.len() / 12));
        for _ in 0..n {
            let f = take(8)?;
            let v = |i: usize| u16::from_le_bytes([f[i], f[i + 1]]) as usize;
            let r = Rect {
                x: v(0),
                y: v(2),
                w: v(4),
                h: v(6),
            };
            let fits = |at: usize, len: usize, max: usize| {
                at.is_multiple_of(BLOCK)
                    && len > 0
                    && at + len <= max
                    && (len.is_multiple_of(BLOCK) || at + len == max)
            };
            if !fits(r.x, r.w, w) || !fits(r.y, r.h, h) {
                return Err("region outside the picture's 16-sample grid");
            }
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            regions.push((r, take(len)?));
        }
        if !rest.is_empty() {
            return Err("trailing bytes");
        }
        Ok(Update { base, regions })
    }
}

fn paste_plane<T: Copy>(dst: &mut [T], dw: usize, src: &[T], sw: usize, at: (usize, usize)) {
    for (row, s) in src.chunks_exact(sw.max(1)).enumerate() {
        let i = (at.1 + row) * dw + at.0;
        dst[i..i + sw].copy_from_slice(s);
    }
}

/// Copies a decoded rectangle (`src`, from [`crop`]-shaped planes) into a `w` × `h`
/// picture. `false` (and nothing copied) when the shapes don't match.
pub fn paste(dst: &mut [Plane], w: usize, h: usize, r: Rect, src: &[Plane]) -> bool {
    let fits = dst.len() == src.len()
        && dst.iter().zip(src).all(|(d, s)| {
            let (_, _, cw, ch) = span(d.width, d.height, w, h, r);
            (s.width, s.height) == (cw, ch) && s.data.len() == cw * ch
        });
    if fits {
        for (d, s) in dst.iter_mut().zip(src) {
            let (x0, y0, ..) = span(d.width, d.height, w, h, r);
            paste_plane(&mut d.data, d.width, &s.data, s.width, (x0, y0));
        }
    }
    fits
}

/// [`paste`] for high-precision planes.
pub fn paste16(dst: &mut [Plane16], w: usize, h: usize, r: Rect, src: &[Plane16]) -> bool {
    let fits = dst.len() == src.len()
        && dst.iter().zip(src).all(|(d, s)| {
            let (_, _, cw, ch) = span(d.width, d.height, w, h, r);
            (s.width, s.height) == (cw, ch) && s.data.len() == cw * ch
        });
    if fits {
        for (d, s) in dst.iter_mut().zip(src) {
            let (x0, y0, ..) = span(d.width, d.height, w, h, r);
            paste_plane(&mut d.data, d.width, &s.data, s.width, (x0, y0));
        }
    }
    fits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4:2:2 picture: luma `w` × `h`, chroma half width.
    fn picture(w: usize, h: usize, v: u8) -> Vec<Plane> {
        let p = |pw: usize| Plane {
            width: pw,
            height: h,
            data: vec![v; pw * h],
        };
        vec![p(w), p(w.div_ceil(2)), p(w.div_ceil(2))]
    }

    fn set(planes: &mut [Plane], plane: usize, x: usize, y: usize, v: u8) {
        let p = &mut planes[plane];
        p.data[y * p.width + x] = v;
    }

    #[test]
    fn changed_blocks_merge_into_rectangles() {
        let old = picture(100, 70, 16);
        let mut new = old.clone();
        // A 2×2-block patch at blocks (1..3, 1..3), one chroma sample in block (5, 4)
        // (the last, partial column and row: 100 = 6·16 + 4, 70 = 4·16 + 6).
        for (x, y) in [(16, 16), (47, 47), (40, 20), (20, 40)] {
            set(&mut new, 0, x, y, 200);
        }
        set(&mut new, 2, 49, 69, 90); // chroma x 49 = luma 98..99
        let d = changed(&old, &new, 100, 70).unwrap();
        assert_eq!(d.changed, 5);
        assert_eq!(d.blocks, 7 * 5);
        assert_eq!(
            d.rects,
            [
                Rect {
                    x: 16,
                    y: 16,
                    w: 32,
                    h: 32
                },
                Rect {
                    x: 96,
                    y: 64,
                    w: 4,
                    h: 6
                },
            ]
        );
        assert!(!d.mostly_changed());
        assert_eq!(changed(&old, &old, 100, 70).unwrap().rects, []);
        assert!(changed(&old, &picture(98, 70, 16), 100, 70).is_none());
    }

    #[test]
    fn crop_and_paste_round_trip_and_the_payload_parses() {
        let (w, h) = (100, 70);
        let mut src = picture(w, h, 16);
        for (i, v) in src[0].data.iter_mut().enumerate() {
            *v = (i % 251) as u8;
        }
        for (i, v) in src[1].data.iter_mut().enumerate() {
            *v = (i % 241) as u8;
        }
        let r = Rect {
            x: 96,
            y: 48,
            w: 4,
            h: 22,
        };
        let part = crop(&src, w, h, r);
        assert_eq!((part[1].width, part[1].height), (2, 22));
        let mut dst = picture(w, h, 16);
        assert!(paste(&mut dst, w, h, r, &part));
        assert_eq!(crop(&dst, w, h, r), part);
        assert!(!paste(&mut dst, w, h, Rect { x: 80, w: 16, ..r }, &part));

        let p = payload(*b"SHQ2", &[(r, vec![1, 2, 3])]);
        let u = Update::parse(&p, w, h).unwrap();
        assert_eq!((u.base, u.regions.len()), (*b"SHQ2", 1));
        assert_eq!(u.regions[0], (r, &[1u8, 2, 3][..]));
        assert_eq!(
            Update::parse(&p, 99, h).unwrap_err(),
            "region outside the picture's 16-sample grid"
        );
        assert!(Update::parse(&p[..p.len() - 1], w, h).is_err());
        let off_grid = payload(*b"SHQ2", &[(Rect { x: 8, ..r }, vec![])]);
        assert!(Update::parse(&off_grid, w, h).is_err());
        let empty = payload(*b"SHQ2", &[]);
        assert!(Update::parse(&empty, w, h).unwrap().regions.is_empty());
    }
}
