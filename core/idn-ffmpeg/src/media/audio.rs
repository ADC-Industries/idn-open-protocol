//! [`AudioReader`]: a file's sound as float samples at a chosen rate and channel count,
//! read at a sample position on the caller's timeline.

use super::*;
use std::collections::VecDeque;

/// How an [`AudioReader`] reads a file.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioReaderConfig {
    /// Samples a second and channels given out (default 48 000, 2).
    pub sample_rate: u32,
    pub channels: u16,
    /// Where in the file it starts (default 0) and stops (default: its end).
    pub in_point: Duration,
    pub out_point: Option<Duration>,
    /// How many times it plays on one timeline (default 1); `None`: forever.
    pub loops: Option<u32>,
}

impl Default for AudioReaderConfig {
    fn default() -> Self {
        AudioReaderConfig {
            sample_rate: 48_000,
            channels: 2,
            in_point: Duration::ZERO,
            out_point: None,
            loops: Some(1),
        }
    }
}

/// A sound file (or a video's soundtrack) read as interleaved float samples on the
/// caller's timeline: sample 0 is the in point, plays follow one another, each exactly
/// `out − in` long (silence where the sound ends before the out point, or starts after the
/// in point). Positions are sample-accurate, as FFmpeg's own tools place the samples (from
/// the file's first timestamp: an MP3's encoder delay skipped). Read on the caller's thread
/// (decoding as it reads: a few milliseconds a second of sound).
pub struct AudioReader {
    demux: Demuxer,
    dec: FileDecoder,
    index: usize,
    tb: (i32, i32),
    rate: u32,
    channels: usize,
    span: Span,
    swr: Option<Swr>,
    /// The next sample's position on the timeline.
    pos: u64,
    /// What is decoded from there on, in order, and its frames.
    queue: VecDeque<Chunk>,
    queued: u64,
    /// The play being decoded, and how much of it is queued so far (from the in point; after
    /// a seek, from the seek's place).
    play: u32,
    play_samples: u64,
    /// Where the next decoded sample sits in the play (negative: before the in point);
    /// `None` until a frame after a seek or a play's start says, by its time.
    at: Option<i64>,
    eof: bool,
    ended: bool,
    info: AudioInfo,
}

/// Decoded sound (interleaved, how much of it is read), or silence (frames).
enum Chunk {
    Sound(Vec<f32>, usize),
    Silence(u64),
}

impl AudioReader {
    /// Opens `path`'s first sound stream.
    pub fn open(path: impl AsRef<Path>, cfg: AudioReaderConfig) -> io::Result<AudioReader> {
        let path = path.as_ref().to_string_lossy().into_owned();
        let demux = Demuxer::open(&path)?;
        let s = demux
            .streams
            .iter()
            .find(|s| !s.video && s.channels > 0)
            .ok_or_else(|| io::Error::other("no sound in the file"))?;
        let dec = FileDecoder::open(s, None)?;
        let file_len = demux
            .duration_us
            .filter(|&us| us > 0)
            .map(|us| Duration::from_micros(us as u64));
        let info = AudioInfo {
            // SAFETY: a codec id from the demuxer; FFmpeg names any id.
            codec: name(unsafe { ff::avcodec_get_name(s.codec_id) }).unwrap_or_default(),
            sample_rate: s.sample_rate,
            channels: s.channels,
            end: stream_end(s, demux.start_us),
        };
        let (index, tb) = (s.index, s.time_base);
        let mut r = AudioReader {
            demux,
            dec,
            index,
            tb,
            rate: cfg.sample_rate.clamp(8_000, 384_000),
            channels: usize::from(cfg.channels.clamp(1, 64)),
            span: Span::new(cfg.in_point, cfg.out_point, file_len, cfg.loops),
            swr: None,
            pos: 0,
            queue: VecDeque::new(),
            queued: 0,
            play: 0,
            play_samples: 0,
            at: None,
            eof: false,
            ended: false,
            info,
        };
        r.seek(0)?;
        Ok(r)
    }

