//! Resampling with a windowed-sinc filter (rubato), fed in fixed chunks, with a ratio
//! the servo adjusts a little at a time.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::io;

/// Planar f32 audio through a resampler: push any number of frames, pull the
/// resampled chunks as they complete.
pub struct Rate {
    inner: SincFixedIn<f32>,
    chunk: usize,
    channels: usize,
    /// Frames waiting for a full chunk, per channel (only the channels in use hold
    /// them), and how many.
    pending: Vec<Vec<f32>>,
    frames: usize,
    /// The channels the caller feeds ([`Rate::set_used`]; all by default): only those
    /// are resampled. One taken out of use is fed silence for `flush` more chunks, so its
    /// filter forgets it and starts clean when it's used again.
    used: Vec<bool>,
    flush: Vec<u8>,
    mask: Vec<bool>,
    /// Input frames left of a channel's fade-in once back in use (a routed
    /// channel started with a step). Faded before resampling: a step in the input comes
    /// out of the filter as one, whatever is done after.
    fade_in: Vec<u32>,
    /// One chunk of input, reused.
    input: Vec<Vec<f32>>,
    /// The output buffer, sized for the largest chunk, reused.
    output: Vec<Vec<f32>>,
    /// The input and output rates are the same (passing through is possible).
    same_rate: bool,
    /// Passing the samples through instead of filtering ([`Rate::set_through`]): only
    /// when the rates are the same and the ratio is exactly 1.0. `want` is what was
    /// asked, `on` what the last chunk did; a change crossfades over one chunk.
    through: Through,
}

/// The pass-through's state. `hist[c]` holds channel `c`'s last `HL` input frames and,
/// after them, room for a chunk: the samples go out `delay` frames late, the filter's own
/// delay, so passing through and filtering line up. Leaving, the filter is reset and fed
/// those frames again (its memory is its last 256 input frames), so it carries on as if it
/// had never stopped. Kept only while allowed; a channel out of use keeps silence there
/// (cleared as it goes), untouched per chunk.
struct Through {
    allowed: bool,
    want: bool,
    on: bool,
    hist: Vec<Vec<f32>>,
    hl: usize,
    delay: usize,
}

/// The filter's length, in input frames.
const SINC_LEN: usize = 128;

/// A channel back in use fades in over this many input frames (10 ms at 48 kHz).
const FADE_IN: u32 = 480;

