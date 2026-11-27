//! Which SpeedHQ decode a source uses: the GPU first, in
//! f64 and, if that doesn't keep up, f32, then the CPU's own decoder, each on the
//! source's own pictures for a moment; the one that keeps up at the least CPU is kept.
//! The GPU only when it saves a tenth of the CPU or more: its pictures wait for the
//! upload and the read-back (on a Radeon 680M: 11–14 ms more hold-back, and no
//! saving on a test card, where the CPU's inverse DCT is cheap). Tried again when the
//! picture format changes, after the GPU falls behind, and every 10 minutes (the content
//! decides: a grainy camera costs the CPU far more than a slide). A source's setting
//! can fix it instead ([`Prefer`]).

use idn_shq_gpu::idct::Precision;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Gpu(Precision),
    Cpu,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Gpu(Precision::F64) => write!(f, "the GPU (f64)"),
            Mode::Gpu(Precision::F32) => write!(f, "the GPU (f32)"),
            Mode::Cpu => write!(f, "the CPU decoder"),
        }
    }
}

/// A source's setting: measured, or fixed. The GPU is "hybrid" in
/// fact (the bitstream read on the CPU, the inverse DCT on the GPU): no SpeedHQ decoder
/// runs wholly on a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefer {
    #[default]
    Auto,
    Gpu,
    Cpu,
}

impl Prefer {
    /// From the saved setting.
    pub fn from_setting(v: Option<&str>) -> Prefer {
        match v {
            Some("gpu") => Prefer::Gpu,
            Some("cpu") => Prefer::Cpu,
            _ => Prefer::Auto,
        }
    }
}

/// What to try, in order, for `prefer`: `gpu` says whether there is a usable GPU and
/// whether it has f64; `idct` is `IDN_OBS_IDCT` (`f32` or `f64` fixes the arithmetic).
pub fn candidates(prefer: Prefer, gpu: Option<bool>, idct: Option<&str>) -> Vec<Mode> {
    let Some(has_f64) = gpu.filter(|_| prefer != Prefer::Cpu) else {
        return vec![Mode::Cpu];
    };
    let mut v = Vec::new();
    if has_f64 && idct != Some("f32") {
        v.push(Mode::Gpu(Precision::F64));
    }
    if idct != Some("f64") || !has_f64 {
        v.push(Mode::Gpu(Precision::F32));
    }
    if prefer == Prefer::Auto {
        v.push(Mode::Cpu);
    }
    v
}

/// One mode's pictures in a trial.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    /// CPU per picture (every thread's), ms.
    pub cpu_ms: f64,
    /// Arrival-to-OBS hold-back, median, ms.
    pub hold_ms: f64,
    pub kept_up: bool,
}

struct Trial {
    mode: Mode,
    started: Instant,
    /// Its pictures seen (the first few, still settling, not counted).
    seen: u32,
    cpu: Duration,
    holds: Vec<f32>,
    /// GPU pictures that took most of a frame period.
    slow: u32,
}

/// Pictures of a trial's own mode passed before counting, and counted at least.
const SETTLE: u32 = 5;
const COUNT: usize = 45;
const MIN_TRIAL: Duration = Duration::from_millis(1500);
const AGAIN: Duration = Duration::from_secs(600);
/// The share of the CPU decoder's CPU the GPU must save to be kept
/// (grainy camera pictures are the usual case; 15% was measured there on a 680M).
const SAVING: f64 = 0.10;
/// GPU pictures in a row over 85% of the frame period that start a new choice.
const SLOW_RUN: u32 = 30;

pub struct Choice {
    /// What may be tried, in order (one: fixed, nothing tried).
    candidates: Vec<Mode>,
    /// Still to try this round.
    queue: Vec<Mode>,
    trial: Option<Trial>,
    results: Vec<(Mode, Measured)>,
    /// In use now.
    pub mode: Mode,
    format: Option<([u8; 4], u32, u32)>,
    decided: Instant,
    /// GPU pictures in a row over 85% of the frame period, outside trials.
    slow_run: u32,
    min_trial: Duration,
}

/// What the source should do after a picture: switch to a mode (a trial's, or the one
/// decided), and say something in the log (the decision).
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub switch: Option<Mode>,
    pub log: Option<String>,
}

