//! Media files decoded in-process, for a host's own timeline (a presentation program's
//! backgrounds and media cues, drawn in its own compositor and mixed in its own mixer):
//! no IDN output, no pace of their own.
//!
//! - [`VideoReader`]: pictures decoded ahead on a thread of its own; the picture for any
//!   time on the caller's clock ([`VideoReader::frame_at`]), with in and out points, a
//!   number of plays and one continuous timeline across them; an exact seek that can wait
//!   for its picture ([`VideoReader::frame_at_wait`]). 8-bit pictures as NV12 or I420,
//!   deeper ones (10-bit HEVC, AV1) as 16-bit samples (P010, or planar), or RGBA.
//! - [`AudioReader`]: a file's sound (a video's soundtrack, an audio file) as float
//!   samples at a chosen rate and channel count, read at a sample position and seekable,
//!   on the same timeline rules.
//! - [`probe`]: codec, size, frame rates (average and base: a variable rate), field
//!   order, bit rates, duration, pixel format and depth, whether this machine's GPU
//!   decodes it; [`thumbnail`]: a still at a chosen time.
//! - [`convert`]: a file made into an MP4 every player takes (H.264 or HEVC, 8-bit
//!   4:2:0, a constant frame rate, AAC at 48 kHz), faster than real time, with progress
//!   and cancelling.
//!
//! ```no_run
//! # fn run() -> std::io::Result<()> {
//! use idn_ffmpeg::media::{VideoReader, VideoReaderConfig};
//! use std::time::Duration;
//!
//! let ff = idn_ffmpeg::Ffmpeg::probe();
//! let v = VideoReader::open("/srv/media/loop.mp4", &ff, VideoReaderConfig {
//!     loops: None, // forever
//!     ..Default::default()
//! })?;
//! // Each frame of the host's compositor: the picture for its media time.
//! if let Some(f) = v.frame_at(Duration::from_millis(16_683)) {
//!     let _ = (f.width, f.height, &f.planes[0].data);
//! }
//! # Ok(()) }
//! ```

use crate::ffi::{self, Demuxer, FileDecoder, Frame, FrameRef, Scaler, StreamInfo, check};
use ffmpeg_sys_next as ff;
use std::ffi::{CStr, c_int};
use std::io;
use std::path::Path;
use std::time::Duration;

mod audio;
mod convert;
mod video;
pub use audio::{AudioReader, AudioReaderConfig};
pub use convert::{ConvertConfig, ConvertProgress, ConvertResult, Deinterlace, convert};
pub use video::{Decoding, VideoReader, VideoReaderConfig};

/// How a picture's samples are laid out ([`VideoFrame::planes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit 4:2:0, Y then interleaved CbCr (two planes).
    Nv12,
    /// 8-bit 4:2:0, Y, Cb, Cr (three planes).
    I420,
    /// 16-bit 4:2:0, Y then interleaved CbCr, the value in the top bits (10 significant
    /// bits for a 10-bit source): DXGI/Vulkan's P010. Little-endian `u16` samples.
    P010,
    /// 16-bit 4:2:0, Y, Cb, Cr, the value scaled to the full 16 bits. Little-endian `u16`.
    Yuv420p16,
    /// 8-bit RGBA, straight alpha (opaque for a video without alpha). BT.709 for HD,
    /// BT.601 below, as FFmpeg's scaler converts.
    Rgba,
}

impl PixelFormat {
    pub(crate) fn av(self) -> ff::AVPixelFormat {
        use ff::AVPixelFormat::*;
        match self {
            PixelFormat::Nv12 => AV_PIX_FMT_NV12,
            PixelFormat::I420 => AV_PIX_FMT_YUV420P,
            PixelFormat::P010 => AV_PIX_FMT_P010LE,
            PixelFormat::Yuv420p16 => AV_PIX_FMT_YUV420P16LE,
            PixelFormat::Rgba => AV_PIX_FMT_RGBA,
        }
    }

