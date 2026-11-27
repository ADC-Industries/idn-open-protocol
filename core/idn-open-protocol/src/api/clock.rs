//! Output clocks: timestamps now, and timecodes for pictures and for sound sent on its
//! own.

use super::*;

/// Now as a frame's timestamp (100 ns since 1970, UTC), on the clock the outputs stamp
/// with by default: for a program that stamps frames with the moment they were made rather
/// than sent ([`PlanarFrame::timestamp`]).
pub fn timestamp_now() -> i64 {
    now_100ns()
}

/// Now as a frame's timestamp (100 ns since 1970, UTC): site time while the machine's
/// site clock is usable (spec/extensions/idn-clock.md §C.3), so timestamps from every
/// machine of the site compare; else the system's clock.
pub(super) fn now_100ns() -> i64 {
    if let Some(site) = crate::node::clock::site_now() {
        return ((site - crate::ptp::UTC_OFFSET_S as i128 * 1_000_000_000) / 100) as i64;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64 / 100)
}

/// A timecode on an output's clock: the time since `epoch` (100 ns units) at `at`, or
/// `expected` (the previous one plus its duration) when that is within `tolerance` of it.
/// Steady streams so get evenly spaced timecodes despite jitter in when frames are handed
/// over, while a pause in sending shows as a gap instead of shifting everything after it
/// (frame counting lost 0.1–0.35 s per pause, lab L2).
///
/// Beyond `band` of the clock (but within `tolerance`), the timecode moves a quarter of
/// the excess towards it: a stream sent at another rate than its label says (30 fps
/// labelled 29.97) then follows the clock within about `band`, instead of drifting half a
/// frame and jumping back. `band` = `tolerance` keeps exact spacing (audio sample
/// counts).
pub(super) fn clock_tc(
    epoch: Instant,
    at: Instant,
    expected: Option<i64>,
    tolerance: i64,
    band: i64,
) -> i64 {
    let t = (at.saturating_duration_since(epoch).as_nanos() / 100) as i64;
    match expected {
        Some(e) if (t - e).abs() <= tolerance => {
            let off = t - e;
            e + (off - off.clamp(-band, band)) / 4
        }
        _ => t,
    }
}

impl AudioClock {
    /// The timecode right after the samples so far, exact (100 ns units).
    pub(super) fn next(&self) -> i64 {
        self.base + (self.samples as i128 * 10_000_000 / self.rate.max(1) as i128) as i64
    }
}