impl Rate {
    /// From `in_rate` to `out_rate` with `channels`, taking `chunk` input frames at a
    /// time; the ratio may later be adjusted by up to ±`max_adjust` (0.002 = 2000 ppm).
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        channels: usize,
        chunk: usize,
        max_adjust: f64,
    ) -> io::Result<Self> {
        let params = SincInterpolationParameters {
            sinc_len: SINC_LEN,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = out_rate as f64 / in_rate.max(1) as f64;
        let inner =
            SincFixedIn::<f32>::new(ratio, 1.0 + max_adjust.max(1e-6), params, chunk, channels)
                .map_err(|e| io::Error::other(format!("resampler: {e}")))?;
        let output = inner.output_buffer_allocate(true);
        // Whole chunks covering the filter's memory (2 × its 128-frame sinc).
        let hl = (2 * SINC_LEN).div_ceil(chunk) * chunk;
        let through = Through {
            allowed: false,
            want: false,
            on: false,
            hist: Vec::new(),
            hl,
            // The filter's own alignment: half its sinc and two frames (its first chunk
            // comes out that much short; `output_delay` says half the sinc). The test
            // below measures it.
            delay: SINC_LEN / 2 + 2,
        };
        Ok(Rate {
            inner,
            chunk,
            channels,
            pending: vec![Vec::with_capacity(chunk * 2); channels],
            frames: 0,
            used: vec![true; channels],
            flush: vec![0; channels],
            mask: vec![true; channels],
            fade_in: vec![0; channels],
            input: vec![vec![0.0; chunk]; channels],
            output,
            through,
            same_rate: in_rate == out_rate,
        })
    }

    /// Lets the samples pass through unfiltered while asked ([`Rate::set_through`]): a
    /// source on the same clock as the consumer, at the same rate, needs no resampling.
    /// Keeps a little history from now on (the filter's memory, to go back to it). No
    /// effect when the rates differ.
    pub fn allow_through(&mut self, allow: bool) {
        let t = &mut self.through;
        if !self.same_rate || allow == t.allowed {
            return;
        }
        t.allowed = allow;
        t.want &= allow;
        t.hist = if allow {
            vec![vec![0.0; t.hl + self.chunk]; self.channels]
        } else {
            Vec::new()
        };
        // Filling it takes the filter's memory again before passing through can start.
        t.on = false;
    }

    /// Asks to pass the samples through (`true`, the ratio set to exactly 1.0 at once:
    /// asked only when the correction is a few ppm at most) or to filter again. Takes effect
    /// on the next chunk, crossfaded over it.
    pub fn set_through(&mut self, on: bool) {
        let t = &mut self.through;
        if !t.allowed || on == t.want {
            return;
        }
        t.want = on;
        if on {
            let _ = self.inner.set_resample_ratio_relative(1.0, false);
        }
    }

    /// Whether the samples pass through (asked and taken effect, or about to).
    pub fn is_through(&self) -> bool {
        self.through.want
    }

    /// Whether channel `c` has sound to resample (all channels, until `set_used`).
    pub fn is_used(&self, c: usize) -> bool {
        self.used.get(c).copied().unwrap_or(false)
    }

    /// Forgets the sound so far: what waits for a chunk and the filter's memory. For a
    /// source back after a pause: the filter still held the sound from before it, which
    /// came out ahead of the new (lab 356-A).
    pub fn forget(&mut self) {
        self.inner.reset();
        for p in &mut self.pending {
            p.clear();
        }
        self.frames = 0;
        self.flush.fill(0);
        for h in &mut self.through.hist {
            h.fill(0.0);
        }
    }

    /// The channels in use (`used[c]`; those past its end aren't): a routed lane feeds a
    /// few of the 64 channels of a mixer's inputs, and resampling all of them cost most
    /// of the mixer's time. Their output is silence; a channel back in use
    /// fades in over 10 ms (its first sound comes after the filter's delay). Gives the
    /// channels just taken out of use, whose output stops at once: what the caller still
    /// holds of them is its to fade out.
    pub fn set_used(&mut self, used: &[bool]) -> Vec<usize> {
        let mut gone = Vec::new();
        // The filter's memory (its sinc length, 128 frames) in chunks, and one more.
        let flush = (256 / self.chunk + 2).min(255) as u8;
        // Back in use with none in use meanwhile and a flush unfinished: nothing went
        // through the filter (a lane with no channel routed is pushed no frames), so it
        // still held the channel's sound from before, which played at full level ahead of
        // the fade-in (lab 356-A). With no channel playing it is started clean.
        let idle = !self.used.iter().any(|&u| u);
        let back =
            (0..self.channels).any(|c| used.get(c).copied().unwrap_or(false) && !self.used[c]);
        if idle && back && self.flush.iter().any(|&f| f > 0) {
            self.inner.reset();
            self.flush.fill(0);
        }
        for c in 0..self.channels {
            let u = used.get(c).copied().unwrap_or(false);
            if u && !self.used[c] {
                // In step with the others: silence up to what they hold.
                self.pending[c].clear();
                self.pending[c].resize(self.frames, 0.0);
                self.flush[c] = 0;
                self.fade_in[c] = FADE_IN;
            } else if !u && self.used[c] {
                self.pending[c].clear();
                if let Some(h) = self.through.hist.get_mut(c) {
                    h.fill(0.0);
                }
                self.flush[c] = flush;
                self.fade_in[c] = 0;
                gone.push(c);
            }
            self.used[c] = u;
        }
        gone
    }

    /// Sets the ratio relative to the nominal one (1.0 = nominal), ramped over the next
    /// chunk so nothing clicks.
    pub fn set_relative(&mut self, rel: f64) {
        if self.through.want {
            return;
        }
        let _ = self.inner.set_resample_ratio_relative(rel, true);
    }

    /// Queues planar frames (`frames[c]` is channel `c`; shorter channels are padded with
    /// silence, extra ones ignored).
    pub fn push(&mut self, frames: &[Vec<f32>]) {
        let n = frames.iter().map(Vec::len).max().unwrap_or(0);
        self.frames += n;
        for c in 0..self.channels {
            if !self.used[c] {
                continue;
            }
            let p = &mut self.pending[c];
            let at = p.len();
            match frames.get(c) {
                Some(f) => {
                    p.extend_from_slice(f);
                    p.resize(p.len() + n - f.len(), 0.0);
                }
                None => p.resize(p.len() + n, 0.0),
            }
            // From its first sound: a route's channel arrives a round trip after the route
            // (the sender asked again), as zeros until then.
            let fade = &mut self.fade_in[c];
            for v in &mut p[at..] {
                if *fade == 0 {
                    break;
                }
                if *fade == FADE_IN && *v == 0.0 {
                    continue;
                }
                *v *= (FADE_IN - *fade + 1) as f32 / FADE_IN as f32;
                *fade -= 1;
            }
        }
    }

    /// Frames queued but not yet resampled (less than a chunk).
    pub fn pending(&self) -> usize {
        self.frames
    }

    /// The filter's delay, in output frames: how far the output lags the input.
    pub fn output_delay(&self) -> usize {
        self.inner.output_delay()
    }

    /// Resamples every complete chunk, handing each result to `f` as planar frames.
    pub fn pull(&mut self, mut f: impl FnMut(&[Vec<f32>], usize)) -> io::Result<()> {
        while self.frames >= self.chunk {
            self.frames -= self.chunk;
            for c in 0..self.channels {
                if self.used[c] {
                    self.input[c].copy_from_slice(&self.pending[c][..self.chunk]);
                    self.pending[c].drain(..self.chunk);
                } else if self.flush[c] > 0 {
                    self.input[c].fill(0.0);
                }
                self.mask[c] = self.used[c] || self.flush[c] > 0;
            }
            let out = if self.through.allowed {
                self.chunk_through()?
            } else {
                self.filter()?
            };
            f(&self.output, out);
        }
        Ok(())
    }

    /// The chunk in `input` through the filter, into `output`: its frame count.
    fn filter(&mut self) -> io::Result<usize> {
        let (_, out) = self
            .inner
            .process_into_buffer(&self.input, &mut self.output, Some(&self.mask))
            .map_err(|e| io::Error::other(format!("resampler: {e}")))?;
        for c in 0..self.channels {
            if !self.used[c] && self.flush[c] > 0 {
                // Being flushed: its filter's tail isn't played (it stopped at once).
                self.flush[c] -= 1;
                self.output[c].fill(0.0);
            }
        }
        Ok(out)
    }

    /// The chunk in `input` while passing through is allowed: into the history, then out
    /// filtered, passed through, or crossfaded from one to the other.
    fn chunk_through(&mut self) -> io::Result<usize> {
        let (chunk, hl) = (self.chunk, self.through.hl);
        for c in (0..self.channels).filter(|&c| self.used[c]) {
            self.through.hist[c][hl..].copy_from_slice(&self.input[c]);
        }
        let (was, now) = (self.through.on, self.through.want);
        if was && !now {
            // Back to filtering: the filter as it would be had it run all along.
            self.inner.reset();
            for k in 0..hl / chunk {
                for c in 0..self.channels {
                    let h = &self.through.hist[c][k * chunk..(k + 1) * chunk];
                    self.input[c].copy_from_slice(h);
                }
                self.inner
                    .process_into_buffer(&self.input, &mut self.output, Some(&self.mask))
                    .map_err(|e| io::Error::other(format!("resampler: {e}")))?;
            }
            for c in 0..self.channels {
                self.input[c].copy_from_slice(&self.through.hist[c][hl..]);
            }
        }
        let out = if now && was {
            self.pass(0)
        } else {
            let out = self.filter()?;
            if now != was {
                // The crossfade: the path left fades out over the chunk as the new one
                // fades in.
                let d = hl - self.through.delay;
                let n = out.min(chunk);
                for c in 0..self.channels {
                    if !self.mask[c] {
                        continue;
                    }
                    let h = &self.through.hist[c][d..d + chunk];
                    let o = &mut self.output[c];
                    for i in 0..n {
                        let w = (i + 1) as f32 / n as f32;
                        let (from, to) = if now { (o[i], h[i]) } else { (h[i], o[i]) };
                        o[i] = from + (to - from) * w;
                    }
                }
                if now {
                    // Passing through from here: exactly a chunk out per chunk in.
                    self.pass(n)
                } else {
                    out
                }
            } else {
                out
            }
        };
        self.through.on = now;
        for c in (0..self.channels).filter(|&c| self.used[c]) {
            self.through.hist[c].copy_within(chunk.., 0);
        }
        Ok(out)
    }

    /// Passes the chunk through (from frame `from` on: before it, the crossfade's), `delay`
    /// frames late. Only the channels in use: the others' output isn't read.
    fn pass(&mut self, from: usize) -> usize {
        let (chunk, d) = (self.chunk, self.through.hl - self.through.delay);
        for c in (0..self.channels).filter(|&c| self.used[c]) {
            self.output[c][from..chunk].copy_from_slice(&self.through.hist[c][d + from..d + chunk]);
        }
        if self.through.on {
            for f in self.flush.iter_mut() {
                *f = f.saturating_sub(1);
            }
        }
        chunk
    }
}

