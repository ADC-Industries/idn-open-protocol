//! Built-in effects (spec/extensions/idn-mixer.md §M.14, M1): a reverb, a stereo delay and a
//! chorus on the FX buses (send and return: their output is all effect), and a 31-band
//! graphic EQ on the main, aux, matrix and headphone buses — so a desk has them with no
//! plugin installed.
//!
//! The reverb is the classic Schroeder–Moorer design (Freeverb's tunings, which its author
//! placed in the public domain): eight damped comb filters in parallel and four all-pass
//! filters in series a side, the right side's delays slightly longer for width, after a
//! pre-delay. The delay feeds back through a low-pass (a tape's darkening), straight or
//! ping-pong. The chorus is two voices a side, each a delay swept by a sine.

use crate::dsp::{Filter, Shape};
use serde::{Deserialize, Serialize};

/// Which effect an FX bus runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    #[default]
    None,
    Reverb,
    Delay,
    Chorus,
}

/// An FX bus's built-in effect and its settings (each kind reads its own).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Effect {
    pub kind: EffectKind,
    /// Reverb: the room's size, 0–1 (how long it rings).
    pub size: f32,
    /// Reverb: how fast the highs die away, 0–1.
    pub damping: f32,
    /// Reverb: before the first reflection, ms (0–200).
    pub predelay: f32,
    /// Reverb: stereo width, 0 (mono) – 1.
    pub width: f32,
    /// Delay: its time, ms (1–2000).
    pub time: f32,
    /// Delay: how much comes round again, 0–0.95.
    pub feedback: f32,
    /// Delay: the repeats' low-pass, Hz (1000–20 000).
    pub tone: f32,
    /// Delay: repeats alternate left and right.
    pub pingpong: bool,
    /// Chorus: the sweep's rate, Hz (0.05–5).
    pub rate: f32,
    /// Chorus: the sweep's depth, ms (0.1–10).
    pub depth: f32,
}

impl Default for Effect {
    fn default() -> Self {
        Effect {
            kind: EffectKind::None,
            size: 0.7,
            damping: 0.4,
            predelay: 20.0,
            width: 1.0,
            time: 375.0,
            feedback: 0.35,
            tone: 6000.0,
            pingpong: false,
            rate: 0.8,
            depth: 3.0,
        }
    }
}

impl Effect {
    /// The settings within their ranges.
    pub fn bounded(mut self) -> Effect {
        let c = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        let d = Effect::default();
        self.size = c(self.size, 0.0, 1.0, d.size);
        self.damping = c(self.damping, 0.0, 1.0, d.damping);
        self.predelay = c(self.predelay, 0.0, 200.0, d.predelay);
        self.width = c(self.width, 0.0, 1.0, d.width);
        self.time = c(self.time, 1.0, 2000.0, d.time);
        self.feedback = c(self.feedback, 0.0, 0.95, d.feedback);
        self.tone = c(self.tone, 1000.0, 20_000.0, d.tone);
        self.rate = c(self.rate, 0.05, 5.0, d.rate);
        self.depth = c(self.depth, 0.1, 10.0, d.depth);
        self
    }
}

/// The graphic EQ's bands, Hz (the ISO third-octave centres).
pub const GEQ_BANDS: [f32; 31] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0,
    8000.0, 10_000.0, 12_500.0, 16_000.0, 20_000.0,
];

/// A third of an octave wide (a bell's Q).
const GEQ_Q: f32 = 4.32;

/// A bus's 31-band graphic EQ: each band's gain, dB (±15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Geq {
    pub on: bool,
    pub gains: Vec<f32>,
}

impl Default for Geq {
    fn default() -> Self {
        Geq {
            on: false,
            gains: vec![0.0; GEQ_BANDS.len()],
        }
    }
}

impl Geq {
    pub fn bounded(mut self) -> Geq {
        self.gains.resize(GEQ_BANDS.len(), 0.0);
        for g in &mut self.gains {
            *g = if g.is_finite() {
                g.clamp(-15.0, 15.0)
            } else {
                0.0
            };
        }
        self
    }
}

