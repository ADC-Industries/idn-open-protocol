//! The VMX codec (OMT's video codec, `VMX1`): libvmx, vendored and compiled in
//! (vendor/libvmx, MIT). One [`Vmx`] codes pictures of one size; pictures are UYVY (8-bit
//! 4:2:2, two bytes a pixel), with an alpha plane after them for UYVA.

use std::os::raw::{c_int, c_void};

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: c_int,
    height: c_int,
}

unsafe extern "C" {
    fn VMX_Create(dimensions: Size, profile: c_int, color_space: c_int) -> *mut c_void;
    fn VMX_Destroy(instance: *mut c_void);
    fn VMX_SetQuality(instance: *mut c_void, q: c_int);
    fn VMX_GetQuality(instance: *mut c_void) -> c_int;
    fn VMX_GetThreads(instance: *mut c_void) -> c_int;
    fn VMX_SetThreads(instance: *mut c_void, n: c_int);
    fn VMX_EncodeUYVY(
        instance: *mut c_void,
        src: *mut u8,
        stride: c_int,
        interlaced: c_int,
    ) -> c_int;
    fn VMX_EncodeUYVA(
        instance: *mut c_void,
        src: *mut u8,
        stride: c_int,
        interlaced: c_int,
    ) -> c_int;
    fn VMX_DecodeUYVY(instance: *mut c_void, dst: *mut u8, stride: c_int) -> c_int;
    fn VMX_DecodeUYVA(instance: *mut c_void, dst: *mut u8, stride: c_int) -> c_int;
    fn VMX_DecodePreviewUYVY(instance: *mut c_void, dst: *mut u8, stride: c_int) -> c_int;
    fn VMX_DecodePreviewUYVA(instance: *mut c_void, dst: *mut u8, stride: c_int) -> c_int;
    fn VMX_LoadFrom(instance: *mut c_void, data: *mut u8, len: c_int) -> c_int;
    fn VMX_SaveTo(instance: *mut c_void, dst: *mut u8, max: c_int) -> c_int;
    fn VMX_GetEncodedPreviewLength(instance: *mut c_void) -> c_int;
}

/// The bitrate targets (OMT's profiles; 1080p60: low 80, standard 200, high 288 Mbit/s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Low = 133,
    Standard = 166,
    High = 199,
}

/// A VMX coder for pictures of one size.
pub struct Vmx {
    inst: *mut c_void,
    width: usize,
    height: usize,
    profile: Profile,
    /// The colour space it was made with (601, 709, or 0: by the height).
    color_space: i32,
    /// Work buffers kept between pictures: the encoder's output room (a picture's size
    /// and more, not zeroed each time) and a coded frame copied
    /// in for the decoder.
    coded: Vec<u8>,
    load: Vec<u8>,
}

// The instance is used from one thread at a time (`&mut self`).
// SAFETY: libvmx keeps no thread-local state, so the instance may move between threads; `Vmx` isn't
// `Sync`, so one thread uses it at a time.
unsafe impl Send for Vmx {}

impl Vmx {
    /// A coder for `width` × `height` (even width) at `profile`, BT.709 for HD and BT.601
    /// below 720 lines (`color_space` 709 or 601; 0: by the height). `fps` above 60 gets
    /// twice the threads, as OMT does.
    pub fn new(
        width: usize,
        height: usize,
        profile: Profile,
        color_space: i32,
        fps: u32,
    ) -> Option<Vmx> {
        if width < 16 || height < 16 || !width.is_multiple_of(2) || width > 8192 || height > 8192 {
            return None;
        }
        // SAFETY: a plain size and enums; null is checked below.
        let inst = unsafe {
            VMX_Create(
                Size {
                    width: width as c_int,
                    height: height as c_int,
                },
                profile as c_int,
                color_space,
            )
        };
        if inst.is_null() {
            return None;
        }
        if fps > 60 {
            // SAFETY: `inst` is the live instance just made.
            unsafe { VMX_SetThreads(inst, VMX_GetThreads(inst) * 2) };
        }
        Some(Vmx {
            inst,
            width,
            height,
            profile,
            color_space,
            coded: Vec::new(),
            load: Vec::new(),
        })
    }

