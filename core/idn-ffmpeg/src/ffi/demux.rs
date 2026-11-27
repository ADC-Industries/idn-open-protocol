//! Reading files and devices (libavformat): streams, packets, seeking.

use super::*;

/// One packet read from a file.
pub struct ReadPacket {
    pub stream: usize,
    pub data: Vec<u8>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub keyframe: bool,
    /// The demuxer's samples to skip (`AV_PKT_DATA_SKIP_SAMPLES`: an AAC file's priming
    /// at its start, its padding at its end), handed to the decoder with the packet.
    pub skip: Option<Vec<u8>>,
}

/// A stream of a file, as the demuxer describes it.
pub struct StreamInfo {
    pub index: usize,
    pub video: bool,
    pub codec_id: ff::AVCodecID,
    pub time_base: (i32, i32),
    pub frame_rate: (i32, i32),
    pub width: usize,
    pub height: usize,
    /// Pictures the decoder must hold back (B-frames); 0 for none.
    pub video_delay: i32,
    pub extradata: Vec<u8>,
    pub params: Params,
    /// Audio: channels and sample rate.
    pub channels: u32,
    pub sample_rate: u32,
    /// Video: how far the picture is to be turned counter-clockwise to show it upright,
    /// 0, 90, 180 or 270 (a phone's portrait video is stored landscape with this).
    pub rotation: u16,
    /// Its first timestamp and its length (stream ticks), where the file says.
    pub start: Option<i64>,
    pub duration: Option<i64>,
    /// Video: its pixel format and colour description as the file states them.
    pub pix_fmt: c_int,
    pub color_space: ff::AVColorSpace,
    pub color_trc: ff::AVColorTransferCharacteristic,
    pub color_primaries: ff::AVColorPrimaries,
    pub color_range: ff::AVColorRange,
    /// Video: the lowest rate all its timestamps fit (FFmpeg's `r_frame_rate`), beside
    /// the average (`frame_rate`); far apart: a variable frame rate.
    pub base_frame_rate: (i32, i32),
    /// Video: how its fields are stored, as the file states.
    pub field_order: ff::AVFieldOrder,
    /// Bits a second, as the file states (0: it doesn't).
    pub bit_rate: i64,
}

/// The display rotation of a stream's parameters (its display matrix), counter-clockwise
/// degrees rounded to a quarter turn: what FFmpeg's own players apply.
pub(super) fn rotation_of(cp: &ff::AVCodecParameters) -> u16 {
    // SAFETY: the side data of live codec parameters; a display matrix is read only when
    // FFmpeg gives one of at least its 9 × 4 bytes.
    unsafe {
        let sd = ff::av_packet_side_data_get(
            cp.coded_side_data,
            cp.nb_coded_side_data,
            ff::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).size < 36 || (*sd).data.is_null() {
            return 0;
        }
        let deg = ff::av_display_rotation_get((*sd).data as *const i32);
        if !deg.is_finite() {
            return 0;
        }
        (((deg / 90.0).round() as i64).rem_euclid(4) * 90) as u16
    }
}

/// An open file (or URL) to read.
pub struct Demuxer {
    ic: *mut ff::AVFormatContext,
    pkt: Packet,
    pub streams: Vec<StreamInfo>,
    /// The duration, if the file says.
    pub duration_us: Option<i64>,
    /// The file's first timestamp (microseconds; 0 when it doesn't say): where its
    /// timeline starts. An MP3 from LAME starts at 25 ms (the encoder's delay, skipped).
    pub start_us: i64,
    /// The whole file's bits a second, as FFmpeg reads it (0: not known).
    pub bit_rate: i64,
    /// A file's path, to read it again from the start ([`Demuxer::rewind`]).
    path: Option<String>,
    /// Set, it interrupts whatever FFmpeg waits on (kept alive with the context, which
    /// points at it).
    _stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

unsafe extern "C" fn demux_interrupt(opaque: *mut std::ffi::c_void) -> c_int {
    // SAFETY: `opaque` is the AtomicBool in the Demuxer's `_stop`, alive as long as the
    // context that calls this.
    let stop = unsafe { &*(opaque as *const std::sync::atomic::AtomicBool) };
    c_int::from(stop.load(std::sync::atomic::Ordering::Relaxed))
}

// SAFETY: the format context and packet are its own, used through &mut self; the stop
// flag is an atomic.
unsafe impl Send for Demuxer {}

impl Drop for Demuxer {
    fn drop(&mut self) {
        // SAFETY: the context opened in `open_with`, closed once.
        unsafe { ff::avformat_close_input(&mut self.ic) };
    }
}

impl Demuxer {
    pub fn open(path: &str) -> io::Result<Self> {
        let mut d = Demuxer::open_with(path, ptr::null(), &[], None)?;
        d.path = Some(path.to_owned());
        Ok(d)
    }

