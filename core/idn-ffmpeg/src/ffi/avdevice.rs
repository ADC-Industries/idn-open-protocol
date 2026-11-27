//! FFmpeg's capture devices (libavdevice), loaded at run time.

use super::*;

/// FFmpeg's capture devices (libavdevice): loaded the first time one is wanted (not
/// linked, so the rest works where it isn't installed), registered with `avformat`.
#[cfg(feature = "capture")]
pub(super) struct DeviceLib {
    _lib: libloading::Library,
    // Its libavfilter, loaded first where it comes with the program (see device_lib).
    _filter: Option<libloading::Library>,
    list: unsafe extern "C" fn(
        *const ff::AVInputFormat,
        *const std::ffi::c_char,
        *mut ff::AVDictionary,
        *mut *mut DeviceInfoList,
    ) -> c_int,
    free: unsafe extern "C" fn(*mut *mut DeviceInfoList),
}

// SAFETY: function pointers into a library kept loaded for the program's life.
#[cfg(feature = "capture")]
unsafe impl Send for DeviceLib {}
#[cfg(feature = "capture")]
unsafe impl Sync for DeviceLib {}

#[cfg(feature = "capture")]
#[repr(C)]
pub(super) struct DeviceInfo {
    device_name: *mut std::ffi::c_char,
    device_description: *mut std::ffi::c_char,
    media_types: *mut c_int,
    nb_media_types: c_int,
}

#[cfg(feature = "capture")]
#[repr(C)]
pub(super) struct DeviceInfoList {
    devices: *mut *mut DeviceInfo,
    nb_devices: c_int,
    default_device: c_int,
}

#[cfg(feature = "capture")]
pub(super) static DEVICES: std::sync::OnceLock<Result<DeviceLib, String>> =
    std::sync::OnceLock::new();

#[cfg(feature = "capture")]
pub(super) fn device_lib() -> io::Result<&'static DeviceLib> {
    DEVICES
        .get_or_init(|| {
            let names: &[&str] = if cfg!(windows) {
                &["avdevice-62.dll", "avdevice-61.dll", "avdevice-60.dll"]
            } else if cfg!(target_os = "macos") {
                &[
                    "libavdevice.62.dylib",
                    "libavdevice.61.dylib",
                    "libavdevice.dylib",
                ]
            } else {
                &[
                    "libavdevice.so.62",
                    "libavdevice.so.61",
                    "libavdevice.so.60",
                    "libavdevice.so",
                ]
            };
            // On Linux, libavdevice needs libavfilter, which the dynamic loader looks for only
            // where the system's libraries are, not beside this program's FFmpeg (the Linux
            // package's /usr/lib/idn-media/lib). Loaded first from here (this program's run
            // path applies), the library finds it loaded.
            let filter = if cfg!(target_os = "linux") {
                ["libavfilter.so.11", "libavfilter.so.10", "libavfilter.so.9"]
                    .iter()
                    // SAFETY: FFmpeg's own libraries; their initialisers have no preconditions.
                    .find_map(|n| unsafe { libloading::Library::new(*n) }.ok())
            } else {
                None
            };
            let lib = names
                .iter()
                // SAFETY: FFmpeg's own libraries; their initialisers have no preconditions.
                .find_map(|n| unsafe { libloading::Library::new(*n) }.ok())
                .ok_or("FFmpeg's capture devices (libavdevice) aren't installed")?;
            // SAFETY: symbols typed as avdevice.h declares them; the library is kept with the
            // pointers (in a static, for the program's life).
            unsafe {
                let register = *lib
                    .get::<unsafe extern "C" fn()>(b"avdevice_register_all")
                    .map_err(|e| e.to_string())?;
                let list = *lib
                    .get(b"avdevice_list_input_sources")
                    .map_err(|e| e.to_string())?;
                let free = *lib
                    .get(b"avdevice_free_list_devices")
                    .map_err(|e| e.to_string())?;
                register();
                Ok(DeviceLib {
                    _lib: lib,
                    _filter: filter,
                    list,
                    free,
                })
            }
        })
        .as_ref()
        .map_err(|e| io::Error::other(e.clone()))
}

