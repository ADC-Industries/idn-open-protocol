//! Whether a lane's samples may pass through unresampled: its drift measured over
//! steady stretches.

use super::*;

/// How long a lane by time is watched for drift before its samples pass through
/// (`IDN_LANE_THROUGH`).
#[cfg(not(test))]
const CALM: Duration = Duration::from_secs(30);
#[cfg(test)]
const CALM: Duration = Duration::from_secs(3);

/// Whether a lane's samples may pass through unresampled (`IDN_LANE_THROUGH`): measured,
/// never assumed. Its offset is how far the lane is from where it should be, in ms: by
/// site time its lateness, else its mean fill less the level it holds. A correction moves
/// the offset (a positive one raises both), so the offset less the corrections made
/// moves with the drift alone: over a steady stretch, the drift is its slope (a line
/// fitted to each second's: the fill's jitter at the stretch's two ends put 3 of 8 lanes
/// on one clock at 13–19 ppm).
pub(super) struct Watch {
    /// Filtering: since when the lane has been steady, and that stretch's line. Under 10
    /// ppm of drift the samples pass through; passing through proves it, as a second fix
    /// within 5 minutes (over about 3 ppm) ends it.
    pub(super) calm: Option<Calm>,
    /// How long that takes: [`CALM`], doubled each time passing through proved the
    /// clocks not one after all (up to 32 times).
    pub(super) calm_for: Duration,
    /// Passing through: when the offset was last set right by a cut or a pad.
    pub(super) fix: Option<Instant>,
    /// Passing through by site time, or by the fill: the measure it was decided by.
    pub(super) timed: bool,
    /// Why it last ended ([`LaneStats::through_ended`]).
    pub(super) ended: &'static str,
}

impl Watch {
    pub(super) fn new() -> Watch {
        Watch {
            calm: None,
            calm_for: CALM,
            fix: None,
            timed: false,
            ended: "",
        }
    }

    /// Filters again, for `why` ([`LaneStats::through_ended`]); after `drift` (the clocks
    /// weren't one) the next try waits longer.
    pub(super) fn leave(&mut self, rate: &mut Rate, why: &'static str) {
        if rate.is_through() {
            self.ended = why;
        }
        rate.set_through(false);
        self.calm = None;
        if why == "drift" {
            self.calm_for = (self.calm_for * 2).min(CALM * 32);
        }
    }

    /// Passing through, `off` ms from where it should be: at a millisecond, a cut or a pad
    /// in a quiet moment (`relevel`), unless the last was under 5 minutes ago.
    pub(super) fn hold(&mut self, rate: &mut Rate, off: f64, now: Instant, relevel: &mut Relevel) {
        if off.abs() < 1.0 || relevel.is_some() {
            return;
        }
        if self
            .fix
            .is_some_and(|f| now.duration_since(f).as_secs() < 300)
        {
            self.leave(rate, "drift");
        } else {
            *relevel = Some((off, now));
            self.fix = Some(now);
        }
    }

    /// Filtering, once a second: the correction applied (ppm) and the offset. Steady for
    /// the calm time with under 10 ppm of drift and within a millisecond, the samples
    /// pass through (the switch crossfaded over a chunk; what offset is left, a cut or a
    /// pad in a quiet moment). Gives the drift measured, at the end of each window.
    pub(super) fn watch(
        &mut self,
        rate: &mut Rate,
        steady: bool,
        corr: f64,
        off: f64,
        now: Instant,
        timed: bool,
    ) -> Option<f64> {
        if !steady {
            self.calm = None;
            return None;
        }
        let c = self.calm.get_or_insert_with(|| Calm::new(now));
        c.add(now, off, corr);
        if now.duration_since(c.since) < self.calm_for {
            return None;
        }
        let drift = c.drift();
        self.calm = None;
        if drift.abs() < 10.0 && off.abs() < 1.0 {
            rate.set_through(true);
            self.fix = None;
            self.timed = timed;
        }
        Some(drift)
    }
}

/// A steady stretch's seconds: the offset less the corrections made so far, against time,
/// summed for a least-squares line.
pub(super) struct Calm {
    pub(super) since: Instant,
    pub(super) last: Instant,
    /// The correction in force since `last` (ppm) and the corrections made (ppm·s).
    pub(super) corr: f64,
    pub(super) made: f64,
    pub(super) n: f64,
    pub(super) st: f64,
    pub(super) stt: f64,
    pub(super) su: f64,
    pub(super) stu: f64,
}

impl Calm {
    pub(super) fn new(now: Instant) -> Calm {
        Calm {
            since: now,
            last: now,
            corr: 0.0,
            made: 0.0,
            n: 0.0,
            st: 0.0,
            stt: 0.0,
            su: 0.0,
            stu: 0.0,
        }
    }

    /// This second's offset (ms), and the correction from now on (ppm).
    pub(super) fn add(&mut self, now: Instant, off: f64, corr: f64) {
        self.made += self.corr * now.duration_since(self.last).as_secs_f64();
        (self.last, self.corr) = (now, corr);
        let t = now.duration_since(self.since).as_secs_f64();
        let u = off - self.made / 1000.0;
        self.n += 1.0;
        self.st += t;
        self.stt += t * t;
        self.su += u;
        self.stu += t * u;
    }

    /// The drift (ppm): the line's slope, 1 ms a second being 1000 ppm.
    pub(super) fn drift(&self) -> f64 {
        let d = self.n * self.stt - self.st * self.st;
        if d <= 0.0 {
            return 0.0;
        }
        -(self.n * self.stu - self.st * self.su) / d * 1000.0
    }
}