    /// Each plane's (bytes in a row, rows) for a `w` × `h` picture.
    fn plane_shapes(self, w: usize, h: usize) -> Vec<(usize, usize)> {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        match self {
            PixelFormat::Nv12 => vec![(w, h), (2 * cw, ch)],
            PixelFormat::I420 => vec![(w, h), (cw, ch), (cw, ch)],
            PixelFormat::P010 => vec![(2 * w, h), (4 * cw, ch)],
            PixelFormat::Yuv420p16 => vec![(2 * w, h), (2 * cw, ch), (2 * cw, ch)],
            PixelFormat::Rgba => vec![(4 * w, h)],
        }
    }

    /// For a source of `bit_depth`: NV12 at 8 bits, P010 above.
    pub fn for_depth(bit_depth: u8) -> PixelFormat {
        if bit_depth > 8 {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        }
    }
}

/// One plane of a picture: its rows packed (`stride` bytes each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneData {
    pub data: Vec<u8>,
    /// Bytes from one row to the next (the row's own bytes: no padding).
    pub stride: usize,
}

/// A decoded picture.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
    /// The source's significant bits per sample (8, 10, 12).
    pub bit_depth: u8,
    pub planes: Vec<PlaneData>,
    /// When it starts on the reader's timeline (in point 0, plays one after the other),
    /// and for how long it shows.
    pub time: Duration,
    pub duration: Duration,
    /// Where it is in the file, and in which play (from 0).
    pub file_time: Duration,
    pub play: u32,
    /// The colour the file states: matrix, transfer (`smpte2084` PQ, `arib-std-b67` HLG
    /// for HDR), primaries; full range or limited.
    pub matrix: Option<String>,
    pub transfer: Option<String>,
    pub primaries: Option<String>,
    pub full_range: bool,
    /// Counter-clockwise turn to show it upright (0, 90, 180, 270; a phone's portrait
    /// video): not applied to the samples.
    pub rotation: u16,
}

/// A file's video stream, as [`probe`] reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// FFmpeg's codec name (`h264`, `hevc`, `av1`, `prores`, …).
    pub codec: String,
    pub width: usize,
    pub height: usize,
    /// Frames a second, as the file says (0 when it doesn't): FFmpeg's average over the
    /// stream, the same as [`avg_frame_rate`](Self::avg_frame_rate).
    pub frame_rate: f64,
    /// The average rate (frames over the stream's length; 0: not known).
    pub avg_frame_rate: f64,
    /// The lowest rate all its timestamps fit (FFmpeg's `r_frame_rate`; 0: not known): a
    /// constant-rate file has it equal to the average.
    pub base_frame_rate: f64,
    /// The two rates differ by more than 1 %: most likely a variable frame rate (a
    /// phone's or a screen recording). A guess from the file's header and first frames,
    /// not every timestamp.
    pub variable_frame_rate: bool,
    /// How its fields are stored, as the file states: `progressive`, `tt` (top first),
    /// `bb` (bottom first), `tb`, `bt`; `None`: it doesn't say.
    pub field_order: Option<String>,
    /// Interlaced, as the file states (field order `tt`, `bb`, `tb` or `bt`); a file
    /// that doesn't say may still be ([`convert`] looks at each picture).
    pub interlaced: bool,
    /// The video's own bits a second, as the file states.
    pub bit_rate: Option<u64>,
    /// FFmpeg's pixel format name (`yuv420p`, `yuv420p10le`, …) and significant bits.
    pub pixel_format: String,
    pub bit_depth: u8,
    /// Has an alpha channel (ProRes 4444, VP9 with alpha, …).
    pub alpha: bool,
    /// Counter-clockwise turn to show it upright.
    pub rotation: u16,
    pub transfer: Option<String>,
    /// This machine's GPU decoder takes the codec (it may still refuse a profile).
    pub hardware_decodable: bool,
    /// Where the video itself ends in the file, where the file says. A play's default end
    /// is the file's duration (`MediaInfo::duration`), the same for its picture and its
    /// sound, so the two loop together; an out point here plays the picture alone.
    pub end: Option<Duration>,
}