    /// The colour space it was made with (601, 709, or 0: by the height).
    pub fn color_space(&self) -> i32 {
        self.color_space
    }

    /// The size it codes.
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// The profile it was made for.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The quality now (the coder adapts it to the profile's bitrate).
    pub fn quality(&self) -> i32 {
        // SAFETY: `inst` is live until `Drop`.
        unsafe { VMX_GetQuality(self.inst) }
    }

    /// Starts at quality `q` (after a profile change: no dip).
    pub fn set_quality(&mut self, q: i32) {
        // SAFETY: `inst` is live until `Drop`.
        unsafe { VMX_SetQuality(self.inst, q) }
    }

    /// Codes a UYVY picture (`width * 2` bytes a row), with its alpha plane after it
    /// (`width` bytes a row) when `alpha`: the coded frame, and how much of it a preview
    /// needs.
    pub fn encode(
        &mut self,
        uyvy: &mut [u8],
        alpha: bool,
        interlaced: bool,
    ) -> Option<(Vec<u8>, usize)> {
        let stride = self.width * 2;
        let need = stride * self.height + if alpha { self.width * self.height } else { 0 };
        if uyvy.len() < need {
            return None;
        }
        // SAFETY: `inst` codes `width` × `height`; `uyvy` holds the picture at `stride` (and the
        // alpha plane after it), its length checked above.
        let r = unsafe {
            if alpha {
                VMX_EncodeUYVA(
                    self.inst,
                    uyvy.as_mut_ptr(),
                    stride as c_int,
                    interlaced as c_int,
                )
            } else {
                VMX_EncodeUYVY(
                    self.inst,
                    uyvy.as_mut_ptr(),
                    stride as c_int,
                    interlaced as c_int,
                )
            }
        };
        if r != 0 {
            return None;
        }
        let room = self.width * self.height * 4 + 65536;
        if self.coded.len() < room {
            self.coded.resize(room, 0);
        }
        // SAFETY: `coded` holds `room` bytes, the most it is told to write.
        let n = unsafe { VMX_SaveTo(self.inst, self.coded.as_mut_ptr(), room as c_int) };
        if n <= 0 {
            return None;
        }
        let out = self.coded[..n as usize].to_vec();
        // SAFETY: `inst` is live until `Drop`.
        let preview = unsafe { VMX_GetEncodedPreviewLength(self.inst) }.max(0) as usize;
        Some((out, preview.min(n as usize)))
    }

    /// Decodes a coded frame to UYVY (and its alpha plane after it, when `alpha`).
    /// The size of a preview of this coder's pictures (libvmx's rule): an eighth of each
    /// side, the width made even (up), an interlaced picture's height made even (down).
    pub fn preview_size(&self, interlaced: bool) -> (usize, usize) {
        let (mut w, mut h) = (self.width >> 3, self.height >> 3);
        if w % 2 != 0 {
            w += 1;
        }
        if interlaced && h % 2 != 0 {
            h -= 1;
        }
        (w, h)
    }

