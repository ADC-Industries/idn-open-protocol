//! Encoders: probing which work, opening them for streams and files (on the GPU where
//! the pictures are), and handing pictures in.

use super::*;

#[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
pub(crate) mod gpu;
#[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
use gpu::GpuWriter;

pub(super) fn test_config(codec: Codec) -> EncoderConfig {
    EncoderConfig {
        codec,
        width: 256,
        height: 144,
        frame_rate: (30, 1),
        bitrate: 1_000_000,
        keyframe_interval: 60,
    }
}

pub(super) fn test_picture() -> Vec<Plane> {
    let p = |w: usize, h: usize, v: u8| Plane {
        width: w,
        height: h,
        data: vec![v; w * h],
    };
    vec![p(256, 144, 64), p(128, 144, 128), p(128, 144, 128)]
}

pub(super) fn open_encoder(
    device: &Option<Arc<ffi::BufferRef>>,
    backend: Backend,
    cfg: &EncoderConfig,
) -> io::Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(open_ff_encoder(device, backend, cfg, false)?))
}

/// An encoder for a muxer (gateways, R.12): parameter sets also as extradata.
pub(crate) struct MuxEncoder {
    pub encoder: Box<dyn VideoEncoder>,
    pub(super) params: Option<ffi::Params>,
}

impl MuxEncoder {
    pub fn parameters(&self) -> io::Result<ffi::Params> {
        match &self.params {
            Some(p) => ffi::Params::from_parameters(p),
            None => Err(io::Error::other("no parameters")),
        }
    }
}

/// An encoder for files and streams of an HDR program (R.24): 10 bits, HEVC (or AV1,
/// where that is the codec: H.264's GPU encoders have no 10 bits, so HEVC instead),
/// tagged BT.2100 with `transfer`.
pub(crate) fn open_encoder_for_files_hdr(
    ff: &Ffmpeg,
    cfg: &EncoderConfig,
    transfer: idn_open_protocol::color::Transfer,
) -> io::Result<MuxEncoder> {
    let mut cfg = cfg.clone();
    if cfg.codec == Codec::H264 {
        cfg.codec = Codec::Hevc;
    }
    let b = ff
        .encoders
        .iter()
        .find(|e| e.0 == cfg.codec)
        .map(|e| e.1)
        .ok_or_else(|| io::Error::other(format!("no hardware {} encoder", cfg.codec.name())))?;
    let e = open_ff_encoder_bits(&ff.device_for(b), b, &cfg, true, Some(transfer))?;
    let params = e.enc.parameters()?;
    Ok(MuxEncoder {
        encoder: Box::new(e),
        params: Some(params),
    })
}

pub(crate) fn open_encoder_for_files(ff: &Ffmpeg, cfg: &EncoderConfig) -> io::Result<MuxEncoder> {
    let b = ff
        .encoders
        .iter()
        .find(|e| e.0 == cfg.codec)
        .map(|e| e.1)
        .ok_or_else(|| io::Error::other(format!("no hardware {} encoder", cfg.codec.name())))?;
    let e = open_ff_encoder(&ff.device_for(b), b, cfg, true)?;
    let params = e.enc.parameters()?;
    Ok(MuxEncoder {
        encoder: Box::new(e),
        params: Some(params),
    })
}