    /// The file's sound stream.
    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    /// The timeline's length in samples (`None`: endless, or not known). [`read`](Self::read)
    /// gives every one of them.
    pub fn length(&self) -> Option<u64> {
        self.span.total().map(|d| self.samples_in(d))
    }

    /// The position the next [`read`](Self::read) without a seek starts at.
    pub fn position(&self) -> u64 {
        self.pos
    }

    fn samples_in(&self, d: Duration) -> u64 {
        (d.as_nanos() * u128::from(self.rate) / 1_000_000_000) as u64
    }

    /// A play's length in samples, where known.
    fn play_len(&self) -> Option<u64> {
        self.span.len.map(|l| self.samples_in(l))
    }

    /// Moves to timeline sample `pos`: the file from a little before it (the decoder warmed
    /// up), decoded up to it.
    pub fn seek(&mut self, pos: u64) -> io::Result<()> {
        let t =
            Duration::from_nanos((u128::from(pos) * 1_000_000_000 / u128::from(self.rate)) as u64);
        let (play, at) = self.span.locate(t);
        self.play = play;
        self.ended = self.span.total().is_some_and(|total| t >= total);
        self.pos = pos;
        // The samples of this play before `at`, as if queued: what comes before it goes.
        self.play_samples = self.samples_in(at.saturating_sub(self.span.start));
        self.decode_from(at)
    }

    /// Decoding again from file time `at`: the demuxer a little before it (from the very
    /// start, the file opened again: an AAC file's priming comes before its time 0, and a
    /// seek misses it), the queue emptied.
    fn decode_from(&mut self, at: Duration) -> io::Result<()> {
        if before(at) <= 0 {
            self.demux.rewind()?;
        } else {
            seek_before(&mut self.demux, at)?;
        }
        self.dec.flush();
        self.swr = None;
        self.queue.clear();
        self.queued = 0;
        self.at = None;
        self.eof = false;
        Ok(())
    }

    /// Fills `out` (interleaved, `channels` per frame) from timeline sample `pos` (a seek
    /// first when it isn't where the last read ended): the frames read, fewer only at the
    /// timeline's end (the rest of `out` silence).
    pub fn read(&mut self, pos: u64, out: &mut [f32]) -> io::Result<usize> {
        if pos != self.pos {
            self.seek(pos)?;
        }
        let want = out.len() / self.channels;
        while self.queued < want as u64 && !self.ended {
            self.decode_some()?;
        }
        let n = self.take(&mut out[..want * self.channels]);
        out[n * self.channels..].iter_mut().for_each(|s| *s = 0.0);
        self.pos += n as u64;
        Ok(n)
    }

    /// Queued frames into `out` (whole frames): how many.
    fn take(&mut self, out: &mut [f32]) -> usize {
        let mut i = 0;
        while i < out.len()
            && let Some(c) = self.queue.front_mut()
        {
            let done = match c {
                Chunk::Sound(v, off) => {
                    let n = (v.len() - *off).min(out.len() - i);
                    out[i..i + n].copy_from_slice(&v[*off..*off + n]);
                    *off += n;
                    i += n;
                    *off == v.len()
                }
                Chunk::Silence(k) => {
                    let n = (*k).min(((out.len() - i) / self.channels) as u64);
                    let samples = n as usize * self.channels;
                    out[i..i + samples].iter_mut().for_each(|s| *s = 0.0);
                    *k -= n;
                    i += samples;
                    *k == 0
                }
            };
            if done {
                self.queue.pop_front();
            }
        }
        let frames = i / self.channels;
        self.queued -= frames as u64;
        frames
    }

    fn silence(&mut self, frames: u64) {
        if frames > 0 {
            self.queued += frames;
            self.play_samples += frames;
            self.queue.push_back(Chunk::Silence(frames));
        }
    }

