//! An adaptive stream's rate control (G4).

use super::*;

impl Adapt {
    const HIGH: Duration = Duration::from_millis(800);
    const LOW: Duration = Duration::from_millis(250);
    const HOLD: Duration = Duration::from_secs(2);
    const SETTLE: Duration = Duration::from_secs(4);
    const UP_AFTER: Duration = Duration::from_secs(60);

    pub(super) fn new(target: u64, floor: u64, t: Instant) -> Adapt {
        Adapt {
            target,
            floor: floor.min(target),
            now: target,
            high_since: None,
            good_since: t,
            changed: t,
        }
    }

    /// A packet that waited `waited` before being written, at `t`: a new rate, if any.
    pub(super) fn sample(&mut self, waited: Duration, t: Instant) -> Option<u64> {
        if waited > Self::HIGH {
            self.good_since = t;
            let since = *self.high_since.get_or_insert(t);
            if t - since >= Self::HOLD && t - self.changed >= Self::SETTLE && self.now > self.floor
            {
                self.high_since = None;
                return self.set(self.now * 3 / 4, t);
            }
            return None;
        }
        self.high_since = None;
        if waited > Self::LOW {
            self.good_since = t;
        } else if self.now < self.target
            && t - self.good_since >= Self::UP_AFTER
            && t - self.changed >= Self::UP_AFTER
        {
            self.good_since = t;
            return self.set(self.now * 5 / 4, t);
        }
        None
    }

    /// Packets were dropped (the queue overflowed): half the rate at once.
    pub(super) fn overflow(&mut self, t: Instant) -> Option<u64> {
        self.good_since = t;
        if self.now > self.floor && t - self.changed >= Duration::from_secs(1) {
            return self.set(self.now / 2, t);
        }
        None
    }

    pub(super) fn set(&mut self, r: u64, t: Instant) -> Option<u64> {
        let r = r.clamp(self.floor, self.target);
        self.changed = t;
        (r != self.now).then(|| {
            self.now = r;
            r
        })
    }
}
