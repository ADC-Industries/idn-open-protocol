//! The processing blocks of a strip (spec/extensions/idn-mixer.md §M.2): filters (the
//! RBJ audio-EQ cookbook's biquads), a gate, a compressor, and meters. f32, per sample.

/// Decibels to a gain.
pub fn db(v: f32) -> f32 {
    if v <= -120.0 {
        0.0
    } else {
        // 10^(v/20), as a power of two (cheaper than powf; per sample in the dynamics).
        (v * (std::f32::consts::LOG2_10 / 20.0)).exp2()
    }
}

/// A gain (or level) to decibels, −144 for silence.
pub fn to_db(v: f32) -> f32 {
    if v <= 1e-7 {
        -144.0
    } else {
        20.0 * std::f32::consts::LOG10_2 * v.log2()
    }
}

/// What a filter does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    HighPass,
    LowShelf,
    Peak,
    HighShelf,
    LowPass,
    Notch,
    /// A vintage-style bell (§M.28): as `Peak`, its width following its gain, wide for a
    /// small boost or cut and narrower for a large one (an analogue desk's "proportional
    /// Q"): the band's `q` at ±6 dB, a third of it near 0 dB, up to two and a half times it
    /// at ±15 dB.
    Veq,
}

/// A vintage bell's working Q for its band's `q` and gain (§M.28).
pub fn veq_q(q: f32, gain_db: f32) -> f32 {
    q * (gain_db.abs() / 6.0).clamp(1.0 / 3.0, 2.5)
}

use fundsp::audionode::AudioNode;
use fundsp::svf::{
    BellMode, FixedSvf, HighpassMode, HighshelfMode, LowpassMode, LowshelfMode, NotchMode,
    SvfParams,
};

/// A second-order filter: fundsp's state-variable filters (Andrew Simper's), which stay
/// well behaved while a knob is turned, unlike a direct-form biquad.
#[derive(Clone)]
pub enum Filter {
    /// Passes everything unchanged.
    Flat,
    HighPass(FixedSvf<f32, HighpassMode<f32>>),
    LowShelf(FixedSvf<f32, LowshelfMode<f32>>),
    Peak(FixedSvf<f32, BellMode<f32>>),
    HighShelf(FixedSvf<f32, HighshelfMode<f32>>),
    LowPass(FixedSvf<f32, LowpassMode<f32>>),
    Notch(FixedSvf<f32, NotchMode<f32>>),
}

impl Filter {
    /// A filter of `shape` at `freq` Hz (sample rate `rate`), `gain_db` for shelves and
    /// peaks, quality `q`.
    pub fn new(shape: Shape, rate: f32, freq: f32, gain_db: f32, q: f32) -> Filter {
        let p = SvfParams {
            sample_rate: rate,
            cutoff: freq.clamp(10.0, rate * 0.45),
            q: q.max(0.1),
            gain: db(gain_db),
        };
        match shape {
            Shape::HighPass => Filter::HighPass(FixedSvf::new(HighpassMode::new(), &p)),
            Shape::LowShelf => Filter::LowShelf(FixedSvf::new(LowshelfMode::new(), &p)),
            Shape::Peak => Filter::Peak(FixedSvf::new(BellMode::new(), &p)),
            Shape::Veq => {
                let p = SvfParams {
                    q: veq_q(q, gain_db).max(0.1),
                    ..p
                };
                Filter::Peak(FixedSvf::new(BellMode::new(), &p))
            }
            Shape::HighShelf => Filter::HighShelf(FixedSvf::new(HighshelfMode::new(), &p)),
            Shape::LowPass => Filter::LowPass(FixedSvf::new(LowpassMode::new(), &p)),
            Shape::Notch => Filter::Notch(FixedSvf::new(NotchMode::new(), &p)),
        }
    }

    /// New settings, keeping the filter's memory (a knob turned while playing).
    pub fn retune(&mut self, shape: Shape, rate: f32, freq: f32, gain_db: f32, q: f32) {
        let (f, q, g) = (freq.clamp(10.0, rate * 0.45), q.max(0.1), db(gain_db));
        match (self, shape) {
            (Filter::HighPass(x), Shape::HighPass) => x.set_cutoff_q(f, q),
            (Filter::LowShelf(x), Shape::LowShelf) => x.set_cutoff_q_gain(f, q, g),
            (Filter::Peak(x), Shape::Peak) => x.set_cutoff_q_gain(f, q, g),
            (Filter::Peak(x), Shape::Veq) => x.set_cutoff_q_gain(f, veq_q(q, gain_db).max(0.1), g),
            (Filter::HighShelf(x), Shape::HighShelf) => x.set_cutoff_q_gain(f, q, g),
            (Filter::LowPass(x), Shape::LowPass) => x.set_cutoff_q(f, q),
            (Filter::Notch(x), Shape::Notch) => x.set_cutoff_q(f, q),
            (me, _) => *me = Filter::new(shape, rate, freq, gain_db, q),
        }
    }

