//! The compositor on the CPU (spec/extensions/idn-switcher.md §S.4, §S.5, §S.9): the
//! program bus with its transition, then each overlay on air, then fade to black. Every
//! operation works plane by plane in bands of rows on scoped threads.

use crate::frame::{Frame, Ycc};

/// A transition's kind (§S.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Cut,
    /// A dissolve.
    Mix,
    /// The new picture comes in from one side (or grows from the centre), with a soft
    /// edge of `soft` (0–0.2 of the frame).
    Wipe(Direction, f32),
    /// Dip through black: the program fades out, then the preview in.
    Fade,
    /// A luma wipe (§S.14): the preview shows where input `pattern`'s picture (greyscale)
    /// is darker than the position (lighter, `invert`ed), with a soft edge of `soft`.
    Luma {
        pattern: u32,
        soft: f32,
        invert: bool,
    },
    /// A stinger (§S.14): stinger slot 1–8's clip over the bus; program and preview swap
    /// at its cut point.
    Stinger(u8),
    /// The preview grows from the centre over the program, eased in and out, until it
    /// fills the frame ([`zoom_rect`]).
    Zoom,
}

/// Where a zoom's preview is at `t` in a `w` × `h` frame: x (even), y, width, height,
/// centred, its size eased in and out (smoothstep) from nothing to the whole frame.
pub fn zoom_rect(w: usize, h: usize, t: f32) -> (usize, usize, usize, usize) {
    let t = t.clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    let zw = (((w as f32 * s).round() as usize) & !1).min(w);
    let zh = ((h as f32 * s).round() as usize).min(h);
    (((w - zw) / 2) & !1, (h - zh) / 2, zw, zh)
}

/// Where a wipe's new picture comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From the left edge.
    Left,
    Right,
    /// From the top.
    Down,
    /// From the bottom.
    Up,
    /// A circle growing from the centre.
    Circle,
}

impl Kind {
    /// `cut`, `mix`, `wipe-left`…, `fade`.
    pub fn name(self) -> String {
        match self {
            Kind::Cut => "cut".into(),
            Kind::Mix => "mix".into(),
            Kind::Fade => "fade".into(),
            Kind::Luma { .. } => "luma".into(),
            Kind::Stinger(n) => format!("stinger-{n}"),
            Kind::Zoom => "zoom".into(),
            Kind::Wipe(d, _) => format!(
                "wipe-{}",
                match d {
                    Direction::Left => "left",
                    Direction::Right => "right",
                    Direction::Down => "down",
                    Direction::Up => "up",
                    Direction::Circle => "circle",
                }
            ),
        }
    }