/// A graphic EQ playing: a bell a band a side, only the bands not at 0 dB run. Each band
/// glides to its setting (1 dB a block at most), and switching the EQ off glides every
/// band to 0 dB: a step was a click, and each change rebuilt the band list on the clock
/// thread (bug round F, F7-7, F7-8).
#[derive(Clone)]
pub struct GeqState {
    /// Each band's gain as it plays now, dB.
    now: [f32; GEQ_BANDS.len()],
    /// Each band's filters (left, right).
    filters: [(Filter, Filter); GEQ_BANDS.len()],
}

impl Default for GeqState {
    fn default() -> Self {
        GeqState {
            now: [0.0; GEQ_BANDS.len()],
            filters: std::array::from_fn(|_| (Filter::Flat, Filter::Flat)),
        }
    }
}

impl GeqState {
    /// The most a band moves in one block, dB.
    const GLIDE_DB: f32 = 1.0;

    /// Runs `left`/`right` through `geq` (its bands gliding to their settings).
    pub fn run(&mut self, geq: &Geq, rate: f32, left: &mut [f32], right: &mut [f32]) {
        for (k, _) in GEQ_BANDS.iter().enumerate() {
            let want = if geq.on {
                geq.gains.get(k).copied().unwrap_or(0.0)
            } else {
                0.0
            };
            let was = self.now[k];
            if was != want {
                let now = was + (want - was).clamp(-Self::GLIDE_DB, Self::GLIDE_DB);
                self.now[k] = now;
                let (l, r) = &mut self.filters[k];
                // A band coming in starts clean.
                if was.abs() < 0.05 {
                    l.reset();
                    r.reset();
                }
                l.retune(Shape::Peak, rate, GEQ_BANDS[k], now, GEQ_Q);
                r.retune(Shape::Peak, rate, GEQ_BANDS[k], now, GEQ_Q);
            }
            if self.now[k].abs() < 0.05 {
                continue;
            }
            let (l, r) = &mut self.filters[k];
            for x in left.iter_mut() {
                *x = l.run(*x);
            }
            for x in right.iter_mut() {
                *x = r.run(*x);
            }
        }
    }
}

/// A delay line of a fixed length.
#[derive(Clone)]
struct Line {
    buf: Vec<f32>,
    at: usize,
}

impl Line {
    fn new(len: usize) -> Line {
        Line {
            buf: vec![0.0; len.max(1)],
            at: 0,
        }
    }

    /// The sample `d` (fractional, under the length) ago, linearly between; no write.
    #[inline]
    fn tap(&self, d: f32) -> f32 {
        let n = self.buf.len();
        let d = d.clamp(1.0, (n - 1) as f32);
        let (i, f) = (d.floor() as usize, d.fract());
        let a = self.buf[(self.at + n - i) % n];
        let b = self.buf[(self.at + n - i - 1) % n];
        a + (b - a) * f
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.buf[self.at] = x;
        self.at = (self.at + 1) % self.buf.len();
    }
}

/// A comb filter with a damped (low-passed) feedback.
#[derive(Clone)]
struct Comb {
    line: Line,
    store: f32,
}

impl Comb {
    #[inline]
    fn run(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.line.buf[self.line.at];
        self.store = y * (1.0 - damp) + self.store * damp;
        let v = x + self.store * feedback;
        self.line.buf[self.line.at] = if v.abs() < 1e-20 { 0.0 } else { v };
        self.line.at = (self.line.at + 1) % self.line.buf.len();
        y
    }
}

/// An all-pass filter (gain 0.5).
#[derive(Clone)]
struct AllPass {
    line: Line,
}

impl AllPass {
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let b = self.line.buf[self.line.at];
        let v = x + b * 0.5;
        self.line.buf[self.line.at] = if v.abs() < 1e-20 { 0.0 } else { v };
        self.line.at = (self.line.at + 1) % self.line.buf.len();
        b - x
    }
}

/// Freeverb's delays at 44.1 kHz (scaled to the rate), and the right side's spread.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;

#[derive(Clone)]
struct Reverb {
    pre: Line,
    combs: [Vec<Comb>; 2],
    alls: [Vec<AllPass>; 2],
}

