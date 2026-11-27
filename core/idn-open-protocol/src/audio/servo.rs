//! The clock-drift servo: a sound card's clock and the network's (or this machine's)
//! never agree exactly, typically by tens of parts per million. Left alone, captured
//! sound creeps against the video's timecodes, and played sound overruns or starves its
//! buffer. The servo nudges a resampling ratio by a few parts per million so the sample
//! count tracks the reference, with no clicks and no audible pitch change (1000 ppm is
//! under two cents).

/// A proportional–integral controller on the lead of the produced samples over their
/// target, giving the relative resampling ratio to apply (1.0 = nominal).
#[derive(Debug, Clone)]
pub struct Servo {
    /// ppm of correction per millisecond of lead.
    kp: f64,
    /// ppm per millisecond of lead per second (removes a steady drift).
    ki: f64,
    /// The largest correction, in ppm.
    limit: f64,
    integral: f64,
    /// Smoothed lead, in ms (the measurement jitters by a device buffer).
    lead: Option<f64>,
    /// The time constant of the smoothing, in seconds.
    smooth_s: f64,
    /// The last correction, in ppm.
    ppm: f64,
}

impl Servo {
    /// A servo limited to `limit_ppm`, fed every device block (tens of ms): the gains
    /// close a 10 ms error in about half a minute and learn a steady drift in about a
    /// minute, slowly enough that a device buffer's worth of measurement jitter moves
    /// the ratio by only a few ppm.
    pub fn new(limit_ppm: f64) -> Self {
        Servo::with_gains(50.0, 2.0, 1.0, limit_ppm)
    }

    /// A servo with its own gains (`kp` ppm per ms, `ki` ppm per ms per second) and
    /// smoothing time constant (0 for none: the caller averages).
    pub fn with_gains(kp: f64, ki: f64, smooth_s: f64, limit_ppm: f64) -> Self {
        Servo {
            kp,
            ki,
            limit: limit_ppm.max(0.0),
            integral: 0.0,
            lead: None,
            smooth_s,
            ppm: 0.0,
        }
    }

    /// Feeds a measurement: the produced samples lead their target by `lead_ms`
    /// (positive: too many were produced), `dt_s` after the previous one. Returns the
    /// relative ratio to resample with from now on.
    pub fn update(&mut self, lead_ms: f64, dt_s: f64) -> f64 {
        if self.limit == 0.0 {
            return 1.0;
        }
        let dt = dt_s.clamp(0.0, 2.0);
        let a = if self.smooth_s > 0.0 {
            (dt / self.smooth_s).min(1.0)
        } else {
            1.0
        };
        let lead = match self.lead {
            Some(l) => l + a * (lead_ms - l),
            None => lead_ms,
        };
        self.lead = Some(lead);
        // Anti-windup: while the output sits at its limit, the integral only moves
        // away from it, so a large transient never winds it into an oscillation.
        let raw = -(self.kp * lead + self.ki * self.integral);
        let pushing_further =
            (raw >= self.limit && lead < 0.0) || (raw <= -self.limit && lead > 0.0);
        if !pushing_further {
            let cap = self.limit / self.ki.max(1e-9);
            self.integral = (self.integral + lead * dt).clamp(-cap, cap);
        }
        self.ppm = -(self.kp * lead + self.ki * self.integral).clamp(-self.limit, self.limit);
        1.0 + self.ppm * 1e-6
    }

    /// The current correction, in ppm (positive: producing more samples per second).
    pub fn ppm(&self) -> f64 {
        self.ppm
    }

    /// The smoothed lead, in ms.
    pub fn lead_ms(&self) -> f64 {
        self.lead.unwrap_or(0.0)
    }

