//! [`convert`]: a media file made into an MP4 every player and decoder takes — H.264 (or
//! HEVC) 8-bit 4:2:0 at a constant frame rate, AAC stereo at 48 kHz — faster than real
//! time, with progress and cancelling, at a low priority (media preparation: a ProRes,
//! MPEG-4 Part 2, 4:2:2, interlaced or variable-rate upload made easy to play live).
//!
//! The pictures are decoded in software, deinterlaced where they are interlaced (one
//! picture a frame, the first field kept and the other's lines rebuilt where they moved),
//! turned upright, fitted inside a size if asked, put on a constant frame rate (repeated
//! or dropped by their times) and encoded on the GPU where this machine has an encoder for
//! the codec, else on the CPU. The sound is read as [`AudioReader`] reads it
//! (sample-accurate on the file's timeline) and encoded as AAC. Written to `OUTPUT.part`
//! and renamed once complete: a cancelled or failed conversion leaves nothing behind.

use super::*;
use crate::encode::{FfEncoder, open_ff_encoder};
use crate::mux::{AacEncoder, Muxer, StreamSpec};
use idn_open_protocol::codec::{Codec, EncoderConfig, VideoEncoder};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Whether pictures are deinterlaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deinterlace {
    /// Those the file or the picture itself says are interlaced (the default).
    #[default]
    Auto,
    /// Every picture (a file that is interlaced without saying so).
    On,
    /// None.
    Off,
}

/// What [`convert`] makes.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertConfig {
    /// H.264 (the default: every player) or HEVC (half the size, newer players).
    pub codec: Codec,
    /// Frames a second (numerator, denominator); `None` (the default): the source's
    /// average, as the nearest common rate (23.976, 24, 25, 29.97, 30, 48, 50, 59.94, 60)
    /// when it is within 2 % of one.
    pub frame_rate: Option<(u32, u32)>,
    pub deinterlace: Deinterlace,
    /// Fitted inside this width and height, its shape kept (never made larger); `None`
    /// (the default): its own size.
    pub max_size: Option<(usize, usize)>,
    /// The video's bits a second, in kbit/s; `None` (the default): by its size and rate
    /// (1080p30 H.264 about 7.5 Mbit/s, HEVC 0.6 of that).
    pub video_kbps: Option<u32>,
    /// The sound's rate (default 48 000) and AAC bit rate (default 192 kbit/s), stereo.
    pub audio_rate: u32,
    pub audio_kbps: u32,
    /// On the GPU where this machine has an encoder for the codec (the default), else
    /// (or `false`) on the CPU.
    pub gpu: bool,
    /// The converting thread (and on Linux the decoder's threads it starts) at a low
    /// priority, put back after (the default): live work on the machine comes first.
    pub low_priority: bool,
}

impl Default for ConvertConfig {
    fn default() -> Self {
        ConvertConfig {
            codec: Codec::H264,
            frame_rate: None,
            deinterlace: Deinterlace::Auto,
            max_size: None,
            video_kbps: None,
            audio_rate: 48_000,
            audio_kbps: 192,
            gpu: true,
            low_priority: true,
        }
    }
}

/// How far a [`convert`] is, given to its progress callback about four times a second.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertProgress {
    /// Converted so far (of the output's timeline), and the whole, where known.
    pub done: Duration,
    pub total: Option<Duration>,
    /// `done / total` (0 to 1), where the total is known.
    pub fraction: Option<f64>,
    /// Pictures written so far.
    pub frames: u64,
    /// How many times faster than real time so far.
    pub speed: f64,
}

/// What a finished [`convert`] made.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertResult {
    /// The encoder used (`VA-API`, `NVENC`, `OpenH264`, …); empty without video.
    pub encoder: String,
    pub codec: Codec,
    /// The picture's size and rate (0 × 0 without video).
    pub width: usize,
    pub height: usize,
    pub frame_rate: (u32, u32),
    /// The output's length.
    pub duration: Duration,
    /// Pictures written, of them repeated (gaps in a variable-rate source) and source
    /// pictures left out (more than one in a frame), and how many were deinterlaced.
    pub frames: u64,
    pub repeated: u64,
    pub dropped: u64,
    pub deinterlaced: u64,
    /// Has sound.
    pub audio: bool,
    /// What it couldn't keep: an HDR picture made 8-bit without tone mapping, an alpha
    /// channel left out.
    pub notes: Vec<String>,
    /// How long it took.
    pub took: Duration,
}