    /// Forgets its memory (a band left out starts clean when it's back).
    pub fn reset(&mut self) {
        match self {
            Filter::Flat => {}
            Filter::HighPass(f) => f.reset(),
            Filter::LowShelf(f) => f.reset(),
            Filter::Peak(f) => f.reset(),
            Filter::HighShelf(f) => f.reset(),
            Filter::LowPass(f) => f.reset(),
            Filter::Notch(f) => f.reset(),
        }
    }

    #[inline]
    pub fn run(&mut self, x: f32) -> f32 {
        match self {
            Filter::Flat => x,
            Filter::HighPass(f) => f.filter_mono(x),
            Filter::LowShelf(f) => f.filter_mono(x),
            Filter::Peak(f) => f.filter_mono(x),
            Filter::HighShelf(f) => f.filter_mono(x),
            Filter::LowPass(f) => f.filter_mono(x),
            Filter::Notch(f) => f.filter_mono(x),
        }
    }
}

/// The filter slopes a channel's high- and low-pass take, dB an octave (§M.28).
pub const SLOPES: [u8; 6] = [6, 12, 18, 24, 36, 48];

/// A high- or low-pass at 6 to 48 dB an octave (§M.28): Butterworth, made of a one-pole
/// stage (for an odd order) and second-order stages with the order's Qs. 12 dB is the one
/// second-order stage at Q 0.707 it always was.
#[derive(Clone)]
pub struct SlopeFilter {
    stages: [Filter; 4],
    n: usize,
    /// The one-pole stage (odd orders): its coefficient and memory, high-pass or not.
    one: Option<(f32, f32, bool)>,
}

impl Default for SlopeFilter {
    fn default() -> Self {
        SlopeFilter {
            stages: [Filter::Flat, Filter::Flat, Filter::Flat, Filter::Flat],
            n: 0,
            one: None,
        }
    }
}

impl SlopeFilter {
    /// Off (`freq` 0) or a `shape` (high- or low-pass) at `freq`, `slope` dB an octave,
    /// keeping its memory where the stages stay.
    pub fn tune(&mut self, shape: Shape, rate: f32, freq: f32, slope: u8) {
        if freq <= 0.0 {
            *self = SlopeFilter::default();
            return;
        }
        let order = (slope / 6).clamp(1, 8) as usize;
        let pairs = order / 2;
        let even = order.is_multiple_of(2);
        for (k, st) in self.stages.iter_mut().take(pairs).enumerate() {
            let k = k + 1;
            let a = if even {
                (2 * k - 1) as f32 * std::f32::consts::PI / (2 * order) as f32
            } else {
                k as f32 * std::f32::consts::PI / order as f32
            };
            st.retune(shape, rate, freq, 0.0, 1.0 / (2.0 * a.cos()));
        }
        for st in self.stages.iter_mut().skip(pairs) {
            *st = Filter::Flat;
        }
        self.n = pairs;
        self.one = (!even).then(|| {
            let f = freq.clamp(10.0, rate * 0.45);
            let g = 1.0 - (-2.0 * std::f32::consts::PI * f / rate).exp();
            let mem = self.one.map_or(0.0, |o| o.1);
            (g, mem, shape == Shape::HighPass)
        });
    }

    #[inline]
    pub fn run(&mut self, x: f32) -> f32 {
        let mut y = x;
        if let Some((g, mem, high)) = &mut self.one {
            *mem += *g * (y - *mem);
            y = if *high { y - *mem } else { *mem };
        }
        for st in &mut self.stages[..self.n] {
            y = st.run(y);
        }
        y
    }
}

/// A dynamics stage's key filters (§M.28): a high-pass and a low-pass on what it hears,
/// each off at 0 Hz, retuned when their frequencies change.
#[derive(Clone)]
pub struct KeyFilter {
    hp: Filter,
    lp: Filter,
    tuned: (f32, f32, f32),
}

impl Default for KeyFilter {
    fn default() -> Self {
        KeyFilter {
            hp: Filter::Flat,
            lp: Filter::Flat,
            tuned: (0.0, 0.0, 0.0),
        }
    }
}

