//! Decoders: opening one for a codec (on the GPU where it decodes it), and pictures out
//! of FFmpeg as the library's, on the GPU or the CPU.

use super::*;

impl Ffmpeg {
    /// A decoder for `codec`, on the GPU where asked for and it decodes the codec.
    pub(super) fn open_ff(&self, codec: Codec) -> io::Result<FfDecoder> {
        if !self.decoders.contains(&codec) {
            return Err(io::Error::other(format!("no {} decoder", codec.name())));
        }
        // The GPU where asked for and it decodes this codec, else software.
        let choice = self.decoding();
        let dec = self
            .hw_decode
            .as_deref()
            .filter(|_| choice == DecoderChoice::Gpu)
            .and_then(|hw| ffi::Decoder::open(codec_id(codec), Some(hw)).ok());
        let dec = match dec {
            Some(d) => d,
            None => ffi::Decoder::open(codec_id(codec), None)?,
        };
        idn_open_protocol::log::log(&format!(
            "{} decoder: {}",
            codec.name(),
            match (dec.hardware, choice, &self.hw_decode) {
                (true, _, Some(hw)) => format!("GPU ({})", hw.name),
                (false, DecoderChoice::Auto, Some(hw)) => {
                    format!("software, all cores ({} if software falls behind)", hw.name)
                }
                (false, DecoderChoice::Gpu, None) => {
                    "software, all cores (no GPU decoder found)".to_owned()
                }
                _ => "software, all cores".to_owned(),
            }
        ));
        let downloads = if dec.hardware {
            Some(Downloads::start(codec)?)
        } else {
            None
        };
        Ok(FfDecoder {
            dec,
            codec,
            hw: self.hw_decode.clone(),
            downloads,
            sw: ffi::Frame::new()?,
            auto: (choice == DecoderChoice::Auto && self.hw_decode.is_some()).then(Pace::default),
            thin: self.picture_rate.and_then(Thin::new),
            #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
            gpu: None,
            #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
            last_gpu: None,
            #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
            hand_in: None,
        })
    }
}

impl DecoderFactory for Ffmpeg {
    fn codecs(&self) -> Vec<Codec> {
        self.decoders.clone()
    }

    fn open(&self, codec: Codec) -> io::Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(self.open_ff(codec)?))
    }

    /// As [`open`](Self::open); a VA-API decoder's pictures then stay on `gpu` (§E.27),
    /// converted there, where its device imports them (else downloaded as before).
    #[cfg(feature = "gpu")]
    fn open_gpu(
        &self,
        codec: Codec,
        gpu: &idn_open_protocol::gpu::GpuHandIn,
    ) -> io::Result<Box<dyn VideoDecoder>> {
        #[cfg(target_os = "linux")]
        {
            let mut d = self.open_ff(codec)?;
            let vaapi = self
                .hw_decode
                .as_deref()
                .is_some_and(|h| h.name == "VA-API");
            if vaapi {
                d.hand_in = Some(gpu.clone());
            }
            if d.dec.hardware && vaapi {
                match va_gpu::VaToGpu::new(gpu) {
                    Ok(g) => {
                        // Converted on the GPU, on the decoding thread: no downloads.
                        d.downloads = None;
                        d.gpu = Some(ToGpu::Va(g));
                        idn_open_protocol::log::log(&format!(
                            "{} decoder: pictures kept on the GPU",
                            codec.name()
                        ));
                    }
                    Err(e) => idn_open_protocol::log::log(&format!(
                        "{} decoder: {e}; downloaded",
                        codec.name()
                    )),
                }
            }
            Ok(Box::new(d))
        }
        // Windows: Direct3D 12 decoding on the application's own device, when it is on
        // Direct3D 12 (the switcher, displays and OBS are); else as before.
        #[cfg(windows)]
        {
            let hw = d3d12_gpu::device_of(gpu).and_then(|dev| match ffi::HwDecode::d3d12_on(dev) {
                Ok(h) => Some(Arc::new(h)),
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "{} decoder: {e}; pictures downloaded",
                        codec.name()
                    ));
                    None
                }
            });
            let Some(hw) = hw else {
                return DecoderFactory::open(self, codec);
            };
            let on_d3d12 = Ffmpeg {
                hw_decode: Some(hw),
                ..self.with_decoder(self.decoding())
            };
            let mut d = on_d3d12.open_ff(codec)?;
            d.hand_in = Some(gpu.clone());
            if d.dec.hardware {
                match d3d12_gpu::D3d12ToGpu::new(gpu) {
                    Ok(g) => {
                        d.downloads = None;
                        d.gpu = Some(ToGpu::D3d12(g));
                        idn_open_protocol::log::log(&format!(
                            "{} decoder: pictures kept on the GPU (Direct3D 12)",
                            codec.name()
                        ));
                    }
                    Err(e) => idn_open_protocol::log::log(&format!(
                        "{} decoder: {e}; downloaded",
                        codec.name()
                    )),
                }
            }
            Ok(Box::new(d))
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = gpu;
            DecoderFactory::open(self, codec)
        }
    }

    fn decodes_aac(&self) -> bool {
        // SAFETY: a lookup in FFmpeg's codec table; only the pointer's nullness is used.
        unsafe { !ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_AAC).is_null() }
    }

    fn open_aac(&self, config: &[u8]) -> io::Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(FfAac(ffi::AudioDec::open(
            ff::AVCodecID::AV_CODEC_ID_AAC,
            config,
        )?)))
    }
}

