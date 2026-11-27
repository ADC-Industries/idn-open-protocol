//! Speech noise suppression (an RNNoise-class network, `nnnoiseless`, BSD-3-Clause, its
//! model built in): one implementation shared by the mixer's channels and the switcher's
//! sound. 48 kHz mono, in blocks of any length; the network works on 10 ms frames, so the
//! sound comes out exactly [`LATENCY`] samples (20 ms) late.

use std::collections::VecDeque;

/// Samples the network takes at a time (10 ms at 48 kHz).
pub const FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE;

/// The delay a [`Denoiser`] adds, in samples: two frames (20 ms) — one gathering a frame,
/// one the network's own (its output overlaps each frame with the one before). Line
/// other signals up by it. (It was taken as one frame, which left a denoised mixer
/// channel 10 ms behind the others.)
pub const LATENCY: usize = 2 * FRAME;

/// The network works on 16-bit sample values.
const SCALE: f32 = 32768.0;

/// One mono signal's noise suppression. Its output is its input [`LATENCY`] samples late,
/// with steady noise (fans, hum, air conditioning, hiss) taken out and speech kept.
pub struct Denoiser {
    state: Box<nnnoiseless::DenoiseState<'static>>,
    /// Input waiting for a whole frame.
    pending: Vec<f32>,
    /// Output ready, primed with a frame of silence (the latency).
    ready: VecDeque<f32>,
    /// The network's first frame is unsettled: it goes out as silence.
    settled: bool,
    /// The last frame's voice probability, 0–1.
    voice: f32,
}

impl Default for Denoiser {
    fn default() -> Self {
        Denoiser::new()
    }
}

impl Clone for Denoiser {
    /// A fresh denoiser (the network's state starts again, a frame of silence first).
    fn clone(&self) -> Self {
        Denoiser::new()
    }
}

impl Denoiser {
    pub fn new() -> Denoiser {
        Denoiser {
            state: nnnoiseless::DenoiseState::new(),
            pending: Vec::with_capacity(FRAME),
            ready: std::iter::repeat_n(0.0, FRAME).collect(),
            settled: false,
            voice: 0.0,
        }
    }

    /// Suppresses the noise in `x` (48 kHz, nominally −1…1) in place; the result is the
    /// input [`LATENCY`] samples late.
    pub fn run(&mut self, x: &mut [f32]) {
        let mut out = [0.0f32; FRAME];
        for v in x.iter_mut() {
            let s = if v.is_finite() { *v } else { 0.0 };
            self.pending.push(s * SCALE);
            if self.pending.len() == FRAME {
                self.voice = self.state.process_frame(&mut out, &self.pending);
                self.pending.clear();
                if self.settled {
                    self.ready.extend(out.iter().map(|s| s / SCALE));
                } else {
                    self.settled = true;
                    self.ready.extend(std::iter::repeat_n(0.0, FRAME));
                }
            }
            *v = self.ready.pop_front().unwrap_or(0.0);
        }
    }

    /// How likely the last frame held speech, 0–1 (the network's voice activity).
    pub fn voice(&self) -> f32 {
        self.voice
    }
}

/// The network's own rate: sound at another is converted to it and back.
pub const NETWORK_RATE: u32 = 48_000;

/// Samples converted at a time (2.9 ms at 44.1 kHz, 1.3 at 96: a chunk is taken whole, so
/// its length is part of the delay; bug run J2-5: 480 added up to 10.9 ms).
const CONV_CHUNK: usize = 128;

/// Noise suppression at any of IDN's rates (44.1, 48 or 96 kHz; `idn-node.md` §N.40):
/// at 48 kHz the network as it is; at another rate the sound is converted to 48 kHz, run
/// through the network and converted back, which adds a fixed delay
/// ([`RateDenoiser::latency`]).
pub struct RateDenoiser {
    rate: u32,
    net: Denoiser,
    conv: Option<Conv>,
}

/// The conversions around the network, and the output primed by the extra delay.
struct Conv {
    down: idn_open_protocol::audio::resample::Rate,
    up: idn_open_protocol::audio::resample::Rate,
    input: Vec<Vec<f32>>,
    mid: Vec<f32>,
    out: VecDeque<f32>,
}

