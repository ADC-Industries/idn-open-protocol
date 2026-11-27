//! swscale: converting and scaling pictures between pixel formats and sizes.

use super::*;

/// Converts and scales pictures between pixel formats and sizes (swscale).
pub struct Scaler {
    ctx: *mut ff::SwsContext,
    key: (c_int, c_int, c_int, usize, usize, c_int),
}

// SAFETY: the context is its own, used through &mut access only.
unsafe impl Send for Scaler {}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: our context (or null, which the free takes), freed once.
        unsafe { ff::sws_freeContext(self.ctx) };
    }
}

impl Scaler {
    /// The source's format with its range: the context is made for one. (One form for
    /// `open` and `run`: `open` kept the bare format, so a full-range source never matched
    /// and the context was made again for every picture.)
    fn format_key(src: FrameRef) -> c_int {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        (unsafe { (*src.0).format }) | (src.full_range() as c_int) << 24
    }

    fn open(src: FrameRef, w: usize, h: usize, dst: ff::AVPixelFormat) -> io::Result<Self> {
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        let (sw, sh, sf) = unsafe { ((*src.0).width, (*src.0).height, (*src.0).format) };
        let ctx = unsafe {
            ff::sws_getContext(
                sw,
                sh,
                super::pix_fmt(sf),
                w as c_int,
                h as c_int,
                dst,
                SWS_AREA,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ctx.is_null() {
            return Err(io::Error::other("no conversion for this picture format"));
        }
        // RGB (screens) to YCbCr: BT.709, limited range, as our pictures are; swscale's
        // own choice is BT.601. YCbCr to YCbCr keeps its matrix and its
        // range: told both sides have the source's, so no path of swscale converts it (some
        // did, from the JPEG formats; others didn't, from packed YUYV); the caller does,
        // once (`to_planar`).
        if !src.is_rgb() && !FrameRef::format_is_rgb(dst) {
            let full = src.full_range() as c_int;
            // SAFETY: our new context; the coefficient tables are swscale's static ones.
            unsafe {
                let c = ff::sws_getCoefficients(ff::SWS_CS_ITU709);
                ff::sws_setColorspaceDetails(ctx, c, full, c, full, 0, 1 << 16, 1 << 16);
            }
        } else if src.is_rgb() && !FrameRef::format_is_rgb(dst) {
            // SAFETY: our new context; the coefficient tables are swscale's static ones.
            unsafe {
                ff::sws_setColorspaceDetails(
                    ctx,
                    ff::sws_getCoefficients(ff::SWS_CS_ITU601),
                    1,
                    ff::sws_getCoefficients(ff::SWS_CS_ITU709),
                    0,
                    0,
                    1 << 16,
                    1 << 16,
                );
            }
        }
        Ok(Scaler {
            ctx,
            key: (sw, sh, Self::format_key(src), w, h, dst as c_int),
        })
    }

    /// Converts `src` into `dst` (allocated with the wanted size and format), reopening
    /// when the shapes change.
    pub fn run(this: &mut Option<Scaler>, src: FrameRef, dst: &Frame) -> io::Result<()> {
        // SAFETY: `dst` is a frame of ours.
        let (w, h, f) = unsafe {
            (
                (*dst.0).width as usize,
                (*dst.0).height as usize,
                (*dst.0).format,
            )
        };
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        let key = unsafe {
            (
                (*src.0).width,
                (*src.0).height,
                Self::format_key(src),
                w,
                h,
                f,
            )
        };
        if this.as_ref().is_none_or(|s| s.key != key) {
            *this = Some(Scaler::open(src, w, h, super::pix_fmt(f))?);
        }
        let s = this.as_ref().unwrap();
        // The frame API may take the ranges from the frames: Y'CbCr keeps its own (made
        // limited by the caller), RGB becomes limited.
        if !FrameRef::format_is_rgb(super::pix_fmt(f)) {
            // SAFETY: `dst` is a frame of ours; `src` a live FrameRef.
            unsafe {
                (*dst.0).color_range = if src.is_rgb() {
                    ff::AVColorRange::AVCOL_RANGE_MPEG
                } else {
                    (*src.0).color_range
                };
            }
        }
        // A view of our own planes (no buffer of FFmpeg's): scaled from its pointers as
        // they are. The frame call references its source first, which copies a frame with
        // no buffer whole (a full copy per downscaled picture). The
        // context holds the ranges and matrices the frame call would have taken.
        // SAFETY: a FrameRef points at a live frame (its contract: until its owner changes it).
        let viewed = unsafe { (*src.0).buf[0].is_null() };
        check(
            unsafe {
                if viewed {
                    let (sf, df) = (&*src.0, &*dst.0);
                    ff::sws_scale(
                        s.ctx,
                        sf.data.as_ptr() as *const *const u8,
                        sf.linesize.as_ptr(),
                        0,
                        sf.height,
                        df.data.as_ptr(),
                        df.linesize.as_ptr(),
                    )
                } else {
                    ff::sws_scale_frame(s.ctx, dst.0, src.0)
                }
            },
            "scaling",
        )?;
        Ok(())
    }
}

/// Converts `src` into `dst` once, with the YCbCr matrices given (BT.709 or BT.601, full
/// or limited range; an RGB side's are ignored): for pictures, where swscale's own choice
/// is BT.601 whatever the size.
pub fn convert_with(
    src: FrameRef,
    dst: &Frame,
    (src_709, src_full): (bool, bool),
    (dst_709, dst_full): (bool, bool),
) -> io::Result<()> {
    // SAFETY: `dst` is a frame of ours.
    let (w, h, f) = unsafe {
        (
            (*dst.0).width as usize,
            (*dst.0).height as usize,
            (*dst.0).format,
        )
    };
    let s = Scaler::open(src, w, h, super::pix_fmt(f))?;
    // SAFETY: swscale's static coefficient tables.
    let table = |bt709: bool| unsafe {
        ff::sws_getCoefficients(if bt709 {
            ff::SWS_CS_ITU709
        } else {
            ff::SWS_CS_ITU601
        })
    };
    // SAFETY: our new context and swscale's static tables; both frames' planes are FFmpeg's
    // (`src` a decoder's or ours, `dst` ours), holding the rows swscale reads and writes.
    unsafe {
        ff::sws_setColorspaceDetails(
            s.ctx,
            table(src_709),
            c_int::from(src_full),
            table(dst_709),
            c_int::from(dst_full),
            0,
            1 << 16,
            1 << 16,
        );
        let (sf, df) = (&*src.0, &*dst.0);
        check(
            ff::sws_scale(
                s.ctx,
                sf.data.as_ptr() as *const *const u8,
                sf.linesize.as_ptr(),
                0,
                sf.height,
                df.data.as_ptr(),
                df.linesize.as_ptr(),
            ),
            "scaling",
        )?;
    }
    Ok(())
}