/// An encoder for a muxer taking pictures on the application's GPU `hand` (§E.27):
/// Windows, FFmpeg's Direct3D 12 encoder on that very device. `None` where there is
/// none (Linux: VA-API's encoder takes them already; another backend, or it fails).
#[cfg(feature = "gpu")]
pub(crate) fn open_encoder_for_files_on_gpu(
    cfg: &EncoderConfig,
    hand: &idn_open_protocol::gpu::GpuHandIn,
) -> Option<MuxEncoder> {
    #[cfg(windows)]
    {
        let name = match cfg.codec {
            Codec::H264 => "h264_d3d12va",
            Codec::Hevc => "hevc_d3d12va",
            Codec::Av1 => "av1_d3d12va",
            #[allow(unreachable_patterns)]
            _ => return None,
        };
        let hw = d3d12_gpu::device_of(hand).and_then(|d| ffi::HwDecode::d3d12_on(d).ok())?;
        let opened = ffi::Encoder::open(&ffi::EncoderParams {
            name,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.frame_rate,
            bitrate: cfg.bitrate,
            gop: cfg.keyframe_interval,
            device: None,
            options: &[],
            global_header: true,
            planar: false,
            ten_bit: None,
            d3d12: Some(&hw.dev),
        });
        let enc = match opened {
            Ok(e) => e,
            Err(e) => {
                idn_open_protocol::log::log(&format!(
                    "{} encoder on the GPU (Direct3D 12): {e}; the usual one",
                    cfg.codec.name()
                ));
                return None;
            }
        };
        let params = enc.parameters().ok()?;
        idn_open_protocol::log::log(&format!(
            "{} encoder: Direct3D 12 on the application's GPU",
            cfg.codec.name()
        ));
        let e = FfEncoder {
            enc,
            nv12: ffi::Frame::nv12(cfg.width, cfg.height).ok()?,
            p010: None,
            i420: None,
            codec: cfg.codec,
            size: (cfg.width, cfg.height),
            timing: (0.0, 0.0, 0),
            timing_since: std::time::Instant::now(),
            _device: Some(Arc::new(ffi::BufferRef(hw.dev.new_ref()))),
            captions: Vec::new(),
            gpu_writer: None,
        };
        Some(MuxEncoder {
            encoder: Box::new(e),
            params: Some(params),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (cfg, hand);
        None
    }
}

pub(super) fn open_ff_encoder(
    device: &Option<Arc<ffi::BufferRef>>,
    backend: Backend,
    cfg: &EncoderConfig,
    global_header: bool,
) -> io::Result<FfEncoder> {
    open_ff_encoder_bits(device, backend, cfg, global_header, None)
}

/// [`open_ff_encoder`], for 10-bit HDR pictures with `ten_bit` (R.24).
pub(super) fn open_ff_encoder_bits(
    device: &Option<Arc<ffi::BufferRef>>,
    backend: Backend,
    cfg: &EncoderConfig,
    global_header: bool,
    ten_bit: Option<idn_open_protocol::color::Transfer>,
) -> io::Result<FfEncoder> {
    let options = backend.options(cfg.codec, cfg.width * cfg.height);
    let enc = ffi::Encoder::open(&ffi::EncoderParams {
        name: backend.encoder_name(cfg.codec),
        width: cfg.width,
        height: cfg.height,
        fps: cfg.frame_rate,
        bitrate: cfg.bitrate,
        gop: cfg.keyframe_interval,
        device: match backend {
            Backend::Vaapi | Backend::Vulkan | Backend::NvencCuda => device.as_deref(),
            // Pictures from system memory: the drivers upload them.
            Backend::Nvenc
            | Backend::Amf
            | Backend::Qsv
            | Backend::OpenH264
            | Backend::X264
            | Backend::Kvazaar
            | Backend::SvtAv1 => None,
        },
        options: &options,
        global_header,
        // OpenH264, Kvazaar and SVT-AV1 take planar 4:2:0 only.
        planar: backend.planar(),
        ten_bit,
        #[cfg(all(windows, feature = "gpu"))]
        d3d12: None,
    })?;
    Ok(FfEncoder {
        enc,
        nv12: ffi::Frame::nv12(cfg.width, cfg.height)?,
        p010: ten_bit
            .map(|_| ffi::Frame::p010(cfg.width, cfg.height))
            .transpose()?,
        i420: backend
            .planar()
            .then(|| ffi::Frame::i420(cfg.width, cfg.height))
            .transpose()?,
        codec: cfg.codec,
        size: (cfg.width, cfg.height),
        timing: (0.0, 0.0, 0),
        timing_since: std::time::Instant::now(),
        _device: device.clone(),
        captions: Vec::new(),
        #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
        gpu_writer: None,
    })
}

impl EncoderFactory for Ffmpeg {
    fn codecs(&self) -> Vec<Codec> {
        self.encoders.iter().map(|e| e.0).collect()
    }

    fn open(&self, cfg: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>> {
        let b = self
            .encoders
            .iter()
            .find(|e| e.0 == cfg.codec)
            .map(|e| e.1)
            .ok_or_else(|| io::Error::other(format!("no hardware {} encoder", cfg.codec.name())))?;
        open_encoder(&self.device_for(b), b, cfg)
    }
}

pub(super) struct FfEncoder {
    /// Pictures on the application's GPU drawn into the input surfaces (§E.27); `Err`:
    /// tried and it can't here, not tried again. First, so dropped first: its Vulkan and
    /// CUDA writers use FFmpeg's device (waiting for it, freeing on it) as they go, and
    /// `enc` and `_device` hold the last references to that device.
    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
    pub(super) gpu_writer: Option<Result<GpuWriter, ()>>,
    pub(super) enc: ffi::Encoder,
    pub(super) nv12: ffi::Frame,
    /// Opened for 10 bits (R.24): its P010 picture.
    pub(super) p010: Option<ffi::Frame>,
    /// For encoders that take planar 4:2:0 (OpenH264): the picture again, deinterleaved.
    pub(super) i420: Option<ffi::Frame>,
    pub(super) codec: Codec,
    pub(super) size: (usize, usize),
    /// Seconds converting, seconds uploading and encoding, pictures: logged every 10 s.
    pub(super) timing: (f64, f64, u32),
    pub(super) timing_since: std::time::Instant,
    /// Keeps the VA-API device open as long as the encoder.
    pub(super) _device: Option<Arc<ffi::BufferRef>>,
    /// Closed captions for the next picture (A/53 `cc_data`).
    pub(super) captions: Vec<u8>,
}

impl FfEncoder {
    /// The access units FFmpeg gave, each keyframe's parameter sets beside it.
    /// The end of a file: the pictures it still holds (media::convert).
    pub(crate) fn finish(&mut self) -> io::Result<Vec<Encoded>> {
        let out = self.enc.flush()?;
        Ok(self.encoded(out))
    }

    /// Its stream's parameters, for a muxer.
    pub(crate) fn parameters(&self) -> io::Result<ffi::Params> {
        self.enc.parameters()
    }

    pub(super) fn encoded(&self, out: Vec<(Vec<u8>, bool)>) -> Vec<Encoded> {
        out.into_iter()
            .map(|(data, keyframe)| {
                let extra = if keyframe {
                    parameter_sets(self.codec, &data)
                } else {
                    Vec::new()
                };
                Encoded {
                    data,
                    keyframe,
                    extra,
                }
            })
            .collect()
    }
}

impl VideoEncoder for FfEncoder {
    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
    fn encode_gpu(
        &mut self,
        picture: &idn_open_protocol::gpu::GpuPicture,
        hand: &idn_open_protocol::gpu::GpuHandIn,
        _pts: i64,
        force_keyframe: bool,
    ) -> Option<io::Result<Vec<Encoded>>> {
        // (`IDN_GPU_ENCODE=0`: read back as before, to compare.)
        if self.i420.is_some()
            || matches!(self.gpu_writer, Some(Err(())))
            || idn_common::switches::off("IDN_GPU_ENCODE")
        {
            return None;
        }
        let hw = self.enc.hw_surface()?;
        if self.gpu_writer.is_none() {
            self.gpu_writer = Some(GpuWriter::new(hand, &hw).map_err(|e| {
                idn_open_protocol::log::log(&format!(
                    "{} encoder: {e}; pictures read back to encode",
                    self.codec.name()
                ));
            }));
        }
        let t0 = std::time::Instant::now();
        let w = self.gpu_writer.as_mut()?.as_mut().ok()?;
        if let Err(e) = w.write(&hw, picture, self.size) {
            idn_open_protocol::log::log(&format!(
                "{} encoder: {e}; pictures read back to encode",
                self.codec.name()
            ));
            self.gpu_writer = Some(Err(()));
            return None;
        }
        if self.timing.2 == 0 && self.timing.0 == 0.0 {
            idn_open_protocol::log::log(&format!(
                "{} encoder: pictures drawn into it on the GPU (none read back)",
                self.codec.name()
            ));
        }
        let t1 = std::time::Instant::now();
        let cc = std::mem::take(&mut self.captions);
        let out = match self.enc.encode_surface(hw, force_keyframe, &cc) {
            Ok(o) => o,
            Err(e) => return Some(Err(e)),
        };
        self.timing.0 += (t1 - t0).as_secs_f64();
        self.timing.1 += t1.elapsed().as_secs_f64();
        self.timing.2 += 1;
        Some(Ok(self.encoded(out)))
    }

    fn captions_next(&mut self, cc: &[u8]) {
        self.captions = cc.to_vec();
    }

    fn encode16(
        &mut self,
        planes: &[idn_open_protocol::shq::Plane16],
        _pts: i64,
        force_keyframe: bool,
    ) -> io::Result<Vec<Encoded>> {
        let (w, h) = self.size;
        let Some(f) = self.p010.as_mut() else {
            return Err(io::Error::other("an encoder for 8 bits"));
        };
        let cw = w.div_ceil(2);
        if planes.len() < 3
            || planes[0].data.len() < w * h
            || planes[1].data.len() < cw * h
            || planes[2].data.len() < cw * h
        {
            return Err(io::Error::other(format!(
                "16-bit picture for a {w}x{h} encoder"
            )));
        }
        f.make_writable()?;
        p216_to_p010(planes, f, (w, h));
        let cc = std::mem::take(&mut self.captions);
        let out = self.enc.encode(f, force_keyframe, &cc)?;
        Ok(self.encoded(out))
    }

    fn encode(
        &mut self,
        planes: &[Plane],
        _layout: Fourcc,
        _pts: i64,
        force_keyframe: bool,
    ) -> io::Result<Vec<Encoded>> {
        if planes.len() < 3
            || (planes[0].width, planes[0].height) != self.size
            || planes[0].data.len() < self.size.0 * self.size.1
        {
            return Err(io::Error::other(format!(
                "picture {}x{} for a {}x{} encoder",
                planes.first().map_or(0, |p| p.width),
                planes.first().map_or(0, |p| p.height),
                self.size.0,
                self.size.1
            )));
        }
        let t0 = std::time::Instant::now();
        let cc = std::mem::take(&mut self.captions);
        // VA-API: the picture written straight into a mapped surface (one pass, not
        // NV12 here and a copy into the surface).
        let mapped = if self.i420.is_none() {
            self.enc.surface()
        } else {
            None
        };
        let (t1, out) = if let Some((hw, m)) = mapped {
            to_nv12(&planes[..3], &m);
            drop(m);
            let t1 = std::time::Instant::now();
            (t1, self.enc.encode_surface(hw, force_keyframe, &cc)?)
        } else {
            self.nv12.make_writable()?;
            to_nv12(&planes[..3], &self.nv12);
            if let Some(f) = self.i420.as_mut() {
                f.make_writable()?;
                nv12_to_i420(&self.nv12, f, self.size);
            }
            let t1 = std::time::Instant::now();
            (
                t1,
                self.enc.encode(
                    self.i420.as_ref().unwrap_or(&self.nv12),
                    force_keyframe,
                    &cc,
                )?,
            )
        };
        self.timing.0 += (t1 - t0).as_secs_f64();
        self.timing.1 += t1.elapsed().as_secs_f64();
        self.timing.2 += 1;
        if self.timing_since.elapsed() >= std::time::Duration::from_secs(10) {
            let n = self.timing.2.max(1) as f64;
            idn_open_protocol::log::log(&format!(
                "{} encoder {}x{}: {:.1} pictures/s, per picture convert {:.1} ms, upload + encode {:.1} ms",
                self.codec.name(),
                self.size.0,
                self.size.1,
                n / self.timing_since.elapsed().as_secs_f64(),
                self.timing.0 * 1e3 / n,
                self.timing.1 * 1e3 / n
            ));
            self.timing = (0.0, 0.0, 0);
            self.timing_since = std::time::Instant::now();
        }
        Ok(self.encoded(out))
    }
}

/// A keyframe's parameter sets (H.264/HEVC) or sequence header (AV1): the extra data
/// (§7.1.1, §E.18.2).
pub(super) fn parameter_sets(c: Codec, data: &[u8]) -> Vec<u8> {
    match c {
        Codec::Av1 => av1_sequence_header(data).unwrap_or_default(),
        _ => idn_open_protocol::hx::access_units(data, c == Codec::Hevc)
            .into_iter()
            .next()
            .map(|au| au.params)
            .unwrap_or_default(),
    }
}

/// The sequence header OBU of an AV1 temporal unit (low-overhead format).
pub(super) fn av1_sequence_header(mut d: &[u8]) -> Option<Vec<u8>> {
    while !d.is_empty() {
        let h = d[0];
        let (kind, ext, has_size) = ((h >> 3) & 0x0f, h & 0x04 != 0, h & 0x02 != 0);
        let mut at = 1 + usize::from(ext);
        let size = if has_size {
            let mut v = 0usize;
            for i in 0..8 {
                let b = *d.get(at)?;
                at += 1;
                v |= ((b & 0x7f) as usize) << (7 * i);
                if b & 0x80 == 0 {
                    break;
                }
            }
            v
        } else {
            d.len() - at
        };
        let end = at.checked_add(size).filter(|&e| e <= d.len())?;
        if kind == 1 {
            return Some(d[..end].to_vec());
        }
        d = &d[end..];
    }
    None
}

/// 16-bit Y'CbCr 4:2:2 planes into a P010 frame: luma as it is (P010 keeps the top 10
/// bits), chroma interleaved and averaged down to 4:2:0 (two rows into one).
/// A 16-bit picture's planes (Y, then 4:2:2 Cb and Cr; alpha left out) scaled to
/// `w` × `h` by swscale (area-averaged, as the 8-bit pictures are): an HDR output whose
/// recording or stream has a size of its own. The scaler and its frame are kept between
/// pictures.
pub(crate) fn scale_p216(
    src: &[idn_open_protocol::shq::Plane16],
    (w, h): (usize, usize),
    scaler: &mut Option<crate::ffi::Scaler>,
    scaled: &mut Option<crate::ffi::Frame>,
) -> io::Result<Vec<idn_open_protocol::shq::Plane16>> {
    use ffmpeg_sys_next as ff;
    let fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV422P16LE;
    let view = crate::ffi::Frame::view16(src, fmt)?;
    if scaled
        .as_ref()
        .is_none_or(|d| d.size() != (w, h) || d.format() != fmt)
    {
        *scaled = Some(crate::ffi::Frame::alloc(w, h, fmt)?);
    }
    let dst = scaled.as_ref().expect("made above");
    crate::ffi::Scaler::run(scaler, view.as_ref(), dst)?;
    let cw = w.div_ceil(2);
    Ok(vec![
        dst.to_plane16(0, w, h),
        dst.to_plane16(1, cw, h),
        dst.to_plane16(2, cw, h),
    ])
}

pub(super) fn p216_to_p010(
    src: &[idn_open_protocol::shq::Plane16],
    f: &ffi::Frame,
    (w, h): (usize, usize),
) {
    let cw = w.div_ceil(2);
    let (y, ys) = f.plane(0);
    let (uv, uvs) = f.plane(1);
    // SAFETY: the frame's planes, allocated for w × h P010 (rows of `ys` and `uvs` bytes).
    unsafe {
        for r in 0..h {
            let row = std::slice::from_raw_parts_mut(y.add(r * ys) as *mut u16, w);
            row.copy_from_slice(&src[0].data[r * w..r * w + w]);
        }
        for r in 0..h.div_ceil(2) {
            let (a, b) = (2 * r, (2 * r + 1).min(h - 1));
            let row = std::slice::from_raw_parts_mut(uv.add(r * uvs) as *mut u16, 2 * cw);
            for c in 0..cw {
                let avg =
                    |p: &[u16]| (p[a * cw + c] as u32 + p[b * cw + c] as u32).div_ceil(2) as u16;
                row[2 * c] = avg(&src[1].data);
                row[2 * c + 1] = avg(&src[2].data);
            }
        }
    }
}

/// Y'CbCr planes of any subsampling into the NV12 frame (chroma averaged down to
/// 4:2:0).
/// NV12's interleaved chroma into I420's two planes.
pub(super) fn nv12_to_i420(src: &ffi::Frame, dst: &ffi::Frame, (w, h): (usize, usize)) {
    let ((sy, sys), (suv, suvs)) = (src.plane(0), src.plane(1));
    let ((dy, dys), (du, dus), (dv, dvs)) = (dst.plane(0), dst.plane(1), dst.plane(2));
    // SAFETY: `src` is NV12 and `dst` I420, both allocated at the encoder's size (w × h),
    // so every row read or written is inside its plane.
    unsafe {
        for r in 0..h {
            std::ptr::copy_nonoverlapping(sy.add(r * sys), dy.add(r * dys), w);
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        for r in 0..ch {
            let uv = std::slice::from_raw_parts(suv.add(r * suvs), cw * 2);
            let (u, v) = (
                std::slice::from_raw_parts_mut(du.add(r * dus), cw),
                std::slice::from_raw_parts_mut(dv.add(r * dvs), cw),
            );
            for (i, p) in uv.as_chunks::<2>().0.iter().enumerate() {
                u[i] = p[0];
                v[i] = p[1];
            }
        }
    }
}

pub(super) fn to_nv12(planes: &[Plane], f: &ffi::Frame) {
    let (y, cb, cr) = (&planes[0], &planes[1], &planes[2]);
    let (w, h) = (y.width, y.height);
    let (dy, sy) = f.plane(0);
    for row in 0..h {
        let src = &y.data[row * w..(row + 1) * w];
        // SAFETY: row `row` of the NV12 frame's Y plane, w bytes (the frame is w × h: the
        // encoder's own or its mapped surface).
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dy.add(row * sy), w) };
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (duv, suv) = f.plane(1);
    // Chroma samples per NV12 sample, across and down: 1 (already halved) or 2.
    let fx = if cb.width < w { 1 } else { 2 };
    let fy = if cb.height < h { 1 } else { 2 };
    // 4:2:2 (two rows averaged) and 4:2:0 (rows as they are) without per-sample index
    // arithmetic: the general loop below took several ms for a 4K picture.
    if fx == 1 && cb.width == cw && cr.width == cw {
        for j in 0..ch {
            // SAFETY: CbCr row j of the NV12 frame (ch rows of 2 × cw bytes at stride `suv`).
            let out = unsafe { std::slice::from_raw_parts_mut(duv.add(j * suv), cw * 2) };
            let r0 = (j * fy).min(cb.height - 1);
            let r1 = (j * fy + fy - 1).min(cb.height - 1);
            fn row(p: &Plane, r: usize, cw: usize) -> &[u8] {
                &p.data[r * cw..(r + 1) * cw]
            }
            let (b0, b1) = (row(cb, r0, cw), row(cb, r1, cw));
            let (c0, c1) = (row(cr, r0, cw), row(cr, r1, cw));
            for (i, o) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                o[0] = (b0[i] as u16 + b1[i] as u16).div_ceil(2) as u8;
                o[1] = (c0[i] as u16 + c1[i] as u16).div_ceil(2) as u8;
            }
        }
        return;
    }
    for j in 0..ch {
        // SAFETY: CbCr row j of the NV12 frame (ch rows of 2 × cw bytes at stride `suv`).
        let out = unsafe { std::slice::from_raw_parts_mut(duv.add(j * suv), cw * 2) };
        let r0 = (j * fy).min(cb.height - 1);
        let r1 = (j * fy + fy - 1).min(cb.height - 1);
        for i in 0..cw {
            let c0 = (i * fx).min(cb.width - 1);
            let c1 = (i * fx + fx - 1).min(cb.width - 1);
            let avg = |p: &Plane| {
                let at = |r: usize, c: usize| p.data[r * p.width + c] as u32;
                ((at(r0, c0) + at(r0, c1) + at(r1, c0) + at(r1, c1) + 2) / 4) as u8
            };
            out[2 * i] = avg(cb);
            out[2 * i + 1] = avg(cr);
        }
    }
}

#[cfg(test)]
mod p216_tests {
    use super::scale_p216;
    use idn_open_protocol::shq::Plane16;

    fn picture(w: usize, h: usize, y: u16, c: u16) -> Vec<Plane16> {
        let cw = w.div_ceil(2);
        vec![
            Plane16 {
                width: w,
                height: h,
                data: vec![y; w * h],
            },
            Plane16 {
                width: cw,
                height: h,
                data: vec![c; cw * h],
            },
            Plane16 {
                width: cw,
                height: h,
                data: vec![c; cw * h],
            },
        ]
    }

    /// A flat 16-bit picture scaled down stays flat, at its sizes (4:2:2: chroma full height).
    #[test]
    fn a_16_bit_picture_scales() {
        let src = picture(1920, 1080, 40_000, 30_000);
        let (mut s, mut f) = (None, None);
        let out = scale_p216(&src, (1280, 720), &mut s, &mut f).unwrap();
        assert_eq!(
            (out[0].width, out[0].height, out[1].width, out[1].height),
            (1280, 720, 640, 720)
        );
        for (p, want) in out.iter().zip([40_000u16, 30_000, 30_000]) {
            assert!(
                p.data.iter().all(|&v| v.abs_diff(want) <= 2),
                "{:?}",
                &p.data[..4]
            );
        }
    }

    #[test]
    #[ignore = "timing: cargo test --release -p idn-ffmpeg --lib scale_p216_speed -- --ignored --nocapture"]
    fn scale_p216_speed() {
        for ((sw, sh), (dw, dh)) in [((1920, 1080), (1280, 720)), ((3840, 2160), (1920, 1080))] {
            let src = picture(sw, sh, 40_000, 30_000);
            let (mut s, mut f) = (None, None);
            let _ = scale_p216(&src, (dw, dh), &mut s, &mut f).unwrap();
            let t = std::time::Instant::now();
            for _ in 0..20 {
                std::hint::black_box(scale_p216(&src, (dw, dh), &mut s, &mut f).unwrap());
            }
            println!(
                "scale_p216 {sw}x{sh} → {dw}x{dh}: {:.2} ms",
                t.elapsed().as_secs_f64() * 50.0
            );
        }
    }
}