    /// Forgets the history (a new stream).
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.lead = None;
        self.ppm = 0.0;
    }

    /// Forgets the smoothed lead but keeps the learned drift (after a refill or a
    /// trim: the clocks haven't changed, the buffer has).
    pub fn resync(&mut self) {
        self.lead = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device running `drift_ppm` off nominal, delivering 20 ms blocks: the servo must
    /// learn the drift and hold the lead near zero without hunting.
    fn run(drift_ppm: f64, secs: f64) -> (Vec<f64>, f64) {
        let mut s = Servo::new(1000.0);
        let (dt, rate) = (0.02, 48000.0);
        let mut lead_samples = 0.0;
        let mut ratio = 1.0;
        let mut leads = Vec::new();
        let steps = (secs / dt) as usize;
        for i in 0..steps {
            // The device produced dt × rate × (1 + drift) samples; we output ratio × that.
            let produced = dt * rate * (1.0 + drift_ppm * 1e-6) * ratio;
            lead_samples += produced - dt * rate;
            // Measurement jitter of up to ±5 ms, as a device buffer gives.
            let jitter = ((i * 7919) % 11) as f64 - 5.0;
            let lead_ms = lead_samples / rate * 1000.0 + jitter;
            ratio = s.update(lead_ms, dt);
            leads.push(lead_samples / rate * 1000.0);
        }
        (leads, s.ppm())
    }

    #[test]
    fn learns_a_steady_drift_without_hunting() {
        for drift in [100.0, -300.0, 25.0] {
            let (leads, ppm) = run(drift, 600.0);
            let peak = leads.iter().fold(0.0f64, |m, l| m.max(l.abs()));
            assert!(peak < 15.0, "{drift} ppm: peak lead {peak:.1} ms");
            // After a few minutes: within a millisecond, and the correction cancels the drift.
            let tail = &leads[leads.len() / 2..];
            let worst = tail.iter().fold(0.0f64, |m, l| m.max(l.abs()));
            assert!(worst < 1.5, "{drift} ppm: late lead {worst:.2} ms");
            assert!(
                (ppm + drift).abs() < 10.0,
                "{drift} ppm: correction {ppm:.1}"
            );
        }
    }

    /// The playback servo: fed once a second with the mean fill (±1.5 ms of noise), a
    /// 100 ppm drift and a 20 ms start-up error must both settle without railing for
    /// long, and the ratio must not swing by more than a few tens of ppm when settled.
    #[test]
    fn once_a_second_settles_without_hunting() {
        let mut s = Servo::with_gains(10.0, 0.08, 0.0, 1000.0);
        let (drift_ppm, rate) = (100.0, 48000.0);
        let mut lead_samples = 20.0 * 48.0; // 20 ms too full at the start
        let mut ratio = 1.0;
        let mut ppms = Vec::new();
        let mut peak = 0.0f64;
        for i in 0..900 {
            let produced = rate * (1.0 + drift_ppm * 1e-6) * ratio;
            lead_samples += produced - rate;
            let noise = ((i * 7919) % 7) as f64 * 0.5 - 1.5;
            ratio = s.update(lead_samples / rate * 1000.0 + noise, 1.0);
            if i >= 300 {
                ppms.push(s.ppm());
                peak = peak.max((lead_samples / rate * 1000.0).abs());
            }
        }
        let mean = ppms.iter().sum::<f64>() / ppms.len() as f64;
        let sd = (ppms.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / ppms.len() as f64).sqrt();
        assert!((mean + drift_ppm).abs() < 10.0, "mean correction {mean:.1}");
        assert!(sd < 15.0, "correction spread {sd:.1} ppm");
        assert!(peak < 6.0, "late lead {peak:.1} ms");
    }

    #[test]
    fn stays_within_its_limit_and_can_be_switched_off() {
        let mut s = Servo::new(200.0);
        let r = s.update(1000.0, 0.02);
        assert!((r - (1.0 - 200e-6)).abs() < 1e-9);
        let mut off = Servo::new(0.0);
        assert_eq!(off.update(1000.0, 0.02), 1.0);
    }
}
