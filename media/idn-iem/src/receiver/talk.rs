//! Push-to-talk (idn-iem.md §I.8.5): the microphone at any rate and channel count, made
//! mono at 48 kHz, coded in 10 ms Opus frames and sent as TALK; the last frame of a press
//! carries LAST.

use crate::codec::Encoder;

/// The talkback frame: 10 ms (the desk's lane holds 40 ms; half the packets of 5 ms).
pub(crate) const TALK_FRAME: usize = 480;
/// Talkback's bit rate (voice).
pub(crate) const TALK_BITRATE: u32 = 48_000;

/// A streaming sample-rate converter (windowed sinc, 32 taps): one channel.
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// The next output's place in `hist`.
    pos: f64,
    hist: Vec<f32>,
    /// The filter's cut-off (of the input's Nyquist).
    cut: f64,
}

const HALF: usize = 16;

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let step = f64::from(from.max(1)) / f64::from(to.max(1));
        Resampler {
            step,
            pos: HALF as f64,
            hist: vec![0.0; HALF],
            cut: (1.0 / step).min(1.0) * 0.92,
        }
    }

    fn kernel(&self, x: f64) -> f64 {
        let a = x.abs();
        if a >= HALF as f64 {
            return 0.0;
        }
        let y = std::f64::consts::PI * x * self.cut;
        let sinc = if y.abs() < 1e-9 { 1.0 } else { y.sin() / y };
        // (Blackman over the taps.)
        let t = (x / HALF as f64 + 1.0) * 0.5;
        let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos()
            + 0.08 * (4.0 * std::f64::consts::PI * t).cos();
        self.cut * sinc * w
    }

    /// `input` in, what it gives out appended to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.hist.extend_from_slice(input);
        while self.pos + (HALF as f64) < self.hist.len() as f64 {
            let base = self.pos.floor() as usize;
            let frac = self.pos - base as f64;
            let mut acc = 0.0f64;
            for j in 0..2 * HALF {
                let k = base + j + 1 - HALF;
                acc += f64::from(self.hist[k]) * self.kernel(frac - (j as f64 + 1.0 - HALF as f64));
            }
            out.push(acc as f32);
            self.pos += self.step;
        }
        let used = (self.pos.floor() as usize).saturating_sub(HALF);
        if used > 0 {
            self.hist.drain(..used);
            self.pos -= used as f64;
        }
    }
}

/// The talkback coder: what's been fed, waiting for a whole frame.
pub(crate) struct Talk {
    pub on: bool,
    enc: Option<Encoder>,
    rate: u32,
    rs: Option<Resampler>,
    mono: Vec<f32>,
    acc: Vec<f32>,
    pub seq: u32,
    pub frames: u64,
}