impl Conv {
    fn new(rate: u32) -> Option<Conv> {
        let down =
            idn_open_protocol::audio::resample::Rate::new(rate, NETWORK_RATE, 1, CONV_CHUNK, 0.0)
                .ok()?;
        let up =
            idn_open_protocol::audio::resample::Rate::new(NETWORK_RATE, rate, 1, CONV_CHUNK, 0.0)
                .ok()?;
        Some(Conv {
            down,
            up,
            input: vec![Vec::with_capacity(CONV_CHUNK * 4)],
            mid: Vec::with_capacity(CONV_CHUNK * 4),
            // Primed with what the two chunked conversions may hold back at most, so the
            // output never runs dry and its delay stays the same.
            out: std::iter::repeat_n(0.0, prime(rate)).collect(),
        })
    }

    /// `x` through: down, `net` (any mono process at 48 kHz), up; in place.
    /// No allocation once warm: the input and the middle kept with their room (bug run
    /// K6-1: two a channel a block on the clock thread, `to_vec` and `take`).
    fn run(&mut self, x: &mut [f32], net: &mut dyn FnMut(&mut [f32])) {
        self.input[0].clear();
        self.input[0].extend_from_slice(x);
        self.down.push(&self.input);
        let mid = &mut self.mid;
        let _ = self.down.pull(|o, n| mid.extend_from_slice(&o[0][..n]));
        if !self.mid.is_empty() {
            net(&mut self.mid);
            self.up.push(std::slice::from_ref(&self.mid));
            self.mid.clear();
            let out = &mut self.out;
            let _ = self.up.pull(|o, n| out.extend(&o[0][..n]));
        }
        for v in x.iter_mut() {
            *v = self.out.pop_front().unwrap_or(0.0);
        }
    }
}

/// The output's priming at `rate`: a chunk each way, in the rate's samples.
fn prime(rate: u32) -> usize {
    2 * (CONV_CHUNK * rate as usize).div_ceil(NETWORK_RATE as usize) + CONV_CHUNK
}

impl Default for RateDenoiser {
    fn default() -> Self {
        RateDenoiser::new(NETWORK_RATE)
    }
}

impl Clone for RateDenoiser {
    /// A fresh one at the same rate (as [`Denoiser`]'s clone).
    fn clone(&self) -> Self {
        RateDenoiser::new(self.rate)
    }
}

impl RateDenoiser {
    /// For sound at `rate` (one of 44100, 48000, 96000; another is taken as 48000).
    pub fn new(rate: u32) -> RateDenoiser {
        let rate = if matches!(rate, 44_100 | 96_000) {
            rate
        } else {
            NETWORK_RATE
        };
        RateDenoiser {
            rate,
            net: Denoiser::new(),
            conv: (rate != NETWORK_RATE).then(|| Conv::new(rate)).flatten(),
        }
    }

    /// Suppresses the noise in `x` (at the denoiser's rate) in place; the result is the
    /// input [`RateDenoiser::latency`] samples late.
    pub fn run(&mut self, x: &mut [f32]) {
        match &mut self.conv {
            None => self.net.run(x),
            Some(c) => {
                let net = &mut self.net;
                c.run(x, &mut |m| net.run(m));
            }
        }
    }

    /// How likely the last frame held speech, 0–1.
    pub fn voice(&self) -> f32 {
        self.net.voice()
    }

    /// The delay it adds at its rate, in samples.
    pub fn latency(&self) -> usize {
        latency(self.rate)
    }
}

