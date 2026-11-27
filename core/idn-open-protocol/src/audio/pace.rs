//! When a block-paced program's blocks are due (roadmap R.26): it
//! follows a reference clock when it has one (the desk: its local sound card; the ASIO
//! driver: the desk's blocks arriving on this machine), else site time (PTP) when this
//! machine's clock is usable, else its own timer. So one clock runs the card, the desk and
//! the programs on the desk, and nothing between them drifts.
//!
//! Each block `k` is due at a monotonic time ([`crate::ptp::mono_ns`]):
//! - **Reference**: its frames against time, fitted over the last 30 s (least squares);
//!   the period is the block's length at the reference's measured rate.
//! - **Site**: block `k` at site time `t₀ + k × block`, `t₀` a multiple of the block, so
//!   programs on one site share their block edges; site time to monotonic by the clock's
//!   mapping.
//! - **Own**: every block's length of this machine's clock, as before R.26.
//!
//! A change of source starts the new one from the block due now, never a jump.

use crate::ptp::mono_ns;
use std::collections::VecDeque;
use std::sync::Arc;

/// A reference clock as read: frames passed, when ([`mono_ns`]), and its nominal rate.
/// `None` before it ran.
pub type ClockRef = Arc<dyn Fn() -> Option<(u64, i64, u32)> + Send + Sync>;

/// The card's readings kept for its rate: one a 100 ms, 30 s.
const CARD_SAMPLES: usize = 300;
/// A card's rate is used once its readings span this long.
const CARD_SETTLE_NS: i64 = 5_000_000_000;
/// How far a reference's measured rate may be from its label (a crystal off by more is a
/// fault or a measurement gone wrong).
const MAX_PPM: f64 = 1000.0;

/// Sleeps `d`, in slices that look at `stop` (a clock's sleep must never outlast its
/// stop). False when stopped.
pub fn sleep_unless_stopped(d: std::time::Duration, stop: &std::sync::atomic::AtomicBool) -> bool {
    let end = std::time::Instant::now() + d;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(50)));
    }
}

/// What the clock follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A sound card's or the desk's clock (the reference handed in).
    Reference,
    /// The site's time (PTP).
    Site,
    /// This machine's own clock.
    Own,
}

/// A program's block times.
pub struct Pacer {
    /// What its blocks are called in the log ("the desk's"), and its reference's name
    /// ("card", "desk").
    what: &'static str,
    reference_name: &'static str,
    block_ns: f64,
    card: Option<ClockRef>,
    source: Source,
    /// Block `anchor.0` is due at monotonic `anchor.1` (ns).
    anchor: (u64, f64),
    /// Own and card: a block's length here, ns (2 ms, or 2 ms at the card's rate).
    period_ns: f64,
    /// Site: the site time block `anchor.0` stands for.
    site_anchor: i128,
    /// The card's readings: (monotonic ns, frames, nominal rate).
    readings: VecDeque<(i64, u64, u32)>,
    last_reading: i64,
    /// The card's measured rate against its label, ppm (for the state).
    ppm: f64,
    last_check: i64,
    /// A source chosen by hand (an engineer's override); `None`: automatic. One not
    /// available is passed over for the automatic choice.
    prefer: Option<Source>,
}

impl Pacer {
    /// A pacer of blocks of `frames` at `rate`, starting with block 0 due now; `what`
    /// names the program in the log, `reference_name` its reference.
    pub fn new(
        what: &'static str,
        reference_name: &'static str,
        frames: u32,
        rate: u32,
        reference: Option<ClockRef>,
    ) -> Pacer {
        let block_ns = frames as f64 * 1e9 / rate.max(1) as f64;
        Pacer {
            what,
            reference_name,
            block_ns,
            card: reference,
            source: Source::Own,
            anchor: (0, mono_ns() as f64),
            period_ns: block_ns,
            site_anchor: 0,
            readings: VecDeque::new(),
            last_reading: 0,
            ppm: 0.0,
            last_check: 0,
            prefer: None,
        }
    }

    /// Chooses the source by hand (`None`: automatic, R.26): followed while available,
    /// else the automatic choice. Looked at again within a block.
    pub fn set_prefer(&mut self, prefer: Option<Source>) {
        if prefer != self.prefer {
            self.prefer = prefer;
            self.last_check = 0;
        }
    }

    /// What it follows now.
    pub fn source(&self) -> Source {
        self.source
    }