impl Reverb {
    fn new(rate: f32) -> Reverb {
        let s = |n: usize| ((n as f32) * rate / 44_100.0).round() as usize;
        let side = |spread: usize| {
            (
                COMBS
                    .iter()
                    .map(|&n| Comb {
                        line: Line::new(s(n + spread)),
                        store: 0.0,
                    })
                    .collect::<Vec<_>>(),
                ALLPASSES
                    .iter()
                    .map(|&n| AllPass {
                        line: Line::new(s(n + spread)),
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let (cl, al) = side(0);
        let (cr, ar) = side(SPREAD);
        Reverb {
            pre: Line::new((0.2 * rate) as usize + 2),
            combs: [cl, cr],
            alls: [al, ar],
        }
    }

    fn run(&mut self, e: &Effect, rate: f32, left: &mut [f32], right: &mut [f32]) {
        let feedback = 0.7 + e.size * 0.28;
        let damp = e.damping * 0.4;
        let pre = e.predelay * 0.001 * rate;
        let (w1, w2) = (e.width / 2.0 + 0.5, (1.0 - e.width) / 2.0);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let x = (*l + *r) * 0.015;
            self.pre.push(x);
            let x = if pre >= 1.0 { self.pre.tap(pre) } else { x };
            let mut out = [0.0f32; 2];
            for (side, o) in out.iter_mut().enumerate() {
                let mut acc = 0.0;
                for c in &mut self.combs[side] {
                    acc += c.run(x, feedback, damp);
                }
                for a in &mut self.alls[side] {
                    acc = a.run(acc);
                }
                *o = acc;
            }
            // Gain to about unity for a room's worth of tail.
            *l = (out[0] * w1 + out[1] * w2) * 3.0;
            *r = (out[1] * w1 + out[0] * w2) * 3.0;
        }
    }
}

#[derive(Clone)]
struct Echo {
    lines: [Line; 2],
    tone: [f32; 2],
}

impl Echo {
    fn new(rate: f32) -> Echo {
        let n = (2.0 * rate) as usize + 2;
        Echo {
            lines: [Line::new(n), Line::new(n)],
            tone: [0.0; 2],
        }
    }

    fn run(&mut self, e: &Effect, rate: f32, left: &mut [f32], right: &mut [f32]) {
        let d = e.time * 0.001 * rate;
        let a = (-2.0 * std::f32::consts::PI * e.tone / rate).exp();
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (yl, yr) = (self.lines[0].tap(d), self.lines[1].tap(d));
            self.tone[0] = yl * (1.0 - a) + self.tone[0] * a;
            self.tone[1] = yr * (1.0 - a) + self.tone[1] * a;
            let (fl, fr) = (self.tone[0] * e.feedback, self.tone[1] * e.feedback);
            if e.pingpong {
                // The input enters on the left; each repeat crosses over.
                self.lines[0].push((*l + *r) * 0.5 + fr);
                self.lines[1].push(fl);
            } else {
                self.lines[0].push(*l + fl);
                self.lines[1].push(*r + fr);
            }
            *l = yl;
            *r = yr;
        }
    }
}

#[derive(Clone)]
struct Chorus {
    lines: [Line; 2],
    phase: f32,
}

impl Chorus {
    fn new(rate: f32) -> Chorus {
        let n = (0.05 * rate) as usize;
        Chorus {
            lines: [Line::new(n), Line::new(n)],
            phase: 0.0,
        }
    }

    fn run(&mut self, e: &Effect, rate: f32, left: &mut [f32], right: &mut [f32]) {
        let base = 0.015 * rate;
        let depth = e.depth * 0.001 * rate;
        let step = 2.0 * std::f32::consts::PI * e.rate / rate;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            self.lines[0].push(*l);
            self.lines[1].push(*r);
            let p = self.phase;
            // Two voices a side, a quarter turn apart, the sides apart again.
            let v = |line: &Line, off: f32| {
                line.tap(base + depth * (0.5 + 0.5 * (p + off).sin()))
                    + line.tap(
                        base * 1.4
                            + depth * (0.5 + 0.5 * (p + off + std::f32::consts::FRAC_PI_2).sin()),
                    )
            };
            *l = 0.5 * v(&self.lines[0], 0.0);
            *r = 0.5 * v(&self.lines[1], std::f32::consts::PI);
            self.phase = (self.phase + step) % (2.0 * std::f32::consts::PI);
        }
    }
}

/// A built-in effect playing.
#[derive(Clone)]
enum FxUnit {
    Reverb(Box<Reverb>),
    Delay(Box<Echo>),
    Chorus(Box<Chorus>),
}

impl FxUnit {
    /// The unit for `kind` (`None` for none).
    fn new(kind: EffectKind, rate: f32) -> Option<FxUnit> {
        match kind {
            EffectKind::None => None,
            EffectKind::Reverb => Some(FxUnit::Reverb(Box::new(Reverb::new(rate)))),
            EffectKind::Delay => Some(FxUnit::Delay(Box::new(Echo::new(rate)))),
            EffectKind::Chorus => Some(FxUnit::Chorus(Box::new(Chorus::new(rate)))),
        }
    }