/// The delay a [`RateDenoiser`] adds at `rate`, in samples: the network's 20 ms, and at
/// another rate than 48 kHz the conversions' (measured once: an impulse through them
/// with the network left out).
pub fn latency(rate: u32) -> usize {
    use std::sync::OnceLock;
    static AT: [OnceLock<usize>; 2] = [OnceLock::new(), OnceLock::new()];
    let slot = match rate {
        44_100 => 0,
        96_000 => 1,
        _ => return LATENCY,
    };
    let net = LATENCY * rate as usize / NETWORK_RATE as usize;
    net + *AT[slot].get_or_init(|| {
        let Some(mut c) = Conv::new(rate) else {
            return 0;
        };
        let lead = 4096;
        let mut x = vec![0.0f32; lead + 4 * prime(rate) + 8192];
        x[lead] = 1.0;
        for b in x.chunks_mut(64) {
            c.run(b, &mut |_| {});
        }
        let peak = x
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map_or(lead, |(i, _)| i);
        peak.saturating_sub(lead)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At 44.1 and 96 kHz: the delay it says is the delay it has (a tone burst's start
    /// lines up within a few samples), and at 48 kHz nothing changes.
    #[test]
    fn other_rates_converted_with_the_delay_said() {
        assert_eq!(latency(48_000), LATENCY);
        for rate in [44_100u32, 96_000] {
            let d = latency(rate);
            assert!(d > LATENCY * rate as usize / 48_000, "{rate}: {d}");
            // The conversions alone (the network left out), as `latency` measures them:
            // a click at 5000 comes out at 5000 + the conversions' part.
            let conv = d - LATENCY * rate as usize / 48_000;
            let mut c = Conv::new(rate).unwrap();
            let mut x = vec![0.0f32; 5000 + conv + 4096];
            x[5000] = 1.0;
            for b in x.chunks_mut(100) {
                c.run(b, &mut |_| {});
            }
            let peak = x
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap()
                .0;
            assert_eq!(peak, 5000 + conv, "{rate}");
            // And the whole thing runs, a block at a time, with no samples lost.
            let mut r = RateDenoiser::new(rate);
            let mut y = vec![0.1f32; rate as usize];
            for b in y.chunks_mut(128) {
                r.run(b);
            }
            assert!(y.iter().all(|v| v.is_finite()));
        }
    }

    /// Steady room noise (a fan: low-passed noise, and mains hum) drops by more than
    /// 20 dB once the network has settled. (Synthetic full-band white noise is a poor
    /// test: the network, trained on real noise, hears it as breath.)
    #[test]
    fn takes_steady_room_noise_out() {
        let mut seed = 0x1357_9bdfu32;
        let mut white = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32 - 0.5
        };
        let mut lp = 0.0f32;
        let mut x: Vec<f32> = (0..96_000)
            .map(|i| {
                lp = 0.95 * lp + 0.05 * white() * 2.0;
                let hum = 0.02 * (std::f32::consts::TAU * 60.0 * i as f32 / 48_000.0).sin();
                lp + hum
            })
            .collect();
        let before: f32 = x[48_000..].iter().map(|v| v * v).sum();
        let mut d = Denoiser::new();
        // In the desk's 2 ms blocks.
        for chunk in x.chunks_mut(96) {
            d.run(chunk);
        }
        let after: f32 = x[48_000..].iter().map(|v| v * v).sum();
        let db = 10.0 * (after.max(1e-12) / before).log10();
        assert!(
            db < -20.0,
            "room noise down by more than 20 dB (was {db:.1} dB)"
        );
    }

    /// The frame structure: the first frame out is the priming (the latency), the second
    /// the unsettled network's first frame — both silent, whatever goes in.
    #[test]
    fn one_frame_late_and_settles_silently() {
        let mut d = Denoiser::new();
        let mut y = vec![0.3f32; 4 * FRAME];
        d.run(&mut y);
        assert!(y[..2 * FRAME].iter().all(|v| *v == 0.0));
    }

    /// Where the sound really comes out: the lag of the peak cross-correlation of a
    /// loud noise (kept in part, as breath) with what comes out. The network's gains
    /// are real per band (no phase), so the peak is the delay.
    #[test]
    fn measured_delay_is_latency() {
        let mut seed = 0x2468_ace1u32;
        let x: Vec<f32> = (0..48_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 0.8
            })
            .collect();
        let mut y = x.clone();
        let mut d = Denoiser::new();
        for chunk in y.chunks_mut(96) {
            d.run(chunk);
        }
        let corr = |lag: usize| -> f64 {
            (24_000..44_000)
                .map(|i| x[i - lag] as f64 * y[i] as f64)
                .sum()
        };
        let best = (0..4 * FRAME)
            .max_by(|a, b| corr(*a).abs().total_cmp(&corr(*b).abs()))
            .unwrap();
        assert_eq!(best, LATENCY, "the sound comes out {best} samples late");
    }

    #[test]
    fn bad_samples_dont_poison_it() {
        let mut d = Denoiser::new();
        let mut x = vec![f32::NAN; 2 * FRAME];
        d.run(&mut x);
        let mut y = vec![0.1f32; 4 * FRAME];
        d.run(&mut y);
        assert!(y.iter().all(|v| v.is_finite()));
    }
}
