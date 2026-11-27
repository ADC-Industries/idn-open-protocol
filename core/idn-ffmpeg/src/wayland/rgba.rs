//! A picture from shared memory reordered as RGBA (the CPU's path).

use super::*;

/// A capture buffer of `bw` × `bh` RGBA pixels whose contents the desktop turned by
/// wl_output transform `t` (0–7: 90° steps counter-clockwise, 4–7 flipped first) turned
/// back: the picture as the monitor shows it, and its size.
pub(super) fn untransform(buf: &[u8], bw: usize, bh: usize, t: u32) -> (Vec<u8>, usize, usize) {
    let quarter = t & 1 == 1;
    let (w, h) = if quarter { (bh, bw) } else { (bw, bh) };
    let src = buf.as_chunks::<4>().0;
    let mut out = vec![[0u8; 4]; w * h];
    // Where the picture's (x, y) went in the buffer.
    let at = |x: usize, y: usize| {
        let fx = if t & 4 != 0 { w - 1 - x } else { x };
        let (sx, sy) = match t & 3 {
            1 => (y, w - 1 - fx),
            2 => (w - 1 - fx, h - 1 - y),
            3 => (h - 1 - y, fx),
            _ => (fx, y),
        };
        sy * bw + sx
    };
    // In 64 × 64 tiles: a quarter turn reads the buffer down its columns, which row by row
    // missed the cache on every pixel (28 ms a 2160p picture).
    const T: usize = 64;
    for ty in (0..h).step_by(T) {
        for tx in (0..w).step_by(T) {
            for y in ty..(ty + T).min(h) {
                // Along a row the buffer is read at a constant step.
                let x1 = (tx + T).min(w);
                let i0 = at(tx, y) as isize;
                let step = if x1 > tx + 1 { at(tx + 1, y) as isize - i0 } else { 0 };
                let row = &mut out[y * w + tx..y * w + x1];
                let mut i = i0;
                for o in row {
                    *o = src[i as usize];
                    i += step;
                }
            }
        }
    }
    (out.into_flattened(), w, h)
}

/// A picture in one of the offered formats as RGBA (alpha 255), straight into the shared
/// buffer the sink gets: `reuse`'s, when nobody else holds it any more and it is the
/// size, else a new one (not zeroed: every byte is written).
pub(super) fn to_rgba(
    fmt: u32,
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    reuse: Option<Arc<[u8]>>,
) -> Arc<[u8]> {
    let n = w * h * 4;
    // Where R, G, B are in each 4-byte pixel: constants for the compiler, so the reorder
    // becomes vector shuffles (as runtime indices, pixel by pixel into a Vec, then copied
    // into an Arc: 11.5 ms and a second copy a 2160p picture).
    let shuffle: fn(&[u8], &mut [Px], usize, usize, usize) = match fmt {
        pod::BGRX | pod::BGRA => shuffle::<2, 1, 0>,
        pod::XRGB => shuffle::<1, 2, 3>,
        pod::XBGR => shuffle::<3, 2, 1>,
        _ => shuffle::<0, 1, 2>, // RGBx, RGBA
    };
    if let Some(mut a) = reuse.filter(|a| a.len() == n)
        && let Some(dst) = Arc::get_mut(&mut a)
    {
        // SAFETY: [u8; 4] and [MaybeUninit<u8>; 4] have one layout; only written.
        let px = unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<Px>(), n / 4) };
        shuffle(src, px, w, h, stride);
        return a;
    }
    let mut a = Arc::<[u8]>::new_uninit_slice(n);
    let dst = Arc::get_mut(&mut a).expect("just made");
    // SAFETY: as above.
    let px = unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<Px>(), n / 4) };
    shuffle(src, px, w, h, stride);
    // SAFETY: every pixel was written (h rows of w).
    unsafe { a.assume_init() }
}

/// One RGBA pixel as written (into memory that may not be initialised yet).
pub(super) type Px = [std::mem::MaybeUninit<u8>; 4];

pub(super) fn shuffle<const R: usize, const G: usize, const B: usize>(
    src: &[u8],
    dst: &mut [Px],
    w: usize,
    h: usize,
    stride: usize,
) {
    for (y, d) in dst.chunks_exact_mut(w).enumerate().take(h) {
        let row = &src[y * stride..y * stride + w * 4];
        for (d, p) in d.iter_mut().zip(row.as_chunks::<4>().0) {
            *d = [p[R], p[G], p[B], 255].map(std::mem::MaybeUninit::new);
        }
    }
}

#[cfg(test)]
mod turn_tests {
    use super::untransform;

    #[test]
    #[ignore = "timing: cargo test --release -p idn-ffmpeg --lib untransform_speed -- --ignored --nocapture"]
    fn untransform_speed() {
        for (w, h) in [(1920usize, 1080usize), (3840, 2160)] {
            let buf = vec![7u8; w * h * 4];
            let t = std::time::Instant::now();
            for _ in 0..10 {
                std::hint::black_box(untransform(&buf, w, h, 1));
            }
            println!("untransform {w}x{h} (90°): {:.2} ms", t.elapsed().as_secs_f64() * 100.0);
        }
    }

    /// A 3 × 2 picture, pixels numbered 0..6 (row by row), as each transform leaves it in
    /// the buffer; turned back, it is the picture again.
    #[test]
    fn every_transform_turns_back() {
        let pic: Vec<u8> = (0..6u8).flat_map(|v| [v, v, v, 255]).collect();
        let (w, h) = (3usize, 2usize);
        for t in 0..8u32 {
            // What the desktop did: the picture's (x, y) to the buffer, as untransform
            // reads it.
            let quarter = t & 1 == 1;
            let (bw, bh) = if quarter { (h, w) } else { (w, h) };
            let mut buf = vec![0u8; bw * bh * 4];
            for y in 0..h {
                for x in 0..w {
                    let fx = if t & 4 != 0 { w - 1 - x } else { x };
                    let (sx, sy) = match t & 3 {
                        1 => (y, w - 1 - fx),
                        2 => (w - 1 - fx, h - 1 - y),
                        3 => (h - 1 - y, fx),
                        _ => (fx, y),
                    };
                    buf[(sy * bw + sx) * 4..][..4].copy_from_slice(&pic[(y * w + x) * 4..][..4]);
                }
            }
            let (back, tw, th) = untransform(&buf, bw, bh, t);
            assert_eq!((tw, th), (w, h), "transform {t}");
            assert_eq!(back, pic, "transform {t}");
        }
        // 90° counter-clockwise: the picture's top-right corner at the buffer's top-left.
        let mut buf = vec![0u8; 6 * 4];
        let (bw, _) = (2usize, 3usize);
        buf[0] = 2; // picture (2, 0)
        let (back, ..) = untransform(&buf, bw, 3, 1);
        assert_eq!(back[2 * 4], 2);
    }
}
