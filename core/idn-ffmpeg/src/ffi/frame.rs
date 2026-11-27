//! Frames and packets: owned AVFrames and AVPackets, and frames that look at pictures or hand them on.

use super::*;

/// An owned AVFrame.
pub struct Frame(pub *mut ff::AVFrame);

// SAFETY: an AVFrame owned by this value; FFmpeg's frames have no thread affinity.
unsafe impl Send for Frame {}

impl Frame {
    pub fn new() -> io::Result<Self> {
        // SAFETY: a plain allocation, checked for null.
        let f = unsafe { ff::av_frame_alloc() };
        if f.is_null() {
            return Err(io::Error::other("out of memory"));
        }
        Ok(Frame(f))
    }

    /// An NV12 frame in system memory.
    pub fn i420(w: usize, h: usize) -> io::Result<Self> {
        let f = Frame::new()?;
        // SAFETY: a new frame of ours, its buffers allocated by FFmpeg for the size and format set.
        unsafe {
            (*f.0).format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;
            (*f.0).width = w as c_int;
            (*f.0).height = h as c_int;
            check(ff::av_frame_get_buffer(f.0, 0), "frame buffer")?;
        }
        Ok(f)
    }

    pub fn nv12(w: usize, h: usize) -> io::Result<Self> {
        let f = Frame::new()?;
        // SAFETY: a new frame of ours, its buffers allocated by FFmpeg for the size and format set.
        unsafe {
            (*f.0).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*f.0).width = w as c_int;
            (*f.0).height = h as c_int;
            check(ff::av_frame_get_buffer(f.0, 0), "frame buffer")?;
        }
        Ok(f)
    }

    /// A P010 frame (10 bits in the top of 16, chroma interleaved 4:2:0): an HDR picture
    /// for an encoder (R.24).
    pub fn p010(w: usize, h: usize) -> io::Result<Self> {
        let f = Frame::new()?;
        // SAFETY: a new frame of ours, its buffers allocated by FFmpeg for the size and format set.
        unsafe {
            (*f.0).format = ff::AVPixelFormat::AV_PIX_FMT_P010LE as c_int;
            (*f.0).width = w as c_int;
            (*f.0).height = h as c_int;
            check(ff::av_frame_get_buffer(f.0, 0), "frame buffer")?;
        }
        Ok(f)
    }

    /// Makes the buffers this frame's own (an encoder may still hold the last ones).
    pub fn make_writable(&mut self) -> io::Result<()> {
        // SAFETY: a frame of ours.
        check(unsafe { ff::av_frame_make_writable(self.0) }, "frame").map(|_| ())
    }

    /// Plane `i`'s rows: (pointer, stride).
    pub fn plane(&self, i: usize) -> (*mut u8, usize) {
        // SAFETY: a frame of ours; `data` and `linesize` are fixed arrays (`i` bounds-checked).
        unsafe { ((*self.0).data[i], (*self.0).linesize[i] as usize) }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: a frame of ours (or null, which the free takes), freed once.
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

pub struct Packet(pub *mut ff::AVPacket);

// SAFETY: an AVPacket owned by this value; FFmpeg's packets have no thread affinity.
unsafe impl Send for Packet {}

impl Packet {
    pub fn new() -> Self {
        // SAFETY: a plain allocation (null on failure: `checked` is the way in for use).
        Packet(unsafe { ff::av_packet_alloc() })
    }

    pub(crate) fn checked() -> io::Result<Self> {
        let p = Self::new();
        oom(p.0)?;
        Ok(p)
    }
}

/// A borrowed AVFrame (a decoder's, valid until its next receive or flush): read-only
/// access without owning it, so the owner can be borrowed mutably meanwhile.
#[derive(Clone, Copy)]
pub struct FrameRef(pub *mut ff::AVFrame);

impl Frame {
    /// Downloads a GPU (VA-API) frame into `sw`; a frame in system memory is used as
    /// it is. The view is valid while both frames live.
    pub fn downloaded(&self, sw: &Frame) -> io::Result<FrameRef> {
        // SAFETY: frames of ours; `sw` takes the download (unreferenced first).
        unsafe {
            if !on_gpu(self.0) {
                return Ok(FrameRef(self.0));
            }
            ff::av_frame_unref(sw.0);
            check(ff::av_hwframe_transfer_data(sw.0, self.0, 0), "download")?;
            // The pixels only: its time, range and colour description too (a full-range
            // picture downloaded as plain NV12 was taken as limited range).
            let _ = ff::av_frame_copy_props(sw.0, self.0);
            Ok(FrameRef(sw.0))
        }
    }
}

impl Frame {
    /// A GPU (VA-API) frame mapped for reading, in place, with its time, range and colour
    /// description: its planes are in memory the CPU doesn't cache, to be read with
    /// [`copy_uncached`] ([`FrameRef::planes_420`] with `uncached`). `None` for a frame
    /// in system memory or where the driver offers no mapping.
    pub fn mapped(&self) -> Option<Frame> {
        // SAFETY: a frame of ours, mapped into a new frame that holds the mapping.
        unsafe {
            if !on_gpu(self.0) {
                return None;
            }
            let m = Frame::new().ok()?;
            if ff::av_hwframe_map(m.0, self.0, ff::AV_HWFRAME_MAP_READ as c_int) < 0 {
                return None;
            }
            let _ = ff::av_frame_copy_props(m.0, self.0);
            Some(m)
        }
    }
}

impl FrameRef {
    /// (width, height) of a video frame.
    pub fn size(&self) -> (usize, usize) {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        unsafe { ((*self.0).width as usize, (*self.0).height as usize) }
    }

    fn format_flags(&self) -> u64 {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        Self::flags_of(unsafe { super::pix_fmt((*self.0).format) })
    }

    fn flags_of(f: ff::AVPixelFormat) -> u64 {
        // SAFETY: a descriptor lookup (null for an unknown format, checked).
        unsafe {
            let d = ff::av_pix_fmt_desc_get(f);
            if d.is_null() { 0 } else { (*d).flags }
        }
    }

    pub(super) fn format_is_rgb(f: ff::AVPixelFormat) -> bool {
        Self::flags_of(f) & ff::AV_PIX_FMT_FLAG_RGB as u64 != 0
    }

    /// An audio frame's samples (per channel).
    pub fn samples(&self) -> usize {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        unsafe { (*self.0).nb_samples.max(0) as usize }
    }

    /// A packed 8-bit RGB picture (RGB0/RGBA, BGR0/BGRA): its bytes, the stride and
    /// whether it is BGR. Valid while the frame is.
    pub fn packed_rgb(&self) -> Option<(&[u8], usize, bool)> {
        use ff::AVPixelFormat::*;
        // SAFETY: a live frame (as above); `data[0]` holds `linesize[0]` × height bytes (FFmpeg's
        // frame), checked for null and a non-negative stride before the slice is made.
        unsafe {
            let f = &*self.0;
            let bgr = match super::pix_fmt(f.format) {
                AV_PIX_FMT_BGR0 | AV_PIX_FMT_BGRA => true,
                AV_PIX_FMT_RGB0 | AV_PIX_FMT_RGBA => false,
                _ => return None,
            };
            let (w, h, stride) = (f.width as usize, f.height as usize, f.linesize[0]);
            if f.data[0].is_null() || stride < 4 * f.width || h == 0 {
                return None;
            }
            let stride = stride as usize;
            Some((
                std::slice::from_raw_parts(f.data[0], stride * (h - 1) + 4 * w),
                stride,
                bgr,
            ))
        }
    }

    /// Its pixel format carries alpha.
    pub fn has_alpha(&self) -> bool {
        self.format_flags() & ff::AV_PIX_FMT_FLAG_ALPHA as u64 != 0
    }

    /// Gives a picture that doesn't say its range (a hardware JPEG decoder's) its stream's.
    pub fn default_range(&self, r: ff::AVColorRange) {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        unsafe {
            if (*self.0).color_range == ff::AVColorRange::AVCOL_RANGE_UNSPECIFIED {
                (*self.0).color_range = r;
            }
        }
    }

    /// Whether the picture's Y'CbCr is full range: flagged so, or one of the JPEG formats.
    pub fn full_range(&self) -> bool {
        use ff::AVPixelFormat::*;
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        let (range, fmt) = unsafe { ((*self.0).color_range, (*self.0).format) };
        range == ff::AVColorRange::AVCOL_RANGE_JPEG
            || matches!(
                super::pix_fmt(fmt),
                AV_PIX_FMT_YUVJ420P
                    | AV_PIX_FMT_YUVJ422P
                    | AV_PIX_FMT_YUVJ444P
                    | AV_PIX_FMT_YUVJ440P
                    | AV_PIX_FMT_YUVJ411P
            )
    }

    /// Its pixel format is RGB (not YCbCr).
    pub fn is_rgb(&self) -> bool {
        self.format_flags() & ff::AV_PIX_FMT_FLAG_RGB as u64 != 0
    }

    /// YCbCr: (BT.709 matrix, else BT.601; full range). JPEG's `yuvj` formats are full
    /// range whatever the frame says.
    pub fn ycbcr(&self) -> (bool, bool) {
        use ff::AVPixelFormat::*;
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        unsafe {
            let f = &*self.0;
            let j = matches!(
                super::pix_fmt(f.format),
                AV_PIX_FMT_YUVJ420P
                    | AV_PIX_FMT_YUVJ422P
                    | AV_PIX_FMT_YUVJ444P
                    | AV_PIX_FMT_YUVJ440P
            );
            (
                f.colorspace == ff::AVColorSpace::AVCOL_SPC_BT709,
                j || f.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
            )
        }
    }

    /// Y'CbCr with its chroma halved both ways (4:2:0: NV12, I420, P010, …), as nearly
    /// every video file and decoder gives pictures.
    pub fn is_420(&self) -> bool {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        let f = unsafe { super::pix_fmt((*self.0).format) };
        // SAFETY: a descriptor lookup, checked for null.
        unsafe {
            let d = ff::av_pix_fmt_desc_get(f);
            !d.is_null()
                && (*d).flags & ff::AV_PIX_FMT_FLAG_RGB as u64 == 0
                && (*d).nb_components >= 3
                && (*d).log2_chroma_w == 1
                && (*d).log2_chroma_h == 1
        }
    }

    /// Whether [`FrameRef::planes_420`] takes this picture's format.
    pub fn direct_420(&self) -> bool {
        use ff::AVPixelFormat::*;
        matches!(
            // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
            unsafe { super::pix_fmt((*self.0).format) },
            AV_PIX_FMT_YUV420P | AV_PIX_FMT_YUVJ420P | AV_PIX_FMT_NV12 | AV_PIX_FMT_P010LE
        )
    }

    /// An NV12, I420 or P010 picture's planes as ours (Y, Cb, Cr at 4:2:0, 8-bit), made
    /// straight from it: swscale into a frame of its own and a copy out of that took 8.4
    /// (NV12 to 4:2:2) and 11.7 ms (P010) of a 2160p picture. `uncached`: the planes are a
    /// mapped GPU surface ([`Frame::mapped`]), read with streaming loads. `None` for other
    /// formats.
    pub fn planes_420(&self, uncached: bool) -> Option<[idn_open_protocol::shq::Plane; 3]> {
        use ff::AVPixelFormat::*;
        use idn_open_protocol::shq::Plane;
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        let f = unsafe { &*self.0 };
        // (chroma interleaved, two bytes a sample with the value in the high bits)
        let (nv, p010) = match super::pix_fmt(f.format)
        {
            AV_PIX_FMT_YUV420P | AV_PIX_FMT_YUVJ420P => (false, false),
            AV_PIX_FMT_NV12 => (true, false),
            AV_PIX_FMT_P010LE => (true, true),
            _ => return None,
        };
        let (w, h) = self.size();
        if w == 0 || h == 0 {
            return None;
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let bytes = if p010 { 2 } else { 1 };
        // Plane i's rows, each `rw` bytes long (null or too narrow: none).
        let rows = |i: usize, rw: usize, rh: usize| -> Option<Vec<&[u8]>> {
            let (p, stride) = (f.data[i], f.linesize[i]);
            if p.is_null() || stride <= 0 || (stride as usize) < rw {
                return None;
            }
            Some(
                (0..rh)
                    // SAFETY: row r of plane i: `p` non-null and `rw` ≤ stride checked above; the
                    // formats taken (4:2:0) have `rh` rows in the plane.
                    .map(|r| unsafe { std::slice::from_raw_parts(p.add(r * stride as usize), rw) })
                    .collect(),
            )
        };
        let fetch = |dst: &mut [u8], src: &[u8]| {
            if uncached {
                copy_uncached(dst, src);
            } else {
                dst.copy_from_slice(src);
            }
        };
        let plane = |pw: usize, ph: usize, data: Vec<u8>| Plane {
            width: pw,
            height: ph,
            data,
        };
        // Rows that need work after reading (P010's samples, NV12's pairs) are read into
        // `row` first, then worked on in cache.
        let mut row = vec![0u8; bytes * w.max(2 * cw)];
        let mut y = vec![0u8; w * h];
        for (src, out) in rows(0, bytes * w, h)?
            .into_iter()
            .zip(y.chunks_exact_mut(w))
        {
            if p010 {
                fetch(&mut row[..2 * w], src);
                round_high_bytes(&row[..2 * w], out);
            } else {
                fetch(out, src);
            }
        }
        let (mut cb, mut cr) = (vec![0u8; cw * ch], vec![0u8; cw * ch]);
        if nv {
            let mut pairs = vec![0u8; 2 * cw];
            for ((src, b), r) in rows(1, bytes * 2 * cw, ch)?
                .into_iter()
                .zip(cb.chunks_exact_mut(cw))
                .zip(cr.chunks_exact_mut(cw))
            {
                let n = bytes * 2 * cw;
                fetch(&mut row[..n], src);
                if p010 {
                    round_high_bytes(&row[..n], &mut pairs);
                    crate::split_pairs(&pairs, b, r);
                } else {
                    crate::split_pairs(&row[..n], b, r);
                }
            }
        } else {
            for (i, c) in [(1, &mut cb), (2, &mut cr)] {
                for (src, out) in rows(i, cw, ch)?.into_iter().zip(c.chunks_exact_mut(cw)) {
                    fetch(out, src);
                }
            }
        }
        Some([plane(w, h, y), plane(cw, ch, cb), plane(cw, ch, cr)])
    }
}

/// 16-bit little-endian samples with the value in the high bits (P010) to 8 bits, rounded:
/// 10-bit 64..940 to 16..235.
pub(super) fn round_high_bytes(src: &[u8], out: &mut [u8]) {
    let n = out.len().min(src.len() / 2);
    let mut i = 0;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SSE2 is part of x86-64; every load and store is in bounds (i + 16 ≤ n).
    unsafe {
        use std::arch::x86_64::*;
        let half = _mm_set1_epi16(0x80);
        while i + 16 <= n {
            let s = src.as_ptr().add(2 * i) as *const __m128i;
            let x = _mm_srli_epi16(_mm_adds_epu16(_mm_loadu_si128(s), half), 8);
            let y = _mm_srli_epi16(_mm_adds_epu16(_mm_loadu_si128(s.add(1)), half), 8);
            _mm_storeu_si128(
                out.as_mut_ptr().add(i) as *mut __m128i,
                _mm_packus_epi16(x, y),
            );
            i += 16;
        }
    }
    for k in i..n {
        out[k] = (u16::from_le_bytes([src[2 * k], src[2 * k + 1]]).saturating_add(0x80) >> 8) as u8;
    }
}

impl Default for Packet {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: a packet of ours (or null, which the free takes), freed once.
        unsafe { ff::av_packet_free(&mut self.0) };
    }
}