impl KeyFilter {
    /// One sample of the key through the filters at `hpf` / `lpf` Hz.
    #[inline]
    pub fn run(&mut self, x: f32, rate: f32, hpf: f32, lpf: f32) -> f32 {
        if hpf <= 0.0 && lpf <= 0.0 {
            return x;
        }
        if self.tuned != (hpf, lpf, rate) {
            self.tuned = (hpf, lpf, rate);
            if hpf > 0.0 {
                self.hp.retune(Shape::HighPass, rate, hpf, 0.0, 0.707);
            } else {
                self.hp = Filter::Flat;
            }
            if lpf > 0.0 {
                self.lp.retune(Shape::LowPass, rate, lpf, 0.0, 0.707);
            } else {
                self.lp = Filter::Flat;
            }
        }
        self.lp.run(self.hp.run(x))
    }
}

/// The time constant of `ms` as a per-sample smoothing factor at `rate`.
fn coeff(ms: f32, rate: f32) -> f32 {
    (-1.0 / (ms.max(0.05) * 0.001 * rate)).exp()
}

/// A compressor (feed-forward, peak detector in dB, soft knee) or, with a ratio below 1
/// and a range, a gate/expander. Its gain is applied by [`Dynamics::run`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dynamics {
    /// Gain reduction now, dB (≥ 0).
    pub reduction: f32,
    env: f32,
    /// The attack and release as smoothing factors, for the (attack, release, rate) they
    /// were worked out for: only again when a knob moves, not every sample.
    times: (f32, f32, f32),
    coeffs: (f32, f32),
    /// Samples left of a gate's hold.
    held: u32,
    /// A compressor's mean square (RMS detection) and its slow follower of the reduction
    /// (auto release).
    ms: f32,
    slow: f32,
    /// For the rate in `.0`: the 40 ms (auto release's quickest), 10 ms (RMS window) and
    /// 500 ms (its follower) smoothing factors.
    fixed: (f32, f32, f32, f32),
}

impl Default for Dynamics {
    fn default() -> Self {
        Dynamics {
            reduction: 0.0,
            env: -144.0,
            times: (-1.0, -1.0, 0.0),
            coeffs: (0.0, 0.0),
            held: 0,
            ms: 0.0,
            slow: 0.0,
            fixed: (0.0, 0.0, 0.0, 0.0),
        }
    }
}

impl Dynamics {
    /// The gain for the reduction now: 1 while (next to) nothing is taken off, the usual
    /// case, without working it out.
    #[inline]
    pub fn gain(&self) -> f32 {
        if self.reduction < 1e-4 {
            1.0
        } else {
            db(-self.reduction)
        }
    }

    /// (attack, release) as smoothing factors.
    #[inline]
    fn coeffs(&mut self, attack: f32, release: f32, rate: f32) -> (f32, f32) {
        if self.times != (attack, release, rate) {
            self.times = (attack, release, rate);
            self.coeffs = (coeff(attack, rate), coeff(release, rate));
        }
        self.coeffs
    }