#[cfg(feature = "capture")]
pub(super) fn devices_registered() -> io::Result<()> {
    device_lib().map(|_| ())
}

/// The devices of capture `format` (`v4l2`, `dshow`): (the name to open, a description,
/// whether it gives video).
#[cfg(feature = "capture")]
pub fn list_devices(format: &str) -> io::Result<Vec<(String, String, bool)>> {
    let lib = device_lib()?;
    let f = CString::new(format).map_err(io::Error::other)?;
    // SAFETY: a NUL-terminated name; the result is checked for null.
    let fmt = unsafe { ff::av_find_input_format(f.as_ptr()) };
    if fmt.is_null() {
        return Err(io::Error::other(format!(
            "this FFmpeg has no {format:?} capture"
        )));
    }
    let mut list: *mut DeviceInfoList = ptr::null_mut();
    // SAFETY: a format FFmpeg found and an out pointer of ours; the list is freed below.
    let r = unsafe { (lib.list)(fmt, ptr::null(), ptr::null_mut(), &mut list) };
    let mut out = Vec::new();
    if r >= 0 && !list.is_null() {
        // SAFETY: the list FFmpeg filled (r ≥ 0, not null): `nb_devices` entries, each with its
        // strings and `nb_media_types` types, all read before it is freed.
        unsafe {
            let l = &*list;
            for i in 0..l.nb_devices.max(0) as usize {
                let d = &**l.devices.add(i);
                let text = |p: *mut std::ffi::c_char| {
                    if p.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(p).to_string_lossy().into_owned()
                    }
                };
                let video = d.nb_media_types == 0
                    || (0..d.nb_media_types as usize).any(|k| {
                        *d.media_types.add(k) == ff::AVMediaType::AVMEDIA_TYPE_VIDEO as c_int
                    });
                out.push((text(d.device_name), text(d.device_description), video));
            }
        }
    }
    if !list.is_null() {
        // SAFETY: the list from above, freed once.
        unsafe { (lib.free)(&mut list) };
    }
    check(r, &format!("listing {format} devices"))?;
    Ok(out)
}

/// A decoder for a file's stream (video or audio): B-frames and threads allowed.
pub struct FileDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: Packet,
    pub frame: Frame,
    sw: Frame,
    /// The last frame came from the hardware decoder.
    hardware: bool,
    /// Opened with a hardware device.
    device: bool,
}

// SAFETY: the context, packet and frames are its own, used through &mut self only.
unsafe impl Send for FileDecoder {}

impl Drop for FileDecoder {
    fn drop(&mut self) {
        // SAFETY: the context allocated in `open_threads`, freed once.
        unsafe { ff::avcodec_free_context(&mut self.ctx) };
    }
}

impl FileDecoder {
    pub fn open(s: &StreamInfo, device: Option<&HwDecode>) -> io::Result<Self> {
        FileDecoder::open_threads(s, device, 4)
    }

    /// Whether it was opened with a hardware device.
    pub fn uses_device(&self) -> bool {
        self.device
    }

    /// For a live source (a capture device): one thread and low delay, so each picture
    /// comes out as soon as it went in.
    pub fn open_live(s: &StreamInfo) -> io::Result<Self> {
        FileDecoder::open_threads(s, None, 1)
    }