    /// Decoded samples queued at their place in the play: what is before the place reached
    /// (or the in point) dropped, a gap before them silence, what is past the play's end
    /// cut.
    fn place(&mut self, mut s: Vec<f32>) {
        let frames = (s.len() / self.channels) as i64;
        let Some(at) = self.at else { return };
        self.at = Some(at + frames);
        let reached = self.play_samples as i64;
        if at > reached {
            self.silence((at - reached) as u64);
        } else if at < reached {
            let drop = ((reached - at).min(frames)) as usize * self.channels;
            s.drain(..drop);
        }
        let mut n = (s.len() / self.channels) as u64;
        if let Some(l) = self.play_len() {
            n = n.min(l.saturating_sub(self.play_samples));
            s.truncate(n as usize * self.channels);
        }
        if n > 0 {
            self.queued += n;
            self.play_samples += n;
            self.queue.push_back(Chunk::Sound(s, 0));
        }
    }

    /// Decodes some more into the queue, handling the end of a play.
    fn decode_some(&mut self) -> io::Result<()> {
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
        while let Some((f, ts)) = self.dec.receive()? {
            any = true;
            let mut skip = 0;
            if self.at.is_none() {
                // The first frame from here: its time says where it sits in the play. Its
                // samples before a place where the file's samples and the output's coincide
                // go, so it is resampled on the same grid as a read from the start (else a
                // fraction of a sample off).
                let ft = file_time(ts.unwrap_or(0), self.tb, self.demux.start_us);
                // SAFETY: a live decoded audio frame.
                let (src, n) = unsafe { ((*f.0).sample_rate.max(1) as u64, (*f.0).nb_samples) };
                let s0 = ((ft.as_nanos() * u128::from(src) + 500_000_000) / 1_000_000_000) as u64;
                let step = src / gcd(src, u64::from(self.rate));
                skip = (step - s0 % step) % step;
                if skip >= n.max(0) as u64 {
                    continue;
                }
                let out0 = ((s0 + skip) * u64::from(self.rate) / src) as i64;
                self.at = Some(out0 - self.samples_in(self.span.start) as i64);
            }
            let s = Swr::run(
                &mut self.swr,
                Some((f, skip as usize)),
                self.rate,
                self.channels,
            )?;
            self.place(s);
            if self.play_len().is_some_and(|l| self.play_samples >= l) {
                return self.play_ended();
            }
        }
        if self.eof && !any {
            // The resampler's last samples (its filter holds a few back).
            let s = Swr::run(&mut self.swr, None, self.rate, self.channels)?;
            self.place(s);
            return self.play_ended();
        }
        Ok(())
    }

    /// The end of a play: silence to its length (the sound ended before the out point),
    /// then the next play from the in point, or the timeline's end.
    fn play_ended(&mut self) -> io::Result<()> {
        match self.play_len() {
            Some(l) => self.silence(l.saturating_sub(self.play_samples)),
            None => {
                // The file's length learnt from its sound, for the next plays.
                self.span.len = Some(Duration::from_nanos(
                    (u128::from(self.play_samples) * 1_000_000_000 / u128::from(self.rate)) as u64,
                ))
                .filter(|l| !l.is_zero());
            }
        }
        if self.span.plays.is_none_or(|n| self.play + 1 < n) && self.span.len.is_some() {
            self.play += 1;
            self.play_samples = 0;
            let (queue, queued) = (std::mem::take(&mut self.queue), self.queued);
            self.decode_from(self.span.start)?;
            (self.queue, self.queued) = (queue, queued);
        } else {
            self.ended = true;
        }
        Ok(())
    }
}

/// Any decoded sound to interleaved float at a rate and channel count (swresample).
struct Swr {
    ctx: *mut ff::SwrContext,
    key: (c_int, c_int, c_int),
}

// SAFETY: the context is its own, used through &mut access only.
unsafe impl Send for Swr {}

impl Drop for Swr {
    fn drop(&mut self) {
        // SAFETY: the context made in `run`, freed once.
        unsafe { ff::swr_free(&mut self.ctx) };
    }
}

