//! Waits that grow while something keeps failing: one copy of the "double it, up to a
//! most" loops a reconnect, a retry or a query pace had each written out.

use std::time::Duration;

/// The waits before each next try: `initial` first, then each `factor` times the one
/// before, up to `max`; [`Backoff::reset`] after a success starts again at `initial`.
/// With `jitter` (a fraction, 0 to 1), each wait given is up to that much shorter at
/// random, so that many machines that failed together (a Site Manager restarting) don't
/// all try again in the same instant; the growth itself follows the plain waits.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    factor: f64,
    jitter: f64,
    /// The plain wait the next `next` gives; `None` before the first.
    plain: Option<Duration>,
    /// The last wait given (jitter in).
    last: Option<Duration>,
}

impl Backoff {
    /// From `initial`, doubling, up to `max`, no jitter.
    pub fn new(initial: Duration, max: Duration) -> Backoff {
        Backoff {
            initial,
            max: max.max(initial),
            factor: 2.0,
            jitter: 0.0,
            plain: None,
            last: None,
        }
    }

    /// Each wait `factor` times the one before (at least 1).
    pub fn factor(mut self, factor: f64) -> Backoff {
        self.factor = if factor.is_finite() {
            factor.max(1.0)
        } else {
            2.0
        };
        self
    }

    /// Each wait up to `fraction` (0 to 1) shorter, at random.
    pub fn jitter(mut self, fraction: f64) -> Backoff {
        self.jitter = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self
    }

    /// The wait before the next try; the one after grows.
    #[allow(clippy::should_implement_trait)] // A wait every time, never the end: not an Iterator.
    pub fn next(&mut self) -> Duration {
        let plain = self.plain.unwrap_or(self.initial);
        self.plain = Some(plain.mul_f64(self.factor).min(self.max));
        let wait = if self.jitter > 0.0 {
            plain.mul_f64(1.0 - self.jitter * unit_random())
        } else {
            plain
        };
        self.last = Some(wait);
        wait
    }

    /// The last wait [`next`](Backoff::next) gave; `None` since the start or a reset.
    pub fn current(&self) -> Option<Duration> {
        self.last
    }

    /// After a success: the next wait is `initial` again.
    pub fn reset(&mut self) {
        self.plain = None;
        self.last = None;
    }
}

/// A number in [0, 1), random enough to spread retries (not for keys): the standard
/// library's per-process random hasher keys, fresh each call.
fn unit_random() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let h = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (h >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn doubles_up_to_the_most_then_starts_again() {
        let mut b = Backoff::new(MS(500), MS(3000));
        assert_eq!(b.current(), None);
        let got: Vec<_> = (0..6).map(|_| b.next()).collect();
        assert_eq!(
            got,
            [MS(500), MS(1000), MS(2000), MS(3000), MS(3000), MS(3000)]
        );
        assert_eq!(b.current(), Some(MS(3000)));
        b.reset();
        assert_eq!(b.current(), None);
        assert_eq!(b.next(), MS(500));
        let mut b = Backoff::new(MS(100), MS(1000)).factor(3.0);
        let got: Vec<_> = (0..4).map(|_| b.next()).collect();
        assert_eq!(got, [MS(100), MS(300), MS(900), MS(1000)]);
    }

    #[test]
    fn jitter_only_shortens() {
        let mut b = Backoff::new(MS(1000), MS(8000)).jitter(0.5);
        for plain in [1000, 2000, 4000, 8000, 8000] {
            let w = b.next();
            assert!(w <= MS(plain) && w >= MS(plain / 2), "{w:?} of {plain}");
        }
    }
}