impl Choice {
    /// `candidates` in the order to try them; the first is used until the first picture.
    pub fn new(candidates: Vec<Mode>) -> Choice {
        let mode = candidates.first().copied().unwrap_or(Mode::Cpu);
        Choice {
            candidates,
            queue: Vec::new(),
            trial: None,
            results: Vec::new(),
            mode,
            format: None,
            decided: Instant::now(),
            slow_run: 0,
            min_trial: MIN_TRIAL,
        }
    }

    /// The GPU failed: only the CPU from now on.
    pub fn gpu_failed(&mut self) {
        self.candidates.retain(|m| *m == Mode::Cpu);
        if self.candidates.is_empty() {
            self.candidates.push(Mode::Cpu);
        }
        self.queue.clear();
        self.trial = None;
        self.mode = Mode::Cpu;
    }

    fn start_round(&mut self) -> Step {
        self.results.clear();
        self.queue = self.candidates.clone();
        self.queue.reverse();
        self.next_trial()
    }

    fn next_trial(&mut self) -> Step {
        // f32 only when f64 didn't keep up (the CPU's share is the same).
        if self
            .results
            .iter()
            .any(|(m, r)| *m == Mode::Gpu(Precision::F64) && r.kept_up)
        {
            self.queue.retain(|m| *m != Mode::Gpu(Precision::F32));
        }
        match self.queue.pop() {
            Some(m) => {
                self.trial = Some(Trial {
                    mode: m,
                    started: Instant::now(),
                    seen: 0,
                    cpu: Duration::ZERO,
                    holds: Vec::new(),
                    slow: 0,
                });
                let switch = (m != self.mode).then_some(m);
                self.mode = m;
                Step { switch, log: None }
            }
            None => self.decide(),
        }
    }

    fn decide(&mut self) -> Step {
        self.trial = None;
        self.decided = Instant::now();
        let get = |m: Mode| self.results.iter().find(|(r, _)| *r == m).map(|(_, r)| *r);
        let gpu = [Precision::F64, Precision::F32]
            .into_iter()
            .map(Mode::Gpu)
            .find_map(|m| get(m).filter(|r| r.kept_up).map(|r| (m, r)));
        let cpu = get(Mode::Cpu);
        let (mode, why) = match (gpu, cpu) {
            (Some((m, g)), Some(c)) if c.kept_up => {
                if g.cpu_ms <= c.cpu_ms * (1.0 - SAVING) {
                    (m, "less CPU")
                } else {
                    (Mode::Cpu, "the GPU saves too little for its delay")
                }
            }
            (Some((m, _)), _) => (m, "the CPU decoder doesn't keep up"),
            // The GPU as set, though behind: its quicker arithmetic.
            (None, _) if !self.candidates.contains(&Mode::Cpu) => {
                let m = [Precision::F32, Precision::F64]
                    .into_iter()
                    .map(Mode::Gpu)
                    .find(|m| self.candidates.contains(m))
                    .unwrap_or(Mode::Gpu(Precision::F32));
                (m, "set to the GPU, though it falls behind")
            }
            (None, _) => (Mode::Cpu, "the GPU doesn't keep up"),
        };
        let fmt = |m: Mode| match get(m) {
            Some(r) => format!(
                "{m}: {:.1} ms of CPU a picture, hold-back {:.1} ms{}",
                r.cpu_ms,
                r.hold_ms,
                if r.kept_up { "" } else { ", behind" }
            ),
            None => format!("{m}: not tried"),
        };
        let tried: Vec<String> = self.results.iter().map(|(m, _)| fmt(*m)).collect();
        let switch = (mode != self.mode).then_some(mode);
        self.mode = mode;
        Step {
            switch,
            log: Some(format!(
                "SpeedHQ decoded by {mode} ({why}; {})",
                tried.join("; ")
            )),
        }
    }