/// A file's sound stream, as [`probe`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// Where the sound itself ends in the file, where the file says (see
    /// [`VideoInfo::end`]).
    pub end: Option<Duration>,
}

/// A media file, as [`probe`] reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// FFmpeg's container name (`mov,mp4,m4a,3gp,3g2,mj2`, `matroska,webm`, …).
    pub container: String,
    pub duration: Option<Duration>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// How many sound streams the file has (`audio` is the first).
    pub audio_tracks: u32,
    /// The whole file's bits a second (FFmpeg's reading: the header's, else its size over
    /// its duration); `None`: not known.
    pub bit_rate: Option<u64>,
}

/// What this machine decodes, and with which FFmpeg ([`capabilities`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The GPU decoder this program uses (`vaapi`, `cuda`, `d3d11va`, `d3d12va`, …), if any.
    pub hardware: Option<String>,
    /// The codecs it decodes there (`h264`, `hevc`, `av1`, `vp9`, …; a GPU may still refuse
    /// a profile, a 10-bit or 4:4:4 one).
    pub hardware_codecs: Vec<String>,
    /// The codecs FFmpeg decodes at all (in software where not on the GPU), of the common
    /// video ones.
    pub software_codecs: Vec<String>,
    /// FFmpeg's own version (`n8.1.3`, `7.1`, …) and its libraries' (`avcodec 62.28.100`, …).
    pub ffmpeg: String,
    pub libraries: Vec<(String, String)>,
}

/// What this machine decodes (the GPU's decoder and its codecs, FFmpeg's software ones) and
/// the FFmpeg it runs with.
pub fn capabilities(ffmpeg: &crate::Ffmpeg) -> Capabilities {
    use ff::AVCodecID::*;
    let common = [
        AV_CODEC_ID_H264,
        AV_CODEC_ID_HEVC,
        AV_CODEC_ID_AV1,
        AV_CODEC_ID_VP9,
        AV_CODEC_ID_VP8,
        AV_CODEC_ID_MPEG2VIDEO,
        AV_CODEC_ID_PRORES,
        AV_CODEC_ID_DNXHD,
        AV_CODEC_ID_MJPEG,
    ];
    // SAFETY: codec table lookups and static names (as in `info_of`).
    let codec_name = |id| name(unsafe { ff::avcodec_get_name(id) }).unwrap_or_default();
    let hw = ffmpeg.hw_decode.as_deref();
    let version = |v: u32| format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff);
    // SAFETY: FFmpeg's version functions (plain values, a static string).
    let libraries = unsafe {
        vec![
            ("avcodec".to_owned(), version(ff::avcodec_version())),
            ("avformat".to_owned(), version(ff::avformat_version())),
            ("avutil".to_owned(), version(ff::avutil_version())),
            ("swscale".to_owned(), version(ff::swscale_version())),
            ("swresample".to_owned(), version(ff::swresample_version())),
        ]
    };
    Capabilities {
        hardware: hw.map(|h| h.name.to_owned()),
        hardware_codecs: hw.map_or(Vec::new(), |h| {
            common
                .iter()
                .filter(|&&id| decodes(id, h))
                .map(|&id| codec_name(id))
                .collect()
        }),
        software_codecs: common
            .iter()
            // SAFETY: a lookup in FFmpeg's codec table.
            .filter(|&&id| !unsafe { ff::avcodec_find_decoder(id) }.is_null())
            .map(|&id| codec_name(id))
            .collect(),
        // SAFETY: a static string of FFmpeg's.
        ffmpeg: name(unsafe { ff::av_version_info() }).unwrap_or_default(),
        libraries,
    }
}