/// Converts `input` into the MP4 `output` (replaced if it exists, once the new one is
/// complete). `progress` is called about four times a second; setting `cancel` stops it
/// (an `Interrupted` error, nothing left behind). Runs on the caller's thread: call it on
/// a thread of its own. `ffmpeg`: this machine's encoders ([`crate::Ffmpeg::probe`]).
///
/// ```no_run
/// # fn run() -> std::io::Result<()> {
/// use idn_ffmpeg::media::{ConvertConfig, convert};
/// use std::sync::atomic::AtomicBool;
///
/// let ff = idn_ffmpeg::Ffmpeg::probe();
/// let cancel = AtomicBool::new(false);
/// let r = convert("upload.mov", "ready.mp4", &ConvertConfig::default(), &ff,
///     |p| println!("{:.0} %", p.fraction.unwrap_or(0.0) * 100.0), &cancel)?;
/// println!("{}x{} with {} in {:?}", r.width, r.height, r.encoder, r.took);
/// # Ok(()) }
/// ```
pub fn convert(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    cfg: &ConvertConfig,
    ffmpeg: &crate::Ffmpeg,
    mut progress: impl FnMut(&ConvertProgress),
    cancel: &AtomicBool,
) -> io::Result<ConvertResult> {
    let _priority = cfg.low_priority.then(Priority::lower);
    let output = output.as_ref();
    let mut part = output.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let r = Job::run(input.as_ref(), &part, cfg, ffmpeg, &mut progress, cancel);
    match r {
        Ok(r) => {
            std::fs::rename(&part, output)?;
            Ok(r)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// The video side of a conversion.
struct VideoOut {
    index: usize,
    stream: usize,
    tb: (i32, i32),
    rate: (u32, u32),
    src: Picture,
    size: (usize, usize),
    deinterlace: Deinterlace,
    /// The file says it is interlaced, and which field comes first.
    file_interlaced: bool,
    file_tff: bool,
    enc: FfEncoder,
    scaler: Option<Scaler>,
    scaler2: Option<Scaler>,
    /// The previous picture as decoded (for the deinterlacer's motion test).
    prev: Option<Vec<Vec<u8>>>,
    /// The last picture placed and the next output frame's number.
    last: Option<Vec<Plane>>,
    n: u64,
    /// Access units written.
    written: i64,
    repeated: u64,
    dropped: u64,
    deinterlaced: u64,
    /// The last decoded picture's time, for one without a timestamp.
    last_time: Option<Duration>,
}

/// The sound side.
struct AudioOut {
    stream: usize,
    reader: AudioReader,
    aac: AacEncoder,
    rate: u32,
    /// Samples read so far, and the whole (`None`: not known; read to the end).
    pos: u64,
    total: Option<u64>,
    ended: bool,
}

struct Job<'a> {
    mux: Muxer,
    video: Option<VideoOut>,
    audio: Option<AudioOut>,
    total: Option<Duration>,
    started: Instant,
    told: Instant,
    progress: &'a mut dyn FnMut(&ConvertProgress),
    cancel: &'a AtomicBool,
}

const WRITE_WAIT: Duration = Duration::from_secs(30);

impl Job<'_> {
    fn run(
        input: &Path,
        part: &Path,
        cfg: &ConvertConfig,
        ffmpeg: &crate::Ffmpeg,
        progress: &mut dyn FnMut(&ConvertProgress),
        cancel: &AtomicBool,
    ) -> io::Result<ConvertResult> {
        let started = Instant::now();
        let path = input.to_string_lossy().into_owned();
        let mut demux = Demuxer::open(&path)?;
        let total = demux
            .duration_us
            .filter(|&us| us > 0)
            .map(|us| Duration::from_micros(us as u64));
        // (Its place in the list: the list leaves out streams other than sound and video.)
        let vs = demux.streams.iter().position(|s| s.video);
        let has_audio = demux.streams.iter().any(|s| !s.video && s.channels > 0);
        if vs.is_none() && !has_audio {
            return Err(io::Error::other("neither video nor sound in the file"));
        }
        let mut notes = Vec::new();
        // The video: its encoder and stream.
        let mut video = None;
        let mut dec = None;
        let mut params = Vec::new();
        let mut encoder = String::new();
        if let Some(i) = vs {
            let s = &demux.streams[i];
            let src = Picture::of(s, PixelFormat::I420, None);
            if src
                .transfer
                .as_deref()
                .is_some_and(|t| t == "smpte2084" || t == "arib-std-b67")
            {
                notes.push("HDR picture made 8-bit SDR without tone mapping".into());
            }
            if depth_of(ffi::pix_fmt(s.pix_fmt)).1 {
                notes.push("alpha channel left out".into());
            }
            let (w, h) = (s.width.max(2), s.height.max(2));
            let (dw, dh) = if src.rotation % 180 == 90 {
                (h, w)
            } else {
                (w, h)
            };
            let size = fit((dw, dh), cfg.max_size);
            let rate = pick_rate(
                cfg.frame_rate,
                rate_of(s.frame_rate),
                rate_of(s.base_frame_rate),
            );
            let (field, file_interlaced) = field_order_of(s.field_order);
            let fps = f64::from(rate.0) / f64::from(rate.1);
            let ec = EncoderConfig {
                codec: cfg.codec,
                width: size.0,
                height: size.1,
                frame_rate: rate,
                bitrate: cfg.video_kbps.map_or_else(
                    || default_bitrate(cfg.codec, size, fps),
                    |k| u64::from(k) * 1000,
                ),
                keyframe_interval: (fps * 2.0).round().max(1.0) as u32,
            };
            let (enc, label) = open_video(ffmpeg, &ec, cfg.gpu)?;
            encoder = label.into();
            params.push((enc.parameters()?, (rate.1 as i32, rate.0 as i32)));
            dec = Some(FileDecoder::open(s, None)?);
            video = Some(VideoOut {
                index: s.index,
                stream: 0,
                tb: s.time_base,
                rate,
                src,
                size,
                deinterlace: cfg.deinterlace,
                file_interlaced,
                file_tff: !matches!(field, Some("bb" | "bt")),
                enc,
                scaler: None,
                scaler2: None,
                prev: None,
                last: None,
                n: 0,
                written: 0,
                repeated: 0,
                dropped: 0,
                deinterlaced: 0,
                last_time: None,
            });
        }
        // The sound.
        let mut audio = None;
        if has_audio {
            let rate = cfg.audio_rate.clamp(8_000, 96_000);
            let reader = AudioReader::open(
                input,
                AudioReaderConfig {
                    sample_rate: rate,
                    channels: 2,
                    ..Default::default()
                },
            )?;
            let aac = AacEncoder::open(rate, cfg.audio_kbps.clamp(32, 512), true)?;
            params.push((aac.parameters()?, (1, rate as i32)));
            let total = reader.length();
            audio = Some(AudioOut {
                stream: params.len() - 1,
                reader,
                aac,
                rate,
                pos: 0,
                total,
                ended: false,
            });
        }
        let specs: Vec<StreamSpec> = params
            .iter()
            .map(|(p, tb)| StreamSpec {
                params: p,
                time_base: *tb,
            })
            .collect();
        let mux = Muxer::open(
            "mp4",
            &part.to_string_lossy(),
            &specs,
            &[("movflags", "+faststart")],
            WRITE_WAIT,
        )?;
        let mut job = Job {
            mux,
            video,
            audio,
            total,
            started,
            told: started,
            progress,
            cancel,
        };
        if let (Some(v), Some(dec)) = (job.video.as_mut().map(|v| v.index), dec.as_mut()) {
            job.run_video(&mut demux, dec, v)?;
        }
        job.pump_audio(None)?;
        if let Some(a) = job.audio.as_mut() {
            for (data, pts) in a.aac.finish()? {
                job.mux.write(
                    a.stream,
                    &data,
                    pts,
                    1024,
                    (1, a.rate as i32),
                    true,
                    WRITE_WAIT,
                )?;
            }
        }
        let duration = job.done();
        job.tell(true);
        let v = job.video.as_ref();
        let r = ConvertResult {
            encoder,
            codec: cfg.codec,
            width: v.map_or(0, |v| v.size.0),
            height: v.map_or(0, |v| v.size.1),
            frame_rate: v.map_or((0, 1), |v| v.rate),
            duration,
            frames: v.map_or(0, |v| v.n),
            repeated: v.map_or(0, |v| v.repeated),
            dropped: v.map_or(0, |v| v.dropped),
            deinterlaced: v.map_or(0, |v| v.deinterlaced),
            audio: job.audio.is_some(),
            notes,
            took: started.elapsed(),
        };
        // The trailer written (and the file closed) before it is renamed.
        drop(job);
        Ok(r)
    }

    /// The video stream decoded, placed on the output's frames and encoded, the sound
    /// written alongside.
    fn run_video(
        &mut self,
        demux: &mut Demuxer,
        dec: &mut FileDecoder,
        index: usize,
    ) -> io::Result<()> {
        let start_us = demux.start_us;
        let mut eof = false;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if !eof {
                match demux.read()? {
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
                self.place(f, ts, start_us)?;
            }
            if eof && !got {
                break;
            }
        }
        // To the file's end (its sound's length) with the last picture, then what the
        // encoder still holds.
        let v = self.video.as_mut().expect("video");
        let fps = f64::from(v.rate.0) / f64::from(v.rate.1);
        let end = self
            .total
            .map_or(v.n, |t| (t.as_secs_f64() * fps).round() as u64)
            .max(v.n.max(1));
        while self.video.as_ref().is_some_and(|v| v.n < end) {
            let Some(last) = self.video.as_ref().and_then(|v| v.last.clone()) else {
                break;
            };
            self.video.as_mut().expect("video").repeated += 1;
            self.emit(&last)?;
        }
        let v = self.video.as_mut().expect("video");
        let out = v.enc.finish()?;
        let (stream, rate) = (v.stream, v.rate);
        for e in out {
            let pts = v.written;
            v.written += 1;
            self.mux.write(
                stream,
                &e.data,
                pts,
                1,
                (rate.1 as i32, rate.0 as i32),
                e.keyframe,
                WRITE_WAIT,
            )?;
        }
        Ok(())
    }

    /// One decoded picture made ready and put on the frames its time falls on.
    fn place(&mut self, f: FrameRef, ts: Option<i64>, start_us: i64) -> io::Result<()> {
        let v = self.video.as_mut().expect("video");
        let fps = f64::from(v.rate.0) / f64::from(v.rate.1);
        let t = match ts {
            Some(ts) => file_time(ts, v.tb, start_us),
            None => v
                .last_time
                .map_or(Duration::ZERO, |t| t + Duration::from_secs_f64(1.0 / fps)),
        };
        v.last_time = Some(t);
        let planes = v.ready(f)?;
        let slot = (t.as_secs_f64() * fps).round() as u64;
        if slot < v.n {
            // Two pictures in one frame: the newer one shows from the next.
            if v.last.is_some() {
                v.dropped += 1;
            }
            v.last = Some(planes);
            return Ok(());
        }
        // Frames before this picture's: the last one again (from the start: this one).
        while self.video.as_ref().is_some_and(|v| v.n < slot) {
            let fill = self
                .video
                .as_ref()
                .and_then(|v| v.last.clone())
                .unwrap_or_else(|| planes.clone());
            self.video.as_mut().expect("video").repeated += 1;
            self.emit(&fill)?;
        }
        self.emit(&planes)?;
        self.video.as_mut().expect("video").last = Some(planes);
        Ok(())
    }

    /// The next output frame encoded and written, the sound brought up to it.
    fn emit(&mut self, planes: &[Plane]) -> io::Result<()> {
        let v = self.video.as_mut().expect("video");
        let out = v.enc.encode(planes, Fourcc::Shq0, v.n as i64, false)?;
        v.n += 1;
        let (stream, rate) = (v.stream, v.rate);
        for e in out {
            let pts = v.written;
            v.written += 1;
            self.mux.write(
                stream,
                &e.data,
                pts,
                1,
                (rate.1 as i32, rate.0 as i32),
                e.keyframe,
                WRITE_WAIT,
            )?;
        }
        let now = Duration::from_secs_f64(v.n as f64 * f64::from(rate.1) / f64::from(rate.0));
        // The sound half a second ahead of the pictures (the muxer interleaves).
        self.pump_audio(Some(now + Duration::from_millis(500)))?;
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        self.tell(false);
        Ok(())
    }

    /// The sound read, encoded and written up to `until` (`None`: to its end).
    fn pump_audio(&mut self, until: Option<Duration>) -> io::Result<()> {
        let Some(a) = self.audio.as_mut() else {
            return Ok(());
        };
        const BLOCK: usize = 4096;
        let mut buf = vec![0f32; BLOCK * 2];
        let (mut l, mut r) = (Vec::with_capacity(BLOCK), Vec::with_capacity(BLOCK));
        while !a.ended {
            let want_to = match until {
                Some(t) => (t.as_secs_f64() * f64::from(a.rate)) as u64,
                None => u64::MAX,
            };
            let want_to = a.total.map_or(want_to, |t| want_to.min(t));
            if a.pos >= want_to {
                if a.total.is_some_and(|t| a.pos >= t) {
                    a.ended = true;
                }
                break;
            }
            let n = ((want_to - a.pos).min(BLOCK as u64)) as usize;
            let got = a.reader.read(a.pos, &mut buf[..n * 2])?;
            if got == 0 {
                a.ended = true;
                break;
            }
            a.pos += got as u64;
            l.clear();
            r.clear();
            for k in 0..got {
                l.push(buf[2 * k]);
                r.push(buf[2 * k + 1]);
            }
            for (data, pts) in a.aac.push(&[&l[..], &r[..]])? {
                self.mux.write(
                    a.stream,
                    &data,
                    pts,
                    1024,
                    (1, a.rate as i32),
                    true,
                    WRITE_WAIT,
                )?;
            }
            if self.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if until.is_none() {
                // (Sound alone: progress as it goes.)
                let done = Duration::from_secs_f64(a.pos as f64 / f64::from(a.rate));
                Self::tell_with(
                    self.progress,
                    &mut self.told,
                    self.started,
                    done,
                    self.total,
                    0,
                    false,
                );
            }
        }
        Ok(())
    }

    /// The output's length so far.
    fn done(&self) -> Duration {
        let v = self.video.as_ref().map_or(Duration::ZERO, |v| {
            Duration::from_secs_f64(v.n as f64 * f64::from(v.rate.1) / f64::from(v.rate.0))
        });
        let a = self.audio.as_ref().map_or(Duration::ZERO, |a| {
            Duration::from_secs_f64(a.pos as f64 / f64::from(a.rate))
        });
        v.max(a)
    }

    fn tell(&mut self, last: bool) {
        let done = self.done();
        let frames = self.video.as_ref().map_or(0, |v| v.n);
        Self::tell_with(
            self.progress,
            &mut self.told,
            self.started,
            done,
            self.total,
            frames,
            last,
        );
    }

    fn tell_with(
        progress: &mut dyn FnMut(&ConvertProgress),
        told: &mut Instant,
        started: Instant,
        done: Duration,
        total: Option<Duration>,
        frames: u64,
        last: bool,
    ) {
        if !last && told.elapsed() < Duration::from_millis(250) {
            return;
        }
        *told = Instant::now();
        let secs = started.elapsed().as_secs_f64().max(1e-3);
        progress(&ConvertProgress {
            done,
            total,
            fraction: total
                .filter(|t| !t.is_zero())
                .map(|t| (done.as_secs_f64() / t.as_secs_f64()).clamp(0.0, 1.0)),
            frames,
            speed: done.as_secs_f64() / secs,
        });
    }
}

impl VideoOut {
    /// A decoded picture as the output's: 4:2:0, deinterlaced, upright, its size, limited
    /// range.
    fn ready(&mut self, f: FrameRef) -> io::Result<Vec<Plane>> {
        // SAFETY: a FrameRef points at a live frame (its contract).
        let flags = unsafe { (*f.0).flags };
        let frame_interlaced = flags & FLAG_INTERLACED != 0;
        let tff = if frame_interlaced {
            flags & FLAG_TOP_FIELD_FIRST != 0
        } else {
            self.file_tff
        };
        let deint = match self.deinterlace {
            Deinterlace::On => true,
            Deinterlace::Off => false,
            Deinterlace::Auto => frame_interlaced || self.file_interlaced,
        };
        let full_range = f.full_range();
        let rotation = self.src.rotation % 360;
        let mut planes = if deint || rotation != 0 {
            // At its own size first: the fields' lines apart, then turned, then scaled.
            let pic = Picture {
                size: None,
                ..self.src.clone()
            };
            let vf = pic.convert(
                f,
                &mut self.scaler,
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
                0,
            )?;
            let (w, h) = (vf.width, vf.height);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut p: Vec<Plane> = vf
                .planes
                .into_iter()
                .zip([(w, h), (cw, ch), (cw, ch)])
                .map(|(d, (pw, ph))| Plane {
                    width: pw,
                    height: ph,
                    data: d.data,
                })
                .collect();
            if deint {
                let now: Vec<Vec<u8>> = p.iter().map(|x| x.data.clone()).collect();
                for (k, pl) in p.iter_mut().enumerate() {
                    let prev = self.prev.as_ref().map(|v| v[k].as_slice());
                    deinterlace_plane(&mut pl.data, prev, pl.width, pl.height, tff);
                }
                self.prev = Some(now);
                self.deinterlaced += 1;
            }
            if rotation != 0 {
                p = p.iter().map(|pl| rotate(pl, rotation)).collect();
            }
            if (p[0].width, p[0].height) != self.size {
                p = scale_planes(&p, self.size, &mut self.scaler2)?;
            }
            p
        } else {
            let pic = Picture {
                size: Some(self.size),
                ..self.src.clone()
            };
            let vf = pic.convert(
                f,
                &mut self.scaler,
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
                0,
            )?;
            let (w, h) = (vf.width, vf.height);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            vf.planes
                .into_iter()
                .zip([(w, h), (cw, ch), (cw, ch)])
                .map(|(d, (pw, ph))| Plane {
                    width: pw,
                    height: ph,
                    data: d.data,
                })
                .collect()
        };
        if full_range {
            to_limited(&mut planes);
        }
        Ok(planes)
    }
}

/// `AV_FRAME_FLAG_INTERLACED` and `AV_FRAME_FLAG_TOP_FIELD_FIRST` (FFmpeg's frame flags).
const FLAG_INTERLACED: c_int = 1 << 3;
const FLAG_TOP_FIELD_FIRST: c_int = 1 << 4;

/// The encoder for `ec`: the GPU's for its codec where asked and it opens, else the CPU's.
fn open_video(
    ff: &crate::Ffmpeg,
    ec: &EncoderConfig,
    gpu: bool,
) -> io::Result<(FfEncoder, &'static str)> {
    let mut why = None;
    if gpu && let Some(&(_, b)) = ff.encoders.iter().find(|e| e.0 == ec.codec) {
        match open_ff_encoder(&ff.device_for(b), b, ec, true) {
            Ok(e) => return Ok((e, b.label())),
            Err(e) => why = Some(e),
        }
    }
    if let Some(&(_, b)) = ff.cpu_encoders.iter().find(|e| e.0 == ec.codec) {
        return Ok((open_ff_encoder(&None, b, ec, true)?, b.label()));
    }
    Err(why.unwrap_or_else(|| {
        io::Error::other(format!("no {} encoder on this machine", ec.codec.name()))
    }))
}

/// `(w, h)` fitted inside `max` (never larger), its shape kept, even sides.
fn fit((w, h): (usize, usize), max: Option<(usize, usize)>) -> (usize, usize) {
    let (w, h) = match max {
        Some((mw, mh)) if mw > 0 && mh > 0 && (w > mw || h > mh) => {
            let k = (mw as f64 / w as f64).min(mh as f64 / h as f64);
            (
                ((w as f64 * k).round() as usize).max(2),
                ((h as f64 * k).round() as usize).max(2),
            )
        }
        _ => (w, h),
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// The output's frame rate: the one asked for, else the source's average as the nearest
/// common rate within 2 %, else as it is (in thousandths), 1 to 120.
fn pick_rate(want: Option<(u32, u32)>, avg: f64, base: f64) -> (u32, u32) {
    if let Some(r) = want.filter(|r| r.0 > 0 && r.1 > 0) {
        return r;
    }
    let r = if avg > 0.0 {
        avg
    } else if base > 0.0 {
        base
    } else {
        30.0
    };
    const COMMON: [(u32, u32); 9] = [
        (24000, 1001),
        (24, 1),
        (25, 1),
        (30000, 1001),
        (30, 1),
        (48, 1),
        (50, 1),
        (60000, 1001),
        (60, 1),
    ];
    let near = COMMON
        .iter()
        .map(|&(n, d)| ((r - f64::from(n) / f64::from(d)).abs() / r, (n, d)))
        .min_by(|a, b| a.0.total_cmp(&b.0));
    match near {
        Some((e, nd)) if e <= 0.02 => nd,
        _ => {
            let k = (r.clamp(1.0, 120.0) * 1000.0).round() as u32;
            let g = gcd(k, 1000);
            (k / g, 1000 / g)
        }
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

/// Bits a second for a picture of `size` at `fps`: about 0.12 bit a pixel for H.264
/// (1080p30: 7.5 Mbit/s), 0.6 of that for HEVC and AV1; 1 to 60 Mbit/s.
fn default_bitrate(codec: Codec, (w, h): (usize, usize), fps: f64) -> u64 {
    let k = if codec == Codec::H264 { 0.12 } else { 0.072 };
    ((w * h) as f64 * fps * k).clamp(1e6, 60e6) as u64
}

/// Full-range samples made limited (Y 16–235, Cb/Cr 16–240), as the encoders take them.
fn to_limited(planes: &mut [Plane]) {
    for (k, p) in planes.iter_mut().enumerate() {
        let (lo, span) = if k == 0 { (16.0, 219.0) } else { (16.0, 224.0) };
        let lut: Vec<u8> = (0..256)
            .map(|v| (lo + f64::from(v) * span / 255.0).round() as u8)
            .collect();
        for s in &mut p.data {
            *s = lut[*s as usize];
        }
    }
}

/// One plane deinterlaced in place, one picture a frame: the first field's lines kept
/// (`tff`: the top's, even lines), the other's rebuilt. A sample of the other field that
/// hasn't changed since `prev` (nor its neighbours above and below) is kept as it is (a
/// still part keeps its full detail); one that moved is made from the lines above and
/// below along the direction they match best (an edge kept sharp), a little moved: half
/// of each.
fn deinterlace_plane(data: &mut [u8], prev: Option<&[u8]>, w: usize, h: usize, tff: bool) {
    if h < 3 || w == 0 || data.len() < w * h {
        return;
    }
    let src = data.to_vec();
    let at =
        |y: usize, x: isize| -> i32 { i32::from(src[y * w + x.clamp(0, w as isize - 1) as usize]) };
    let first = if tff { 1 } else { 0 };
    for y in (first..h).step_by(2) {
        // The kept lines around it (both of the kept field).
        let up = if y == 0 { 1 } else { y - 1 };
        let dn = if y + 1 < h { y + 1 } else { y - 1 };
        for x in 0..w {
            let xi = x as isize;
            let (a0, b0) = (at(up, xi), at(dn, xi));
            let mut best = ((a0 - b0).abs(), (a0 + b0 + 1) / 2);
            for d in [-1isize, 1] {
                let (a, b) = (at(up, xi + d), at(dn, xi - d));
                // (Straight up and down unless a slant matches clearly better.)
                if (a - b).abs() + 8 < best.0 {
                    best = ((a - b).abs(), (a + b + 1) / 2);
                }
            }
            let spatial = best.1;
            let i = y * w + x;
            let own = i32::from(src[i]);
            let v = match prev {
                Some(p) if p.len() >= w * h => {
                    let d = |k: usize| (i32::from(src[k]) - i32::from(p[k])).abs();
                    let moved = d(i).max(d(up * w + x)).max(d(dn * w + x));
                    if moved < 10 {
                        own
                    } else if moved < 24 {
                        (own + spatial + 1) / 2
                    } else {
                        spatial
                    }
                }
                _ => spatial,
            };
            data[i] = v.clamp(0, 255) as u8;
        }
    }
}

/// A plane turned counter-clockwise by `deg` (90, 180, 270).
fn rotate(p: &Plane, deg: u16) -> Plane {
    let (w, h) = (p.width, p.height);
    let (nw, nh) = if deg % 180 == 90 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; nw * nh];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match deg {
                90 => (y, w - 1 - x),
                180 => (w - 1 - x, h - 1 - y),
                270 => (h - 1 - y, x),
                _ => (x, y),
            };
            out[ny * nw + nx] = p.data[y * w + x];
        }
    }
    Plane {
        width: nw,
        height: nh,
        data: out,
    }
}

/// 4:2:0 planes scaled to `size` (FFmpeg's scaler, area averaging).
fn scale_planes(
    p: &[Plane],
    size: (usize, usize),
    scaler: &mut Option<Scaler>,
) -> io::Result<Vec<Plane>> {
    let src = Frame::alloc(
        p[0].width,
        p[0].height,
        ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
    )?;
    for (k, pl) in p.iter().enumerate().take(3) {
        let (dst, stride) = src.plane(k);
        for r in 0..pl.height {
            // SAFETY: plane `k` of the frame just allocated for this size holds `height`
            // rows of at least `width` bytes `stride` apart.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pl.data[r * pl.width..].as_ptr(),
                    dst.add(r * stride),
                    pl.width,
                );
            }
        }
    }
    let dst = Frame::alloc(size.0, size.1, ff::AVPixelFormat::AV_PIX_FMT_YUV420P)?;
    Scaler::run(scaler, src.as_ref(), &dst)?;
    let (cw, ch) = (size.0.div_ceil(2), size.1.div_ceil(2));
    Ok([(size.0, size.1), (cw, ch), (cw, ch)]
        .into_iter()
        .enumerate()
        .map(|(k, (w, h))| {
            let (s, stride) = dst.plane(k);
            let mut data = Vec::with_capacity(w * h);
            for r in 0..h {
                // SAFETY: as above, for the frame allocated for `size`.
                data.extend_from_slice(unsafe { std::slice::from_raw_parts(s.add(r * stride), w) });
            }
            Plane {
                width: w,
                height: h,
                data,
            }
        })
        .collect())
}

