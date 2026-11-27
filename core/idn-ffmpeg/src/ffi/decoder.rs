//! An open decoder (software or on a GPU), and the pictures it hands over.

use super::*;

/// An open decoder.
pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: Packet,
    frame: Frame,
    next: Frame,
    pub hardware: bool,
}

// SAFETY: the context, packet and frames are its own, used through &mut self only.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the context allocated in `open`, freed once.
        unsafe { ff::avcodec_free_context(&mut self.ctx) };
    }
}

/// Chooses the GPU's format ([`HwDecode::attach`] keeps it in `opaque`) when the decoder
/// offers it, else the first software format.
pub(super) unsafe extern "C" fn pick_hw(
    ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let mut p = fmts;
    // SAFETY: FFmpeg calls this with its context and a list of formats ended by
    // AV_PIX_FMT_NONE; `opaque` holds the format `attach` put there.
    unsafe {
        let want = (*ctx).opaque as isize as i32;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as i32 == want {
                return *p;
            }
            p = p.add(1);
        }
        *fmts
    }
}

/// A decoded picture's planes as FFmpeg gave them.
pub struct Picture<'a> {
    pub format: ff::AVPixelFormat,
    pub width: usize,
    pub height: usize,
    pub planes: [(&'a [u8], usize); 3],
    /// `AVFrame::color_range` as an integer (1 = MPEG/limited, 2 = JPEG/full, 0 = unset).
    pub color_range: c_int,
    /// `AVFrame::colorspace` as an integer (1 = BT.709, 5/6 = BT.601, 9/10 = BT.2020).
    pub colorspace: c_int,
    /// The planes are GPU memory mapped for reading, which the CPU doesn't cache: read
    /// them with [`copy_uncached`], not plain copies.
    pub uncached: bool,
}

/// Copies `src` to `dst` from memory the CPU doesn't cache (see
/// [`idn_open_protocol::mem::copy_uncached`]).
pub fn copy_uncached(dst: &mut [u8], src: &[u8]) {
    idn_open_protocol::mem::copy_uncached(dst, src)
}

impl Decoder {
    pub fn open(id: ff::AVCodecID, device: Option<&HwDecode>) -> io::Result<Self> {
        // SAFETY: a lookup in FFmpeg's codec table, checked for null.
        let codec = unsafe { ff::avcodec_find_decoder(id) };
        if codec.is_null() {
            return Err(io::Error::other(format!(
                "no decoder for {id:?} in this FFmpeg"
            )));
        }
        // SAFETY: `codec` was found above (not null).
        let ctx = oom(unsafe { ff::avcodec_alloc_context3(codec) })?;
        let dec = Decoder {
            ctx,
            pkt: Packet::checked()?,
            frame: Frame::new()?,
            next: Frame::new()?,
            hardware: device.is_some(),
        };
        // SAFETY: `ctx` just allocated for `codec` and not open yet.
        unsafe {
            let c = &mut *ctx;
            // A picture out for each access unit in: no frame-threading delay.
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            // Slices in parallel (no added delay, unlike frame threads), on every core:
            // FFmpeg's default is one thread.
            c.thread_type = ff::FF_THREAD_SLICE;
            c.thread_count = 0;
            // No picture larger than 8K (8192x4320) from the stream's own description:
            // a header saying 1080p could carry parameters of 16000x16000, gigabytes a
            // picture (bug run 2026-11-24).
            c.max_pixels = 8192 * 4320;
            if let Some(hw) = device {
                hw.attach(ctx);
            }
            check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                "opening the decoder",
            )?;
        }
        Ok(dec)
    }

    /// Decodes one access unit tagged `pts`: the newest picture out, if any, still in
    /// GPU memory for a hardware decoder ([`picture_of`] downloads it), with its pts.
    pub fn decode_frame(&mut self, data: &[u8], pts: i64) -> io::Result<Option<(Frame, i64)>> {
        // SAFETY: the decoder's own context, packet and frames, live while it is; the packet is
        // written through its pointer only (no reference held across the fill).
        unsafe {
            fill_packet(self.pkt.0, data, "packet")?;
            // (taken after the fill, which writes the packet through the pointer)
            let p = &mut *self.pkt.0;
            p.pts = pts;
            p.dts = pts;
            let r = ff::avcodec_send_packet(self.ctx, self.pkt.0);
            ff::av_packet_unref(self.pkt.0);
            check(r, "decode")?;
            // Everything the decoder has: normally one picture per access unit, but a
            // decoder may hold one back and give two later. The newest is shown (live
            // video); leaving pictures queued would make the next send fail with EAGAIN.
            // avcodec_receive_frame clears its frame first, even when it then returns
            // EAGAIN, so each picture is received into a scratch frame and moved over.
            let mut got = false;
            loop {
                let r = ff::avcodec_receive_frame(self.ctx, self.next.0);
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "decode")?;
                ff::av_frame_unref(self.frame.0);
                ff::av_frame_move_ref(self.frame.0, self.next.0);
                got = true;
            }
            if !got {
                return Ok(None);
            }
            let out = Frame::new()?;
            ff::av_frame_move_ref(out.0, self.frame.0);
            let pts = match (*out.0).pts {
                ff::AV_NOPTS_VALUE => (*out.0).best_effort_timestamp,
                v => v,
            };
            Ok(Some((out, pts)))
        }
    }
}

