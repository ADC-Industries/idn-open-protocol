//! FFmpeg muxing and AAC encoding for the gateways (R.12). As in `ffi`, each pointer is
//! owned by the struct holding it.

use crate::ffi::{EAGAIN, Packet, Params, check, err};
use ffmpeg_sys_next as ff;
use std::ffi::{CString, c_int, c_void};
use std::io;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// An AAC encoder (FFmpeg's own, LGPL): planar float in, 1024-sample frames.
pub struct AacEncoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: Packet,
    /// Samples waiting for a full frame, per channel (two).
    fifo: [Vec<f32>; 2],
    /// The pts (in samples) of the next frame.
    next_pts: i64,
    synced: bool,
    pub rate: u32,
}

// SAFETY: the context, frame and packet are its own, used through &mut self only.
unsafe impl Send for AacEncoder {}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: allocated in `open` (the frees take null), freed once.
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

impl AacEncoder {
    /// Stereo AAC-LC at `rate` and `kbps`.
    pub fn open(rate: u32, kbps: u32, global_header: bool) -> io::Result<Self> {
        // SAFETY: a lookup in FFmpeg's codec table, checked for null.
        let codec = unsafe { ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_AAC) };
        if codec.is_null() {
            return Err(io::Error::other("no AAC encoder in this FFmpeg"));
        }
        // SAFETY: `codec` was found above (not null).
        let ctx = crate::ffi::oom(unsafe { ff::avcodec_alloc_context3(codec) })?;
        // SAFETY: a plain allocation, checked by `oom`.
        let frame = match crate::ffi::oom(unsafe { ff::av_frame_alloc() }) {
            Ok(f) => f,
            Err(e) => {
                let mut ctx = ctx;
                // SAFETY: allocated above, freed once (no encoder owns it yet).
                unsafe { ff::avcodec_free_context(&mut ctx) };
                return Err(e);
            }
        };
        // (Out of memory here: the context and frame freed too.)
        let pkt = match Packet::checked() {
            Ok(p) => p,
            Err(e) => {
                let (mut ctx, mut frame) = (ctx, frame);
                // SAFETY: allocated above, freed once (no encoder owns them yet).
                unsafe {
                    ff::av_frame_free(&mut frame);
                    ff::avcodec_free_context(&mut ctx);
                }
                return Err(e);
            }
        };
        let enc = AacEncoder {
            ctx,
            frame,
            pkt,
            fifo: [Vec::new(), Vec::new()],
            next_pts: 0,
            synced: false,
            rate,
        };
        // SAFETY: `ctx`, `frame` and `pkt` were allocated above (null-checked) and are ours; the
        // context is set up before it is opened, and read through the pointer after.
        unsafe {
            let c = &mut *ctx;
            c.sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            c.sample_rate = rate as c_int;
            ff::av_channel_layout_default(&mut c.ch_layout, 2);
            c.bit_rate = kbps as i64 * 1000;
            c.time_base = ff::AVRational {
                num: 1,
                den: rate as c_int,
            };
            if global_header {
                c.flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
            }
            check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                "opening AAC",
            )?;
            let f = &mut *frame;
            f.format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int;
            // Read through the pointer: avcodec_open2 wrote the context after `c` was taken.
            f.nb_samples = (*ctx).frame_size;
            f.sample_rate = rate as c_int;
            ff::av_channel_layout_default(&mut f.ch_layout, 2);
            check(ff::av_frame_get_buffer(frame, 0), "audio frame")?;
        }
        Ok(enc)
    }

    pub fn parameters(&self) -> io::Result<Params> {
        Params::from_context(self.ctx)
    }

    /// Lines the sample clock up with `pts` (samples, where the next pushed sample
    /// belongs by the source's own timestamps): the first time, and again only when the
    /// two have drifted apart by more than `tolerance` samples (a source restart or a
    /// gap). In between, the count of samples pushed is the clock, so timestamp jitter
    /// never makes it jump.
    pub fn sync_to(&mut self, pts: i64, tolerance: i64) {
        let next_in = self.next_pts + self.fifo[0].len() as i64;
        if !self.synced || (next_in - pts).abs() > tolerance {
            self.next_pts = pts - self.fifo[0].len() as i64;
            self.synced = true;
        }
    }

    /// How far the sample clock is ahead of `pts` (samples, where the next pushed sample
    /// belongs by its timestamp).
    pub fn drift(&self, pts: i64) -> i64 {
        self.next_pts + self.fifo[0].len() as i64 - pts
    }

    /// Lines the next pushed sample up with `pts` exactly, never going back on what is
    /// queued or encoded: sound that comes late against the count is preceded by silence
    /// (up to a second; more, the clock jumps), early sound loses its first samples, and
    /// how many is returned (to skip from the next push). For the first sound, and sound
    /// after silence was filled in.
    pub fn align(&mut self, pts: i64) -> usize {
        let queued = self.fifo[0].len() as i64;
        let next_in = self.next_pts + queued;
        if !self.synced || pts - next_in > self.rate as i64 {
            self.next_pts = pts - queued;
            self.synced = true;
            0
        } else if pts > next_in {
            for f in &mut self.fifo {
                f.resize(f.len() + (pts - next_in) as usize, 0.0);
            }
            0
        } else {
            (next_in - pts) as usize
        }
    }

    /// The end of a file: what is left (padded with silence to a frame) encoded, and the
    /// packets the encoder still holds.
    pub fn finish(&mut self) -> io::Result<Vec<(Vec<u8>, i64)>> {
        // SAFETY: our open context.
        let n = (unsafe { (*self.ctx).frame_size } as usize).max(1);
        let left = self.fifo[0].len() % n;
        let mut out = if left > 0 {
            let pad = vec![0.0f32; n - left];
            self.push(&[&pad[..], &pad[..]])?
        } else {
            Vec::new()
        };
        // SAFETY: our open encoder; a null frame asks it to finish.
        let r = unsafe { ff::avcodec_send_frame(self.ctx, ptr::null()) };
        if r < 0 && r != ff::AVERROR_EOF {
            check(r, "AAC")?;
        }
        loop {
            // SAFETY: our open encoder and our packet.
            let r = unsafe { ff::avcodec_receive_packet(self.ctx, self.pkt.0) };
            if r == EAGAIN || r == ff::AVERROR_EOF {
                break;
            }
            check(r, "AAC")?;
            // SAFETY: the packet just received, copied out before it is unreferenced.
            unsafe {
                let data = crate::ffi::packet_bytes(self.pkt.0);
                let pts = (*self.pkt.0).pts;
                ff::av_packet_unref(self.pkt.0);
                if !data.is_empty() {
                    out.push((data, pts));
                }
            }
        }
        Ok(out)
    }

    /// Takes planar samples (one channel is doubled; more than two: the first two) and
    /// returns AAC packets: (data, pts in samples).
    pub fn push<C: AsRef<[f32]>>(&mut self, channels: &[C]) -> io::Result<Vec<(Vec<u8>, i64)>> {
        let (l, r) = match channels {
            [] => return Ok(Vec::new()),
            [m] => (m, m),
            [l, r, ..] => (l, r),
        };
        // Both channels take exactly the left's length (the right cut or padded): the
        // copy below reads `n` samples from each, and a shorter right channel read past
        // its end.
        let (l, r) = (l.as_ref(), r.as_ref());
        self.fifo[0].extend_from_slice(l);
        self.fifo[1].extend(
            r.iter()
                .copied()
                .chain(std::iter::repeat(0.0))
                .take(l.len()),
        );
        // SAFETY: our open context.
        let n = (unsafe { (*self.ctx).frame_size } as usize).max(1);
        let mut out = Vec::new();
        while self.fifo[0].len() >= n {
            // SAFETY: our frame, made writable, holds `frame_size` (n) samples in each of its two
            // planes; both fifos hold at least n.
            unsafe {
                check(ff::av_frame_make_writable(self.frame), "audio frame")?;
                for ch in 0..2 {
                    let dst = (*self.frame).data[ch] as *mut f32;
                    ptr::copy_nonoverlapping(self.fifo[ch].as_ptr(), dst, n);
                }
                (*self.frame).pts = self.next_pts;
            }
            for f in &mut self.fifo {
                f.drain(..n);
            }
            self.next_pts += n as i64;
            check(
                // SAFETY: our open encoder and our frame.
                unsafe { ff::avcodec_send_frame(self.ctx, self.frame) },
                "AAC",
            )?;
            loop {
                // SAFETY: our open encoder and our packet.
                let r = unsafe { ff::avcodec_receive_packet(self.ctx, self.pkt.0) };
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "AAC")?;
                // SAFETY: the packet just received, copied out before it is unreferenced.
                unsafe {
                    let data = crate::ffi::packet_bytes(self.pkt.0);
                    let pts = (*self.pkt.0).pts;
                    ff::av_packet_unref(self.pkt.0);
                    if !data.is_empty() {
                        out.push((data, pts));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Network I/O gives up when this deadline (ms since `epoch`) passes: a dead server
/// never blocks a writer for good.
struct Deadline {
    epoch: Instant,
    at_ms: AtomicU64,
}

unsafe extern "C" fn interrupt(opaque: *mut c_void) -> c_int {
    // SAFETY: `opaque` is the Muxer's Arc'd Deadline, alive as long as the context that
    // calls this.
    let d = unsafe { &*(opaque as *const Deadline) };
    c_int::from(d.epoch.elapsed().as_millis() as u64 > d.at_ms.load(Ordering::Relaxed))
}

/// A stream to describe to a muxer: its parameters and packet time base.
pub struct StreamSpec<'a> {
    pub params: &'a Params,
    pub time_base: (i32, i32),
}

/// An open muxer writing to a file or a network URL.
pub struct Muxer {
    oc: *mut ff::AVFormatContext,
    deadline: Arc<Deadline>,
    time_bases: Vec<ff::AVRational>,
    pkt: Packet,
    header: bool,
}

// SAFETY: the context and packet are its own, used through &mut self only.
unsafe impl Send for Muxer {}

impl Drop for Muxer {
    fn drop(&mut self) {
        // SAFETY: our context from `open`: the trailer only after a header, its I/O closed if it
        // opened one, then freed once.
        unsafe {
            if self.header {
                self.extend(Duration::from_secs(3));
                ff::av_write_trailer(self.oc);
            }
            if !(*self.oc).oformat.is_null()
                && (*(*self.oc).oformat).flags & ff::AVFMT_NOFILE as c_int == 0
            {
                ff::avio_closep(&mut (*self.oc).pb);
            }
            ff::avformat_free_context(self.oc);
        }
    }
}

impl Muxer {
    /// Opens `url` as `format` (`flv`, `mpegts`, `mp4`, `matroska`) with these streams
    /// and options, writing the header; network operations wait at most `timeout`.
    pub fn open(
        format: &str,
        url: &str,
        streams: &[StreamSpec],
        options: &[(&str, &str)],
        timeout: Duration,
    ) -> io::Result<Self> {
        let (f, u) = (
            CString::new(format).unwrap(),
            CString::new(url).map_err(io::Error::other)?,
        );
        let mut oc = ptr::null_mut();
        check(
            // SAFETY: an out pointer of ours and NUL-terminated strings; the result is checked.
            unsafe {
                ff::avformat_alloc_output_context2(&mut oc, ptr::null(), f.as_ptr(), u.as_ptr())
            },
            "output format",
        )?;
        let deadline = Arc::new(Deadline {
            epoch: Instant::now(),
            at_ms: AtomicU64::new(0),
        });
        // (Checked: an unchecked one was used as a null packet when out of memory.)
        let pkt = match Packet::checked() {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: the context allocated above, freed once (no Muxer owns it yet).
                unsafe { ff::avformat_free_context(oc) };
                return Err(e);
            }
        };
        let mut m = Muxer {
            oc,
            deadline,
            time_bases: Vec::new(),
            pkt,
            header: false,
        };
        m.extend(timeout);
        // SAFETY: `oc` allocated above and owned by `m` (freed on every early return); the
        // interrupt's opaque is `m.deadline`, kept with it; streams indexed below `nb_streams`.
        unsafe {
            (*oc).interrupt_callback = ff::AVIOInterruptCB {
                callback: Some(interrupt),
                opaque: Arc::as_ptr(&m.deadline) as *mut c_void,
            };
            for s in streams {
                let st = ff::avformat_new_stream(oc, ptr::null());
                if st.is_null() {
                    return Err(io::Error::other("out of memory"));
                }
                check(
                    ff::avcodec_parameters_copy((*st).codecpar, s.params.0),
                    "stream",
                )?;
                (*st).time_base = ff::AVRational {
                    num: s.time_base.0,
                    den: s.time_base.1,
                };
                m.time_bases.push((*st).time_base);
            }
            let mut opts: *mut ff::AVDictionary = ptr::null_mut();
            for (k, v) in options {
                let (k, v) = (CString::new(*k).unwrap(), CString::new(*v).unwrap());
                ff::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            if (*(*oc).oformat).flags & ff::AVFMT_NOFILE as c_int == 0 {
                let r = ff::avio_open2(
                    &mut (*oc).pb,
                    u.as_ptr(),
                    ff::AVIO_FLAG_WRITE as c_int,
                    &(*oc).interrupt_callback,
                    &mut opts,
                );
                if r < 0 {
                    ff::av_dict_free(&mut opts);
                    return Err(err(r, &format!("opening {}", redact(url))));
                }
            }
            let r = ff::avformat_write_header(oc, &mut opts);
            ff::av_dict_free(&mut opts);
            check(r, "writing the header")?;
            // The muxer may change the time bases.
            for (i, tb) in m.time_bases.iter_mut().enumerate() {
                *tb = (**(*oc).streams.add(i)).time_base;
            }
        }
        m.header = true;
        Ok(m)
    }

    fn extend(&self, by: Duration) {
        let at = self.deadline.epoch.elapsed() + by;
        self.deadline
            .at_ms
            .store(at.as_millis() as u64, Ordering::Relaxed);
    }

    /// Writes one packet of stream `index`, its times in `tb` units.
    /// Writes a packet: its time `pts` and length `duration`, both in `tb`. The length
    /// matters in a fragmented MP4: a fragment's last packet is as long as it says (a
    /// guess there put a hole of 3-17 ms after a recording's first sound packet, its
    /// sound then played early by players that follow the sample order).
    #[allow(clippy::too_many_arguments)]
    pub fn write(
        &mut self,
        index: usize,
        data: &[u8],
        pts: i64,
        duration: i64,
        tb: (i32, i32),
        keyframe: bool,
        timeout: Duration,
    ) -> io::Result<()> {
        self.extend(timeout);
        // SAFETY: our packet, filled with `data` (length checked), on our open muxer; `index`
        // is checked against our streams by `time_bases[index]` before the write.
        unsafe {
            let p = self.pkt.0;
            crate::ffi::fill_packet(p, data, "packet")?;
            (*p).stream_index = index as c_int;
            (*p).pts = pts;
            (*p).dts = pts;
            (*p).duration = duration.max(0);
            if keyframe {
                (*p).flags |= ff::AV_PKT_FLAG_KEY as c_int;
            }
            ff::av_packet_rescale_ts(
                p,
                ff::AVRational {
                    num: tb.0,
                    den: tb.1,
                },
                self.time_bases[index],
            );
            // Takes the packet's data.
            let r = ff::av_interleaved_write_frame(self.oc, p);
            check(r, "writing")?;
        }
        Ok(())
    }
}

/// A URL with any stream key or passphrase hidden, for logs and status.
pub fn redact(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let hide_path = base.starts_with("rtmp");
    let mut out = if hide_path {
        // rtmp(s)://host/app/KEY: the last path part is the key.
        match base.rsplit_once('/') {
            Some((head, key)) if !key.is_empty() && head.matches('/').count() >= 3 => {
                format!("{head}/•••")
            }
            _ => base.to_owned(),
        }
    } else {
        base.to_owned()
    };
    if url.contains('?') {
        out.push_str("?•••");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn keys_are_hidden() {
        assert_eq!(
            redact("rtmp://a.rtmp.youtube.com/live2/abcd-efgh"),
            "rtmp://a.rtmp.youtube.com/live2/•••"
        );
        assert_eq!(
            redact("rtmps://live-api-s.facebook.com:443/rtmp/FB-123?s_bl=1"),
            "rtmps://live-api-s.facebook.com:443/rtmp/•••?•••"
        );
        assert_eq!(
            redact("srt://ingest.example:9000?streamid=secret&passphrase=x"),
            "srt://ingest.example:9000?•••"
        );
        assert_eq!(redact("/tmp/service.mp4"), "/tmp/service.mp4");
    }
}

#[cfg(test)]
mod aac_tests {
    use super::*;

    /// Channels of different lengths (an `AudioBlock` is the caller's to make): no read
    /// past the shorter one, and the channels stay in step.
    #[test]
    fn uneven_channels_are_evened() {
        let Ok(mut e) = AacEncoder::open(48_000, 128, false) else {
            eprintln!("no AAC encoder in this FFmpeg: skipped");
            return;
        };
        let _ = e.push(&[vec![0.1f32; 4096], vec![0.1f32; 10]]).unwrap();
        let _ = e.push(&[vec![0.1f32; 100], vec![0.1f32; 5000]]).unwrap();
        assert_eq!(e.fifo[0].len(), e.fifo[1].len());
    }
}