    /// Decodes the preview of a coded frame (the start of it a sender sends as a preview,
    /// or a whole frame): UYVY at [`preview_size`](Self::preview_size), alpha plane after
    /// it when `alpha`.
    pub fn decode_preview(
        &mut self,
        data: &[u8],
        alpha: bool,
        interlaced: bool,
    ) -> Option<(Vec<u8>, usize, usize)> {
        self.load.clear();
        self.load.extend_from_slice(data);
        let src = &mut self.load;
        // SAFETY: `src` holds `src.len()` bytes; libvmx checks the frame against that length.
        if unsafe { VMX_LoadFrom(self.inst, src.as_mut_ptr(), src.len() as c_int) } != 0 {
            return None;
        }
        let (w, h) = self.preview_size(interlaced);
        if w == 0 || h == 0 {
            return None;
        }
        // Room for the larger of the two heights libvmx may use: the frame decides which,
        // not the header's interlaced flag a sender sets (bug run 2026-11-25: a flag that
        // didn't match the coding had libvmx write a row past the end). The picture as the
        // header says is kept.
        let room = self.preview_size(false).1.max(h);
        let stride = w * 2;
        let mut out = vec![0u8; stride * room + if alpha { w * room } else { 0 }];
        // SAFETY: `out` holds a preview-sized picture at `stride` at the larger height (and
        // its alpha plane when `alpha`).
        let r = unsafe {
            if alpha {
                VMX_DecodePreviewUYVA(self.inst, out.as_mut_ptr(), stride as c_int)
            } else {
                VMX_DecodePreviewUYVY(self.inst, out.as_mut_ptr(), stride as c_int)
            }
        };
        out.truncate(stride * h + if alpha { w * h } else { 0 });
        (r == 0).then_some((out, w, h))
    }

    pub fn decode(&mut self, data: &[u8], alpha: bool) -> Option<Vec<u8>> {
        // (Copied: VMX_LoadFrom takes a mutable pointer; into the kept buffer.)
        self.load.clear();
        self.load.extend_from_slice(data);
        let src = &mut self.load;
        // SAFETY: `src` holds `src.len()` bytes; libvmx checks the frame against that length.
        if unsafe { VMX_LoadFrom(self.inst, src.as_mut_ptr(), src.len() as c_int) } != 0 {
            return None;
        }
        let stride = self.width * 2;
        let mut out =
            vec![0u8; stride * self.height + if alpha { self.width * self.height } else { 0 }];
        // SAFETY: `out` holds the instance's whole picture at `stride` (and its alpha plane when
        // `alpha`).
        let r = unsafe {
            if alpha {
                VMX_DecodeUYVA(self.inst, out.as_mut_ptr(), stride as c_int)
            } else {
                VMX_DecodeUYVY(self.inst, out.as_mut_ptr(), stride as c_int)
            }
        };
        (r == 0).then_some(out)
    }
}

impl Drop for Vmx {
    fn drop(&mut self) {
        // SAFETY: made in `new`, destroyed once, here.
        unsafe { VMX_Destroy(self.inst) }
    }
}

/// One row of UYVY from a row of Y and the row's 4:2:2 Cb and Cr (`out` 2 × the Y row).
pub fn uyvy_row(out: &mut [u8], y: &[u8], cb: &[u8], cr: &[u8]) {
    // SAFETY: SSE2 is part of x86-64; the function stays within the slices it is given.
    #[cfg(target_arch = "x86_64")]
    let done = unsafe { pack_sse2(out, y, cb, cr) };
    #[cfg(not(target_arch = "x86_64"))]
    let done = 0;
    pack(
        &mut out[done * 2..],
        &y[done..],
        &cb[done / 2..],
        &cr[done / 2..],
    );
}

/// Y, Cb and Cr into UYVY, a pair of samples at a time.
fn pack(out: &mut [u8], y: &[u8], cb: &[u8], cr: &[u8]) {
    for (((o, y), &u), &v) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(y.as_chunks::<2>().0.iter())
        .zip(cb)
        .zip(cr)
    {
        o[0] = u;
        o[1] = y[0];
        o[2] = v;
        o[3] = y[1];
    }
}