/// FFmpeg's AAC decoder (§E.19.2).
pub(super) struct FfAac(ffi::AudioDec);

impl AudioDecoder for FfAac {
    fn decode(&mut self, frame: &[u8]) -> io::Result<Vec<Vec<f32>>> {
        self.0.decode(frame)
    }
}

pub(super) struct FfDecoder {
    pub(super) dec: ffi::Decoder,
    pub(super) codec: Codec,
    /// The GPU decoder (kept open as long as the decoder), for moving to the GPU.
    pub(super) hw: Option<Arc<ffi::HwDecode>>,
    /// [`DecoderChoice::Auto`] while decoding in software: whether it keeps up.
    pub(super) auto: Option<Pace>,
    /// For GPU decoding: downloads and conversions on helper threads.
    pub(super) downloads: Option<Downloads>,
    /// Scratch frame for downloads on this thread (software decoding has none).
    pub(super) sw: ffi::Frame,
    /// With [`Ffmpeg::with_picture_rate`]: the pictures handed over.
    pub(super) thin: Option<Thin>,
    /// Opened with the application's GPU ([`DecoderFactory::open_gpu`]): VA-API pictures
    /// kept there (§E.27), and the last one for [`VideoDecoder::take_gpu`].
    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
    pub(super) gpu: Option<ToGpu>,
    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
    pub(super) last_gpu: Option<idn_open_protocol::gpu::GpuPicture>,
    /// The application's GPU, kept for a move to VA-API under Auto: pictures then stay
    /// there too, not downloaded.
    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
    pub(super) hand_in: Option<idn_open_protocol::gpu::GpuHandIn>,
}

