//! An open encoder: how it takes pictures (system memory or a GPU frames pool), its options and rate control, sending pictures and receiving access units; a stream's parameters.

use super::*;
use idn_common::sync::LockOk as _;

/// How an encoder takes pictures.
pub enum Upload {
    /// VA-API: NV12 copied into a GPU surface from this frames pool.
    Vaapi(BufferRef),
    /// Vulkan Video or CUDA (NVENC): NV12 copied into a frame of this pool, or drawn
    /// into it on the GPU.
    Hw(BufferRef),
    /// Direct3D 12 on the application's device: pictures copied in on the GPU from its
    /// own (§E.27), from this frames pool.
    #[cfg(all(windows, feature = "gpu"))]
    D3d12(BufferRef),
    /// NVENC and others: NV12 in system memory.
    System,
}

/// What to open an encoder with.
pub struct EncoderParams<'a> {
    pub name: &'a str,
    pub width: usize,
    pub height: usize,
    pub fps: (u32, u32),
    pub bitrate: u64,
    pub gop: u32,
    pub device: Option<&'a BufferRef>,
    pub options: &'a [(&'a str, &'a str)],
    /// Parameter sets as extradata (for MP4, MKV and FLV), besides in-band.
    pub global_header: bool,
    /// Planar 4:2:0 (YUV420P) pictures instead of NV12 (software encoders that take
    /// nothing else: OpenH264).
    pub planar: bool,
    /// 10 bits for an HDR picture (R.24): P010 pictures (planar: YUV420P10), HEVC's Main
    /// 10 profile, BT.2020 tagged with this transfer (PQ or HLG).
    pub ten_bit: Option<idn_open_protocol::color::Transfer>,
    /// A Direct3D 12 device (`HwDecode::d3d12_on`) whose textures the encoder takes:
    /// the application's own, its pictures copied in there.
    #[cfg(all(windows, feature = "gpu"))]
    pub d3d12: Option<&'a BufferRef>,
}

/// An open encoder.
pub struct Encoder {
    ctx: *mut ff::AVCodecContext,
    upload: Upload,
    pkt: Packet,
    n: i64,
    /// The driver wouldn't map a surface for writing: copied in as before.
    no_map: bool,
    /// SVT-AV1: opened and freed under [`SVT`].
    svt: bool,
}

// SAFETY: the context, its frames pool and the packet are its own, used through &mut
// self (SVT-AV1's shared state guarded by `SVT`).
unsafe impl Send for Encoder {}

/// SVT-AV1's set-up and tear-down share process-wide state that isn't safe to run on two
/// threads at once (an encoder opening while another opens or closes crashed in
/// `svt_av1_enc_init_handle` on a NULL table, 2026-11-06): its encoders are opened and
/// freed one at a time.
static SVT: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Drop for Encoder {
    fn drop(&mut self) {
        let _one = self.svt.then(|| SVT.lock_ok());
        // SAFETY: the context allocated in `open`, freed once.
        unsafe { ff::avcodec_free_context(&mut self.ctx) };
    }
}

pub(super) fn set_opt(obj: *mut std::ffi::c_void, k: &str, v: &str) -> io::Result<()> {
    let (k2, v2) = (CString::new(k).unwrap(), CString::new(v).unwrap());
    // Options an encoder doesn't have are skipped (they differ between versions).
    // SAFETY: `obj` is a codec context's private data (callers pass `priv_data`);
    // NUL-terminated strings.
    let r = unsafe { ff::av_opt_set(obj, k2.as_ptr(), v2.as_ptr(), ff::AV_OPT_SEARCH_CHILDREN) };
    if r < 0 && r != ff::AVERROR_OPTION_NOT_FOUND {
        return Err(err(r, &format!("option {k}={v}")));
    }
    Ok(())
}

