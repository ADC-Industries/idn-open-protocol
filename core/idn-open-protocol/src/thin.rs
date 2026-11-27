//! Pictures above the rate a program needs, left out: a 4K60 camera or file on a 30 fps
//! program was decoded and handed over 60 times a second, every second picture for
//! nothing. Used by inputs ([`InputConfig::picture_rate`]) and by
//! video decoders (`idn-ffmpeg`'s `Ffmpeg::with_picture_rate`).
//!
//! [`InputConfig::picture_rate`]: crate::input::InputConfig::picture_rate

use std::collections::VecDeque;

/// Of a stream at twice the pictures a second wanted or more, the pictures kept: every
/// k-th, k from the pictures' own times (not their arrival: a decoder behind takes them
/// in as fast as it decodes).
#[derive(Debug, Clone)]
pub struct Thin {
    /// Seconds between the pictures wanted.
    every: f64,
    /// The last pictures' times (100 ns), in the order they came (with B-frames, access
    /// units' times go back and forth).
    times: VecDeque<i64>,
    /// Seconds between pictures: their times' spread over their number.
    gap: f64,
    n: u64,
}

impl Thin {
    const WINDOW: usize = 32;

    /// At most `fps` pictures a second; `None` for a rate of zero.
    pub fn new(fps: (u32, u32)) -> Option<Thin> {
        let rate = fps.0 as f64 / fps.1.max(1) as f64;
        (rate > 0.0).then(|| Thin {
            every: 1.0 / rate,
            times: VecDeque::new(),
            gap: 0.0,
            n: 0,
        })
    }

    /// A picture (or access unit) of time `t` (100 ns) came.
    pub fn arrived(&mut self, t: i64) {
        // A jump (a loop, a seek, a new stream) starts again.
        if self
            .times
            .back()
            .is_some_and(|&b| t.abs_diff(b) > 10_000_000)
        {
            self.times.clear();
        }
        self.times.push_back(t);
        if self.times.len() > Self::WINDOW {
            self.times.pop_front();
        }
        self.gap = if self.times.len() == Self::WINDOW {
            let (lo, hi) = self
                .times
                .iter()
                .fold((i64::MAX, i64::MIN), |(l, h), &t| (l.min(t), h.max(t)));
            hi.abs_diff(lo) as f64 / 1e7 / (Self::WINDOW - 1) as f64
        } else {
            0.0
        };
    }

    /// Whether the picture that came last is kept: of 60 a second for 30, every second
    /// one; of 50 for 30, all (thinned they'd be 25).
    pub fn keep(&mut self) -> bool {
        let k = if self.gap > 0.0 {
            ((self.every / self.gap) + 0.15).floor().clamp(1.0, 8.0) as u64
        } else {
            1
        };
        self.n += 1;
        (self.n - 1).is_multiple_of(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(fps_in: f64, fps_out: (u32, u32), n: usize) -> usize {
        let mut t = Thin::new(fps_out).unwrap();
        (0..n)
            .filter(|&i| {
                t.arrived((i as f64 * 1e7 / fps_in) as i64);
                t.keep()
            })
            .count()
    }

    #[test]
    fn only_whole_multiples_are_thinned() {
        // Until a window's worth (31) all are kept; then every second, fourth, all.
        assert_eq!(kept(60.0, (30, 1), 632), 31 + 300);
        assert_eq!(kept(120.0, (30, 1), 632), 31 + 150);
        assert_eq!(kept(50.0, (30, 1), 632), 632);
        assert_eq!(kept(59.94, (30000, 1001), 632), 31 + 300);
        assert_eq!(kept(30.0, (30, 1), 632), 632);
        assert!(Thin::new((0, 1)).is_none());
    }
}