    /// A compressor's sample: above `threshold` dB the level rises 1/`ratio` as fast,
    /// with a knee `knee` dB wide; gain reduction moves at `attack`/`release` ms.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub fn compress(
        &mut self,
        x: f32,
        rate: f32,
        threshold: f32,
        ratio: f32,
        attack: f32,
        release: f32,
        knee: f32,
    ) -> f32 {
        let level = to_db(x.abs());
        let over = level - threshold;
        let r = ratio.max(1.0);
        let want = if knee > 0.0 && over.abs() < knee / 2.0 {
            (1.0 / r - 1.0) * (over + knee / 2.0).powi(2) / (2.0 * knee)
        } else if over > 0.0 {
            over * (1.0 / r - 1.0)
        } else {
            0.0
        };
        let target = -want; // reduction, ≥ 0
        let (ca, cr) = self.coeffs(attack, release, rate);
        let c = if target > self.reduction { ca } else { cr };
        self.reduction = target + c * (self.reduction - target);
        x * self.gain()
    }

    /// The fixed smoothing factors at `rate` (worked out once per rate).
    #[inline]
    fn fixed(&mut self, rate: f32) -> (f32, f32, f32) {
        if self.fixed.0 != rate {
            self.fixed = (
                rate,
                coeff(40.0, rate),
                coeff(10.0, rate),
                coeff(500.0, rate),
            );
        }
        (self.fixed.1, self.fixed.2, self.fixed.3)
    }

    /// A compressor's gain for this sample (§M.28), its detector fed `key` (the channel's
    /// own sound or another's): as [`Dynamics::compress`], hearing peaks or the RMS level,
    /// and with `auto_release` a release from 40 ms (after a short peak) to `release`
    /// (while it keeps compressing, over about half a second).
    // (Out of line: inlined beside the plain one it slowed every strip, used or not;
    // profile round 13.)
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub fn compress_keyed(
        &mut self,
        key: f32,
        rate: f32,
        threshold: f32,
        ratio: f32,
        attack: f32,
        release: f32,
        knee: f32,
        rms: bool,
        auto_release: bool,
    ) -> f32 {
        let (fast, window, follow) = self.fixed(rate);
        let level = if rms {
            let p = key * key;
            self.ms = p + window * (self.ms - p);
            10.0 * self.ms.max(1e-15).log10()
        } else {
            to_db(key.abs())
        };
        let over = level - threshold;
        let r = ratio.max(1.0);
        let want = if knee > 0.0 && over.abs() < knee / 2.0 {
            (1.0 / r - 1.0) * (over + knee / 2.0).powi(2) / (2.0 * knee)
        } else if over > 0.0 {
            over * (1.0 / r - 1.0)
        } else {
            0.0
        };
        let target = -want;
        let (ca, cr) = self.coeffs(attack, release, rate);
        let c = if target > self.reduction {
            ca
        } else if auto_release {
            // How long it has been compressing (6 dB held: all of `release`).
            self.slow = self.reduction + follow * (self.slow - self.reduction);
            let w = (self.slow / 6.0).clamp(0.0, 1.0);
            fast + (cr.max(fast) - fast) * w
        } else {
            cr
        };
        self.reduction = target + c * (self.reduction - target);
        self.gain()
    }

    /// A gate's, expander's or ducker's gain for this sample (§M.28), its detector fed
    /// `key` (filtered, or another channel's): open (no reduction) above `threshold`
    /// and `hold` ms after; below it a gate closes by `range` dB, an expander by `ratio`
    /// − 1 dB a dB under it (at most `range`). A ducker is the other way round: down by
    /// `range` above the threshold and `hold` ms after. Opening (a ducker: ducking)
    /// moves at `attack`, the rest at `release`.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub fn gate_keyed(
        &mut self,
        key: f32,
        rate: f32,
        threshold: f32,
        range: f32,
        attack: f32,
        release: f32,
        hold: f32,
        mode: crate::desk::GateMode,
        ratio: f32,
    ) -> f32 {
        use crate::desk::GateMode;
        let level = to_db(key.abs());
        let (ca, cr) = self.coeffs(attack, release, rate);
        self.env = if level > self.env {
            level
        } else {
            level + cr * (self.env - level)
        };
        let range = range.max(0.0);
        let above = self.env >= threshold;
        if above {
            self.held = (hold.max(0.0) * 0.001 * rate) as u32;
        } else if self.held > 0 {
            self.held -= 1;
        }
        let on = above || self.held > 0;
        let target = match mode {
            GateMode::Gate if on => 0.0,
            GateMode::Gate => range,
            GateMode::Expander if on => 0.0,
            GateMode::Expander => {
                ((threshold - self.env) * (ratio.max(1.0) - 1.0)).clamp(0.0, range)
            }
            GateMode::Ducker if on => range,
            GateMode::Ducker => 0.0,
        };
        let opening = match mode {
            GateMode::Ducker => target > self.reduction,
            _ => target < self.reduction,
        };
        let c = if opening { ca } else { cr };
        self.reduction = target + c * (self.reduction - target);
        self.gain()
    }

    /// A limiter's sample: nothing goes far above `ceiling` dB (a compressor of very high
    /// ratio with a 0.05 ms attack; no look-ahead, so a sharp transient can pass for a
    /// sample or two), coming back at `release` ms.
    #[inline]
    pub fn limit(&mut self, x: f32, rate: f32, ceiling: f32, release: f32) -> f32 {
        self.compress(x, rate, ceiling, 1000.0, 0.05, release, 0.0)
    }

    /// A gate's sample: below `threshold` dB (its detector holding peaks for `release`
    /// ms) the gain goes down by `range` dB.
    #[inline(always)]
    pub fn gate(
        &mut self,
        x: f32,
        rate: f32,
        threshold: f32,
        range: f32,
        attack: f32,
        release: f32,
    ) -> f32 {
        let level = to_db(x.abs());
        let (ca, cr) = self.coeffs(attack, release, rate);
        // A peak envelope: rises at once, falls at the release.
        self.env = if level > self.env {
            level
        } else {
            level + cr * (self.env - level)
        };
        let target = if self.env >= threshold {
            0.0
        } else {
            range.max(0.0)
        };
        let c = if target < self.reduction { ca } else { cr };
        self.reduction = target + c * (self.reduction - target);
        x * self.gain()
    }
}

/// A de-esser's processing state (§M.11): a high-pass sidechain finds sibilance above its
/// frequency; split-band, only the band above it is turned down (a Linkwitz-Riley
/// crossover, whose two halves sum back flat), else the whole signal.
#[derive(Clone)]
pub struct DeEss {
    side: Filter,
    lo: [Filter; 2],
    hi: [Filter; 2],
    dyn_: Dynamics,
    freq: f32,
}

