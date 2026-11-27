//! [`VideoReader`]: a file's pictures decoded ahead on a thread of its own, given for any
//! time on the caller's clock.

use super::*;
use crate::DecoderChoice;
use idn_common::sync::LockOk as _;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// How a [`VideoReader`] reads a file.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoReaderConfig {
    /// Where in the file it starts (default 0) and stops (default: its end).
    pub in_point: Duration,
    pub out_point: Option<Duration>,
    /// How many times it plays, one after the other on one timeline (default 1); `None`:
    /// forever.
    pub loops: Option<u32>,
    /// The pictures' layout; `None` (the default): NV12 for 8-bit sources, P010 deeper.
    pub format: Option<PixelFormat>,
    /// Scaled to this size; `None` (the default): the file's.
    pub size: Option<(usize, usize)>,
    /// Pictures decoded ahead of the last time asked for (default 8).
    pub ahead: usize,
    /// Where to decode: `None` (the default) the [`crate::Ffmpeg`]'s choice (the GPU
    /// where it decodes the codec), else as given.
    pub decoder: Option<DecoderChoice>,
}

impl Default for VideoReaderConfig {
    fn default() -> Self {
        VideoReaderConfig {
            in_point: Duration::ZERO,
            out_point: None,
            loops: Some(1),
            format: None,
            size: None,
            ahead: 8,
            decoder: None,
        }
    }
}

/// How a [`VideoReader`] decodes ([`VideoReader::decoding`]): it can change once, when the
/// first pictures show the GPU didn't take the stream, or its decoding fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoding {
    /// The GPU decoder in use (`vaapi`, `cuda`, `d3d11va`, `d3d12va`, …); `None`: software.
    pub gpu: Option<String>,
    /// Why software, when it is: chosen, no GPU decoder here, the GPU doesn't take this
    /// stream (a profile or chroma it lacks: FFmpeg decodes it in software), or the GPU's
    /// decoding failed (its words).
    pub why_software: Option<String>,
}

impl Decoding {
    fn software(why: impl Into<String>) -> Decoding {
        Decoding {
            gpu: None,
            why_software: Some(why.into()),
        }
    }
}

struct State {
    /// The time last asked for.
    want: Duration,
    /// A seek to this time, for the thread.
    seek: Option<Duration>,
    /// Decoded pictures in time order, from the one showing at `want` on.
    queue: VecDeque<Arc<VideoFrame>>,
    /// The timeline is decoded to its end.
    ended: bool,
    error: Option<String>,
    /// The timeline's length, where known: a time past it is its end.
    total: Option<Duration>,
    decoding: Decoding,
}

struct Shared {
    st: Mutex<State>,
    cv: Condvar,
    stop: AtomicBool,
}

/// A video file's pictures for the caller's timeline (see the module's documentation).
/// Dropping it stops its thread.
pub struct VideoReader {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    info: VideoInfo,
    span: Span,
    format: PixelFormat,
}