/// Y, Cb and Cr into UYVY 16 luma samples at a time (Cb and Cr interleaved, then with Y);
/// the luma samples done.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pack_sse2(out: &mut [u8], y: &[u8], cb: &[u8], cr: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let n = (out.len() / 32)
        .min(y.len() / 16)
        .min(cb.len() / 8)
        .min(cr.len() / 8);
    for i in 0..n {
        // SAFETY: i < n keeps every read and write inside its slice.
        unsafe {
            let yv = _mm_loadu_si128(y.as_ptr().add(i * 16) as *const __m128i);
            let u = _mm_loadl_epi64(cb.as_ptr().add(i * 8) as *const __m128i);
            let v = _mm_loadl_epi64(cr.as_ptr().add(i * 8) as *const __m128i);
            let uv = _mm_unpacklo_epi8(u, v);
            let o = out.as_mut_ptr().add(i * 32) as *mut __m128i;
            _mm_storeu_si128(o, _mm_unpacklo_epi8(uv, yv));
            _mm_storeu_si128(o.add(1), _mm_unpackhi_epi8(uv, yv));
        }
    }
    n * 16
}

/// Planar 8-bit Y'CbCr 4:2:2 (Y `w×h`, Cb and Cr `w/2×h`) as UYVY, alpha (`w×h`) after it
/// if given.
pub fn planes_to_uyvy(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    a: Option<&[u8]>,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 2 + a.map_or(0, |_| w * h)];
    let (pic, rest) = out.split_at_mut(w * h * 2);
    for (r, o) in pic.chunks_exact_mut(w * 2).enumerate() {
        uyvy_row(
            o,
            &y[r * w..][..w],
            &cb[r * w / 2..][..w / 2],
            &cr[r * w / 2..][..w / 2],
        );
    }
    if let Some(a) = a {
        rest.copy_from_slice(&a[..w * h]);
    }
    out
}

/// UYVY (and alpha after it) as planar 4:2:2: (Y, Cb, Cr, alpha).
#[allow(clippy::type_complexity)]
pub fn uyvy_to_planes(
    p: &[u8],
    w: usize,
    h: usize,
    alpha: bool,
) -> (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>) {
    let (mut y, mut cb, mut cr) = (vec![0u8; w * h], vec![0u8; w * h / 2], vec![0u8; w * h / 2]);
    // Pushed a byte at a time it was most of a 1080p30 receiver's CPU; 16 bytes at a
    // time with SSSE3 where there is one.
    let src = &p[..w * h * 2];
    let mut done = 0;
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("ssse3") {
        // SAFETY: SSSE3 is there; the function stays within the slices it is given.
        done = unsafe { unpack_ssse3(src, &mut y, &mut cb, &mut cr) };
    }
    unpack(
        &src[done * 2..],
        &mut y[done..],
        &mut cb[done / 2..],
        &mut cr[done / 2..],
    );
    let a = alpha.then(|| p[w * h * 2..w * h * 3].to_vec());
    (y, cb, cr, a)
}

/// UYVY into Y, Cb and Cr, a pair of samples at a time.
fn unpack(src: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8]) {
    for (((q, y), u), v) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(y.as_chunks_mut::<2>().0.iter_mut())
        .zip(cb.iter_mut())
        .zip(cr.iter_mut())
    {
        *u = q[0];
        y[0] = q[1];
        *v = q[2];
        y[1] = q[3];
    }
}