impl Default for DeEss {
    fn default() -> Self {
        DeEss {
            side: Filter::Flat,
            lo: [Filter::Flat, Filter::Flat],
            hi: [Filter::Flat, Filter::Flat],
            dyn_: Dynamics::default(),
            freq: 0.0,
        }
    }
}

impl DeEss {
    /// Gain reduction now, dB.
    pub fn reduction(&self) -> f32 {
        self.dyn_.reduction
    }

    /// One sample: above `threshold` dB in the band over `freq` Hz, reduced 4:1 up to
    /// `range` dB, fast (0.5 ms attack, 60 ms release).
    #[inline]
    pub fn run(
        &mut self,
        x: f32,
        rate: f32,
        freq: f32,
        threshold: f32,
        range: f32,
        split: bool,
    ) -> f32 {
        if freq != self.freq {
            self.side.retune(Shape::HighPass, rate, freq, 0.0, 0.707);
            for f in &mut self.lo {
                f.retune(Shape::LowPass, rate, freq, 0.0, 0.707);
            }
            for f in &mut self.hi {
                f.retune(Shape::HighPass, rate, freq, 0.0, 0.707);
            }
            self.freq = freq;
        }
        let s = self.side.run(x);
        let _ = self.dyn_.compress(s, rate, threshold, 4.0, 0.5, 60.0, 0.0);
        self.dyn_.reduction = self.dyn_.reduction.min(range.max(0.0));
        let g = self.dyn_.gain();
        // Linkwitz-Riley (4th order): two Butterworth stages each side.
        let low = self.lo[0].run(x);
        let low = self.lo[1].run(low);
        let high = self.hi[0].run(x);
        let high = self.hi[1].run(high);
        if split {
            low + high * g
        } else {
            (low + high) * g
        }
    }
}

/// An in-place radix-2 FFT of `re`/`im` (length a power of two).
pub fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let a = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (a.cos(), a.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (u, v) = (start + k, start + k + len / 2);
                let tr = re[v] * cr - im[v] * ci;
                let ti = re[v] * ci + im[v] * cr;
                re[v] = re[u] - tr;
                im[v] = im[u] - ti;
                re[u] += tr;
                im[u] += ti;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
        }
        len <<= 1;
    }
}

/// The spectrum of `x` (a power-of-two length, at `rate`) in `bands` bands spaced
/// evenly in log frequency from 20 Hz to 20 kHz: each band's level in dBFS (a full-scale
/// sine reads 0), from a Hann-windowed FFT.
pub fn spectrum(x: &[f32], rate: f32, bands: usize) -> Vec<f32> {
    let n = x.len();
    let mut re: Vec<f32> = x
        .iter()
        .enumerate()
        .map(|(i, v)| v * (0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()))
        .collect();
    let mut im = vec![0.0; n];
    fft(&mut re, &mut im);
    // The Hann window's gain (0.5) and the half spectrum (×2) cancel: |X| / (n/4) per sine.
    let mag: Vec<f32> = (0..n / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() / (n as f32 / 4.0))
        .collect();
    let bin = rate / n as f32;
    (0..bands)
        .map(|b| {
            let lo = 20.0 * 1000f32.powf(b as f32 / bands as f32);
            let hi = 20.0 * 1000f32.powf((b + 1) as f32 / bands as f32);
            let (k0, k1) = (
                (lo / bin) as usize,
                ((hi / bin) as usize).max((lo / bin) as usize + 1),
            );
            let peak = mag[k0.min(mag.len() - 1)..k1.min(mag.len())]
                .iter()
                .fold(0f32, |m, &v| m.max(v));
            to_db(peak)
        })
        .collect()
}

/// Loudness (ITU-R BS.1770 / EBU R128, §M.11) of a stereo signal at 48 kHz: K-weighted
/// (the standard's two filters, with its coefficients for 48 kHz), mean square per 100 ms,
/// momentary over the last 400 ms and short-term over the last 3 s, in LUFS.
#[derive(Clone)]
pub struct Loudness {
    /// Per side, the two biquads' state (x1, x2, y1, y2).
    z: [[[f64; 4]; 2]; 2],
    /// The sum of squares and samples of the 100 ms being filled.
    acc: f64,
    count: usize,
    /// The last 30 blocks' mean squares (both sides summed), newest last.
    blocks: std::collections::VecDeque<f64>,
    /// Integrated loudness (§M.15): every 400 ms gating block (one each 100 ms) above
    /// −70 LUFS, by its loudness in 0.05 LU bins from −70: (blocks, their mean squares'
    /// sum). Fixed memory however long it runs.
    gates: Vec<(u64, f64)>,
    /// The K-weighting at the desk's rate ([`k_weight`]) and samples in 100 ms.
    k: [[f64; 5]; 2],
    block: usize,
}