impl VideoDecoder for FfDecoder {
    fn decode(&mut self, data: &[u8], keyframe: bool, pts: i64) -> io::Result<Option<Decoded>> {
        if let Some(t) = &mut self.thin {
            t.arrived(pts);
        }
        if let Some(p) = &mut self.auto {
            p.arrived();
            // Software fell behind: the GPU from this keyframe on (it needs one to start).
            if p.to_gpu && keyframe {
                let gpu = self.hw.as_deref().and_then(|hw| {
                    ffi::Decoder::open(codec_id(self.codec), Some(hw))
                        .ok()
                        .map(|d| (d, hw.name))
                });
                if let Some((gpu, name)) = gpu {
                    self.dec = gpu;
                    // Handed the application's GPU: converted there, as `open_gpu` does.
                    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
                    if let Some(h) = &self.hand_in {
                        self.gpu = ToGpu::for_decoder(name, h);
                    }
                    #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
                    let on_gpu = self.gpu.is_some();
                    #[cfg(not(all(feature = "gpu", any(target_os = "linux", windows))))]
                    let on_gpu = false;
                    if !on_gpu {
                        self.downloads = Some(Downloads::start(self.codec)?);
                    }
                    idn_open_protocol::log::log(&format!(
                        "{} decoder: GPU ({name}) from this keyframe on",
                        self.codec.name()
                    ));
                }
                self.auto = None;
            }
        }
        let t0 = std::time::Instant::now();
        let frame = match self.dec.decode_frame(data, pts) {
            Ok(f) => f,
            // The GPU refused a keyframe (a profile it lacks): software from here on. An
            // error on another frame is a damaged or unreferenced picture, not a reason to
            // leave the GPU (a 4K stream joined mid-way ended up decoded in
            // software at 20 fps); the input waits for the next keyframe.
            Err(e) if self.dec.hardware && keyframe => {
                idn_open_protocol::log::log(&format!(
                    "{} decoder: the GPU refused a keyframe ({e}); software from here on",
                    self.codec.name()
                ));
                self.dec = ffi::Decoder::open(codec_id(self.codec), None)?;
                self.downloads = None;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        // A picture not wanted: decoded (the next ones refer to it), nothing more.
        let frame = frame.filter(|_| self.thin.as_mut().is_none_or(Thin::keep));
        // Kept on the application's GPU (§E.27): converted there, here, and handed over
        // with the planes' sizes only. Should that fail, downloaded from then on.
        #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
        if self.dec.hardware
            && let Some(g) = self.gpu.as_mut()
            && let Some((f, pts)) = &frame
        {
            match g.picture(f) {
                Ok(pic) => {
                    let (w, h) = (pic.planes[0].width as usize, pic.planes[0].height as usize);
                    let (full_range, matrix) = colour_of(f);
                    let empty = |pw: usize, ph: usize| Plane {
                        width: pw,
                        height: ph,
                        data: Vec::new(),
                    };
                    // (4:2:0 or 4:2:2, as decoded.)
                    let (cw, ch) = (pic.planes[1].width as usize, pic.planes[1].height as usize);
                    self.last_gpu = Some(pic);
                    return Ok(Some(Decoded {
                        width: w,
                        height: h,
                        planes: vec![empty(w, h), empty(cw, ch), empty(cw, ch)],
                        full_range,
                        matrix,
                        pts: Some(*pts),
                    }));
                }
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "{} decoder: {e}; downloaded from here on",
                        self.codec.name()
                    ));
                    self.gpu = None;
                    self.downloads = Some(Downloads::start(self.codec)?);
                }
            }
        }
        let Some(dl) = self.downloads.as_mut().filter(|_| self.dec.hardware) else {
            // In system memory already: converted here.
            let r = match frame {
                Some((f, pts)) => ffi::picture_of(&f, &self.sw, from_ffmpeg)?.map(|mut d| {
                    d.pts = Some(pts);
                    Some(d)
                }),
                None => Ok(None),
            };
            let secs = t0.elapsed().as_secs_f64();
            // Pictures larger than 1080p: the GPU from the next keyframe, keeping up or not
            // (chosen by CPU). Software took every core: a 2160p30 H.264
            // receiver 1.1 cores in software, ~0.3 on VA-API with the download (~20 ms
            // later, profile round 7); a 2160p camera in a switcher, whose pictures stay
            // on its GPU (nothing to download), 2.9 cores against 1.0 (lab 350). At 1080p
            // and below software stays: it costs little there and is the quicker.
            if let Some(p) = &mut self.auto
                && !p.to_gpu
                && let Ok(Some(d)) = &r
                && d.width * d.height > 1920 * 1088
            {
                p.to_gpu = true;
                #[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
                let stays = self.hand_in.is_some();
                #[cfg(not(all(feature = "gpu", any(target_os = "linux", windows))))]
                let stays = false;
                idn_open_protocol::log::log(&format!(
                    "{} decoder: {}x{} pictures{}: moving to the GPU at the next keyframe",
                    self.codec.name(),
                    d.width,
                    d.height,
                    if stays {
                        " stay on the GPU there"
                    } else {
                        " above 1080p cost less CPU there"
                    }
                ));
            }
            if let Some(p) = &mut self.auto
                && p.decoded(secs)
            {
                idn_open_protocol::log::log(&format!(
                    "{} decoder: software takes {:.1} ms a picture, pictures come every {:.1} ms; \
                     moving to the GPU at the next keyframe",
                    self.codec.name(),
                    p.work * 1e3,
                    p.interval * 1e3
                ));
            }
            return r;
        };
        dl.decode_s += t0.elapsed().as_secs_f64();
        if let Some((f, pts)) = frame {
            dl.submit(f, pts)?;
        }
        dl.log_timing(self.codec);
        // Hand over the oldest finished picture; decoding waits only when the helpers
        // are all busy (each picture then lags at most that many behind).
        let full = dl.in_flight.len() > download_depth();
        dl.next(full).transpose()
    }

    fn poll(&mut self) -> io::Result<Option<Decoded>> {
        match self.downloads.as_mut() {
            Some(dl) => dl.next(false).transpose(),
            None => Ok(None),
        }
    }

    #[cfg(feature = "gpu")]
    fn take_gpu(&mut self) -> Option<idn_open_protocol::gpu::GpuPicture> {
        #[cfg(any(target_os = "linux", windows))]
        {
            self.last_gpu.take()
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            None
        }
    }
}