    fn open_threads(s: &StreamInfo, device: Option<&HwDecode>, threads: c_int) -> io::Result<Self> {
        // SAFETY: a lookup in FFmpeg's codec table, checked for null.
        let codec = unsafe { ff::avcodec_find_decoder(s.codec_id) };
        if codec.is_null() {
            return Err(io::Error::other(format!("no decoder for {:?}", s.codec_id)));
        }
        // SAFETY: `codec` was found above (not null).
        let ctx = oom(unsafe { ff::avcodec_alloc_context3(codec) })?;
        let d = FileDecoder {
            ctx,
            pkt: Packet::checked()?,
            frame: Frame::new()?,
            sw: Frame::new()?,
            hardware: false,
            device: device.is_some(),
        };
        // SAFETY: `ctx` just allocated and not open; `s.params` are valid parameters (copied
        // from the demuxer's).
        unsafe {
            check(
                ff::avcodec_parameters_to_context(ctx, s.params.0),
                "decoder",
            )?;
            (*ctx).pkt_timebase = ff::AVRational {
                num: s.time_base.0,
                den: s.time_base.1,
            };
            // Frame threading delays output by a frame per thread: with one thread per
            // core on a big machine that was half a second, more than the player reads
            // ahead, so pictures came out after their time. Four is plenty
            // for software decoding of what hardware doesn't take.
            (*ctx).thread_count = threads;
            if threads == 1 {
                (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            }
            if let Some(hw) = device {
                hw.attach(ctx);
            }
            check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                "opening the decoder",
            )?;
        }
        Ok(d)
    }

    /// The next decoded frame as it came out (still in GPU memory from the hardware
    /// decoder; [`picture_of`]-style download elsewhere), owned, with its time and
    /// whether it is a GPU frame.
    pub fn receive_owned(&mut self) -> io::Result<Option<(Frame, Option<i64>, bool)>> {
        // SAFETY: our open decoder and its own frame; the frame moved out is a new one of ours.
        unsafe {
            ff::av_frame_unref(self.frame.0);
            let r = ff::avcodec_receive_frame(self.ctx, self.frame.0);
            if r == EAGAIN || r == ff::AVERROR_EOF {
                return Ok(None);
            }
            check(r, "decode")?;
            let ts = (*self.frame.0).best_effort_timestamp;
            let ts = (ts != ff::AV_NOPTS_VALUE).then_some(ts);
            self.hardware = on_gpu(self.frame.0);
            let out = Frame::new()?;
            ff::av_frame_move_ref(out.0, self.frame.0);
            Ok(Some((out, ts, self.hardware)))
        }
    }

    pub fn send(&mut self, p: &ReadPacket) -> io::Result<()> {
        // SAFETY: our packet, filled with `p.data` (`fill_packet` checks the length), its side
        // data allocated with `skip.len()` bytes before they are copied in; unreferenced after.
        unsafe {
            let k = self.pkt.0;
            fill_packet(k, &p.data, "packet")?;
            (*k).pts = p.pts.unwrap_or(ff::AV_NOPTS_VALUE);
            (*k).dts = p.dts.unwrap_or(ff::AV_NOPTS_VALUE);
            if p.keyframe {
                (*k).flags |= ff::AV_PKT_FLAG_KEY as c_int;
            }
            // The priming or padding to cut: without it a file's sound after a seek to
            // its start (a loop) came out 10 ms late, and its last frame 10 ms too long.
            if let Some(skip) = &p.skip {
                let d = ff::av_packet_new_side_data(
                    k,
                    ff::AVPacketSideDataType::AV_PKT_DATA_SKIP_SAMPLES,
                    skip.len(),
                );
                if !d.is_null() {
                    ptr::copy_nonoverlapping(skip.as_ptr(), d, skip.len());
                }
            }
            let r = ff::avcodec_send_packet(self.ctx, k);
            ff::av_packet_unref(k);
            // A full decoder (EAGAIN) is drained by the caller first; other errors
            // (a damaged packet) cost that packet only.
            if r < 0 && r != EAGAIN {
                return Err(err(r, "decode"));
            }
        }
        Ok(())
    }