    /// Back to the start, as when opened: a file is opened again. A seek to 0 misses
    /// what comes before it: an AAC file's priming packet and the samples to skip with
    /// it, so the decoder started cold and the first 10 ms of sound came out wrong (a
    /// looped file's sound 10 ms late at every seam).
    pub fn rewind(&mut self) -> io::Result<()> {
        match self.path.clone() {
            Some(p) => {
                *self = Demuxer::open(&p)?;
                Ok(())
            }
            None => self.seek(0),
        }
    }

    /// A network stream (`srt://`, `rist://`, `rtsp://`, …) with FFmpeg's options for it,
    /// which `stop` interrupts: opening, and every read after (a listener nobody calls, a pull that stalled: they waited for ever, and the
    /// thread was left behind holding its source and port).
    pub fn open_url_until(
        url: &str,
        options: &[(&str, &str)],
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> io::Result<Self> {
        Demuxer::open_with(url, ptr::null(), options, Some(stop))
    }

    /// A capture device (roadmap R.19): FFmpeg's input `format` (`x11grab`, `v4l2`,
    /// `dshow`, `lavfi`, …) at `url`, with its options (`framerate`, `video_size`, …).
    #[cfg(feature = "capture")]
    pub fn open_device(format: &str, url: &str, options: &[(&str, &str)]) -> io::Result<Self> {
        devices_registered()?;
        let f = CString::new(format).map_err(io::Error::other)?;
        // SAFETY: a NUL-terminated name; the result is checked for null.
        let fmt = unsafe { ff::av_find_input_format(f.as_ptr()) };
        if fmt.is_null() {
            return Err(io::Error::other(format!(
                "this FFmpeg has no {format:?} capture"
            )));
        }
        Demuxer::open_with(url, fmt, options, None)
    }

    fn open_with(
        path: &str,
        fmt: *const ff::AVInputFormat,
        options: &[(&str, &str)],
        stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> io::Result<Self> {
        let p = CString::new(path).map_err(io::Error::other)?;
        let mut ic = ptr::null_mut();
        if let Some(s) = &stop {
            // Made first, so the interruption is there while opening (on a failed open
            // avformat_open_input frees it).
            // SAFETY: a plain allocation, checked for null.
            ic = unsafe { ff::avformat_alloc_context() };
            if ic.is_null() {
                return Err(io::Error::other("out of memory"));
            }
            // SAFETY: `ic` just allocated; `opaque` points into the Arc the Demuxer keeps for as
            // long as the context.
            unsafe {
                (*ic).interrupt_callback = ff::AVIOInterruptCB {
                    callback: Some(demux_interrupt),
                    opaque: std::sync::Arc::as_ptr(s) as *mut std::ffi::c_void,
                };
            }
        }
        let mut dict: *mut ff::AVDictionary = ptr::null_mut();
        for (k, v) in options {
            let (k, v) = (
                CString::new(*k).map_err(io::Error::other)?,
                CString::new(*v).map_err(io::Error::other)?,
            );
            // SAFETY: NUL-terminated strings, copied into the dictionary.
            unsafe { ff::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0) };
        }
        // SAFETY: `ic` is null or ours (FFmpeg frees it on failure); `fmt` is null or FFmpeg's;
        // the dictionary is ours.
        let r = unsafe { ff::avformat_open_input(&mut ic, p.as_ptr(), fmt, &mut dict) };
        // SAFETY: the dictionary of the call above, freed once.
        unsafe { ff::av_dict_free(&mut dict) };
        check(r, &format!("opening {path}"))?;
        let mut d = Demuxer {
            ic,
            pkt: Packet::checked()?,
            streams: Vec::new(),
            duration_us: None,
            start_us: 0,
            bit_rate: 0,
            path: None,
            _stop: stop,
        };
        // SAFETY: `ic` opened above; its `nb_streams` streams and their parameters are FFmpeg's,
        // read (and copied) while it is open.
        unsafe {
            check(
                ff::avformat_find_stream_info(ic, ptr::null_mut()),
                "reading the file",
            )?;
            d.bit_rate = (*ic).bit_rate.max(0);
            let dur = (*ic).duration;
            d.duration_us = (dur > 0).then_some(dur);
            let start = (*ic).start_time;
            d.start_us = if start == ff::AV_NOPTS_VALUE {
                0
            } else {
                start
            };
            for i in 0..(*ic).nb_streams as usize {
                let st = &**(*ic).streams.add(i);
                let cp = &*st.codecpar;
                let video = cp.codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
                if !video && cp.codec_type != ff::AVMediaType::AVMEDIA_TYPE_AUDIO {
                    continue;
                }
                let extradata = if cp.extradata.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(cp.extradata, cp.extradata_size as usize).to_vec()
                };
                let params = Params(oom(ff::avcodec_parameters_alloc())?);
                check(ff::avcodec_parameters_copy(params.0, st.codecpar), "stream")?;
                d.streams.push(StreamInfo {
                    index: i,
                    video,
                    codec_id: cp.codec_id,
                    time_base: (st.time_base.num, st.time_base.den),
                    frame_rate: (st.avg_frame_rate.num, st.avg_frame_rate.den),
                    width: cp.width.max(0) as usize,
                    height: cp.height.max(0) as usize,
                    video_delay: cp.video_delay,
                    extradata,
                    params,
                    channels: cp.ch_layout.nb_channels.max(0) as u32,
                    sample_rate: cp.sample_rate.max(0) as u32,
                    rotation: if video { rotation_of(cp) } else { 0 },
                    start: (st.start_time != ff::AV_NOPTS_VALUE).then_some(st.start_time),
                    duration: (st.duration != ff::AV_NOPTS_VALUE && st.duration > 0)
                        .then_some(st.duration),
                    pix_fmt: cp.format,
                    color_space: cp.color_space,
                    color_trc: cp.color_trc,
                    color_primaries: cp.color_primaries,
                    color_range: cp.color_range,
                    base_frame_rate: (st.r_frame_rate.num, st.r_frame_rate.den),
                    field_order: cp.field_order,
                    bit_rate: cp.bit_rate.max(0),
                });
            }
        }
        Ok(d)
    }