/// A decoder's pictures moved onto the application's GPU: VA-API's surfaces (Linux),
/// Direct3D 12's (Windows).
#[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
pub(super) enum ToGpu {
    #[cfg(target_os = "linux")]
    Va(va_gpu::VaToGpu),
    #[cfg(windows)]
    D3d12(d3d12_gpu::D3d12ToGpu),
}

#[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
impl ToGpu {
    /// For the GPU decoder named `name` (`HwDecode::name`), on `hand`'s device, if it can.
    pub(super) fn for_decoder(
        name: &str,
        hand: &idn_open_protocol::gpu::GpuHandIn,
    ) -> Option<ToGpu> {
        match name {
            #[cfg(target_os = "linux")]
            "VA-API" => va_gpu::VaToGpu::new(hand).ok().map(ToGpu::Va),
            #[cfg(windows)]
            "D3D12VA" => d3d12_gpu::D3d12ToGpu::new(hand).ok().map(ToGpu::D3d12),
            _ => {
                let _ = hand;
                None
            }
        }
    }

    pub(super) fn picture(
        &mut self,
        f: &ffi::Frame,
    ) -> io::Result<idn_open_protocol::gpu::GpuPicture> {
        match self {
            #[cfg(target_os = "linux")]
            ToGpu::Va(g) => g.picture(f),
            #[cfg(windows)]
            ToGpu::D3d12(g) => g.picture(f),
        }
    }
}

#[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
pub(super) fn colour_of(f: &ffi::Frame) -> (Option<bool>, Option<&'static str>) {
    // SAFETY: plain fields of a live frame.
    let (range, space) = unsafe { ((*f.0).color_range as i32, (*f.0).colorspace as i32) };
    (
        match range {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        },
        match space {
            1 => Some("bt_709"),
            5..=7 => Some("bt_601"),
            9 | 10 => Some("bt_2020"),
            _ => None,
        },
    )
}

/// Interleaved pairs (`src`: a b a b …) into `a` and `b`. 16 pairs a step with SSE2
/// (byte by byte, the split was a third of a 4K picture's CPU).
pub(crate) fn split_pairs(src: &[u8], a: &mut [u8], b: &mut [u8]) {
    let n = a.len().min(b.len()).min(src.len() / 2);
    let mut i = 0;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SSE2 is part of x86-64; every load and store is in bounds (i + 16 ≤ n).
    unsafe {
        use std::arch::x86_64::*;
        let low = _mm_set1_epi16(0x00ff);
        while i + 16 <= n {
            let s = src.as_ptr().add(2 * i) as *const __m128i;
            let (x, y) = (_mm_loadu_si128(s), _mm_loadu_si128(s.add(1)));
            let ea = _mm_packus_epi16(_mm_and_si128(x, low), _mm_and_si128(y, low));
            let eb = _mm_packus_epi16(_mm_srli_epi16(x, 8), _mm_srli_epi16(y, 8));
            _mm_storeu_si128(a.as_mut_ptr().add(i) as *mut __m128i, ea);
            _mm_storeu_si128(b.as_mut_ptr().add(i) as *mut __m128i, eb);
            i += 16;
        }
    }
    for k in i..n {
        a[k] = src[2 * k];
        b[k] = src[2 * k + 1];
    }
}