/// The integrated meter's bins: −70 to +5 LUFS in 0.05 LU.
const GATE_BINS: usize = 1500;

impl Default for Loudness {
    fn default() -> Self {
        Loudness {
            z: [[[0.0; 4]; 2]; 2],
            acc: 0.0,
            count: 0,
            blocks: std::collections::VecDeque::new(),
            gates: vec![(0, 0.0); GATE_BINS],
            k: k_weight(crate::desk::rate_hz()),
            block: (crate::desk::rate_hz() / 10) as usize,
        }
    }
}

/// BS.1770's K-weighting at `rate` Hz (bug run J2-2: the 48 kHz coefficients were used at
/// every rate): the high shelf and the high-pass from their analogue prototypes (the
/// standard's frequencies, gains and Qs), bilinear; at 48 kHz the standard's own values.
pub fn k_weight(rate: u32) -> [[f64; 5]; 2] {
    let fs = f64::from(rate.max(8000));
    let shelf = {
        let (f0, g, q) = (
            1_681.974_450_955_533,
            3.999_843_853_973_347,
            0.707_175_236_955_419_6,
        );
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let vh = 10f64.powf(g / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
            2.0 * (k * k - 1.0) / a0,
            (1.0 - k / q + k * k) / a0,
        ]
    };
    let high_pass = {
        let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        [
            1.0,
            -2.0,
            1.0,
            2.0 * (k * k - 1.0) / a0,
            (1.0 - k / q + k * k) / a0,
        ]
    };
    [shelf, high_pass]
}

/// BS.1770's K-weighting at 48 kHz as the standard lists it: a high shelf (+4 dB above
/// ~1.5 kHz), then a high-pass (~38 Hz): (b0, b1, b2, a1, a2). The test's reference for
/// [`k_weight`].
#[cfg(test)]
const K_WEIGHT: [[f64; 5]; 2] = [
    [
        1.535_124_859_586_97,
        -2.691_696_189_406_38,
        1.198_392_810_852_85,
        -1.690_659_293_182_41,
        0.732_480_774_215_85,
    ],
    [1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21],
];

impl Loudness {
    /// Takes a block of the left and right channels.
    pub fn take(&mut self, l: &[f32], r: &[f32]) {
        for k in 0..l.len().min(r.len()) {
            let mut sq = 0.0;
            for (side, x) in [l[k], r[k]].into_iter().enumerate() {
                let mut v = f64::from(x);
                for (stage, c) in self.k.iter().enumerate() {
                    let z = &mut self.z[side][stage];
                    let y = c[0] * v + c[1] * z[0] + c[2] * z[1] - c[3] * z[2] - c[4] * z[3];
                    (z[1], z[0], z[3], z[2]) = (z[0], v, z[2], y);
                    v = y;
                }
                sq += v * v;
            }
            self.acc += sq;
            self.count += 1;
            if self.count == self.block {
                self.blocks.push_back(self.acc / self.block as f64);
                if self.blocks.len() > 30 {
                    self.blocks.pop_front();
                }
                // A 400 ms gating block (75% overlapping the last), for the integrated
                // loudness: kept above the absolute gate.
                if self.blocks.len() >= 4 {
                    let ms = self.blocks.iter().rev().take(4).sum::<f64>() / 4.0;
                    let lk = -0.691 + 10.0 * ms.max(1e-20).log10();
                    if lk > -70.0 {
                        let bin = (((lk + 70.0) * 20.0) as usize).min(GATE_BINS - 1);
                        self.gates[bin].0 += 1;
                        self.gates[bin].1 += ms;
                    }
                }
                (self.acc, self.count) = (0.0, 0);
            }
        }
    }

    fn over(&self, n: usize) -> f32 {
        let take = self.blocks.len().min(n);
        if take == 0 {
            return -144.0;
        }
        let ms: f64 = self.blocks.iter().rev().take(take).sum::<f64>() / take as f64;
        if ms <= 1e-15 {
            -144.0
        } else {
            (-0.691 + 10.0 * ms.log10()) as f32
        }
    }

    /// The last 400 ms, LUFS.
    pub fn momentary(&self) -> f32 {
        self.over(4)
    }

    /// The last 3 s, LUFS.
    pub fn short_term(&self) -> f32 {
        self.over(30)
    }