/// Calls `f` with a decoded frame's planes, downloading it into `sw` first if it is in
/// GPU memory. May run on another thread than the decoder's (VA-API is thread-safe).
pub fn picture_of<T>(fr: &Frame, sw: &Frame, f: impl FnOnce(Picture) -> T) -> io::Result<T> {
    // SAFETY: `fr` and `sw` are live frames of the caller's; each plane is read only within
    // its linesize × rows (null planes give empty slices; bottom-up ones are refused); the format is checked (`pix_fmt`).
    unsafe {
        let mut fr = fr.0;
        // In GPU memory: mapped for reading and read in place with streaming loads (FFmpeg's
        // download maps it too, then copies plainly: most of a 4K picture's time); a copy
        // made on the GPU's side (the download) where mapping isn't offered.
        let mut mapped: Option<Frame> = None;
        if on_gpu(fr) {
            let m = Frame::new()?;
            if ff::av_hwframe_map(m.0, fr, ff::AV_HWFRAME_MAP_READ as c_int) >= 0 {
                fr = m.0;
                mapped = Some(m);
            } else {
                ff::av_frame_unref(sw.0);
                check(ff::av_hwframe_transfer_data(sw.0, fr, 0), "download")?;
                fr = sw.0;
            }
        }
        let fmt = super::pix_fmt((*fr).format);
        let (w, h) = ((*fr).width as usize, (*fr).height as usize);
        // A bottom-up plane (negative stride) is refused here, not read as a short one.
        if (0..3).any(|i| !(*fr).data[i].is_null() && (*fr).linesize[i] <= 0) {
            return Err(io::Error::other("a picture stored bottom-up"));
        }
        let rows = |i: usize, n: usize| -> (&[u8], usize) {
            let stride = (*fr).linesize[i] as usize;
            if (*fr).data[i].is_null() {
                (&[][..], 0)
            } else {
                (
                    std::slice::from_raw_parts((*fr).data[i], stride * n),
                    stride,
                )
            }
        };
        let ch = h.div_ceil(2);
        let pic = Picture {
            format: fmt,
            width: w,
            height: h,
            planes: [rows(0, h), rows(1, ch), rows(2, ch)],
            color_range: (*fr).color_range as c_int,
            colorspace: (*fr).colorspace as c_int,
            uncached: mapped.is_some(),
        };
        // (`mapped` unmaps when dropped, after `f`.)
        Ok(f(pic))
    }
}

/// The FFmpeg pixel format of planes laid out as `layout`.
pub fn planar_format(layout: idn_open_protocol::shq::Fourcc) -> ff::AVPixelFormat {
    use idn_open_protocol::shq::Fourcc;
    match layout {
        Fourcc::Shq0 => ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
        Fourcc::Shq4 | Fourcc::Shq9 => ff::AVPixelFormat::AV_PIX_FMT_YUV444P,
        Fourcc::Shq2 | Fourcc::Shq7 => ff::AVPixelFormat::AV_PIX_FMT_YUV422P,
    }
}

/// Whether planes of these (width, height, samples) hold a picture of `format` as big as the
/// first: swscale reads what the format says is there, so a short plane (a picture from the
/// network) would have it read past the end.
fn planes_fit(format: ff::AVPixelFormat, planes: &[(usize, usize, usize)]) -> io::Result<()> {
    // SAFETY: a descriptor lookup (static; null for an unknown format, checked).
    let d = unsafe { ff::av_pix_fmt_desc_get(format) };
    if d.is_null() {
        return Err(io::Error::other("an unknown pixel format"));
    }
    // SAFETY: checked non-null; FFmpeg's static table.
    let (cw, ch) = unsafe { ((*d).log2_chroma_w, (*d).log2_chroma_h) };
    let (w, h) = (planes[0].0, planes[0].1);
    let ok = planes
        .iter()
        .take(3)
        .enumerate()
        .all(|(i, &(pw, ph, len))| {
            let (need_w, need_h) = if i == 0 {
                (w, h)
            } else {
                (w.div_ceil(1 << cw), h.div_ceil(1 << ch))
            };
            pw >= need_w && ph >= need_h && pw.checked_mul(ph).is_some_and(|n| len >= n)
        });
    if ok && w > 0 && h > 0 {
        Ok(())
    } else {
        Err(io::Error::other(
            "the planes are smaller than the picture's format needs",
        ))
    }
}