impl Talk {
    pub fn new() -> Talk {
        Talk {
            on: false,
            enc: None,
            rate: crate::RATE,
            rs: None,
            mono: Vec::new(),
            acc: Vec::new(),
            seq: 0,
            frames: 0,
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        if self.enc.is_none() {
            self.enc = Some(Encoder::new(TALK_FRAME, TALK_BITRATE)?);
        }
        self.on = true;
        self.acc.clear();
        Ok(())
    }

    /// `pcm` (interleaved, `channels` a frame, at `rate`) in; each whole frame coded and
    /// given to `send` (its Opus bytes, its samples, whether it is the last).
    pub fn feed(
        &mut self,
        pcm: &[f32],
        rate: u32,
        channels: u16,
        mut send: impl FnMut(&[u8], u16, bool, u32),
    ) {
        if !self.on || rate == 0 || channels == 0 {
            return;
        }
        let c = usize::from(channels);
        self.mono.clear();
        self.mono.extend(pcm.chunks_exact(c).map(|f| {
            let s = f
                .iter()
                .map(|x| if x.is_finite() { *x } else { 0.0 })
                .sum::<f32>();
            s / c as f32
        }));
        if rate == crate::RATE {
            self.acc.extend_from_slice(&self.mono);
        } else {
            if self.rate != rate || self.rs.is_none() {
                self.rate = rate;
                self.rs = Some(Resampler::new(rate, crate::RATE));
            }
            if let Some(rs) = &mut self.rs {
                rs.process(&self.mono, &mut self.acc);
            }
        }
        self.flush(false, &mut send);
    }

    /// The press ends: what's left (padded to a frame) goes as the last.
    pub fn stop(&mut self, mut send: impl FnMut(&[u8], u16, bool, u32)) {
        if !self.on {
            return;
        }
        let pad = (TALK_FRAME - self.acc.len() % TALK_FRAME) % TALK_FRAME;
        self.acc.extend(std::iter::repeat_n(0.0, pad));
        if self.acc.is_empty() {
            self.acc.resize(TALK_FRAME, 0.0);
        }
        self.flush(true, &mut send);
        self.on = false;
        self.rs = None;
    }

    fn flush(&mut self, last: bool, send: &mut impl FnMut(&[u8], u16, bool, u32)) {
        let Some(enc) = &mut self.enc else { return };
        while self.acc.len() >= TALK_FRAME {
            let frame: Vec<f32> = self
                .acc
                .drain(..TALK_FRAME)
                .map(|v| v.clamp(-1.0, 1.0))
                .collect();
            let end = last && self.acc.len() < TALK_FRAME;
            if let Ok(data) = enc.encode(&frame) {
                send(data, TALK_FRAME as u16, end, self.seq);
                self.seq = self.seq.wrapping_add(1);
                self.frames += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Amplitude of `f` in `x` at 48 kHz.
    fn amp(x: &[f32], f: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, v) in x.iter().enumerate() {
            let a = 2.0 * std::f64::consts::PI * f64::from(f) * n as f64 / 48_000.0;
            re += f64::from(*v) * a.cos();
            im -= f64::from(*v) * a.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// 44.1 and 16 kHz to 48 kHz: the right number of samples, the tone at its level.
    #[test]
    fn rates_convert() {
        for rate in [44_100u32, 16_000, 96_000] {
            let mut rs = Resampler::new(rate, 48_000);
            let src = tone(1000.0, rate, rate as usize);
            let mut out = Vec::new();
            for c in src.chunks(441) {
                rs.process(c, &mut out);
            }
            // (Less the filter's half-length at the end, still waiting for what follows.)
            assert!(
                (out.len() as i64 - 48_000).abs() < 60,
                "{rate}: {}",
                out.len()
            );
            let a = amp(&out[4800..43_200], 1000.0);
            assert!((a - 0.5).abs() < 0.02, "{rate}: {a}");
        }
    }

    /// Frames of 10 ms, the last marked, seq counting on.
    #[test]
    fn a_press_is_frames_then_last() {
        let mut t = Talk::new();
        let mut got = Vec::new();
        t.feed(&tone(440.0, 48_000, 1000), 48_000, 1, |_, _, _, _| {});
        assert!(t.frames == 0, "not on: nothing");
        t.start().unwrap();
        let stereo: Vec<f32> = tone(440.0, 48_000, 1000)
            .iter()
            .flat_map(|v| [*v, *v])
            .collect();
        t.feed(&stereo, 48_000, 2, |d, n, last, seq| {
            got.push((d.len(), n, last, seq))
        });
        assert_eq!(got.len(), 2);
        t.stop(|d, n, last, seq| got.push((d.len(), n, last, seq)));
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|g| g.1 == 480 && g.0 > 0));
        assert_eq!(
            got.iter().map(|g| (g.2, g.3)).collect::<Vec<_>>(),
            [(false, 0), (false, 1), (true, 2)]
        );
        // A press with nothing fed still says it ended.
        t.start().unwrap();
        let mut n = 0;
        t.stop(|_, _, last, _| {
            assert!(last);
            n += 1
        });
        assert_eq!(n, 1);
    }
}