    /// Since the last reset (EBU R128, BS.1770-4): the gating blocks above −70 LUFS, then
    /// those within 10 LU of their mean, LUFS (−144 before any).
    pub fn integrated(&self) -> f32 {
        let mean = |from: usize| {
            let (n, sum) = self.gates[from..]
                .iter()
                .fold((0u64, 0.0f64), |(n, s), &(k, v)| (n + k, s + v));
            (n > 0).then(|| -0.691 + 10.0 * (sum / n as f64).log10())
        };
        let Some(abs) = mean(0) else {
            return -144.0;
        };
        let rel = ((abs - 10.0 + 70.0) * 20.0).max(0.0) as usize;
        mean(rel.min(GATE_BINS - 1)).unwrap_or(abs) as f32
    }

    /// Starts the integrated loudness again (a new service).
    pub fn reset_integrated(&mut self) {
        self.gates.iter_mut().for_each(|g| *g = (0, 0.0));
    }
}

#[cfg(test)]
mod loudness_tests {
    use super::*;

    /// BS.1770's check: a 997 Hz sine at −23 dBFS on both sides reads −23 LUFS (±0.1),
    /// integrated too; a long quiet stretch below the relative gate doesn't pull it down.
    #[test]
    fn integrated_loudness_gates() {
        let mut m = Loudness::default();
        let amp = 10f32.powf(-23.0 / 20.0);
        let tone = |m: &mut Loudness, secs: usize, a: f32| {
            for b in 0..secs * 500 {
                let x: Vec<f32> = (0..96)
                    .map(|k| {
                        a * (2.0 * std::f32::consts::PI * 997.0 * (b * 96 + k) as f32 / 48_000.0)
                            .sin()
                    })
                    .collect();
                m.take(&x, &x);
            }
        };
        tone(&mut m, 10, amp);
        assert!((m.integrated() + 23.0).abs() < 0.2, "{}", m.integrated());
        // 20 s at −50 dBFS: under the relative gate (−33), left out.
        tone(&mut m, 20, 10f32.powf(-50.0 / 20.0));
        assert!((m.integrated() + 23.0).abs() < 0.2, "{}", m.integrated());
        m.reset_integrated();
        assert_eq!(m.integrated(), -144.0);
    }
}

/// A meter: peak (held for 1.5 s) and RMS over the last ~300 ms, as levels (not dB).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Meter {
    pub peak: f32,
    pub rms: f32,
    held: f32,
    held_for: f32,
    ms: f32,
}