/// The converting thread at a low priority while it lives (Linux: threads it starts too),
/// then as it was.
struct Priority {
    #[cfg(unix)]
    was: libc::c_int,
    #[cfg(windows)]
    was: i32,
}

impl Priority {
    fn lower() -> Priority {
        #[cfg(target_os = "linux")]
        // SAFETY: this thread's own nice value (Linux: per thread), read then set.
        unsafe {
            let tid = libc::gettid() as libc::id_t;
            let was = libc::getpriority(libc::PRIO_PROCESS, tid);
            libc::setpriority(libc::PRIO_PROCESS, tid, 10);
            Priority { was }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            Priority { was: 0 }
        }
        #[cfg(windows)]
        // SAFETY: the current thread's pseudo-handle.
        unsafe {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, GetThreadPriority, SetThreadPriority,
                THREAD_PRIORITY_BELOW_NORMAL,
            };
            let was = GetThreadPriority(GetCurrentThread());
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            Priority { was }
        }
    }
}

impl Drop for Priority {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        // SAFETY: this thread's own nice value. (Lowering it back needs no privilege only
        // when it was not lower before: a failure leaves it at 10.)
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, self.was);
        }
        #[cfg(windows)]
        // SAFETY: the current thread's pseudo-handle.
        unsafe {
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
            SetThreadPriority(GetCurrentThread(), self.was);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_fit_and_stay_even() {
        assert_eq!(fit((1920, 1080), None), (1920, 1080));
        assert_eq!(fit((3840, 2160), Some((1920, 1080))), (1920, 1080));
        assert_eq!(fit((1080, 1920), Some((1920, 1080))), (608, 1080));
        assert_eq!(fit((1280, 720), Some((1920, 1080))), (1280, 720));
        assert_eq!(fit((721, 481), None), (720, 480));
    }

    #[test]
    fn rates_snap_to_common_ones() {
        assert_eq!(pick_rate(None, 29.97, 29.97), (30000, 1001));
        assert_eq!(pick_rate(None, 29.6, 30.0), (30000, 1001));
        assert_eq!(pick_rate(None, 25.0, 50.0), (25, 1));
        assert_eq!(pick_rate(None, 0.0, 0.0), (30, 1));
        assert_eq!(pick_rate(None, 12.5, 12.5), (25, 2));
        assert_eq!(pick_rate(Some((50, 1)), 25.0, 25.0), (50, 1));
        assert!(
            rates_differ(29.6, 30.0) && !rates_differ(29.97, 29.97) && !rates_differ(0.0, 30.0)
        );
    }

    #[test]
    fn deinterlacing_rebuilds_the_other_field_where_it_moved() {
        // Combing: even lines 200, odd lines 50 (the second field elsewhere).
        let (w, h) = (8, 8);
        let mut d: Vec<u8> = (0..h)
            .flat_map(|y| vec![if y % 2 == 0 { 200 } else { 50 }; w])
            .collect();
        deinterlace_plane(&mut d, None, w, h, true);
        assert!(d.iter().all(|&v| v == 200), "{d:?}");
        // Still since the last picture: kept as it is.
        let still: Vec<u8> = (0..w * h).map(|i| (i * 3 % 256) as u8).collect();
        let mut s = still.clone();
        deinterlace_plane(&mut s, Some(&still), w, h, true);
        assert_eq!(s, still);
        // Bottom field first: the odd lines kept.
        let mut b: Vec<u8> = (0..h)
            .flat_map(|y| vec![if y % 2 == 0 { 200 } else { 50 }; w])
            .collect();
        deinterlace_plane(&mut b, None, w, h, false);
        assert!(b.iter().all(|&v| v == 50), "{b:?}");
    }

    #[test]
    fn planes_turn_and_ranges_narrow() {
        let p = Plane {
            width: 3,
            height: 2,
            data: vec![1, 2, 3, 4, 5, 6],
        };
        // 1 2 3      3 6
        // 4 5 6  ->  2 5   (90° counter-clockwise)
        //            1 4
        let r = rotate(&p, 90);
        assert_eq!(
            (r.width, r.height, r.data.clone()),
            (2, 3, vec![3, 6, 2, 5, 1, 4])
        );
        assert_eq!(rotate(&rotate(&r, 270), 0).data, p.data);
        assert_eq!(rotate(&p, 180).data, vec![6, 5, 4, 3, 2, 1]);
        let mut planes = vec![
            Plane {
                width: 2,
                height: 1,
                data: vec![0, 255],
            },
            Plane {
                width: 1,
                height: 1,
                data: vec![128],
            },
            Plane {
                width: 1,
                height: 1,
                data: vec![255],
            },
        ];
        to_limited(&mut planes);
        assert_eq!(planes[0].data, vec![16, 235]);
        assert_eq!((planes[1].data[0], planes[2].data[0]), (128, 240));
        assert_eq!(default_bitrate(Codec::H264, (1920, 1080), 30.0), 7_464_960);
    }
}