    /// The next packet; `None` at the end.
    pub fn read(&mut self) -> io::Result<Option<ReadPacket>> {
        // SAFETY: our open context and packet; the packet's data and side data are copied out
        // before it is unreferenced.
        unsafe {
            let r = ff::av_read_frame(self.ic, self.pkt.0);
            if r == ff::AVERROR_EOF {
                return Ok(None);
            }
            check(r, "reading")?;
            let p = &*self.pkt.0;
            let none = |v: i64| (v != ff::AV_NOPTS_VALUE).then_some(v);
            let mut n = 0usize;
            let sd = ff::av_packet_get_side_data(
                self.pkt.0,
                ff::AVPacketSideDataType::AV_PKT_DATA_SKIP_SAMPLES,
                &mut n,
            );
            let out = ReadPacket {
                stream: p.stream_index as usize,
                data: packet_bytes(self.pkt.0),
                pts: none(p.pts),
                dts: none(p.dts),
                keyframe: p.flags & ff::AV_PKT_FLAG_KEY as c_int != 0,
                skip: (!sd.is_null() && n > 0).then(|| std::slice::from_raw_parts(sd, n).to_vec()),
            };
            ff::av_packet_unref(self.pkt.0);
            Ok(Some(out))
        }
    }

    /// The container's name as FFmpeg says it (`mov,mp4,m4a,3gp,3g2,mj2`, …).
    pub fn format_name(&self) -> String {
        // SAFETY: our open context; its input format and name are FFmpeg's static data.
        unsafe {
            let f = (*self.ic).iformat;
            if f.is_null() || (*f).name.is_null() {
                return String::new();
            }
            std::ffi::CStr::from_ptr((*f).name)
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Seeks to the keyframe at or before `us` (microseconds from the start).
    pub fn seek(&mut self, us: i64) -> io::Result<()> {
        check(
            // SAFETY: our open context.
            unsafe { ff::av_seek_frame(self.ic, -1, us, ff::AVSEEK_FLAG_BACKWARD as c_int) },
            "seeking",
        )
        .map(|_| ())
    }
}