    fn kind(&self) -> EffectKind {
        match self {
            FxUnit::Reverb(_) => EffectKind::Reverb,
            FxUnit::Delay(_) => EffectKind::Delay,
            FxUnit::Chorus(_) => EffectKind::Chorus,
        }
    }

    /// The bus's sum in, the effect out (all wet), in place.
    fn run(&mut self, e: &Effect, rate: f32, left: &mut [f32], right: &mut [f32]) {
        match self {
            FxUnit::Reverb(r) => r.run(e, rate, left, right),
            FxUnit::Delay(d) => d.run(e, rate, left, right),
            FxUnit::Chorus(c) => c.run(e, rate, left, right),
        }
    }
}

/// What an FX bus's effect needs kept between blocks: its unit (made again when its kind
/// changes) and how long its output has been silent (it stops being run then).
#[derive(Clone, Default)]
pub struct FxState {
    unit: Option<FxUnit>,
    quiet: usize,
}

impl FxState {
    /// Whether it is worth running: an effect fed, or one still ringing.
    pub fn running(&self, e: &Effect, fed: bool) -> bool {
        e.kind != EffectKind::None && (fed || self.quiet < crate::desk::rate_hz() as usize * 2)
    }

    /// Makes (or drops) the unit for `e`'s kind now, off the clock thread: made in `run`,
    /// a reverb's buffers were allocated (and the old freed) on it (bug round F, F7-8).
    pub fn prepare(&mut self, e: &Effect, rate: f32) {
        if self.unit.as_ref().map(FxUnit::kind) != Some(e.kind) {
            self.unit = FxUnit::new(e.kind, rate);
            self.quiet = 0;
        }
    }