/// A C string of FFmpeg's (a static name), or none.
pub(crate) fn name(p: *const std::ffi::c_char) -> Option<String> {
    // SAFETY: FFmpeg's name functions return null or a static NUL-terminated string.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// A pixel format's significant bits per sample, and whether it has alpha.
pub(crate) fn depth_of(fmt: ff::AVPixelFormat) -> (u8, bool) {
    // SAFETY: a descriptor lookup, checked for null.
    unsafe {
        let d = ff::av_pix_fmt_desc_get(fmt);
        if d.is_null() {
            return (8, false);
        }
        (
            (*d).comp[0].depth.clamp(1, 16) as u8,
            (*d).flags & ff::AV_PIX_FMT_FLAG_ALPHA as u64 != 0,
        )
    }
}

/// Where a stream's own content ends in the file (from the file's first timestamp).
fn stream_end(s: &StreamInfo, start_us: i64) -> Option<Duration> {
    let d = s.duration?;
    Some(file_time(s.start.unwrap_or(0) + d, s.time_base, start_us))
}

/// A rational rate as a number (0 when it isn't one).
pub(crate) fn rate_of((num, den): (i32, i32)) -> f64 {
    if num > 0 && den > 0 {
        f64::from(num) / f64::from(den)
    } else {
        0.0
    }
}

/// Both rates known and more than 1 % apart.
pub(crate) fn rates_differ(avg: f64, base: f64) -> bool {
    avg > 0.0 && base > 0.0 && ((avg - base).abs() / base) > 0.01
}

/// A field order's name (`None`: not stated), and whether it is interlaced.
pub(crate) fn field_order_of(f: ff::AVFieldOrder) -> (Option<&'static str>, bool) {
    use ff::AVFieldOrder::*;
    match f {
        AV_FIELD_PROGRESSIVE => (Some("progressive"), false),
        AV_FIELD_TT => (Some("tt"), true),
        AV_FIELD_BB => (Some("bb"), true),
        AV_FIELD_TB => (Some("tb"), true),
        AV_FIELD_BT => (Some("bt"), true),
        _ => (None, false),
    }
}

fn info_of(s: &StreamInfo, hw: Option<&ffi::HwDecode>, start_us: i64) -> VideoInfo {
    let fmt = ffi::pix_fmt(s.pix_fmt);
    let (bit_depth, alpha) = depth_of(fmt);
    let (avg, base) = (rate_of(s.frame_rate), rate_of(s.base_frame_rate));
    let (field_order, interlaced) = field_order_of(s.field_order);
    VideoInfo {
        // SAFETY: a codec id from the demuxer; FFmpeg names any id (static string).
        codec: name(unsafe { ff::avcodec_get_name(s.codec_id) }).unwrap_or_default(),
        width: s.width,
        height: s.height,
        frame_rate: avg,
        avg_frame_rate: avg,
        base_frame_rate: base,
        variable_frame_rate: rates_differ(avg, base),
        field_order: field_order.map(str::to_owned),
        interlaced,
        bit_rate: (s.bit_rate > 0).then_some(s.bit_rate as u64),
        // SAFETY: a format value from the stream's parameters; null for an unknown one.
        pixel_format: name(unsafe { ff::av_get_pix_fmt_name(fmt) }).unwrap_or_default(),
        bit_depth,
        alpha,
        rotation: s.rotation,
        transfer: transfer_name(s.color_trc),
        hardware_decodable: hw.is_some_and(|hw| decodes(s.codec_id, hw)),
        end: stream_end(s, start_us),
    }
}

fn transfer_name(t: ff::AVColorTransferCharacteristic) -> Option<String> {
    if t == ff::AVColorTransferCharacteristic::AVCOL_TRC_UNSPECIFIED {
        return None;
    }
    // SAFETY: a name lookup (null for an unknown value).
    name(unsafe { ff::av_color_transfer_name(t) })
}

/// Whether FFmpeg's decoder for `id` has a configuration for `hw`'s device (its pixel
/// format).
pub(crate) fn decodes(id: ff::AVCodecID, hw: &ffi::HwDecode) -> bool {
    // SAFETY: FFmpeg's codec table and its hardware configurations, read only; each
    // pointer checked for null.
    unsafe {
        let codec = ff::avcodec_find_decoder(id);
        if codec.is_null() {
            return false;
        }
        for i in 0.. {
            let c = ff::avcodec_get_hw_config(codec, i);
            if c.is_null() {
                return false;
            }
            if (*c).pix_fmt == hw.fmt {
                return true;
            }
        }
        false
    }
}

/// What a media file holds: its streams (the first video and the first sound), its
/// duration, whether this machine's GPU decodes its video.
pub fn probe(path: impl AsRef<Path>, ffmpeg: &crate::Ffmpeg) -> io::Result<MediaInfo> {
    let path = path.as_ref().to_string_lossy().into_owned();
    let d = Demuxer::open(&path)?;
    let video = d.streams.iter().find(|s| s.video);
    let audio = d.streams.iter().find(|s| !s.video && s.channels > 0);
    Ok(MediaInfo {
        container: d.format_name(),
        duration: d
            .duration_us
            .filter(|&us| us > 0)
            .map(|us| Duration::from_micros(us as u64)),
        video: video.map(|s| info_of(s, ffmpeg.hw_decode.as_deref(), d.start_us)),
        audio: audio.map(|s| AudioInfo {
            // SAFETY: as in `info_of`.
            codec: name(unsafe { ff::avcodec_get_name(s.codec_id) }).unwrap_or_default(),
            sample_rate: s.sample_rate,
            channels: s.channels,
            end: stream_end(s, d.start_us),
        }),
        audio_tracks: d
            .streams
            .iter()
            .filter(|s| !s.video && s.channels > 0)
            .count() as u32,
        bit_rate: (d.bit_rate > 0).then_some(d.bit_rate as u64),
    })
}

/// A media file's facts as the site's media library lists them (its
/// `LibraryServer::set_prober`, media library v2): `{"duration", "width", "height",
/// "codec", "alpha", "audio_tracks"}`, from [`probe`]; `None` when FFmpeg can't read it.
pub fn library_facts(path: &Path, ffmpeg: &crate::Ffmpeg) -> Option<serde_json::Value> {
    let m = probe(path, ffmpeg).ok()?;
    let v = m.video.as_ref();
    Some(serde_json::json!({
        "duration": m.duration.map(|d| d.as_secs_f64()),
        "width": v.map(|v| v.width), "height": v.map(|v| v.height),
        "codec": v.map(|v| v.codec.clone()).or(m.audio.as_ref().map(|a| a.codec.clone())),
        "alpha": v.map(|v| v.alpha),
        "audio_tracks": m.audio_tracks,
    }))
}

/// A still of a video at `at` (the picture showing then; past the end, the last), as
/// RGBA at most `max_width` wide (its shape kept; not turned by its rotation). Decoded in
/// software, at once (no thread).
pub fn thumbnail(path: impl AsRef<Path>, at: Duration, max_width: usize) -> io::Result<VideoFrame> {
    let path = path.as_ref().to_string_lossy().into_owned();
    let mut d = Demuxer::open(&path)?;
    let s = d
        .streams
        .iter()
        .find(|s| s.video)
        .ok_or_else(|| io::Error::other("no video in the file"))?;
    let (index, tb, w, h) = (s.index, s.time_base, s.width.max(1), s.height.max(1));
    let mut dec = FileDecoder::open(s, None)?;
    let (tw, th) = if w > max_width.max(16) {
        let tw = max_width.max(16) & !1;
        (tw, ((h * tw / w).max(2)) & !1)
    } else {
        (w, h)
    };
    let fmt = PixelFormat::Rgba;
    let info = Picture::of(s, fmt, Some((tw, th)));
    let start_us = d.start_us;
    let _ = seek_before(&mut d, at);
    let mut scaler = None;
    let mut best: Option<(Frame, Duration)> = None;
    let mut done = false;
    let mut eof = false;
    while !done {
        if !eof {
            match d.read()? {
                Some(p) if p.stream == index => dec.send(&p)?,
                Some(_) => continue,
                None => {
                    eof = true;
                    dec.drain();
                }
            }
        }
        let mut got = false;
        while let Some((f, ts)) = dec.receive()? {
            got = true;
            let t = file_time(ts.unwrap_or(0), tb, start_us);
            if t > at && best.is_some() {
                done = true;
                break;
            }
            best = Some((owned_copy(f)?, t));
            if t >= at {
                done = true;
                break;
            }
        }
        if eof && !got {
            break;
        }
    }
    let (f, t) = best.ok_or_else(|| io::Error::other("no picture decoded"))?;
    // Its time in the file (the timeline is the file's own, from 0).
    info.convert(f.as_ref(), &mut scaler, t, Duration::ZERO, t, 0)
}

/// Where to seek the demuxer to decode from `t` exactly (microseconds): half a second
/// before it. A seek finds the keyframe by decode time, which with B-frames is before the
/// picture's own: seeking to `t` itself could land on the keyframe after it.
pub(crate) fn before(t: Duration) -> i64 {
    t.saturating_sub(Duration::from_millis(500)).as_micros() as i64
}

/// A copy of a decoded frame that outlives the decoder's next one (its buffers shared).
pub(crate) fn owned_copy(f: FrameRef) -> io::Result<Frame> {
    let out = Frame::new()?;
    // SAFETY: both frames live; av_frame_ref takes new references to `f`'s buffers.
    check(unsafe { ff::av_frame_ref(out.0, f.0) }, "frame")?;
    Ok(out)
}

/// A stream timestamp in `tb` as file time: from the file's first timestamp (`start_us`,
/// [`Demuxer::start_us`]), as FFmpeg's own tools count it (before it: 0). An MP3's sound
/// starts at 25 ms by its timestamps, at 0 here.
pub(crate) fn file_time(ts: i64, tb: (i32, i32), start_us: i64) -> Duration {
    if tb.1 <= 0 {
        return Duration::ZERO;
    }
    let ns = ts as i128 * tb.0 as i128 * 1_000_000_000 / tb.1 as i128 - start_us as i128 * 1_000;
    Duration::from_nanos(ns.max(0) as u64)
}

/// Where the demuxer seeks to decode from file time `at` exactly: [`before`] it, from the
/// file's first timestamp.
pub(crate) fn seek_before(d: &mut Demuxer, at: Duration) -> io::Result<()> {
    d.seek(before(at) + d.start_us)
}

/// How pictures of a stream come out: its colour, the format and size asked for.
#[derive(Clone)]
pub(crate) struct Picture {
    pub format: PixelFormat,
    pub size: Option<(usize, usize)>,
    pub bit_depth: u8,
    pub matrix: Option<String>,
    pub transfer: Option<String>,
    pub primaries: Option<String>,
    pub full_range: bool,
    pub rotation: u16,
}

impl Picture {
    pub fn of(s: &StreamInfo, format: PixelFormat, size: Option<(usize, usize)>) -> Picture {
        let (bit_depth, _) = depth_of(ffi::pix_fmt(s.pix_fmt));
        // SAFETY: name lookups (null for unknown values).
        let (matrix, primaries) = unsafe {
            (
                name(ff::av_color_space_name(s.color_space)),
                name(ff::av_color_primaries_name(s.color_primaries)),
            )
        };
        let unspecified = |n: &Option<String>| n.as_deref().is_none_or(|n| n == "unknown");
        Picture {
            format,
            size,
            bit_depth,
            matrix: matrix
                .filter(|_| s.color_space != ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED)
                .filter(|m| !unspecified(&Some(m.clone()))),
            transfer: transfer_name(s.color_trc),
            primaries: primaries
                .filter(|_| s.color_primaries != ff::AVColorPrimaries::AVCOL_PRI_UNSPECIFIED),
            full_range: s.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
            rotation: s.rotation,
        }
    }

    /// `f` (a decoded frame, on the GPU or not) as a [`VideoFrame`] in this format and size.
    pub fn convert(
        &self,
        f: FrameRef,
        scaler: &mut Option<Scaler>,
        time: Duration,
        duration: Duration,
        file_time: Duration,
        play: u32,
    ) -> io::Result<VideoFrame> {
        // A GPU frame: downloaded first.
        let sw;
        // SAFETY: `f` is live; `hw_frames_ctx` set means its samples are on the GPU.
        let src = if unsafe { !(*f.0).hw_frames_ctx.is_null() } {
            sw = Frame::new()?;
            // SAFETY: a new frame of ours, filled from the live GPU frame.
            check(
                unsafe { ff::av_hwframe_transfer_data(sw.0, f.0, 0) },
                "download",
            )?;
            sw.as_ref()
        } else {
            f
        };
        let (sw0, sh0) = src.size();
        let (w, h) = self.size.unwrap_or((sw0, sh0));
        let dst = Frame::new()?;
        // SAFETY: a new frame of ours, its buffers allocated for the size and format set.
        unsafe {
            (*dst.0).format = self.format.av() as c_int;
            (*dst.0).width = w as c_int;
            (*dst.0).height = h as c_int;
            check(ff::av_frame_get_buffer(dst.0, 0), "frame buffer")?;
        }
        Scaler::run(scaler, src, &dst)?;
        let planes = self
            .format
            .plane_shapes(w, h)
            .into_iter()
            .enumerate()
            .map(|(i, (row, rows))| {
                let (p, stride) = dst.plane(i);
                let mut data = Vec::with_capacity(row * rows);
                for r in 0..rows {
                    // SAFETY: plane `i` of the frame just allocated holds `rows` rows of at
                    // least `row` bytes `stride` apart (FFmpeg's allocation for this format).
                    data.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(p.add(r * stride), row)
                    });
                }
                PlaneData { data, stride: row }
            })
            .collect();
        Ok(VideoFrame {
            width: w,
            height: h,
            format: self.format,
            bit_depth: self.bit_depth,
            planes,
            time,
            duration,
            file_time,
            play,
            matrix: self.matrix.clone(),
            transfer: self.transfer.clone(),
            primaries: self.primaries.clone(),
            full_range: self.full_range,
            rotation: self.rotation,
        })
    }
}

