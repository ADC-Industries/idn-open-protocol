//! The pictures' way from `idn-display` (the user's session) to the media source (inside
//! Windows' Frame Server, another session): a named shared memory the source makes for the
//! session at the screen, open to that session's user alone ([`name`]). (One for every
//! interactive user let another signed-in user watch or replace the camera's pictures;
//! bug round E, E3-7.)
//!
//! Layout (little-endian): a 64-byte header — `IDNV`, version 1, width, height, pictures a
//! second (u32 each), 4 bytes free, a sequence number (u64: odd while a picture is being
//! written), the writer's clock when it last wrote (`GetTickCount64`, ms, u64) — then one
//! NV12 picture (`width × height × 3 / 2` bytes).

use idn_open_protocol::shq::Plane;

/// The shared memory of Windows session `session` (the `Global\` namespace: across
/// sessions; the media source makes it, the session's idn-display writes it).
pub fn name(session: u32) -> String {
    format!("Global\\IDNCamera-v2-S{session}")
}
pub const MAGIC: [u8; 4] = *b"IDNV";
pub const VERSION: u32 = 1;
pub const HEADER: usize = 64;
/// The camera's size and rate (what apps are offered).
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const FPS: u32 = 30;
/// A writer silent this long: black.
pub const STALE_MS: u64 = 2000;

/// Header offsets.
pub const OFF_WIDTH: usize = 8;
pub const OFF_HEIGHT: usize = 12;
pub const OFF_FPS: usize = 16;
pub const OFF_SEQ: usize = 24;
pub const OFF_WRITTEN: usize = 32;

/// The whole size for `w × h`.
pub fn size(w: u32, h: u32) -> usize {
    HEADER + nv12_len(w, h)
}

pub fn nv12_len(w: u32, h: u32) -> usize {
    (w as usize * h as usize) * 3 / 2
}

/// A header for a camera of `w × h` at `fps`.
pub fn header(w: u32, h: u32, fps: u32) -> [u8; HEADER] {
    let mut b = [0u8; HEADER];
    b[0..4].copy_from_slice(&MAGIC);
    b[4..8].copy_from_slice(&VERSION.to_le_bytes());
    b[OFF_WIDTH..OFF_WIDTH + 4].copy_from_slice(&w.to_le_bytes());
    b[OFF_HEIGHT..OFF_HEIGHT + 4].copy_from_slice(&h.to_le_bytes());
    b[OFF_FPS..OFF_FPS + 4].copy_from_slice(&fps.to_le_bytes());
    b
}

/// Black NV12.
pub fn black(w: u32, h: u32) -> Vec<u8> {
    let y = w as usize * h as usize;
    let mut v = vec![16u8; y];
    v.resize(y * 3 / 2, 128);
    v
}

/// Packs Y'CbCr planes (any subsampling) as NV12 of `w × h` (even): fitted with the aspect
/// kept, centred, black around; nearest samples.
pub fn pack_nv12(planes: &[Plane], w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize & !1, h as usize & !1);
    let mut out = black(w as u32, h as u32);
    let [y, cb, cr] = match planes {
        [y, cb, cr, ..] if y.width > 0 && y.height > 0 && cb.width > 0 && cb.height > 0 => {
            [y, cb, cr]
        }
        _ => return out,
    };
    let k = (w as f64 / y.width as f64).min(h as f64 / y.height as f64);
    let fw = ((y.width as f64 * k) as usize).min(w) & !1;
    let fh = ((y.height as f64 * k) as usize).min(h) & !1;
    let (ox, oy) = (((w - fw) / 2) & !1, ((h - fh) / 2) & !1);
    for row in 0..fh {
        let sy = row * y.height / fh;
        let dst = (oy + row) * w + ox;
        for col in 0..fw {
            out[dst + col] = y.data[sy * y.width + col * y.width / fw];
        }
    }
    let uv = w * h;
    for row in 0..fh / 2 {
        let sy = (row * 2) * y.height / fh;
        let cy = sy * cb.height / y.height;
        let dst = uv + ((oy / 2 + row) * w) + ox;
        for col in 0..fw / 2 {
            let sx = (col * 2) * y.width / fw;
            let cx = sx * cb.width / y.width;
            out[dst + col * 2] = cb.data[cy * cb.width + cx];
            out[dst + col * 2 + 1] = cr.data[cy * cr.width + cx];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Plane {
        Plane {
            width: w,
            height: h,
            data: (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .map(|(x, y)| f(x, y))
                .collect(),
        }
    }

    #[test]
    fn the_header_and_black() {
        let h = header(1920, 1080, 30);
        assert_eq!(&h[0..4], b"IDNV");
        assert_eq!(
            u32::from_le_bytes(h[OFF_WIDTH..OFF_WIDTH + 4].try_into().unwrap()),
            1920
        );
        assert_eq!(size(4, 2), 64 + 12);
        assert_eq!(
            black(4, 2),
            vec![16, 16, 16, 16, 16, 16, 16, 16, 128, 128, 128, 128]
        );
    }

    #[test]
    fn a_422_picture_as_nv12() {
        let y = plane(4, 2, |x, y| (50 + x + 10 * y) as u8);
        let cb = plane(2, 2, |x, y| (100 + x + 2 * y) as u8);
        let cr = plane(2, 2, |x, y| (200 + x + 2 * y) as u8);
        let out = pack_nv12(&[y, cb, cr], 4, 2);
        // Y as it is; one CbCr row (4:2:0) from the first chroma row, interleaved.
        assert_eq!(
            out,
            vec![50, 51, 52, 53, 60, 61, 62, 63, 100, 200, 101, 201]
        );
    }

    #[test]
    fn letterboxed_into_the_camera() {
        // 2:1 into 4:4: two rows of picture; NV12 starts it on an even row (here 0).
        let y = plane(4, 2, |_, _| 99);
        let c = plane(2, 1, |_, _| 77);
        let out = pack_nv12(&[y, c.clone(), c], 4, 4);
        assert_eq!(&out[..8], &[99; 8]);
        assert_eq!(&out[8..16], &[16; 8]);
        assert_eq!(&out[16..20], &[77, 77, 77, 77]);
        assert_eq!(&out[20..24], &[128; 4]);
        // Wider than the camera (8×4 into 4×8 → 4×2): bars above and below, on even rows.
        let y = plane(8, 4, |_, _| 99);
        let c = plane(4, 4, |_, _| 77);
        let out = pack_nv12(&[y, c.clone(), c], 4, 8);
        assert_eq!(&out[..8], &[16; 8]);
        assert_eq!(&out[8..12], &[99; 4]);
    }
}