impl Frame {
    /// An AVFrame looking at our planes (no copy): valid while `planes` lives.
    pub fn view(
        planes: &[idn_open_protocol::shq::Plane],
        format: ff::AVPixelFormat,
    ) -> io::Result<Self> {
        if planes.len() < 3 {
            return Err(io::Error::other("a picture needs three planes"));
        }
        planes_fit(
            format,
            &planes
                .iter()
                .map(|p| (p.width, p.height, p.data.len()))
                .collect::<Vec<_>>(),
        )?;
        let f = Frame::new()?;
        // SAFETY: fields of a frame of ours; the plane pointers are only stored (the caller
        // keeps `planes` alive while the view is used, as documented).
        unsafe {
            let fr = &mut *f.0;
            fr.format = format as c_int;
            fr.width = planes[0].width as c_int;
            fr.height = planes[0].height as c_int;
            for (i, p) in planes.iter().take(3).enumerate() {
                fr.data[i] = p.data.as_ptr() as *mut u8;
                fr.linesize[i] = p.width as c_int;
            }
        }
        Ok(f)
    }

    /// A view of 16-bit planes (Y, Cb, Cr; `format` a 16-bit planar one), as [`view`](Self::view).
    pub fn view16(
        planes: &[idn_open_protocol::shq::Plane16],
        format: ff::AVPixelFormat,
    ) -> io::Result<Self> {
        if planes.len() < 3 {
            return Err(io::Error::other("a picture needs three planes"));
        }
        planes_fit(
            format,
            &planes
                .iter()
                .map(|p| (p.width, p.height, p.data.len()))
                .collect::<Vec<_>>(),
        )?;
        let f = Frame::new()?;
        // SAFETY: fields of a frame of ours; the plane pointers are only stored (the caller
        // keeps `planes` alive while the view is used, as documented).
        unsafe {
            let fr = &mut *f.0;
            fr.format = format as c_int;
            fr.width = planes[0].width as c_int;
            fr.height = planes[0].height as c_int;
            for (i, p) in planes.iter().take(3).enumerate() {
                fr.data[i] = p.data.as_ptr() as *mut u8;
                fr.linesize[i] = (p.width * 2) as c_int;
            }
        }
        Ok(f)
    }

    /// Plane `i` of a planar 16-bit frame as one of ours (`w` × `h` samples).
    pub fn to_plane16(&self, i: usize, w: usize, h: usize) -> idn_open_protocol::shq::Plane16 {
        let (p, stride) = self.plane(i);
        let mut data = Vec::with_capacity(w * h);
        for r in 0..h {
            // SAFETY: row r of the plane, w samples of 2 bytes (aligned: FFmpeg's buffers).
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(p.add(r * stride) as *const u16, w)
            });
        }
        idn_open_protocol::shq::Plane16 {
            width: w,
            height: h,
            data,
        }
    }

    /// A frame of `format` and size with its own buffers.
    pub fn alloc(w: usize, h: usize, format: ff::AVPixelFormat) -> io::Result<Self> {
        let f = Frame::new()?;
        // SAFETY: a new frame of ours, its buffers allocated by FFmpeg for the size and format set.
        unsafe {
            (*f.0).format = format as c_int;
            (*f.0).width = w as c_int;
            (*f.0).height = h as c_int;
            check(ff::av_frame_get_buffer(f.0, 0), "frame buffer")?;
        }
        Ok(f)
    }

    /// Plane `i` of a planar 8-bit frame as one of ours (`w` × `h` samples).
    pub fn to_plane(&self, i: usize, w: usize, h: usize) -> idn_open_protocol::shq::Plane {
        let (p, stride) = self.plane(i);
        let mut data = Vec::with_capacity(w * h);
        for r in 0..h {
            // SAFETY: row r of the plane, w bytes (the size the caller allocated the frame with).
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(p.add(r * stride), w) });
        }
        idn_open_protocol::shq::Plane {
            width: w,
            height: h,
            data,
        }
    }
}