    /// A picture handed to OBS: its format, the mode it was decoded in (`Gpu` when it
    /// came as coefficients and the GPU reconstructed it), its CPU (every thread's), the
    /// GPU's time for it, its hold-back and the frame period, seconds.
    pub fn picture(
        &mut self,
        format: ([u8; 4], u32, u32),
        decoded: Mode,
        cpu: Duration,
        gpu_secs: Option<f64>,
        hold_ms: f32,
        period: f64,
    ) -> Step {
        if self.candidates.len() < 2 {
            return Step::default();
        }
        let slow = gpu_secs.is_some_and(|s| s > 0.85 * period);
        if self.format != Some(format) {
            self.format = Some(format);
            return self.start_round();
        }
        let Some(t) = self.trial.as_mut() else {
            // Decided: the GPU falling behind, or time for another look.
            self.slow_run = if slow { self.slow_run + 1 } else { 0 };
            if self.slow_run >= SLOW_RUN || self.decided.elapsed() >= AGAIN {
                self.slow_run = 0;
                return self.start_round();
            }
            return Step::default();
        };
        // Pictures still on their way from the mode before don't count (a GPU trial counts
        // any GPU picture: a new precision takes over at once).
        if on_gpu(decoded) != on_gpu(t.mode) {
            return Step::default();
        }
        t.seen += 1;
        if t.seen <= SETTLE {
            return Step::default();
        }
        t.cpu += cpu;
        t.holds.push(hold_ms);
        t.slow += u32::from(slow);
        // Clearly behind: ended at once (a 4K60 trial of f64 and then f32, both behind,
        // held pictures back up to 102 ms and lost 13 of them). Over a frame
        // period behind on any picture counted, or GPU pictures too slow for one in ten
        // of the full count.
        let behind = hold_ms as f64 > 2.0 * period * 1e3 || t.slow as usize > COUNT / 10;
        if !behind && (t.holds.len() < COUNT || t.started.elapsed() < self.min_trial) {
            return Step::default();
        }
        let n = t.holds.len();
        t.holds.sort_by(|a, b| a.total_cmp(b));
        let hold = t.holds[n / 2] as f64;
        let measured = Measured {
            cpu_ms: t.cpu.as_secs_f64() * 1e3 / n as f64,
            hold_ms: hold,
            // Nine in ten GPU pictures within 85% of the period, and the pictures not
            // held back a frame period or more.
            kept_up: !behind && (t.slow as usize) * 10 <= n && hold < period * 1e3,
        };
        self.results.push((t.mode, measured));
        self.next_trial()
    }
}