    /// The next decoded frame (in system memory) and its time, if one is ready. The
    /// frame is the decoder's until the next `receive` or `flush`.
    pub fn receive(&mut self) -> io::Result<Option<(FrameRef, Option<i64>)>> {
        // SAFETY: our open decoder and its own frames; the FrameRef returned is valid until the
        // next receive or flush, as documented.
        unsafe {
            ff::av_frame_unref(self.frame.0);
            let r = ff::avcodec_receive_frame(self.ctx, self.frame.0);
            if r == EAGAIN || r == ff::AVERROR_EOF {
                return Ok(None);
            }
            check(r, "decode")?;
            let ts = (*self.frame.0).best_effort_timestamp;
            let ts = (ts != ff::AV_NOPTS_VALUE).then_some(ts);
            self.hardware = on_gpu(self.frame.0);
            if self.hardware {
                ff::av_frame_unref(self.sw.0);
                check(
                    ff::av_hwframe_transfer_data(self.sw.0, self.frame.0, 0),
                    "download",
                )?;
                (*self.sw.0).pts = (*self.frame.0).pts;
                return Ok(Some((FrameRef(self.sw.0), ts)));
            }
            Ok(Some((FrameRef(self.frame.0), ts)))
        }
    }

    /// Forgets everything queued (after a seek).
    pub fn flush(&mut self) {
        // SAFETY: an open decoder of ours.
        unsafe { ff::avcodec_flush_buffers(self.ctx) };
    }

    /// Tells the decoder no more packets are coming, so that `receive` gives out what it
    /// still holds; afterwards it takes no more packets (reopen, or `flush`).
    pub fn drain(&mut self) {
        // SAFETY: a null packet is the drain signal avcodec_send_packet documents.
        unsafe {
            let _ = ff::avcodec_send_packet(self.ctx, ptr::null());
        }
    }
}

impl Frame {
    /// (width, height) of a video frame.
    pub fn size(&self) -> (usize, usize) {
        self.as_ref().size()
    }

    /// A borrow of this frame.
    pub fn as_ref(&self) -> FrameRef {
        FrameRef(self.0)
    }

    /// The pixel format (as set).
    pub fn format(&self) -> ff::AVPixelFormat {
        // SAFETY: the frame is ours and live while `self` is.
        unsafe { super::pix_fmt((*self.0).format) }
    }
}

/// Audio of any format, rate and layout to planar float stereo at 48 kHz.
pub struct Resampler {
    ctx: *mut ff::SwrContext,
    key: (c_int, c_int, c_int),
}

// SAFETY: the context is its own, used through &mut access only.
unsafe impl Send for Resampler {}

impl Drop for Resampler {
    fn drop(&mut self) {
        // SAFETY: the context made in `run`, freed once.
        unsafe { ff::swr_free(&mut self.ctx) };
    }
}

impl Resampler {
    /// Converts a decoded audio frame: two channels of samples at 48 kHz.
    pub fn run(this: &mut Option<Resampler>, f: FrameRef) -> io::Result<[Vec<f32>; 2]> {
        // SAFETY: a FrameRef points at a live audio frame; the resampler is ours (freed in Drop);
        // the sample format is checked (`sample_fmt`); swr reads the frame's nb_samples and
        // writes at most `max` into ours.
        unsafe {
            let fr = &*f.0;
            let key = (fr.format, fr.sample_rate, fr.ch_layout.nb_channels);
            if this.as_ref().is_none_or(|r| r.key != key) {
                let mut ctx = ptr::null_mut();
                let mut out_layout: ff::AVChannelLayout = std::mem::zeroed();
                ff::av_channel_layout_default(&mut out_layout, 2);
                check(
                    ff::swr_alloc_set_opts2(
                        &mut ctx,
                        &out_layout,
                        ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP,
                        48_000,
                        &fr.ch_layout,
                        super::sample_fmt(fr.format),
                        fr.sample_rate,
                        0,
                        ptr::null_mut(),
                    ),
                    "resampler",
                )?;
                if let Err(e) = check(ff::swr_init(ctx), "resampler") {
                    ff::swr_free(&mut ctx);
                    return Err(e);
                }
                *this = Some(Resampler { ctx, key });
            }
            let r = this.as_ref().unwrap();
            let max = (fr.nb_samples as i64 * 48_000 / fr.sample_rate.max(1) as i64 + 256) as usize;
            let mut out = [vec![0f32; max], vec![0f32; max]];
            let mut outp = [
                out[0].as_mut_ptr() as *mut u8,
                out[1].as_mut_ptr() as *mut u8,
            ];
            let n = check(
                ff::swr_convert(
                    r.ctx,
                    outp.as_mut_ptr(),
                    max as c_int,
                    fr.extended_data as *mut *const u8,
                    fr.nb_samples,
                ),
                "resampling",
            )? as usize;
            out[0].truncate(n);
            out[1].truncate(n);
            Ok(out)
        }
    }
}