    /// What it follows now, by name: the reference's, "site" or "own".
    pub fn source_name(&self) -> &'static str {
        match self.source {
            Source::Reference => self.reference_name,
            Source::Site => "site",
            Source::Own => "own",
        }
    }

    /// The reference's rate against its label, ppm (0 when not following it).
    pub fn ppm(&self) -> f64 {
        if self.source == Source::Reference {
            self.ppm
        } else {
            0.0
        }
    }

    /// Replaces the reference (the ASIO driver's desk: a new one, or none).
    pub fn set_reference(&mut self, reference: Option<ClockRef>) {
        self.card = reference;
        self.readings.clear();
    }

    /// When block `k` is due (monotonic ns). Called once a block, in order; it looks at
    /// the card and the site clock as it goes.
    pub fn due(&mut self, k: u64) -> i64 {
        let now = mono_ns();
        self.read_card(now);
        // Which source, looked at once a second (a card that stopped, a site clock that
        // locked or lost its lock).
        if now - self.last_check >= 1_000_000_000 {
            self.last_check = now;
            let want = self.choose();
            if want != self.source {
                let at = self.at(k);
                self.switch(want, k, at);
            }
        }
        // A card's rate, followed as it is measured (the period re-anchored, not
        // jumped).
        if self.source == Source::Reference
            && let Some(period) = self.card_period()
            && (period - self.period_ns).abs() > 1e-3
        {
            let at = self.at(k);
            self.anchor = (k, at);
            self.period_ns = period;
            self.ppm = (self.block_ns / period - 1.0) * 1e6;
        }
        let at = self.at(k);
        // Due far ahead of now: what it follows stepped back (site time set back while
        // still usable). Waited out, the program was silent for the step and its stop
        // waited too: on from now.
        if self.source == Source::Site && at - now as f64 > (8.0 * self.block_ns).max(50e6) {
            crate::log::log(&format!(
                "{} clock: its time stepped back {:.0} ms; on from now",
                self.what,
                (at - now as f64) / 1e6
            ));
            self.anchor_at(k, now as f64);
            return self.at(k).round() as i64;
        }
        at.round() as i64
    }

    fn at(&self, k: u64) -> f64 {
        let n = k.saturating_sub(self.anchor.0) as f64;
        match self.source {
            Source::Site => match site_mapping() {
                Some(m) => m.mono(self.site_anchor + (n * self.block_ns) as i128) as f64,
                None => self.anchor.1 + n * self.block_ns,
            },
            _ => self.anchor.1 + n * self.period_ns,
        }
    }

    fn switch(&mut self, to: Source, k: u64, at: f64) {
        let was = self.source_name();
        self.source = to;
        crate::log::log(&format!(
            "{} clock follows {} (was {was})",
            self.what,
            self.source_name()
        ));
        self.anchor_at(k, at);
    }

    /// Block `k` due at `at` (on site time: the next block edge from there).
    fn anchor_at(&mut self, k: u64, at: f64) {
        self.period_ns = self.block_ns;
        self.anchor = (k, at);
        if self.source == Source::Site
            && let Some(m) = site_mapping()
        {
            // The next 2 ms edge of site time at or after the block due now.
            let site = m.site(at.round() as i64);
            let edge = (self.block_ns as i128).max(1);
            let next = (site + edge - 1).div_euclid(edge) * edge;
            self.site_anchor = next;
            self.anchor = (k, m.mono(next) as f64);
        }
    }

    /// The rule: a card that runs, else usable site time, else our own.
    fn choose(&self) -> Source {
        let card_runs = self
            .readings
            .back()
            .is_some_and(|r| mono_ns() - r.0 < 500_000_000);
        // A card that runs is followed at once: at its label's rate until its own is
        // measured (then re-anchored, no jump). Waiting for 5 s of readings first, the
        // desk ran on site time while the card played (its output ran dry once a
        // second against the card's clock, a refill each time: the delay grew), then
        // switched mid-sound (lab 463(d)-A); and a card that restarted its count after
        // such a gap never had 5 s of readings, so the desk never followed it.
        let (card_ok, site_ok) = (self.card.is_some() && card_runs, site_mapping().is_some());
        match self.prefer {
            Some(Source::Reference) if card_ok => return Source::Reference,
            Some(Source::Site) if site_ok => return Source::Site,
            Some(Source::Own) => return Source::Own,
            _ => {}
        }
        if card_ok {
            Source::Reference
        } else if site_ok {
            Source::Site
        } else {
            Source::Own
        }
    }

    fn read_card(&mut self, now: i64) {
        let Some(card) = &self.card else { return };
        if now - self.last_reading < 100_000_000 {
            return;
        }
        self.last_reading = now;
        let Some((frames, at, rate)) = card() else {
            return;
        };
        if self.readings.back().is_some_and(|r| r.0 == at) {
            return; // no callback since: the card stopped
        }
        // Its count went back or its rate changed (the card started again): measured
        // afresh, not fitted across the restart (up to 30 s of a rate rejected).
        if self
            .readings
            .back()
            .is_some_and(|r| frames < r.1 || rate != r.2)
        {
            self.readings.clear();
        }
        if self.readings.len() >= CARD_SAMPLES {
            self.readings.pop_front();
        }
        self.readings.push_back((at, frames, rate));
    }

    /// A block's length at the card's measured rate, once measured long enough.
    fn card_period(&self) -> Option<f64> {
        let (first, last) = (self.readings.front()?, self.readings.back()?);
        if last.0 - first.0 < CARD_SETTLE_NS || first.2 == 0 {
            return None;
        }
        // Least squares: frames against time.
        let n = self.readings.len() as f64;
        let (t0, f0) = (first.0 as f64, first.1 as f64);
        let (mut st, mut sf, mut stt, mut stf) = (0.0, 0.0, 0.0, 0.0);
        for r in &self.readings {
            let (t, f) = ((r.0 as f64 - t0) * 1e-9, r.1 as f64 - f0);
            st += t;
            sf += f;
            stt += t * t;
            stf += t * f;
        }
        let d = n * stt - st * st;
        if d <= 0.0 {
            return None;
        }
        let rate = (n * stf - st * sf) / d;
        let ppm = (rate / first.2 as f64 - 1.0) * 1e6;
        if !ppm.is_finite() || ppm.abs() > MAX_PPM {
            return None;
        }
        Some(self.block_ns / (1.0 + ppm * 1e-6))
    }
}