    pub fn run(&mut self, e: &Effect, rate: f32, left: &mut [f32], right: &mut [f32]) {
        if self.unit.as_ref().map(FxUnit::kind) != Some(e.kind) {
            self.unit = FxUnit::new(e.kind, rate);
            self.quiet = 0;
        }
        let Some(u) = self.unit.as_mut() else { return };
        let loud = left.iter().chain(right.iter()).any(|v| v.abs() > 1e-6);
        u.run(e, rate, left, right);
        let out = left.iter().chain(right.iter()).any(|v| v.abs() > 1e-6);
        self.quiet = if loud || out {
            0
        } else {
            self.quiet + left.len()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impulse(n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0; n];
        l[0] = 1.0;
        (l.clone(), l)
    }

    fn energy(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum()
    }

    #[test]
    fn a_reverb_rings_and_dies_away() {
        let rate = 48_000.0;
        let e = Effect {
            kind: EffectKind::Reverb,
            predelay: 0.0,
            ..Default::default()
        };
        let mut st = FxState::default();
        let (mut l, mut r) = impulse(96);
        st.run(&e, rate, &mut l, &mut r);
        // A second of tail, block by block: some in the first half second, decaying.
        let mut early = 0.0;
        let mut late = 0.0;
        for b in 0..1000 {
            let (mut l, mut r) = (vec![0.0; 96], vec![0.0; 96]);
            st.run(&e, rate, &mut l, &mut r);
            if b < 250 {
                early += energy(&l) + energy(&r);
            } else if b >= 750 {
                late += energy(&l) + energy(&r);
            }
            assert!(l.iter().chain(&r).all(|v| v.is_finite() && v.abs() < 2.0));
        }
        assert!(early > 1e-4, "{early}");
        assert!(late < early / 10.0, "decays: {early} then {late}");
        // A bigger room rings longer.
        let big = Effect {
            size: 1.0,
            ..e.clone()
        };
        let small = Effect { size: 0.1, ..e };
        let tail = |e: &Effect| {
            let mut st = FxState::default();
            let (mut l, mut r) = impulse(96);
            st.run(e, rate, &mut l, &mut r);
            let mut t = 0.0;
            for b in 0..800 {
                let (mut l, mut r) = (vec![0.0; 96], vec![0.0; 96]);
                st.run(e, rate, &mut l, &mut r);
                if b > 400 {
                    t += energy(&l) + energy(&r);
                }
            }
            t
        };
        assert!(tail(&big) > tail(&small) * 5.0);
    }

    #[test]
    fn a_delay_repeats_at_its_time() {
        let rate = 48_000.0;
        let e = Effect {
            kind: EffectKind::Delay,
            time: 10.0,
            feedback: 0.5,
            tone: 20_000.0,
            ..Default::default()
        };
        let mut st = FxState::default();
        let mut out = Vec::new();
        for b in 0..20 {
            let (mut l, mut r) = if b == 0 {
                impulse(96)
            } else {
                (vec![0.0; 96], vec![0.0; 96])
            };
            st.run(&e, rate, &mut l, &mut r);
            out.extend(l);
        }
        // 10 ms = 480 samples: the first repeat there, the second at 960, about half.
        let peak = |from: usize| {
            out[from..from + 20]
                .iter()
                .fold(0f32, |m, v| m.max(v.abs()))
        };
        assert!(peak(475) > 0.8, "{}", peak(475));
        assert!((peak(955) / peak(475) - 0.5).abs() < 0.1, "{}", peak(955));
        assert!(out[100..470].iter().all(|v| v.abs() < 1e-3));
        // Ping-pong: the first repeat on the left, the second on the right.
        let e = Effect {
            pingpong: true,
            ..e
        };
        let mut st = FxState::default();
        let (mut ls, mut rs) = (Vec::new(), Vec::new());
        for b in 0..20 {
            let (mut l, mut r) = if b == 0 {
                impulse(96)
            } else {
                (vec![0.0; 96], vec![0.0; 96])
            };
            st.run(&e, rate, &mut l, &mut r);
            ls.extend(l);
            rs.extend(r);
        }
        let pk =
            |v: &[f32], from: usize| v[from..from + 20].iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(pk(&ls, 475) > 0.5 && pk(&rs, 475) < 1e-3);
        assert!(pk(&rs, 955) > 0.2 && pk(&ls, 955) < 1e-3);
    }

    #[test]
    fn a_chorus_and_the_graphic_eq() {
        let rate = 48_000.0;
        let e = Effect {
            kind: EffectKind::Chorus,
            ..Default::default()
        };
        let mut st = FxState::default();
        let tone: Vec<f32> = (0..96 * 200)
            .map(|k| (k as f32 * 0.05).sin() * 0.5)
            .collect();
        let mut out = 0.0;
        for b in tone.chunks(96) {
            let (mut l, mut r) = (b.to_vec(), b.to_vec());
            st.run(&e, rate, &mut l, &mut r);
            out += energy(&l);
        }
        assert!(out > 1.0, "the chorus passes the tone: {out}");
        // A 1 kHz band up 12 dB lifts a 1 kHz tone by about that, not a 100 Hz one.
        let mut g = Geq {
            on: true,
            ..Default::default()
        };
        g.gains[17] = 12.0;
        let level = |f: f32| {
            let mut s = GeqState::default();
            let mut peak = 0.0f32;
            for b in 0..200 {
                let mut l: Vec<f32> = (0..96)
                    .map(|k| (2.0 * std::f32::consts::PI * f * (b * 96 + k) as f32 / rate).sin())
                    .collect();
                let mut r = l.clone();
                s.run(&g, rate, &mut l, &mut r);
                if b > 100 {
                    peak = l.iter().fold(peak, |m, v| m.max(v.abs()));
                }
            }
            20.0 * peak.log10()
        };
        assert!((level(1000.0) - 12.0).abs() < 0.5, "{}", level(1000.0));
        assert!(level(100.0).abs() < 0.5, "{}", level(100.0));
        assert_eq!(
            Geq {
                gains: vec![99.0; 3],
                ..Default::default()
            }
            .bounded()
            .gains
            .len(),
            31
        );
    }
}