/// UYVY into Y, Cb and Cr 16 bytes (8 luma samples) at a time; the luma samples done.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn unpack_ssse3(src: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8]) -> usize {
    use std::arch::x86_64::*;
    // Y to bytes 0–7, Cb to 8–11, Cr to 12–15.
    let order = _mm_setr_epi8(1, 3, 5, 7, 9, 11, 13, 15, 0, 4, 8, 12, 2, 6, 10, 14);
    let n = (src.len() / 16)
        .min(y.len() / 8)
        .min(cb.len() / 4)
        .min(cr.len() / 4);
    for i in 0..n {
        // SAFETY: i < n keeps every read and write inside its slice.
        unsafe {
            let v = _mm_loadu_si128(src.as_ptr().add(i * 16) as *const __m128i);
            let s = _mm_shuffle_epi8(v, order);
            _mm_storel_epi64(y.as_mut_ptr().add(i * 8) as *mut __m128i, s);
            let c = _mm_srli_si128::<8>(s);
            (cb.as_mut_ptr().add(i * 4) as *mut i32).write_unaligned(_mm_cvtsi128_si32(c));
            (cr.as_mut_ptr().add(i * 4) as *mut i32)
                .write_unaligned(_mm_cvtsi128_si32(_mm_srli_si128::<4>(c)));
        }
    }
    n * 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uyvy_unpacks_as_the_plain_loop_does() {
        // 38 × 3: rows not a multiple of 16 bytes, the tail by the plain loop.
        let (w, h) = (38usize, 3usize);
        let p: Vec<u8> = (0..w * h * 2).map(|i| (i * 37 % 251) as u8).collect();
        let (y, cb, cr, _) = uyvy_to_planes(&p, w, h, false);
        let (mut y2, mut cb2, mut cr2) = (vec![0; w * h], vec![0; w * h / 2], vec![0; w * h / 2]);
        unpack(&p, &mut y2, &mut cb2, &mut cr2);
        assert_eq!((&y, &cb, &cr), (&y2, &cb2, &cr2));
        // And back: packed in one go as by the plain loop.
        type Packer = fn(&mut [u8], &[u8], &[u8], &[u8]);
        let row = |f: Packer| {
            let mut o = vec![0u8; w * 2];
            f(&mut o, &y2[..w], &cb2[..w / 2], &cr2[..w / 2]);
            o
        };
        assert_eq!(row(uyvy_row), row(pack));
        assert_eq!(row(uyvy_row), p[..w * 2]);
    }

    #[test]
    fn a_picture_round_trips_through_the_codec() {
        let (w, h) = (320usize, 180usize);
        // A ramp in luma, flat colour: easy to check after a lossy codec.
        let y: Vec<u8> = (0..w * h).map(|i| 16 + ((i % w) * 219 / w) as u8).collect();
        let cb = vec![100u8; w * h / 2];
        let cr = vec![150u8; w * h / 2];
        let a: Vec<u8> = (0..w * h)
            .map(|i| if i % w < w / 2 { 255 } else { 0 })
            .collect();
        let mut uyvy = planes_to_uyvy(&y, &cb, &cr, Some(&a), w, h);
        assert_eq!(
            uyvy_to_planes(&uyvy, w, h, true),
            (y.clone(), cb.clone(), cr.clone(), Some(a.clone()))
        );
        let mut enc = Vmx::new(w, h, Profile::High, 709, 30).unwrap();
        let (coded, preview) = enc.encode(&mut uyvy, true, false).unwrap();
        assert!(
            coded.len() < w * h * 3 && preview > 0 && preview < coded.len(),
            "{} {preview}",
            coded.len()
        );
        let mut dec = Vmx::new(w, h, Profile::Standard, 709, 30).unwrap();
        let back = dec.decode(&coded, true).unwrap();
        let (y2, cb2, cr2, a2) = uyvy_to_planes(&back, w, h, true);
        let err = |p: &[u8], q: &[u8]| {
            p.iter()
                .zip(q)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .max()
                .unwrap()
        };
        // A lossy codec: close on average, a few levels at worst.
        let mean = |p: &[u8], q: &[u8]| {
            p.iter()
                .zip(q)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .sum::<i32>() as f64
                / p.len() as f64
        };
        assert!(
            mean(&y, &y2) < 1.5 && err(&y, &y2) <= 12,
            "luma {} {}",
            mean(&y, &y2),
            err(&y, &y2)
        );
        assert!(err(&cb, &cb2) <= 6 && err(&cr, &cr2) <= 6);
        let a2 = a2.unwrap();
        assert!(mean(&a, &a2) < 2.0, "alpha {}", mean(&a, &a2));
        assert!(Vmx::new(15, 16, Profile::Standard, 0, 30).is_none());
        assert!(dec.decode(b"not a frame", false).is_none());
    }
}