/// One block from rate `from` to `to`, simply, with no state between blocks: going
/// down, each output sample the mean of its span (a box filter: enough for speech
/// recognition); going up, between its neighbours (linear). For listening and captions,
/// not the desk ([`Rate`] is the quality path). Captions and the listening page each had
/// their own.
pub fn simple(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    let step = from as f64 / to as f64;
    let n = (samples.len() as f64 / step) as usize;
    if from > to {
        (0..n)
            .map(|i| {
                let a = ((i as f64 * step) as usize).min(samples.len() - 1);
                let b = (((i + 1) as f64 * step) as usize).clamp(a + 1, samples.len());
                samples[a..b].iter().sum::<f32>() / (b - a) as f32
            })
            .collect()
    } else {
        (0..n)
            .map(|i| {
                let at = i as f64 * step;
                let k = at as usize;
                let f = (at - k as f64) as f32;
                let a = samples[k.min(samples.len() - 1)];
                let b = samples[(k + 1).min(samples.len() - 1)];
                a + (b - a) * f
            })
            .collect()
    }
}

/// Interleaved device samples to planar channels (appending), `channels` wide.
pub fn deinterleave(data: &[f32], channels: usize, out: &mut [Vec<f32>]) {
    if channels == 0 {
        return;
    }
    for frame in data.chunks_exact(channels) {
        for (c, s) in frame.iter().enumerate() {
            if let Some(o) = out.get_mut(c) {
                o.push(*s);
            }
        }
    }
}