/// An audio decoder for raw frames of one stream (AAC with its AudioSpecificConfig).
pub struct AudioDec {
    ctx: *mut ff::AVCodecContext,
    pkt: Packet,
    frame: Frame,
}

// SAFETY: the context, packet and frame are its own, used through &mut self only.
unsafe impl Send for AudioDec {}

impl Drop for AudioDec {
    fn drop(&mut self) {
        // SAFETY: the context allocated in `open`, freed once.
        unsafe { ff::avcodec_free_context(&mut self.ctx) };
    }
}

impl AudioDec {
    pub fn open(id: ff::AVCodecID, extradata: &[u8]) -> io::Result<Self> {
        // SAFETY: a lookup in FFmpeg's codec table, checked for null.
        let codec = unsafe { ff::avcodec_find_decoder(id) };
        if codec.is_null() {
            return Err(io::Error::other(format!("no decoder for {id:?}")));
        }
        // SAFETY: `codec` was found above (not null).
        let ctx = oom(unsafe { ff::avcodec_alloc_context3(codec) })?;
        let d = AudioDec {
            ctx,
            pkt: Packet::checked()?,
            frame: Frame::new()?,
        };
        // SAFETY: `ctx` just allocated and not open; the extradata is copied into a buffer of
        // FFmpeg's, padded as it requires, which the context then owns.
        unsafe {
            // Planar float out, whichever AAC decoder this FFmpeg has.
            (*ctx).request_sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            if !extradata.is_empty() {
                // FFmpeg owns (and frees) extradata: allocated with its padding.
                let n = extradata.len();
                let buf = ff::av_mallocz(n + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
                if buf.is_null() {
                    return Err(io::Error::other("out of memory"));
                }
                ptr::copy_nonoverlapping(extradata.as_ptr(), buf, n);
                (*ctx).extradata = buf;
                (*ctx).extradata_size = n as c_int;
            }
            check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                "opening the audio decoder",
            )?;
        }
        Ok(d)
    }

    /// Decodes one frame into planar float channels (empty while the decoder waits).
    pub fn decode(&mut self, data: &[u8]) -> io::Result<Vec<Vec<f32>>> {
        // SAFETY: our packet and frame with our open decoder; each channel pointer read from
        // `extended_data` holds `nb_samples` floats (planar float checked first).
        unsafe {
            let k = self.pkt.0;
            fill_packet(k, data, "packet")?;
            let r = ff::avcodec_send_packet(self.ctx, k);
            ff::av_packet_unref(k);
            check(r, "decode")?;
            let mut out: Vec<Vec<f32>> = Vec::new();
            loop {
                ff::av_frame_unref(self.frame.0);
                let r = ff::avcodec_receive_frame(self.ctx, self.frame.0);
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    return Ok(out);
                }
                check(r, "decode")?;
                let f = &*self.frame.0;
                if f.format != ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int {
                    return Err(io::Error::other(
                        "audio decoder gave a sample format other than planar float",
                    ));
                }
                let (ch, n) = (f.ch_layout.nb_channels as usize, f.nb_samples as usize);
                out.resize(ch.max(out.len()), Vec::new());
                for (c, o) in out.iter_mut().enumerate().take(ch) {
                    let p = *f.extended_data.add(c) as *const f32;
                    o.extend_from_slice(std::slice::from_raw_parts(p, n));
                }
            }
        }
    }
}