/// The span of a file that plays (in point to out point), and how often.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Span {
    pub start: Duration,
    /// Its length (`None`: to the end of a file whose length isn't known yet).
    pub len: Option<Duration>,
    /// Plays (`None`: forever).
    pub plays: Option<u32>,
}

impl Span {
    pub fn new(
        in_point: Duration,
        out_point: Option<Duration>,
        file_len: Option<Duration>,
        plays: Option<u32>,
    ) -> Span {
        let end = out_point.or(file_len);
        Span {
            start: in_point,
            len: end
                .map(|e| e.saturating_sub(in_point))
                .filter(|l| !l.is_zero()),
            plays: plays.map(|n| n.max(1)),
        }
    }

    /// The whole timeline's length (`None`: endless, or not known).
    pub fn total(&self) -> Option<Duration> {
        Some(self.len? * self.plays?)
    }

    /// A time on the timeline as (play, place in the file); past the end, the last play's
    /// end.
    pub fn locate(&self, t: Duration) -> (u32, Duration) {
        let Some(len) = self.len else {
            return (0, self.start + t);
        };
        let mut play = (t.as_nanos() / len.as_nanos().max(1)) as u32;
        let mut into = t.saturating_sub(len * play);
        if let Some(n) = self.plays
            && play >= n
        {
            play = n - 1;
            into = len;
        }
        (play, self.start + into)
    }

    /// The timeline time of `file_time` in `play`.
    pub fn time_of(&self, play: u32, file_time: Duration) -> Duration {
        self.len.map_or(Duration::ZERO, |l| l * play) + file_time.saturating_sub(self.start)
    }
}

#[cfg(test)]
mod tests;