/// This machine's site clock when usable: monotonic to site time.
fn site_mapping() -> Option<crate::ptp::Mapping> {
    crate::node::clock::status()
        .filter(|c| c.usable())
        .and_then(|c| c.mapping)
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_common::sync::LockOk as _;
    use std::sync::Mutex;

    /// A card 100 ppm fast: once measured, the desk's blocks come 100 ppm sooner.
    #[test]
    fn the_desk_follows_a_card_s_rate() {
        let t0 = mono_ns();
        let state = Arc::new(Mutex::new((0u64, t0)));
        let s2 = state.clone();
        let card: ClockRef = Arc::new(move || {
            let (f, at) = *s2.lock_ok();
            Some((f, at, 48_000))
        });
        let mut p = Pacer::new("test", "card", 96, 48_000, Some(card));
        // Readings over 6 s of the card's time, fed as if 100 ms passed between them.
        for i in 0..=60u64 {
            let at = t0 + (i as i64) * 100_000_000;
            *state.lock_ok() = ((i as f64 * 4800.0 * 1.0001) as u64, at);
            p.last_reading = 0;
            p.read_card(at);
        }
        let period = p.card_period().expect("measured");
        let ppm = (p.block_ns / period - 1.0) * 1e6;
        assert!((ppm - 100.0).abs() < 1.0, "{ppm} ppm");
    }

    #[test]
    fn a_card_that_runs_is_followed_at_once() {
        let state = Arc::new(Mutex::new((960u64, mono_ns())));
        let s2 = state.clone();
        let card: ClockRef = Arc::new(move || {
            let (f, at) = *s2.lock_ok();
            Some((f, at, 48_000))
        });
        let mut p = Pacer::new("test", "card", 96, 48_000, Some(card));
        p.read_card(mono_ns());
        assert_eq!(
            p.choose(),
            Source::Reference,
            "followed as it runs, not 5 s later"
        );
        assert!(
            p.card_period().is_none(),
            "at its label's rate until measured"
        );
        let k = 10;
        let at = p.due(k);
        assert_eq!(p.source(), Source::Reference);
        assert!(
            (p.due(k + 1) - at - 2_000_000).abs() <= 1,
            "a block at the label's rate"
        );
    }

    #[test]
    fn a_card_that_starts_again_is_measured_afresh() {
        let t0 = mono_ns();
        let state = Arc::new(Mutex::new((0u64, t0)));
        let s2 = state.clone();
        let card: ClockRef = Arc::new(move || {
            let (f, at) = *s2.lock_ok();
            Some((f, at, 48_000))
        });
        let mut p = Pacer::new("test", "card", 96, 48_000, Some(card));
        let feed = |p: &mut Pacer, from: u64, to: u64, base: u64| {
            for i in from..=to {
                let at = t0 + (i as i64) * 100_000_000;
                *state.lock_ok() = (base + (i - from) * 4800, at);
                p.last_reading = 0;
                p.read_card(at);
            }
        };
        feed(&mut p, 0, 60, 1_000_000);
        assert!(p.card_period().is_some());
        // Started again: its count from 0. Only the new readings count.
        feed(&mut p, 61, 61, 0);
        assert_eq!(p.readings.len(), 1);
        feed(&mut p, 62, 125, 4800);
        let period = p.card_period().expect("measured again");
        assert!((period - p.block_ns).abs() < 1.0, "{period}");
    }

    /// No card and no site clock: its own 2 ms.
    #[test]
    fn without_a_card_or_site_clock_it_keeps_its_own_time() {
        let mut p = Pacer::new("test", "card", 96, 48_000, None);
        let a = p.due(0);
        let b = p.due(500);
        assert_eq!(p.source(), Source::Own);
        assert!(((b - a) as f64 - 500.0 * p.block_ns).abs() < 1e3);
    }
}
