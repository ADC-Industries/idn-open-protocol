//! How late a lane's blocks arrive (roadmap R.30): the lateness of each
//! arrival against the earliest of the last two seconds, so a lane can hold just what its
//! path needs, growing at once and coming back down when the path is calm.
//!
//! A block's arrival is set against the sound counted so far: `offset = elapsed −
//! sound`, a constant plus the path's delay and the two clocks' drift. Its lateness is the
//! offset less the smallest offset of the last [`BASE`] (over two seconds, drift of
//! 100 ppm moves it 0.2 ms: the base follows it). The need is the largest lateness of
//! the last seconds, kept per second.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The span the earliest arrival is taken over.
const BASE: Duration = Duration::from_secs(2);
/// How long the lateness is kept, per second.
const KEEP: usize = 120;

/// One lane's arrivals.
#[derive(Debug, Default)]
pub(crate) struct Jitter {
    start: Option<Instant>,
    /// Sound counted since `start`, ms.
    sound_ms: f64,
    /// (arrival, offset) over the last [`BASE`], offsets increasing (a monotonic queue:
    /// its front is the minimum).
    base: VecDeque<(Instant, f64)>,
    /// Per second since `start`: the largest lateness, newest last.
    seconds: VecDeque<(u64, f64)>,
}

impl Jitter {
    /// Starts again (the source paused or restarted, the lane refilled: what came before
    /// says nothing of the path now).
    pub(crate) fn reset(&mut self) {
        *self = Jitter::default();
    }

    /// A block of `block_ms` of sound arrived at `now`; its lateness, ms.
    pub(crate) fn arrive(&mut self, now: Instant, block_ms: f64) -> f64 {
        let start = *self.start.get_or_insert(now);
        self.sound_ms += block_ms;
        let offset = now.duration_since(start).as_secs_f64() * 1000.0 - self.sound_ms;
        while self
            .base
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > BASE)
        {
            self.base.pop_front();
        }
        while self.base.back().is_some_and(|(_, o)| *o >= offset) {
            self.base.pop_back();
        }
        self.base.push_back((now, offset));
        let late = offset - self.base.front().map_or(offset, |(_, o)| *o);
        let sec = now.duration_since(start).as_secs();
        match self.seconds.back_mut() {
            Some((s, m)) if *s == sec => *m = m.max(late),
            _ => {
                self.seconds.push_back((sec, late));
                if self.seconds.len() > KEEP {
                    self.seconds.pop_front();
                }
            }
        }
        late
    }

    /// The largest lateness of the last `secs` seconds, once there are that many to look
    /// at (else `None`: too soon to say).
    pub(crate) fn need_ms(&self, now: Instant, secs: u64) -> Option<f64> {
        let start = self.start?;
        let sec = now.duration_since(start).as_secs();
        if sec < secs {
            return None;
        }
        Some(
            self.seconds
                .iter()
                .filter(|(s, _)| sec - s < secs)
                .map(|(_, m)| *m)
                .fold(0.0, f64::max),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks every 2 ms on time, then one 7 ms late: lateness 0 until it, 7 there; the
    /// need over the window holds the 7 while it is in it.
    #[test]
    fn lateness_against_the_earliest_arrival() {
        let t0 = Instant::now();
        let mut j = Jitter::default();
        let ms = |m: f64| t0 + Duration::from_secs_f64(m / 1000.0);
        let mut worst: f64 = 0.0;
        for k in 0..2000u32 {
            let mut at = f64::from(k) * 2.0;
            if k == 1500 {
                at += 7.0;
            }
            let late = j.arrive(ms(at), 2.0);
            worst = worst.max(late);
            if k < 1500 {
                assert!(late.abs() < 1e-6, "k={k}: {late}");
            }
        }
        assert!((worst - 7.0).abs() < 1e-6);
        let now = ms(3999.0);
        assert!((j.need_ms(now, 3).unwrap() - 7.0).abs() < 1e-6);
        // Too soon, and after reset: nothing to say.
        assert!(j.need_ms(ms(10.0), 30).is_none());
        j.reset();
        assert!(j.need_ms(now, 1).is_none());
    }

    /// A sender's clock 100 ppm fast (its blocks come a little early, steadily): no
    /// lateness to speak of, the base follows it.
    #[test]
    fn drift_is_not_jitter() {
        let t0 = Instant::now();
        let mut j = Jitter::default();
        let mut worst: f64 = 0.0;
        for k in 0..30_000u32 {
            let at = f64::from(k) * 2.0 * (1.0 - 100e-6);
            worst = worst.max(j.arrive(t0 + Duration::from_secs_f64(at / 1000.0), 2.0));
        }
        assert!(worst < 0.05, "{worst}");
        // And 100 ppm slow: lateness creeps over two seconds only (0.2 ms).
        let mut j = Jitter::default();
        let mut worst: f64 = 0.0;
        for k in 0..30_000u32 {
            let at = f64::from(k) * 2.0 * (1.0 + 100e-6);
            worst = worst.max(j.arrive(t0 + Duration::from_secs_f64(at / 1000.0), 2.0));
        }
        assert!(worst < 0.25, "{worst}");
    }
}