impl Encoder {
    pub fn open(p: &EncoderParams) -> io::Result<Self> {
        let name = CString::new(p.name).unwrap();
        // SAFETY: a NUL-terminated name; the result is checked for null.
        let codec = unsafe { ff::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(io::Error::other(format!(
                "no {} encoder in this FFmpeg",
                p.name
            )));
        }
        // SAFETY: `codec` was found above (not null).
        let ctx = oom(unsafe { ff::avcodec_alloc_context3(codec) })?;
        let pkt = Packet::checked()?;
        let mut enc = Encoder {
            ctx,
            upload: Upload::System,
            pkt,
            n: 0,
            no_map: false,
            svt: p.name.starts_with("libsvtav1"),
        };
        // SAFETY: `ctx` just allocated and not open; the frames context made here is checked
        // and owned (the codec context takes its own reference).
        unsafe {
            let c = &mut *ctx;
            c.width = p.width as c_int;
            c.height = p.height as c_int;
            c.time_base = ff::AVRational {
                num: p.fps.1 as c_int,
                den: p.fps.0 as c_int,
            };
            c.framerate = ff::AVRational {
                num: p.fps.0 as c_int,
                den: p.fps.1 as c_int,
            };
            c.gop_size = p.gop as c_int;
            c.max_b_frames = 0;
            // Pictures above 1080p in four slices, so that software decoders can decode
            // each in parallel (4K H.264 at 20 fps on one thread).
            if p.width * p.height > 1920 * 1080 {
                c.slices = 4;
            }
            c.bit_rate = p.bitrate as i64;
            // (SVT-AV1 takes a maximum rate only with constant quality: its own constant
            // bit rate, set in its options, refuses one.)
            if p.name != "libsvtav1" {
                c.rc_max_rate = p.bitrate as i64;
                // About one frame of buffer: low delay.
                c.rc_buffer_size =
                    (p.bitrate as f64 * p.fps.1 as f64 / p.fps.0.max(1) as f64) as c_int;
            }
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            if p.global_header {
                c.flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
            }
            // What the library's pictures are: BT.709, limited range (§3.10.3).
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            // An HDR picture (R.24): BT.2100.
            if let Some(t) = p.ten_bit {
                c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT2020_NCL;
                c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT2020;
                c.color_trc = match t {
                    idn_open_protocol::color::Transfer::Hlg => {
                        ff::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67
                    }
                    _ => ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084,
                };
                if p.name.starts_with("hevc") {
                    set_opt(c.priv_data, "profile", "main10")?;
                }
            }
            let sw = if p.ten_bit.is_some() {
                ff::AVPixelFormat::AV_PIX_FMT_P010LE
            } else {
                ff::AVPixelFormat::AV_PIX_FMT_NV12
            };
            match p.device {
                Some(dev) => {
                    // The device's frames: VA-API surfaces, Vulkan images (Vulkan Video)
                    // or CUDA memory (NVENC).
                    let (pix, what) = match device_type(dev) {
                        ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN => {
                            (ff::AVPixelFormat::AV_PIX_FMT_VULKAN, "Vulkan")
                        }
                        ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA => {
                            (ff::AVPixelFormat::AV_PIX_FMT_CUDA, "CUDA")
                        }
                        _ => (ff::AVPixelFormat::AV_PIX_FMT_VAAPI, "VA-API"),
                    };
                    let frames = ff::av_hwframe_ctx_alloc(dev.0);
                    if frames.is_null() {
                        return Err(io::Error::other(format!("{what} frames context")));
                    }
                    let frames = BufferRef(frames);
                    let fc = &mut *((*frames.0).data as *mut ff::AVHWFramesContext);
                    fc.format = pix;
                    fc.sw_format = sw;
                    fc.width = p.width as c_int;
                    fc.height = p.height as c_int;
                    fc.initial_pool_size = 8;
                    check(
                        ff::av_hwframe_ctx_init(frames.0),
                        &format!("{what} frames pool"),
                    )?;
                    c.pix_fmt = pix;
                    c.hw_frames_ctx = frames.new_ref();
                    enc.upload = if what == "VA-API" {
                        Upload::Vaapi(frames)
                    } else {
                        Upload::Hw(frames)
                    };
                }
                #[cfg(all(windows, feature = "gpu"))]
                None if p.d3d12.is_some() => {
                    let dev = p.d3d12.expect("checked");
                    let frames = ff::av_hwframe_ctx_alloc(dev.0);
                    if frames.is_null() {
                        return Err(io::Error::other("D3D12 frames context"));
                    }
                    let frames = BufferRef(frames);
                    let fc = &mut *((*frames.0).data as *mut ff::AVHWFramesContext);
                    fc.format = ff::AVPixelFormat::AV_PIX_FMT_D3D12;
                    fc.sw_format = sw;
                    fc.width = p.width as c_int;
                    fc.height = p.height as c_int;
                    fc.initial_pool_size = 8;
                    // `AVD3D12VAFramesContext`: its format first (left for FFmpeg), then
                    // the resource flags (`flags` in FFmpeg 7, `resource_flags` in 8):
                    // none needed for a copy in, so none set.
                    check(ff::av_hwframe_ctx_init(frames.0), "D3D12 frames pool")?;
                    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_D3D12;
                    c.hw_frames_ctx = frames.new_ref();
                    enc.upload = Upload::D3d12(frames);
                }
                None if p.planar && p.ten_bit.is_some() => {
                    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV420P10LE
                }
                None if p.planar => c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
                None => c.pix_fmt = sw,
            }
            for (k, v) in p.options {
                set_opt(c.priv_data, k, v)?;
            }
            let _one = enc.svt.then(|| SVT.lock_ok());
            check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                &format!("opening {}", p.name),
            )?;
        }
        Ok(enc)
    }

    /// The stream's parameters, for a muxer.
    pub fn parameters(&self) -> io::Result<Params> {
        Params::from_context(self.ctx)
    }

    /// An input surface (VA-API, or Direct3D 12 on the application's device), not
    /// mapped: for writing from the GPU.
    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
    pub fn hw_surface(&mut self) -> Option<Frame> {
        let frames = match &self.upload {
            Upload::Vaapi(f) | Upload::Hw(f) => f,
            #[cfg(windows)]
            Upload::D3d12(f) => f,
            Upload::System => return None,
        };
        let hw = Frame::new().ok()?;
        // SAFETY: a frame of this encoder's pool.
        (unsafe { ff::av_hwframe_get_buffer(frames.0, hw.0, 0) } >= 0).then_some(hw)
    }

    /// A VA-API surface for the next picture and the same surface mapped for writing as
    /// NV12: the picture is written straight into it (no NV12 in system memory copied in
    /// afterwards: a second pass over every 4K picture), the map
    /// dropped, then the surface given to [`Encoder::encode_surface`]. `None` without
    /// VA-API, or where the driver doesn't map surfaces for writing.
    pub fn surface(&mut self) -> Option<(Frame, Frame)> {
        let Upload::Vaapi(frames) = &self.upload else {
            return None;
        };
        if self.no_map {
            return None;
        }
        let hw = Frame::new().ok()?;
        let m = Frame::new().ok()?;
        // SAFETY: frames of ours; `hw` from this encoder's pool, mapped into `m`.
        unsafe {
            if ff::av_hwframe_get_buffer(frames.0, hw.0, 0) < 0 {
                return None;
            }
            (*m.0).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            let flags = ff::AV_HWFRAME_MAP_WRITE as c_int | ff::AV_HWFRAME_MAP_OVERWRITE as c_int;
            if ff::av_hwframe_map(m.0, hw.0, flags) < 0 {
                idn_open_protocol::log::log(
                    "VA-API: surfaces can't be mapped for writing: copied in",
                );
                self.no_map = true;
                return None;
            }
        }
        Some((hw, m))
    }

    /// Encodes a surface from [`Encoder::surface`], written and unmapped, as
    /// [`Encoder::encode`] does a picture in system memory.
    pub fn encode_surface(
        &mut self,
        hw: Frame,
        keyframe: bool,
        captions: &[u8],
    ) -> io::Result<Vec<(Vec<u8>, bool)>> {
        // SAFETY: a frame of ours.
        unsafe { (*hw.0).pts = self.n };
        self.send(&hw, keyframe, captions)
    }

    /// Encodes an NV12 picture, with closed captions (A/53 `cc_data`; empty: none);
    /// returns the access units out: (data, keyframe).
    pub fn encode(
        &mut self,
        nv12: &Frame,
        keyframe: bool,
        captions: &[u8],
    ) -> io::Result<Vec<(Vec<u8>, bool)>> {
        let input = match &self.upload {
            Upload::System => {
                // SAFETY: a frame of ours.
                unsafe { (*nv12.0).pts = self.n };
                None
            }
            #[cfg(all(windows, feature = "gpu"))]
            Upload::D3d12(frames) => {
                let hw = Frame::new()?;
                // SAFETY: `hw` from this encoder's pool, filled from `nv12` (of the pool's format).
                unsafe {
                    check(
                        ff::av_hwframe_get_buffer(frames.0, hw.0, 0),
                        "D3D12 surface",
                    )?;
                    check(ff::av_hwframe_transfer_data(hw.0, nv12.0, 0), "upload")?;
                    (*hw.0).pts = self.n;
                }
                Some(hw)
            }
            Upload::Vaapi(frames) | Upload::Hw(frames) => {
                let hw = Frame::new()?;
                // SAFETY: `hw` from this encoder's pool, filled from `nv12` (of the pool's format).
                unsafe {
                    check(
                        ff::av_hwframe_get_buffer(frames.0, hw.0, 0),
                        "a GPU surface",
                    )?;
                    check(ff::av_hwframe_transfer_data(hw.0, nv12.0, 0), "upload")?;
                    (*hw.0).pts = self.n;
                }
                Some(hw)
            }
        };
        self.send(input.as_ref().unwrap_or(nv12), keyframe, captions)
    }

    /// The picture `f` (its pts set) to the encoder, with its captions and keyframe.
    fn send(
        &mut self,
        f: &Frame,
        keyframe: bool,
        captions: &[u8],
    ) -> io::Result<Vec<(Vec<u8>, bool)>> {
        // SAFETY: a frame of ours; the side data is allocated with `captions.len()` bytes
        // before they are copied in.
        unsafe {
            // Captions ride as side data the encoder turns into SEI; the system frame is
            // used again, so last picture's are taken off first.
            ff::av_frame_remove_side_data(f.0, ff::AVFrameSideDataType::AV_FRAME_DATA_A53_CC);
            if !captions.is_empty() {
                let sd = ff::av_frame_new_side_data(
                    f.0,
                    ff::AVFrameSideDataType::AV_FRAME_DATA_A53_CC,
                    captions.len(),
                );
                if !sd.is_null() {
                    std::ptr::copy_nonoverlapping(captions.as_ptr(), (*sd).data, captions.len());
                }
            }
            // An I picture is an IDR with these encoders (VA-API always; NVENC with
            // forced-idr).
            (*f.0).pict_type = if keyframe {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
        }
        self.n += 1;
        // SAFETY: our open encoder and a frame of ours.
        check(unsafe { ff::avcodec_send_frame(self.ctx, f.0) }, "encode")?;
        self.drain()
    }

    /// The end: the pictures it still holds encoded and given out (a file's last ones);
    /// it takes no more after this.
    pub fn flush(&mut self) -> io::Result<Vec<(Vec<u8>, bool)>> {
        // SAFETY: our open encoder; a null frame asks it to finish.
        let r = unsafe { ff::avcodec_send_frame(self.ctx, std::ptr::null()) };
        if r < 0 && r != ff::AVERROR_EOF {
            check(r, "finishing")?;
        }
        self.drain()
    }

    fn drain(&mut self) -> io::Result<Vec<(Vec<u8>, bool)>> {
        let mut out = Vec::new();
        loop {
            // SAFETY: our open encoder and our packet.
            let r = unsafe { ff::avcodec_receive_packet(self.ctx, self.pkt.0) };
            if r == EAGAIN || r == ff::AVERROR_EOF {
                return Ok(out);
            }
            check(r, "encode")?;
            // SAFETY: the packet just received, copied out before it is unreferenced.
            unsafe {
                let data = packet_bytes(self.pkt.0);
                let key = (*self.pkt.0).flags & ff::AV_PKT_FLAG_KEY as c_int != 0;
                ff::av_packet_unref(self.pkt.0);
                if !data.is_empty() {
                    out.push((data, key));
                }
            }
        }
    }
}