/// Source channels onto `n` device channels: channel for channel; a mono source goes to
/// the first two; the rest are silent.
pub fn map_channels(src: &[Vec<f32>], frames: usize, n: usize) -> Vec<Vec<f32>> {
    (0..n)
        .map(|c| {
            let from = if src.len() == 1 && c < 2 { 0 } else { c };
            match src.get(from) {
                Some(s) => {
                    let mut v = s[..frames.min(s.len())].to_vec();
                    v.resize(frames, 0.0);
                    v
                }
                None => vec![0.0; frames],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {

    /// A channel's output is the same whether the others are resampled with it or left
    /// out (the vendored rubato walks only the active channels: vendor/README.md).
    #[test]
    fn a_channel_resamples_the_same_alone_or_with_others() {
        use super::Rate;
        let tone = |k: usize| -> Vec<f32> {
            (0..4800)
                .map(|i| ((i * (k + 3)) as f32 * 0.01).sin() * 0.5)
                .collect()
        };
        let run = |used: &[bool]| -> Vec<Vec<f32>> {
            let mut r = Rate::new(48_000, 48_000, 4, 48, 0.003).unwrap();
            r.set_used(used);
            r.set_relative(1.0007);
            let input: Vec<Vec<f32>> = (0..4).map(tone).collect();
            let mut out = vec![Vec::new(); 4];
            for block in 0..10 {
                let part: Vec<Vec<f32>> = input
                    .iter()
                    .map(|c| c[block * 480..(block + 1) * 480].to_vec())
                    .collect();
                r.push(&part);
                r.pull(|planar, n| {
                    for (o, p) in out.iter_mut().zip(planar) {
                        o.extend_from_slice(&p[..n]);
                    }
                })
                .unwrap();
            }
            out
        };
        let all = run(&[true; 4]);
        let some = run(&[false, true, false, true]);
        assert_eq!(all[1], some[1]);
        assert_eq!(all[3], some[3]);
        assert!(some[0].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn simple_rates_down_by_spans_and_up_between() {
        use super::simple;
        // 48 → 16 kHz: each output the mean of three.
        let x: Vec<f32> = (0..9).map(|i| i as f32).collect();
        assert_eq!(simple(&x, 48_000, 16_000), [1.0, 4.0, 7.0]);
        // 24 → 48 kHz: halfway values between.
        assert_eq!(
            simple(&[0.0, 2.0, 4.0], 24_000, 48_000),
            [0.0, 1.0, 2.0, 3.0, 4.0, 4.0]
        );
        assert_eq!(simple(&[1.0, 2.0], 48_000, 48_000), [1.0, 2.0]);
        assert_eq!(simple(&[], 44_100, 48_000), Vec::<f32>::new());
    }

    use super::*;

    #[test]
    fn same_rate_passes_a_sine_through() {
        let mut r = Rate::new(48000, 48000, 1, 480, 0.002).unwrap();
        let sine: Vec<f32> = (0..48000)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin())
            .collect();
        r.push(std::slice::from_ref(&sine));
        let mut out = Vec::new();
        r.pull(|o, n| out.extend_from_slice(&o[0][..n])).unwrap();
        assert!(out.len() >= 48000 - 480, "{}", out.len());
        // After the filter's delay (found by trying lags), the output is the input within
        // the filter's ripple.
        let err_at = |d: usize| {
            (d + 1000..out.len().min(40000))
                .map(|i| (out[i] - sine[i - d]).abs())
                .fold(0.0f32, f32::max)
        };
        let (d, err) = (0..400usize)
            .map(|d| (d, err_at(d)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        assert!(err < 0.02, "max error {err} at lag {d}");
    }

    /// Only the channels in use are resampled; the others are silent, and one taken out
    /// of use and back starts clean (no old sound from the filter's memory).
    #[test]
    fn unused_channels_are_silent_and_come_back_clean() {
        let sine = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin())
                .collect()
        };
        let mut r = Rate::new(48000, 48000, 3, 96, 0.002).unwrap();
        let mut full = Rate::new(48000, 48000, 3, 96, 0.002).unwrap();
        r.set_used(&[true, false, true]);
        let block = vec![sine(4800), sine(4800), sine(4800)];
        r.push(&block);
        full.push(&block);
        let (mut a, mut b) = (vec![Vec::new(); 3], vec![Vec::new(); 3]);
        r.pull(|o, n| (0..3).for_each(|c| a[c].extend_from_slice(&o[c][..n])))
            .unwrap();
        full.pull(|o, n| (0..3).for_each(|c| b[c].extend_from_slice(&o[c][..n])))
            .unwrap();
        assert_eq!(a[0], b[0], "a channel in use as before");
        assert_eq!(a[2], b[2]);
        assert!(a[1].iter().all(|&v| v == 0.0), "unused: silent");
        // Channel 0 out of use: silent once flushed; back in use on silence: still silent
        // (nothing of the old sine left in the filter).
        r.set_used(&[false, false, true]);
        r.push(&block);
        r.pull(|_, _| {}).unwrap();
        r.set_used(&[true, false, true]);
        r.push(&[vec![0.0; 960], vec![0.0; 960], sine(960)]);
        let mut back = Vec::new();
        r.pull(|o, n| back.extend_from_slice(&o[0][..n])).unwrap();
        assert!(back.iter().all(|&v| v.abs() < 1e-6), "{:?}", &back[..8]);
    }

    /// A channel put back in use on a playing sound fades in rather than starting with
    /// a step.
    #[test]
    fn a_channel_back_in_use_fades_in() {
        let mut r = Rate::new(48000, 48000, 2, 96, 0.002).unwrap();
        let ones = |n: usize| vec![vec![0.5f32; n], vec![0.5f32; n]];
        r.push(&ones(4800));
        r.pull(|_, _| {}).unwrap();
        r.set_used(&[true, false]);
        r.push(&ones(960));
        r.pull(|_, _| {}).unwrap();
        r.set_used(&[true, true]);
        // (Zeros first, as a route's channel before the sender has it.)
        r.push(&[vec![0.5f32; 480], vec![0.0f32; 480]]);
        r.push(&ones(4800));
        let mut out = Vec::new();
        r.pull(|o, n| out.extend_from_slice(&o[1][..n])).unwrap();
        // No jump between neighbouring samples bigger than the ramp's (0.5 / 480).
        let step = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0f32, f32::max);
        assert!(step < 0.01, "largest step {step}");
        assert!(
            (out[out.len() - 1] - 0.5).abs() < 0.01,
            "then at full level"
        );
    }

    /// Passing through lines up with the filter: the samples come out the filter's delay
    /// late, the switch either way is a crossfade (no step), and back to filtering the
    /// output is exactly what a filter that never stopped makes.
    #[test]
    fn passing_through_lines_up_with_the_filter() {
        let chunk = 480;
        let sig = |n: usize| -> Vec<Vec<f32>> {
            (0..2)
                .map(|c| {
                    (0..chunk)
                        .map(|i| {
                            let t = (n * chunk + i) as f32 / 48_000.0;
                            0.4 * (t * 997.0 * std::f32::consts::TAU + c as f32).sin()
                                + 0.2 * (t * 6_100.0 * std::f32::consts::TAU).sin()
                        })
                        .collect()
                })
                .collect()
        };
        let mut reference = Rate::new(48_000, 48_000, 2, chunk, 0.002).unwrap();
        let mut rate = Rate::new(48_000, 48_000, 2, chunk, 0.002).unwrap();
        rate.allow_through(true);
        let delay = rate.through.delay;
        let mut input: Vec<f32> = Vec::new();
        let mut step = 0f32;
        let mut last = None::<f32>;
        for n in 0..60 {
            if n == 20 {
                rate.set_through(true);
            }
            if n == 40 {
                rate.set_through(false);
            }
            let block = sig(n);
            input.extend_from_slice(&block[0]);
            let mut want = Vec::new();
            reference.push(&block);
            reference
                .pull(|p, k| want.extend_from_slice(&p[0][..k]))
                .unwrap();
            let mut got = Vec::new();
            rate.push(&block);
            rate.pull(|p, k| got.extend_from_slice(&p[0][..k])).unwrap();
            if n > 0 {
                assert_eq!(got.len(), chunk, "chunk {n}");
            }
            if (21..40).contains(&n) {
                let at = n * chunk - delay;
                assert_eq!(
                    &got[..],
                    &input[at..at + chunk],
                    "passed through, chunk {n}"
                );
            } else if !(20..=40).contains(&n) {
                assert_eq!(got, want, "filtered, chunk {n}");
            }
            if n == 10 {
                // The filter's output is the input `delay` late (the signal is well inside
                // its passband).
                let at = n * chunk - delay;
                let off = want.iter().zip(&input[at..]).map(|(a, b)| (a - b).abs());
                assert!(
                    off.fold(0.0, f32::max) < 0.01,
                    "the filter's delay isn't {delay}"
                );
            }
            for &v in &got {
                if let Some(l) = last {
                    step = step.max((v - l).abs());
                }
                last = Some(v);
            }
        }
        // The signal's own steepest step is about 0.4·2π·997/48000 + 0.2·2π·6100/48000.
        assert!(step < 0.22, "largest step {step}");
    }

    #[test]
    fn ratio_changes_the_frame_count() {
        let mut r = Rate::new(48000, 48000, 2, 960, 0.002).unwrap();
        r.set_relative(1.001);
        r.push(&[vec![0.0; 96000], vec![0.0; 96000]]);
        let mut n = 0;
        r.pull(|_, k| n += k).unwrap();
        assert!((96000..=96200).contains(&n), "{n}");
        let mut r = Rate::new(44100, 48000, 1, 441, 0.002).unwrap();
        r.push(&[vec![0.0; 44100]]);
        let mut n = 0;
        r.pull(|_, k| n += k).unwrap();
        assert!((47900..=48100).contains(&n), "{n}");
    }

    #[test]
    fn channel_mapping() {
        let mono = vec![vec![0.5; 4]];
        let m = map_channels(&mono, 4, 3);
        assert_eq!(m[0], m[1]);
        assert_eq!(m[2], vec![0.0; 4]);
        let stereo = vec![vec![1.0; 2], vec![2.0; 2]];
        let m = map_channels(&stereo, 2, 1);
        assert_eq!(m, vec![vec![1.0; 2]]);
        let mut out = vec![Vec::new(); 2];
        deinterleave(&[1.0, 2.0, 3.0, 4.0], 2, &mut out);
        assert_eq!(out, vec![vec![1.0, 3.0], vec![2.0, 4.0]]);
    }
}