impl VideoReader {
    /// Opens `path`'s first video stream and starts decoding from the in point.
    pub fn open(
        path: impl AsRef<Path>,
        ffmpeg: &crate::Ffmpeg,
        cfg: VideoReaderConfig,
    ) -> io::Result<VideoReader> {
        let path = path.as_ref().to_string_lossy().into_owned();
        let demux = Demuxer::open(&path)?;
        let s = demux
            .streams
            .iter()
            .find(|s| s.video)
            .ok_or_else(|| io::Error::other("no video in the file"))?;
        let hw = ffmpeg.hw_decode.clone();
        let info = info_of(s, hw.as_deref(), demux.start_us);
        let file_len = demux
            .duration_us
            .filter(|&us| us > 0)
            .map(|us| Duration::from_micros(us as u64));
        let span = Span::new(cfg.in_point, cfg.out_point, file_len, cfg.loops);
        let format = cfg.format.unwrap_or(PixelFormat::for_depth(info.bit_depth));
        let choice = cfg.decoder.unwrap_or_else(|| ffmpeg.decoding());
        let device = hw.filter(|_| choice != DecoderChoice::Software);
        let (dec, decoding) = match device.as_deref() {
            Some(d) => match FileDecoder::open(s, Some(d)) {
                Ok(dec) => (
                    dec,
                    Decoding {
                        gpu: Some(d.name.to_owned()),
                        why_software: None,
                    },
                ),
                Err(e) => (
                    FileDecoder::open(s, None)?,
                    Decoding::software(format!("the GPU's decoder didn't open: {e}")),
                ),
            },
            None => (
                FileDecoder::open(s, None)?,
                Decoding::software(if choice == DecoderChoice::Software {
                    "software chosen"
                } else {
                    "no GPU decoder on this machine"
                }),
            ),
        };
        let picture = Picture::of(s, format, cfg.size);
        let frame_dur = if s.frame_rate.0 > 0 && s.frame_rate.1 > 0 {
            Duration::from_secs_f64(f64::from(s.frame_rate.1) / f64::from(s.frame_rate.0))
        } else {
            Duration::from_millis(40)
        };
        let (index, tb) = (s.index, s.time_base);
        let shared = Arc::new(Shared {
            st: Mutex::new(State {
                want: Duration::ZERO,
                seek: Some(Duration::ZERO),
                queue: VecDeque::new(),
                ended: false,
                error: None,
                total: span.total(),
                decoding,
            }),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let mut w = Worker {
            shared: shared.clone(),
            demux,
            dec,
            index,
            tb,
            picture,
            scaler: None,
            span,
            frame_dur,
            ahead: cfg.ahead.max(1),
            play: 0,
            held: None,
            seek_to: None,
            eof: false,
            last_end: None,
            told_software: false,
        };
        let thread = std::thread::Builder::new()
            .name("idn-media-video".into())
            .spawn(move || w.run())?;
        Ok(VideoReader {
            shared,
            thread: Some(thread),
            info,
            span,
            format,
        })
    }

    /// Whether it decodes on the GPU (which) or in software, and why.
    pub fn decoding(&self) -> Decoding {
        self.shared.st.lock_ok().decoding.clone()
    }

    /// The file's video stream.
    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    /// The pictures' layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The timeline's length: every play of the span (`None`: endless, or a file whose
    /// length isn't known).
    pub fn length(&self) -> Option<Duration> {
        self.span.total()
    }

    /// The picture showing at `t` on the timeline, if it is decoded: the last one starting
    /// at or before `t` (past the end, the last picture). Never waits; asking moves the
    /// decoding on (or seeks, for a time before the pictures held or well past them).
    pub fn frame_at(&self, t: Duration) -> Option<Arc<VideoFrame>> {
        let mut st = self.shared.st.lock_ok();
        let f = take_at(&mut st, t);
        self.shared.cv.notify_all();
        f
    }

    /// As [`frame_at`](Self::frame_at), waiting up to `timeout` for the picture showing at
    /// `t` (an exact seek's first picture): `None` if it isn't decoded by then.
    pub fn frame_at_wait(&self, t: Duration, timeout: Duration) -> Option<Arc<VideoFrame>> {
        let end = Instant::now() + timeout;
        let mut st = self.shared.st.lock_ok();
        let t = clamped(&st, t);
        loop {
            if let Some(f) = take_at(&mut st, t)
                && (covers(&f, t) || st.ended || st.queue.len() > 1)
            {
                self.shared.cv.notify_all();
                return Some(f);
            }
            self.shared.cv.notify_all();
            let now = Instant::now();
            if now >= end || st.error.is_some() {
                return None;
            }
            st = self
                .shared
                .cv
                .wait_timeout(st, end - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Whether the timeline is decoded to its end (the last picture is held).
    pub fn ended(&self) -> bool {
        self.shared.st.lock_ok().ended
    }

    /// Why decoding stopped, if it failed.
    pub fn error(&self) -> Option<String> {
        self.shared.st.lock_ok().error.clone()
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Whether `f` shows at `t`.
fn covers(f: &VideoFrame, t: Duration) -> bool {
    f.time <= t && t < f.time + f.duration.max(Duration::from_millis(1))
}

/// A time past the timeline's end: its end (its last picture, not a seek further on).
fn clamped(st: &State, t: Duration) -> Duration {
    st.total.map_or(t, |total| {
        t.min(total.saturating_sub(Duration::from_nanos(1)))
    })
}

/// The picture at `t` from the queue (those before it let go), `want` set; a seek asked
/// for when `t` is before the queue or well past it.
fn take_at(st: &mut State, t: Duration) -> Option<Arc<VideoFrame>> {
    let t = clamped(st, t);
    st.want = t;
    let front = st.queue.front().map(|f| f.time);
    let back = st.queue.back().map(|f| f.time + f.duration);
    let behind = front.is_some_and(|f| t < f);
    let far = back.is_some_and(|b| t > b + Duration::from_secs(2));
    if (behind || far) && st.seek.is_none() {
        st.queue.clear();
        st.ended = false;
        st.seek = Some(t);
        return None;
    }
    let i = st.queue.iter().rposition(|f| f.time <= t)?;
    st.queue.drain(..i);
    st.queue.front().cloned()
}

struct Worker {
    shared: Arc<Shared>,
    demux: Demuxer,
    dec: FileDecoder,
    index: usize,
    tb: (i32, i32),
    picture: Picture,
    scaler: Option<Scaler>,
    span: Span,
    frame_dur: Duration,
    ahead: usize,
    /// The play being decoded.
    play: u32,
    /// After a seek: the last picture before the seek's time, kept until the next one
    /// shows it was the one showing then.
    held: Option<(Frame, Duration)>,
    /// The file time a seek is decoding up to.
    seek_to: Option<Duration>,
    eof: bool,
    /// The end of the last picture of the file seen (its length, when the file doesn't
    /// say).
    last_end: Option<Duration>,
    /// The pictures were found to come from software, though the GPU's decoder opened.
    told_software: bool,
}

impl Worker {
    fn run(&mut self) {
        while !self.shared.stop.load(Ordering::Relaxed) {
            let seek = {
                let mut st = self.shared.st.lock_ok();
                let ahead = st.queue.iter().filter(|f| f.time > st.want).count();
                if st.seek.is_none() && (st.ended || ahead >= self.ahead) {
                    let _ = self
                        .shared
                        .cv
                        .wait_timeout(st, Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                st.seek.take()
            };
            let step = match seek {
                Some(t) => self.seek(t),
                None => self.step(),
            };
            if let Err(e) = step {
                // The GPU refusing the file (a profile it doesn't take): software from
                // where the caller is, once.
                if self.dec.uses_device()
                    && let Some(s) = self.demux.streams.iter().find(|s| s.index == self.index)
                    && let Ok(sw) = FileDecoder::open(s, None)
                {
                    idn_open_protocol::log::log(&format!(
                        "media: the GPU's decoding failed ({e}); software decoding"
                    ));
                    self.dec = sw;
                    let mut st = self.shared.st.lock_ok();
                    st.decoding = Decoding::software(format!("the GPU's decoding failed: {e}"));
                    st.queue.clear();
                    st.seek = Some(st.want);
                    continue;
                }
                let mut st = self.shared.st.lock_ok();
                st.error = Some(e.to_string());
                st.ended = true;
                self.shared.cv.notify_all();
            }
        }
    }

    /// To timeline time `t`: its play and file time, the demuxer at the keyframe before.
    fn seek(&mut self, t: Duration) -> io::Result<()> {
        let (play, at) = self.span.locate(t);
        self.play = play;
        seek_before(&mut self.demux, at)?;
        self.dec.flush();
        self.eof = false;
        self.held = None;
        self.seek_to = Some(at);
        Ok(())
    }

    /// One packet read and its pictures handled; the end of a play handled.
    fn step(&mut self) -> io::Result<()> {
        if !self.eof {
            match self.demux.read()? {
                Some(p) if p.stream == self.index => self.dec.send(&p)?,
                Some(_) => return Ok(()),
                None => {
                    self.eof = true;
                    self.dec.drain();
                }
            }
        }
        let mut any = false;
        while let Some((f, ts, gpu)) = self.dec.receive_owned()? {
            any = true;
            if !gpu && self.dec.uses_device() && !self.told_software {
                // Opened on the GPU, decoded in software: FFmpeg's own fallback for a
                // stream the GPU doesn't take (4:2:2 H.264 on VA-API, …).
                self.told_software = true;
                self.shared.st.lock_ok().decoding = Decoding::software(
                    "the GPU doesn't take this stream (its profile or chroma); FFmpeg decodes it in software",
                );
            }
            let ft = file_time(ts.unwrap_or(0), self.tb, self.demux.start_us);
            if ft < self.span.start {
                // Before the in point (from the keyframe before it).
                self.hold(f, ft);
                continue;
            }
            let end = self.span.len.map(|l| self.span.start + l);
            if end.is_some_and(|e| ft >= e) {
                // A seek to the play's end: its last picture, held, is the one showing.
                if let Some((h, hft)) = self.held.take() {
                    self.push(h.as_ref(), hft)?;
                }
                return self.play_ended();
            }
            if let Some(to) = self.seek_to {
                if ft + self.frame_dur <= to {
                    self.hold(f, ft);
                    continue;
                }
                self.seek_to = None;
                if ft > to
                    && let Some((h, hft)) = self.held.take()
                {
                    self.push(h.as_ref(), hft)?;
                }
            }
            self.held = None;
            self.last_end = Some(ft + self.frame_dur);
            self.push(f.as_ref(), ft)?;
        }
        if self.eof && !any {
            // The file's end: a held picture (an out point past the last one) is the last.
            if let Some((h, hft)) = self.held.take() {
                self.push(h.as_ref(), hft)?;
            }
            return self.play_ended();
        }
        Ok(())
    }

    fn hold(&mut self, f: Frame, ft: Duration) {
        self.held = Some((f, ft));
    }

    fn push(&mut self, f: FrameRef, ft: Duration) -> io::Result<()> {
        let ft = ft.max(self.span.start);
        let time = self.span.time_of(self.play, ft);
        let v = self
            .picture
            .convert(f, &mut self.scaler, time, self.frame_dur, ft, self.play)?;
        let mut st = self.shared.st.lock_ok();
        if st.seek.is_none() {
            st.queue.push_back(Arc::new(v));
        }
        self.shared.cv.notify_all();
        Ok(())
    }

    /// The end of a play: the next one from the in point, or the timeline's end.
    fn play_ended(&mut self) -> io::Result<()> {
        if self.span.len.is_none() {
            // A file that didn't say its length: learnt now, for the next plays.
            self.span.len = self
                .last_end
                .map(|e| e.saturating_sub(self.span.start))
                .filter(|l| !l.is_zero());
        }
        let more = self.span.plays.is_none_or(|n| self.play + 1 < n) && self.span.len.is_some();
        if more {
            self.play += 1;
            seek_before(&mut self.demux, self.span.start)?;
            self.dec.flush();
            self.eof = false;
            self.held = None;
            self.seek_to = (!self.span.start.is_zero()).then_some(self.span.start);
        } else {
            let mut st = self.shared.st.lock_ok();
            st.ended = true;
            self.shared.cv.notify_all();
        }
        Ok(())
    }
}