impl Meter {
    /// Takes a block of `samples` (duration `secs`).
    pub fn take(&mut self, samples: &[f32], secs: f32) {
        let peak = samples.iter().fold(0f32, |m, v| m.max(v.abs()));
        let sq = samples.iter().map(|v| v * v).sum::<f32>() / samples.len().max(1) as f32;
        let a = (secs / 0.3).min(1.0);
        self.ms += a * (sq - self.ms);
        self.rms = self.ms.sqrt();
        if peak >= self.held {
            self.held = peak;
            self.held_for = 0.0;
        } else {
            self.held_for += secs;
            if self.held_for > 1.5 {
                // Falls 20 dB a second after the hold.
                self.held = (self.held * db(-20.0 * secs)).max(peak);
            }
        }
        self.peak = self.held;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gain a filter gives a sine of `freq` (after settling).
    fn gain(f: &Filter, freq: f32) -> f32 {
        let mut f = f.clone();
        let rate = 48000.0;
        let mut peak = 0f32;
        for i in 0..48000 {
            let y = f.run((2.0 * std::f32::consts::PI * freq * i as f32 / rate).sin());
            if i > 24000 {
                peak = peak.max(y.abs());
            }
        }
        to_db(peak)
    }

    #[test]
    fn filters_do_what_they_say() {
        let r = 48000.0;
        let hp = Filter::new(Shape::HighPass, r, 100.0, 0.0, 0.707);
        assert!(gain(&hp, 30.0) < -18.0, "{}", gain(&hp, 30.0));
        assert!(gain(&hp, 1000.0).abs() < 0.2);
        let pk = Filter::new(Shape::Peak, r, 1000.0, 6.0, 1.0);
        assert!(
            (gain(&pk, 1000.0) - 6.0).abs() < 0.2,
            "{}",
            gain(&pk, 1000.0)
        );
        assert!(gain(&pk, 100.0).abs() < 0.5);
        let ls = Filter::new(Shape::LowShelf, r, 200.0, -6.0, 0.707);
        assert!((gain(&ls, 30.0) + 6.0).abs() < 0.5, "{}", gain(&ls, 30.0));
        assert!(gain(&ls, 5000.0).abs() < 0.3);
        let hs = Filter::new(Shape::HighShelf, r, 5000.0, 4.0, 0.707);
        assert!(
            (gain(&hs, 15000.0) - 4.0).abs() < 0.5,
            "{}",
            gain(&hs, 15000.0)
        );
        assert!(gain(&hs, 200.0).abs() < 0.3);
        assert!(gain(&Filter::Flat, 440.0).abs() < 0.01);
        let lp = Filter::new(Shape::LowPass, r, 2000.0, 0.0, 0.707);
        assert!(gain(&lp, 12000.0) < -18.0, "{}", gain(&lp, 12000.0));
        assert!(gain(&lp, 200.0).abs() < 0.2);
        let notch = Filter::new(Shape::Notch, r, 1000.0, 0.0, 2.0);
        assert!(gain(&notch, 1000.0) < -30.0, "{}", gain(&notch, 1000.0));
        assert!(gain(&notch, 100.0).abs() < 0.3);
    }

    #[test]
    fn the_compressor_and_gate() {
        let mut c = Dynamics::default();
        let x = db(-4.0);
        let mut y = 0.0;
        for _ in 0..48000 {
            y = c.compress(x, 48000.0, -20.0, 4.0, 5.0, 100.0, 0.0);
        }
        // 16 dB over at 4:1: 4 dB over, so −16 dBFS.
        assert!((to_db(y) + 16.0).abs() < 0.2, "{}", to_db(y));
        let mut g = Dynamics::default();
        let mut y = 1.0;
        for _ in 0..48000 {
            y = g.gate(db(-60.0), 48000.0, -40.0, 30.0, 1.0, 50.0);
        }
        assert!((to_db(y) + 90.0).abs() < 0.5, "closed: {}", to_db(y));
        for _ in 0..4800 {
            y = g.gate(db(-10.0), 48000.0, -40.0, 30.0, 1.0, 50.0);
        }
        assert!((to_db(y) + 10.0).abs() < 0.2, "open: {}", to_db(y));
        let mut l = Dynamics::default();
        let mut y = 0.0;
        for _ in 0..4800 {
            y = l.limit(db(0.0), 48000.0, -6.0, 50.0);
        }
        assert!((to_db(y) + 6.0).abs() < 0.1, "limited: {}", to_db(y));
        // De-esser, split band at 5 kHz: a loud 12 kHz tone turned down by the range (10 dB),
        // a 200 Hz one untouched (the crossover's halves sum back flat).
        let tone = |f: f32, d: &mut DeEss| {
            let mut peak = 0f32;
            for i in 0..48000 {
                let x = 0.5 * (2.0 * std::f32::consts::PI * f * i as f32 / 48000.0).sin();
                let y = d.run(x, 48000.0, 5000.0, -30.0, 10.0, true);
                if i > 24000 {
                    peak = peak.max(y.abs());
                }
            }
            to_db(peak) - to_db(0.5)
        };
        let hi = tone(12000.0, &mut DeEss::default());
        assert!((hi + 10.0).abs() < 1.5, "sibilance reduced: {hi}");
        let lo = tone(200.0, &mut DeEss::default());
        assert!(lo.abs() < 0.3, "low band untouched: {lo}");
    }

    /// EBU Tech 3341's reference: a 1 kHz sine at −23 dBFS on both channels reads
    /// −23 LUFS (±0.1).
    #[test]
    fn k_weighting_at_48k_is_the_standards_and_scales() {
        let k = k_weight(48_000);
        for (a, b) in k.iter().flatten().zip(K_WEIGHT.iter().flatten()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        // At 96 kHz the shelf's pole moves: not the 48 kHz values.
        assert!((k_weight(96_000)[0][3] - K_WEIGHT[0][3]).abs() > 0.05);
    }

    #[test]
    fn loudness_reads_the_reference_tone() {
        let mut l = Loudness::default();
        let a = db(-23.0);
        let x: Vec<f32> = (0..48000 * 4)
            .map(|i| a * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin())
            .collect();
        for c in x.chunks(96) {
            l.take(c, c);
        }
        assert!((l.momentary() + 23.0).abs() < 0.1, "{}", l.momentary());
        assert!((l.short_term() + 23.0).abs() < 0.1, "{}", l.short_term());
    }

    #[test]
    fn the_spectrum_finds_a_tone() {
        let x: Vec<f32> = (0..8192)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin())
            .collect();
        let s = spectrum(&x, 48000.0, 120);
        let (k, top) = s
            .iter()
            .enumerate()
            .fold((0, -200f32), |b, (k, &v)| if v > b.1 { (k, v) } else { b });
        let f = 20.0 * 1000f32.powf((k as f32 + 0.5) / 120.0);
        assert!((f - 1000.0).abs() < 60.0, "peak at {f} Hz");
        assert!((top + 6.0).abs() < 1.5, "−6 dBFS read as {top}");
        assert!(s[20] < -60.0, "quiet far from it: {}", s[20]);
    }
}