/// Owned codec parameters (what a muxer needs to describe a stream).
pub struct Params(pub *mut ff::AVCodecParameters);

// SAFETY: owned parameters, plain data, only read through &self.
unsafe impl Send for Params {}
unsafe impl Sync for Params {}

impl Params {
    pub fn from_context(ctx: *const ff::AVCodecContext) -> io::Result<Self> {
        // SAFETY: a plain allocation (checked by `oom`).
        let p = Params(oom(unsafe { ff::avcodec_parameters_alloc() })?);
        check(
            // SAFETY: `p` allocated above; `ctx` is the caller's codec context (an encoder's).
            unsafe { ff::avcodec_parameters_from_context(p.0, ctx) },
            "codec parameters",
        )?;
        Ok(p)
    }
}

impl Params {
    /// The stream's bit rate the container states, bits a second (0: not stated).
    pub fn bit_rate(&self) -> i64 {
        // SAFETY: parameters of ours.
        unsafe { (*self.0).bit_rate.max(0) }
    }

    /// The codec's own header (H.264/HEVC parameter sets, as the encoder wrote them).
    pub fn extradata(&self) -> Vec<u8> {
        // SAFETY: parameters of ours; `extradata_size` bytes at `extradata` when it is set.
        unsafe {
            let p = &*self.0;
            if p.extradata.is_null() || p.extradata_size <= 0 {
                return Vec::new();
            }
            std::slice::from_raw_parts(p.extradata, p.extradata_size as usize).to_vec()
        }
    }

    /// A copy.
    pub fn from_parameters(p: &Params) -> io::Result<Self> {
        // SAFETY: a plain allocation (checked by `oom`).
        let c = Params(oom(unsafe { ff::avcodec_parameters_alloc() })?);
        check(
            // SAFETY: `c` allocated above; `p` is live parameters of ours.
            unsafe { ff::avcodec_parameters_copy(c.0, p.0) },
            "codec parameters",
        )?;
        Ok(c)
    }
}

impl Drop for Params {
    fn drop(&mut self) {
        // SAFETY: parameters of ours (or null, which the free takes), freed once.
        unsafe { ff::avcodec_parameters_free(&mut self.0) };
    }
}