fn on_gpu(m: Mode) -> bool {
    matches!(m, Mode::Gpu(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    const F64: Mode = Mode::Gpu(Precision::F64);
    const F32: Mode = Mode::Gpu(Precision::F32);
    const FMT: ([u8; 4], u32, u32) = (*b"SHQ2", 3840, 2160);
    const PERIOD: f64 = 1.0 / 30.0;

    /// Feeds pictures the way the source would: decoded in the mode in use, costing what
    /// `cost` says for it (CPU ms, GPU ms, hold-back ms). Returns the log lines.
    fn run(
        c: &mut Choice,
        cost: impl Fn(Mode) -> (f64, Option<f64>, f32),
        n: usize,
    ) -> Vec<String> {
        let mut logs = Vec::new();
        for _ in 0..n {
            let m = c.mode;
            let (cpu, gpu, hold) = cost(m);
            let step = c.picture(
                FMT,
                m,
                Duration::from_secs_f64(cpu / 1e3),
                gpu.map(|g| g / 1e3),
                hold,
                PERIOD,
            );
            if let Some(s) = step.switch {
                assert_eq!(s, c.mode);
            }
            logs.extend(step.log);
        }
        logs
    }

    fn quick(candidates: Vec<Mode>) -> Choice {
        let mut c = Choice::new(candidates);
        c.min_trial = Duration::ZERO;
        c
    }

    #[test]
    fn a_gpu_that_saves_enough_is_kept_and_f32_not_tried() {
        let mut c = quick(vec![F64, F32, Mode::Cpu]);
        let logs = run(
            &mut c,
            |m| match m {
                Mode::Cpu => (40.0, None, 6.0),
                _ => (20.0, Some(10.0), 18.0),
            },
            200,
        );
        assert_eq!(c.mode, F64);
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert!(logs[0].contains("the GPU (f64) (less CPU"), "{}", logs[0]);
        assert!(!logs[0].contains("f32"), "{}", logs[0]);
    }

    #[test]
    fn a_gpu_that_saves_little_gives_way_to_the_cpu() {
        // Lab 198 on the test card: the GPU path cost more.
        let mut c = quick(vec![F64, F32, Mode::Cpu]);
        let logs = run(
            &mut c,
            |m| match m {
                Mode::Cpu => (8.0, None, 3.6),
                _ => (11.0, Some(17.0), 17.0),
            },
            200,
        );
        assert_eq!(c.mode, Mode::Cpu);
        assert!(logs[0].contains("saves too little"), "{}", logs[0]);
    }

    #[test]
    fn a_slow_f64_tries_f32() {
        let mut c = quick(vec![F64, F32, Mode::Cpu]);
        let logs = run(
            &mut c,
            |m| match m {
                F64 => (20.0, Some(31.0), 40.0),
                F32 => (20.0, Some(12.0), 15.0),
                Mode::Cpu => (45.0, None, 8.0),
            },
            300,
        );
        assert_eq!(c.mode, F32, "{logs:?}");
        assert!(
            logs[0].contains("the GPU (f64): 20.0 ms of CPU a picture, hold-back 40.0 ms, behind")
        );
    }

    #[test]
    fn the_gpu_when_the_cpu_cant_keep_up() {
        let mut c = quick(vec![F64, Mode::Cpu]);
        run(
            &mut c,
            |m| match m {
                Mode::Cpu => (50.0, None, 45.0),
                _ => (45.0, Some(20.0), 25.0),
            },
            200,
        );
        assert_eq!(c.mode, F64);
    }

    #[test]
    fn a_new_format_or_a_gpu_falling_behind_chooses_again() {
        let mut c = quick(vec![F64, Mode::Cpu]);
        let cheap_gpu = |m| match m {
            Mode::Cpu => (40.0, None, 6.0),
            _ => (20.0, Some(10.0), 18.0),
        };
        assert_eq!(run(&mut c, cheap_gpu, 200).len(), 1);
        // The GPU slows down for good (another program took it): a new round, the CPU.
        let logs = run(
            &mut c,
            |m| match m {
                Mode::Cpu => (40.0, None, 6.0),
                _ => (20.0, Some(31.0), 40.0),
            },
            200,
        );
        assert_eq!(c.mode, Mode::Cpu, "{logs:?}");
        // Another format: another round.
        let step = c.picture(
            (*b"SHQ0", 3840, 2160),
            Mode::Cpu,
            Duration::ZERO,
            None,
            5.0,
            PERIOD,
        );
        assert_eq!(step.switch, Some(F64));
    }

    #[test]
    fn the_setting_decides_what_is_tried() {
        use Prefer::*;
        assert_eq!(candidates(Auto, Some(true), None), [F64, F32, Mode::Cpu]);
        assert_eq!(candidates(Auto, Some(false), None), [F32, Mode::Cpu]);
        assert_eq!(candidates(Auto, None, None), [Mode::Cpu]);
        assert_eq!(candidates(Gpu, Some(true), None), [F64, F32]);
        assert_eq!(candidates(Gpu, Some(true), Some("f32")), [F32]);
        assert_eq!(candidates(Gpu, Some(false), Some("f64")), [F32]);
        assert_eq!(candidates(Gpu, None, None), [Mode::Cpu]);
        assert_eq!(candidates(Cpu, Some(true), None), [Mode::Cpu]);
        assert_eq!(Prefer::from_setting(Some("gpu")), Gpu);
        assert_eq!(Prefer::from_setting(Some("hybrid")), Auto);
        assert_eq!(Prefer::from_setting(None), Auto);
    }

    #[test]
    fn set_to_the_gpu_it_stays_there_even_behind() {
        let mut c = quick(vec![F64, F32]);
        let logs = run(&mut c, |_| (20.0, Some(40.0), 50.0), 200);
        assert_eq!(c.mode, F32, "{logs:?}");
        assert!(logs[0].contains("set to the GPU"), "{}", logs[0]);
    }

    #[test]
    fn a_trial_far_behind_ends_at_once() {
        // 4K60 trials of f64 and f32, both behind, cost 13 pictures.
        let mut c = quick(vec![F64, F32, Mode::Cpu]);
        c.min_trial = MIN_TRIAL;
        let mut n = 0;
        while c.mode != Mode::Cpu && n < 100 {
            let m = c.mode;
            c.picture(
                FMT,
                m,
                Duration::from_millis(8),
                Some(0.030),
                70.0,
                1.0 / 60.0,
            );
            n += 1;
        }
        // The first picture starts the round; then each GPU trial its settling pictures
        // and one counted.
        assert_eq!(n, 1 + 2 * (SETTLE as usize + 1), "{n} pictures");
    }

    #[test]
    fn one_candidate_is_never_tried_against_anything() {
        let mut c = quick(vec![Mode::Cpu]);
        assert!(run(&mut c, |_| (40.0, None, 6.0), 100).is_empty());
        assert_eq!(c.mode, Mode::Cpu);
        c = quick(vec![F64, Mode::Cpu]);
        c.gpu_failed();
        assert!(run(&mut c, |_| (40.0, None, 6.0), 100).is_empty());
        assert_eq!(c.mode, Mode::Cpu);
    }
}