impl Swr {
    /// `f` resampled, less its first `skip` samples; `None`: the samples the resampler
    /// still holds (at the end).
    fn run(
        this: &mut Option<Swr>,
        f: Option<(FrameRef, usize)>,
        rate: u32,
        channels: usize,
    ) -> io::Result<Vec<f32>> {
        let Some((f, skip)) = f else {
            return Ok(this.as_ref().map(|r| r.flush(channels)).unwrap_or_default());
        };
        // SAFETY: `f` is a live audio frame; the context is ours; swr writes at most `max`
        // frames of `channels` interleaved floats into `out`.
        unsafe {
            let fr = &*f.0;
            let key = (fr.format, fr.sample_rate, fr.ch_layout.nb_channels);
            if this.as_ref().is_none_or(|r| r.key != key) {
                let mut ctx = std::ptr::null_mut();
                let mut out_layout: ff::AVChannelLayout = std::mem::zeroed();
                ff::av_channel_layout_default(&mut out_layout, channels as c_int);
                check(
                    ff::swr_alloc_set_opts2(
                        &mut ctx,
                        &out_layout,
                        ff::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                        rate as c_int,
                        &fr.ch_layout,
                        ffi::sample_fmt(fr.format),
                        fr.sample_rate,
                        0,
                        std::ptr::null_mut(),
                    ),
                    "resampler",
                )?;
                // Mono to more channels: the one channel copied whole into each (FFmpeg's
                // own rule puts it 3 dB down into left and right).
                if fr.ch_layout.nb_channels == 1 && channels > 1 {
                    let whole = vec![1.0f64; channels];
                    check(
                        ff::swr_set_matrix(ctx, whole.as_ptr(), 1),
                        "resampler matrix",
                    )?;
                }
                if let Err(e) = check(ff::swr_init(ctx), "resampler") {
                    ff::swr_free(&mut ctx);
                    return Err(e);
                }
                *this = Some(Swr { ctx, key });
            }
            let r = this.as_ref().expect("made above");
            let max = (fr.nb_samples as i64 * i64::from(rate) / i64::from(fr.sample_rate.max(1))
                + 256) as usize;
            let mut out = vec![0f32; max * channels];
            let mut outp = [out.as_mut_ptr() as *mut u8];
            // The input from sample `skip` on: each plane's start moved on (one plane holds
            // every channel in a packed format).
            let skip = skip.min(fr.nb_samples.max(0) as usize);
            let fmt = ffi::sample_fmt(fr.format);
            let planar = ff::av_sample_fmt_is_planar(fmt) != 0;
            let planes = if planar {
                fr.ch_layout.nb_channels.max(1) as usize
            } else {
                1
            };
            let step = ff::av_get_bytes_per_sample(fmt).max(0) as usize
                * if planar {
                    1
                } else {
                    fr.ch_layout.nb_channels.max(1) as usize
                };
            let input: Vec<*const u8> = (0..planes)
                .map(|p| (*fr.extended_data.add(p) as *const u8).add(skip * step))
                .collect();
            let n = check(
                ff::swr_convert(
                    r.ctx,
                    outp.as_mut_ptr(),
                    max as c_int,
                    input.as_ptr() as *mut *const u8,
                    fr.nb_samples - skip as c_int,
                ),
                "resampling",
            )? as usize;
            out.truncate(n * channels);
            Ok(out)
        }
    }
}

impl Swr {
    /// What the resampler holds back (its filter's last samples), drained.
    fn flush(&self, channels: usize) -> Vec<f32> {
        // SAFETY: our context; swr writes at most `max` frames of `channels` interleaved
        // floats into `out`; a null input drains it.
        unsafe {
            let max = (ff::swr_get_out_samples(self.ctx, 0).max(0) + 64) as usize;
            let mut out = vec![0f32; max * channels];
            let mut outp = [out.as_mut_ptr() as *mut u8];
            let n = ff::swr_convert(
                self.ctx,
                outp.as_mut_ptr(),
                max as c_int,
                std::ptr::null_mut(),
                0,
            );
            out.truncate(n.max(0) as usize * channels);
            out
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}