pub(super) fn from_ffmpeg(p: ffi::Picture) -> io::Result<Decoded> {
    use ff::AVPixelFormat::*;
    let (w, h) = (p.width, p.height);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    // A row out of the picture: from a mapped GPU surface with streaming loads.
    let copy = |dst: &mut [u8], src: &[u8]| {
        if p.uncached {
            ffi::copy_uncached(dst, src);
        } else {
            dst.copy_from_slice(src);
        }
    };
    // Row by row (a 4K picture's rows byte by byte took several ms).
    let plane8 = |(d, stride): (&[u8], usize), pw: usize, ph: usize| {
        let mut data = vec![0u8; pw * ph];
        for (r, row) in data.chunks_exact_mut(pw.max(1)).enumerate() {
            copy(row, &d[r * stride..r * stride + pw]);
        }
        Plane {
            width: pw,
            height: ph,
            data,
        }
    };
    // 16-bit little-endian samples, `shift` bits down to 8, row by row (each row copied
    // out first; sample by sample through iterators, a 4K P010 picture took 50 ms).
    let plane16 = |(d, stride): (&[u8], usize), pw: usize, ph: usize, shift: u32| {
        let mut data = vec![0u8; pw * ph];
        let mut row = vec![0u8; 2 * pw];
        for (r, out) in data.chunks_exact_mut(pw.max(1)).enumerate() {
            copy(&mut row, &d[r * stride..r * stride + 2 * pw]);
            for (o, s) in out.iter_mut().zip(row.as_chunks::<2>().0) {
                *o = (u16::from_le_bytes(*s) >> shift) as u8;
            }
        }
        Plane {
            width: pw,
            height: ph,
            data,
        }
    };
    // P010 (the value in the high bits): each sample's high byte, split out with SSE2.
    let p010_luma = |(d, stride): (&[u8], usize)| {
        let mut data = vec![0u8; w * h];
        let (mut row, mut lo) = (vec![0u8; 2 * w], vec![0u8; w]);
        for (r, out) in data.chunks_exact_mut(w.max(1)).enumerate() {
            copy(&mut row, &d[r * stride..r * stride + 2 * w]);
            split_pairs(&row, &mut lo, out);
        }
        Plane {
            width: w,
            height: h,
            data,
        }
    };
    let planes = match p.format {
        AV_PIX_FMT_YUV420P | AV_PIX_FMT_YUVJ420P => vec![
            plane8(p.planes[0], w, h),
            plane8(p.planes[1], cw, ch),
            plane8(p.planes[2], cw, ch),
        ],
        AV_PIX_FMT_NV12 => {
            // Interleaved Cb Cr rows, each copied out first (cached), then split.
            let (d, stride) = p.planes[1];
            let (mut cb, mut cr) = (vec![0u8; cw * ch], vec![0u8; cw * ch]);
            let mut row = vec![0u8; 2 * cw];
            for r in 0..ch {
                copy(&mut row, &d[r * stride..r * stride + 2 * cw]);
                let (b, c) = (&mut cb[r * cw..(r + 1) * cw], &mut cr[r * cw..(r + 1) * cw]);
                split_pairs(&row, b, c);
            }
            let plane = |data| Plane {
                width: cw,
                height: ch,
                data,
            };
            vec![plane8(p.planes[0], w, h), plane(cb), plane(cr)]
        }
        // 10-bit: P010 keeps the value in the high bits, yuv420p10 in the low ones.
        AV_PIX_FMT_P010LE => {
            // Interleaved 16-bit Cb Cr: high bytes, then those split.
            let (d, stride) = p.planes[1];
            let (mut cb, mut cr) = (vec![0u8; cw * ch], vec![0u8; cw * ch]);
            let (mut row, mut lo, mut hi) =
                (vec![0u8; 4 * cw], vec![0u8; 2 * cw], vec![0u8; 2 * cw]);
            for r in 0..ch {
                copy(&mut row, &d[r * stride..r * stride + 4 * cw]);
                split_pairs(&row, &mut lo, &mut hi);
                let (b, c) = (&mut cb[r * cw..(r + 1) * cw], &mut cr[r * cw..(r + 1) * cw]);
                split_pairs(&hi, b, c);
            }
            let plane = |data| Plane {
                width: cw,
                height: ch,
                data,
            };
            vec![p010_luma(p.planes[0]), plane(cb), plane(cr)]
        }
        AV_PIX_FMT_YUV420P10LE => vec![
            plane16(p.planes[0], w, h, 2),
            plane16(p.planes[1], cw, ch, 2),
            plane16(p.planes[2], cw, ch, 2),
        ],
        f => {
            return Err(io::Error::other(format!(
                "decoded format {f:?} not handled"
            )));
        }
    };
    Ok(Decoded {
        width: w,
        height: h,
        planes,
        full_range: match p.color_range {
            1 => Some(false), // AVCOL_RANGE_MPEG
            2 => Some(true),  // AVCOL_RANGE_JPEG
            _ => None,
        },
        matrix: match p.colorspace {
            1 => Some("bt_709"),
            5..=7 => Some("bt_601"), // BT.470BG, SMPTE 170M, SMPTE 240M
            9 | 10 => Some("bt_2020"),
            _ => None,
        },
        pts: None,
    })
}