    /// From [`name`](Self::name), with the wipe's soft edge.
    pub fn parse(s: &str, soft: f32) -> Option<Kind> {
        let soft = soft.clamp(0.0, 0.2);
        Some(match s {
            "cut" => Kind::Cut,
            "mix" => Kind::Mix,
            "fade" => Kind::Fade,
            "wipe-left" | "wipe" => Kind::Wipe(Direction::Left, soft),
            "wipe-right" => Kind::Wipe(Direction::Right, soft),
            "wipe-down" => Kind::Wipe(Direction::Down, soft),
            "wipe-up" => Kind::Wipe(Direction::Up, soft),
            "wipe-circle" => Kind::Wipe(Direction::Circle, soft),
            "zoom" => Kind::Zoom,
            s if s.starts_with("stinger-") => {
                let n: u8 = s["stinger-".len()..].parse().ok()?;
                if !(1..=8).contains(&n) {
                    return None;
                }
                Kind::Stinger(n)
            }
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum P {
    Y,
    Cb,
    Cr,
}

fn plane(f: &Frame, p: P) -> &[u8] {
    match p {
        P::Y => &f.y,
        P::Cb => &f.cb,
        P::Cr => &f.cr,
    }
}

/// The level black has in a plane.
fn black(p: P) -> u8 {
    if p == P::Y { Ycc::BLACK.0 } else { 128 }
}

/// Bands to split a plane of `samples` into: enough to finish a 2160p layer in a few ms,
/// few enough that a busy machine (decoders, a browser) doesn't hold the frame up waiting
/// for the slowest of many short threads.
fn threads(samples: usize) -> usize {
    let cores = crate::par::threads();
    if samples > 1920 * 1080 {
        cores.min(8)
    } else {
        (cores / 2).clamp(1, 4)
    }
}

/// Runs `f(plane, first row, rows)` over each plane of `out` in bands of rows, in
/// parallel; `rows` is that band of the plane, row after row.
fn each_plane(out: &mut Frame, f: &(dyn Fn(P, usize, &mut [u8]) + Sync)) {
    let (w, h) = (out.width, out.height);
    let cw = Frame::chroma_width(w);
    let bands = if w * h >= 320 * 180 {
        threads(w * h)
    } else {
        1
    };
    let rows_per = h.div_ceil(bands).max(1);
    crate::par::scope(|s| {
        for (p, data, pw) in [
            (P::Y, &mut out.y, w),
            (P::Cb, &mut out.cb, cw),
            (P::Cr, &mut out.cr, cw),
        ] {
            for (i, band) in data.chunks_mut(rows_per * pw).enumerate() {
                s.spawn(move || f(p, i * rows_per, band));
            }
        }
    });
}

/// `a` + (`b` − `a`) × `w` / 256.
#[inline]
fn lerp(a: u8, b: u8, w: u32) -> u8 {
    ((a as u32 * (256 - w) + b as u32 * w + 128) >> 8) as u8
}

/// A luma wipe (§S.14) from `a` to `b` at `t`: `pattern` (the same size) decides where `b`
/// shows — where its luma, as 0–1 from limited range (1 − that when `invert`), is below the
/// position, with a soft edge of `soft` — by the wipes' weight. A chroma sample takes the
/// mean of its two pixels' pattern.
pub fn luma(a: &Frame, b: &Frame, pattern: &Frame, soft: f32, invert: bool, t: f32) -> Frame {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return a.clone();
    }
    if t >= 1.0 {
        return b.clone();
    }
    let soft = soft.clamp(0.0, 1.0);
    // The pattern's level (0..=255 of 16..235) to the new picture's weight, once.
    let table: [u32; 256] = std::array::from_fn(|v| {
        let mut s = ((v as f32 - 16.0) / 219.0).clamp(0.0, 1.0);
        if invert {
            s = 1.0 - s;
        }
        wipe_weight(s, t, soft)
    });
    let pw_ = pattern.width;
    let mut out = a.clone();
    out.a = None;
    each_plane(&mut out, &|p, row0, rows| {
        let width = pw(a.width, p);
        let src = plane(b, p);
        for (r, line) in rows.chunks_mut(width).enumerate() {
            let row = row0 + r;
            let pat = &pattern.y[row * pw_..row * pw_ + pw_];
            let s = &src[row * width..(row + 1) * width];
            for (i, (v, &n)) in line.iter_mut().zip(s).enumerate() {
                let level = if p == P::Y {
                    pat[i] as u32
                } else {
                    let l = pat[2 * i] as u32;
                    (l + *pat.get(2 * i + 1).unwrap_or(&pat[2 * i]) as u32).div_ceil(2)
                };
                let k = table[level as usize];
                if k != 0 {
                    *v = lerp(*v, n, k);
                }
            }
        }
    });
    out
}

/// A wipe's new-picture weight (0..=256) at position `s` (0..1) at `t`, edge `soft`.
fn wipe_weight(s: f32, t: f32, soft: f32) -> u32 {
    let m = if soft > 0.0 {
        ((t * (1.0 + soft) - s) / soft).clamp(0.0, 1.0)
    } else if s < t {
        1.0
    } else {
        0.0
    };
    (m * 256.0).round() as u32
}

/// The program bus during a transition from `a` (program) to `b` (preview) at `t`
/// (0.0–1.0); both at the same size.
pub fn transition(a: &Frame, b: &Frame, kind: Kind, t: f32) -> Frame {
    let t = t.clamp(0.0, 1.0);
    match kind {
        Kind::Cut => {
            if t < 1.0 {
                a.clone()
            } else {
                b.clone()
            }
        }
        // A stinger's bus is the program; the swap and its clip are the caller's.
        Kind::Stinger(_) => a.clone(),
        _ if t <= 0.0 => a.clone(),
        _ if t >= 1.0 => b.clone(),
        // Without its pattern (see `luma`): a mix.
        Kind::Mix | Kind::Luma { .. } => {
            let w = (t * 256.0).round() as u32;
            let mut out = a.clone();
            out.a = None;
            each_plane(&mut out, &|p, row0, rows| {
                let src = plane(b, p);
                let start = row0 * pw(a.width, p);
                for (i, v) in rows.iter_mut().enumerate() {
                    *v = lerp(*v, src[start + i], w);
                }
            });
            out
        }
        Kind::Zoom => {
            let (x, y, zw, zh) = zoom_rect(a.width, a.height, t);
            let mut out = a.clone();
            out.a = None;
            if zw >= 2 && zh >= 1 {
                let small = b.scaled((0.0, 0.0, 1.0, 1.0), zw, zh);
                key_at(&mut out, &small, x, y, 1.0);
            }
            out
        }
        Kind::Fade => {
            // First half: the program to black; second: black to the preview.
            let (src, w) = if t < 0.5 {
                (a, (t * 512.0).round() as u32)
            } else {
                (b, ((1.0 - t) * 512.0).round() as u32)
            };
            let mut out = src.clone();
            out.a = None;
            each_plane(&mut out, &|p, _, rows| {
                let k = black(p);
                for v in rows.iter_mut() {
                    *v = lerp(*v, k, w.min(256));
                }
            });
            out
        }
        Kind::Wipe(dir, soft) => {
            let (fw, fh) = (a.width as f32, a.height as f32);
            let mut out = a.clone();
            out.a = None;
            // The new picture's weight (0..=256) at a position s (0..1 along the wipe).
            let weight = move |s: f32| -> u32 {
                let m = if soft > 0.0 {
                    ((t * (1.0 + soft) - s) / soft).clamp(0.0, 1.0)
                } else if s < t {
                    1.0
                } else {
                    0.0
                };
                (m * 256.0).round() as u32
            };
            each_plane(&mut out, &|p, row0, rows| {
                let width = pw(a.width, p);
                let src = plane(b, p);
                // Chroma samples sit between two luma columns.
                let xscale = if p == P::Y { 1.0 } else { 2.0 };
                let x_of = |i: usize| i as f32 * xscale + xscale / 2.0;
                match dir {
                    // A straight wipe: the weight depends on the column (or the row) only,
                    // so it is worked out once, not per sample (a wipe held
                    // near its start took up to 43 ms a frame).
                    Direction::Left | Direction::Right => {
                        let w: Vec<u32> = (0..width)
                            .map(|i| {
                                weight(if dir == Direction::Left {
                                    x_of(i) / fw
                                } else {
                                    1.0 - x_of(i) / fw
                                })
                            })
                            .collect();
                        for (r, line) in rows.chunks_mut(width).enumerate() {
                            let s = &src[(row0 + r) * width..(row0 + r + 1) * width];
                            for ((v, &n), &k) in line.iter_mut().zip(s).zip(&w) {
                                if k != 0 {
                                    *v = lerp(*v, n, k);
                                }
                            }
                        }
                    }
                    Direction::Down | Direction::Up => {
                        for (r, line) in rows.chunks_mut(width).enumerate() {
                            let y = (row0 + r) as f32 + 0.5;
                            let k = weight(if dir == Direction::Down {
                                y / fh
                            } else {
                                1.0 - y / fh
                            });
                            if k != 0 {
                                let s = &src[(row0 + r) * width..(row0 + r + 1) * width];
                                line.iter_mut()
                                    .zip(s)
                                    .for_each(|(v, &n)| *v = lerp(*v, n, k));
                            }
                        }
                    }
                    // A circle: squared distances against the edge's two radii; a square
                    // root only within the soft edge.
                    Direction::Circle => {
                        let half_diag = (fw * fw + fh * fh).sqrt() / 2.0;
                        let edge = t * (1.0 + soft);
                        let (r_in, r_out) = ((edge - soft).max(0.0) * half_diag, edge * half_diag);
                        let (in2, out2) = (r_in * r_in, r_out * r_out);
                        for (r, line) in rows.chunks_mut(width).enumerate() {
                            let dy = (row0 + r) as f32 + 0.5 - fh / 2.0;
                            if dy * dy >= out2 {
                                continue; // the whole row is outside
                            }
                            let s = &src[(row0 + r) * width..(row0 + r + 1) * width];
                            for (i, (v, &n)) in line.iter_mut().zip(s).enumerate() {
                                let dx = x_of(i) - fw / 2.0;
                                let d2 = dx * dx + dy * dy;
                                if d2 <= in2 {
                                    *v = n;
                                } else if d2 < out2 {
                                    *v = lerp(*v, n, weight(d2.sqrt() / half_diag));
                                }
                            }
                        }
                    }
                }
            });
            out
        }
    }
}

fn pw(width: usize, p: P) -> usize {
    if p == P::Y {
        width
    } else {
        Frame::chroma_width(width)
    }
}

/// Keys `src` (same size as `dst`) over `dst` by its alpha (opaque without one) times
/// `opacity` (0.0–1.0): a full-frame overlay (§S.5).
pub fn key(dst: &mut Frame, src: &Frame, opacity: f32) {
    key_at(dst, src, 0, 0, opacity);
}

/// Keys `src` over the part of `dst` with its top left at (`x`, `y`) (`x` even), clipped.
pub fn key_at(dst: &mut Frame, src: &Frame, x: usize, y: usize, opacity: f32) {
    let o = (opacity.clamp(0.0, 1.0) * 256.0).round() as u32;
    if o == 0 || x >= dst.width || y >= dst.height {
        return;
    }
    let (dw, sw) = (dst.width, src.width);
    let (dcw, scw) = (Frame::chroma_width(dw), Frame::chroma_width(sw));
    let n = sw.min(dw - x); // pixels per row
    let cn = scw.min(dcw - x / 2);
    let rows = src.height.min(dst.height - y);
    let alpha = src.a.as_deref();
    // The weight 0..=256 of each coverage, worked out once rather than per sample.
    let table: [u32; 256] = std::array::from_fn(|a| (a as u32 * o + 127) / 255);
    let weight = |a: u8| table[a as usize];
    each_plane(dst, &|p, row0, band| {
        let width = pw(dw, p);
        let s = plane(src, p);
        for (r, line) in band.chunks_mut(width).enumerate() {
            let row = row0 + r;
            if row < y || row >= y + rows {
                continue;
            }
            let sr = row - y;
            match p {
                P::Y => {
                    let d = &mut line[x..x + n];
                    let sv = &s[sr * sw..sr * sw + n];
                    match alpha {
                        None => d
                            .iter_mut()
                            .zip(sv)
                            .for_each(|(d, &v)| *d = lerp(*d, v, o.min(256))),
                        Some(a) => {
                            let av = &a[sr * sw..sr * sw + n];
                            for ((d, &v), &a) in d.iter_mut().zip(sv).zip(av) {
                                *d = lerp(*d, v, weight(a));
                            }
                        }
                    }
                }
                _ => {
                    let d = &mut line[x / 2..x / 2 + cn];
                    let sv = &s[sr * scw..sr * scw + cn];
                    match alpha {
                        None => d
                            .iter_mut()
                            .zip(sv)
                            .for_each(|(d, &v)| *d = lerp(*d, v, o.min(256))),
                        Some(a) => {
                            // A chroma sample covers two pixels: their mean coverage.
                            let ar = &a[sr * sw..sr * sw + sw];
                            for (i, (d, &v)) in d.iter_mut().zip(sv).enumerate() {
                                let l = ar[2 * i] as u32;
                                let r = *ar.get(2 * i + 1).unwrap_or(&ar[2 * i]) as u32;
                                *d = lerp(*d, v, weight((l + r).div_ceil(2) as u8));
                            }
                        }
                    }
                }
            }
        }
    });
}

/// A picture-in-picture (§S.5): `src` (already scaled to the rectangle's size) at
/// (`x`, `y`) of `dst` (x even), with a border `border` px wide in `colour` around it,
/// keyed by `opacity`. Only the rectangle is touched.
pub fn pip(
    dst: &mut Frame,
    src: &Frame,
    x: usize,
    y: usize,
    border: usize,
    colour: Ycc,
    opacity: f32,
) {
    let b = border.min(dst.width / 4);
    if b > 0 {
        let (bx0, by0) = (x.saturating_sub(b) & !1, y.saturating_sub(b));
        let (bx1, by1) = (
            (x + src.width + b).min(dst.width),
            (y + src.height + b).min(dst.height),
        );
        if bx1 > bx0 && by1 > by0 {
            key_at(
                dst,
                &Frame::solid(bx1 - bx0, by1 - by0, colour),
                bx0,
                by0,
                opacity,
            );
        }
    }
    key_at(dst, src, x, y, opacity);
}

/// Fade to black (§S.4): the whole picture towards black by `t` (1.0: black).
pub fn to_black(f: &mut Frame, t: f32) {
    let w = (t.clamp(0.0, 1.0) * 256.0).round() as u32;
    if w == 0 {
        return;
    }
    each_plane(f, &|p, _, rows| {
        let k = black(p);
        for v in rows.iter_mut() {
            *v = lerp(*v, k, w);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The zoom: nothing of the preview at the start, all of it at the end; halfway, the
    /// preview in a centred rectangle (eased: half the frame each way), the program around.
    #[test]
    fn a_zoom_grows_the_preview_from_the_centre() {
        let (w, h) = (64, 36);
        let a = Frame::solid(w, h, Ycc(40, 128, 128));
        let b = Frame::solid(w, h, Ycc(200, 128, 128));
        assert_eq!(zoom_rect(w, h, 0.0), (32, 18, 0, 0));
        assert_eq!(zoom_rect(w, h, 1.0), (0, 0, w, h));
        let (x, y, zw, zh) = zoom_rect(w, h, 0.5);
        assert_eq!((x, y, zw, zh), (16, 9, 32, 18));
        let at = |f: &Frame, px: usize, py: usize| f.y[py * w + px];
        let mid = transition(&a, &b, Kind::Zoom, 0.5);
        assert_eq!(at(&mid, w / 2, h / 2), 200, "the preview in the middle");
        assert_eq!(at(&mid, 2, 2), 40, "the program at the edge");
        assert_eq!(at(&transition(&a, &b, Kind::Zoom, 0.05), 2, 2), 40);
        assert_eq!(transition(&a, &b, Kind::Zoom, 1.0).y, b.y);
        assert_eq!(Kind::parse("zoom", 0.0), Some(Kind::Zoom));
        assert_eq!(Kind::Zoom.name(), "zoom");
    }

    fn pair() -> (Frame, Frame) {
        (
            Frame::solid(1280, 720, Ycc(60, 100, 200)),
            Frame::solid(1280, 720, Ycc(200, 160, 40)),
        )
    }

    #[test]
    fn a_cut_is_exact_and_a_mix_is_the_mean() {
        let (a, b) = pair();
        assert_eq!(transition(&a, &b, Kind::Cut, 0.5), a);
        assert_eq!(transition(&a, &b, Kind::Cut, 1.0), b);
        let m = transition(&a, &b, Kind::Mix, 0.5);
        assert!(m.y.iter().all(|&v| v == 130));
        assert!(m.cb.iter().all(|&v| v == 130) && m.cr.iter().all(|&v| v == 120));
    }

    #[test]
    fn a_wipe_from_the_left_covers_its_share() {
        let (a, b) = pair();
        let w = transition(&a, &b, Kind::Wipe(Direction::Left, 0.0), 0.25);
        let row = &w.y[360 * 1280..361 * 1280];
        assert!(row[..318].iter().all(|&v| v == 200), "left quarter new");
        assert!(row[322..].iter().all(|&v| v == 60), "the rest old");
        // A soft edge ramps instead.
        let s = transition(&a, &b, Kind::Wipe(Direction::Left, 0.1), 0.5);
        let r = &s.y[360 * 1280..361 * 1280];
        assert!(r[..400].iter().all(|&v| v == 200) && r[900..].iter().all(|&v| v == 60));
        assert!(r[600] > 60 && r[600] < 200, "{}", r[600]);
        // Circle: the centre first.
        let c = transition(&a, &b, Kind::Wipe(Direction::Circle, 0.0), 0.3);
        assert_eq!(c.y[360 * 1280 + 640], 200);
        assert_eq!(c.y[5 * 1280 + 5], 60);
    }

    #[test]
    fn a_fade_dips_through_black() {
        let (a, b) = pair();
        let q = transition(&a, &b, Kind::Fade, 0.25);
        assert_eq!(q.y[0], 38); // half way from 60 to 16
        assert_eq!(transition(&a, &b, Kind::Fade, 0.5).y[0], 16);
        assert_eq!(transition(&a, &b, Kind::Fade, 0.75).y[0], 108); // half way 16 → 200
        assert_eq!(transition(&a, &b, Kind::Fade, 1.0), b);
    }

    #[test]
    fn keys_follow_alpha() {
        let (mut dst, _) = pair();
        let mut lower = Frame::solid(1280, 720, Ycc::WHITE);
        let mut a = vec![0u8; 1280 * 720];
        a[600 * 1280..].fill(255); // the bottom: opaque
        a[300 * 1280..301 * 1280].fill(128); // a line half covered
        lower.a = Some(a);
        key(&mut dst, &lower, 1.0);
        assert_eq!(dst.y[100 * 1280 + 10], 60, "transparent leaves");
        assert_eq!(dst.y[650 * 1280 + 10], 235, "opaque replaces");
        assert_eq!(dst.y[300 * 1280 + 10], 148, "half blends");
        assert_eq!(dst.cb[650 * 640 + 10], 128);
        // An overlay half faded in.
        let (mut d2, _) = pair();
        key(&mut d2, &lower, 0.5);
        assert_eq!(d2.y[650 * 1280 + 10], 148);
    }

    #[test]
    fn a_pip_lands_in_its_rectangle_with_its_border() {
        let (mut dst, b) = pair();
        let small = b.scaled((0.0, 0.0, 1.0, 1.0), 320, 180);
        pip(&mut dst, &small, 900, 40, 4, Ycc::WHITE, 1.0);
        assert_eq!(dst.y[100 * 1280 + 1000], 200, "inside: the picture");
        assert_eq!(dst.y[38 * 1280 + 1000], 235, "the border above it");
        assert_eq!(dst.y[100 * 1280 + 898], 235, "and left of it");
        assert_eq!(dst.y[400 * 1280 + 1000], 60, "below: the program");
        assert_eq!(dst.y[100 * 1280 + 800], 60);
    }

    #[test]
    fn fade_to_black_reaches_black() {
        let (mut a, _) = pair();
        to_black(&mut a, 1.0);
        assert_eq!(a, Frame::black(1280, 720));
    }
}

#[cfg(test)]
mod luma_tests {
    use super::*;

    /// A left-to-right ramp as the pattern: at the half, the left half is the new picture.
    #[test]
    fn a_ramp_pattern_wipes_like_a_wipe() {
        let (w, h) = (64, 8);
        let a = Frame::solid(w, h, Ycc::BLACK);
        let b = Frame::solid(w, h, Ycc::WHITE);
        let mut ramp = Frame::solid(w, h, Ycc::BLACK);
        for r in 0..h {
            for x in 0..w {
                ramp.y[r * w + x] = (16 + x * 219 / (w - 1)) as u8;
            }
        }
        let out = luma(&a, &b, &ramp, 0.0, false, 0.5);
        assert_eq!(out.y[3 * w + 5], 235, "left: the new picture");
        assert_eq!(out.y[3 * w + 60], 16, "right: still the old");
        let inv = luma(&a, &b, &ramp, 0.0, true, 0.5);
        assert_eq!(inv.y[3 * w + 5], 16, "inverted: the other way");
        assert_eq!(inv.y[3 * w + 60], 235);
        assert_eq!(
            luma(&a, &b, &ramp, 0.1, false, 1.0).y[0],
            235,
            "done: the new picture"
        );
        assert_eq!(Kind::parse("stinger-3", 0.0), Some(Kind::Stinger(3)));
        assert_eq!(Kind::parse("stinger-9", 0.0), None);
        assert_eq!(Kind::Stinger(3).name(), "stinger-3");
    }
}
