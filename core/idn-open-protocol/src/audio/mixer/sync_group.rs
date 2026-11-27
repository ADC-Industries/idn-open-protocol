//! A mixer's adapting lanes held in line (R.30, idn-routing.md §A.14).

use super::*;

/// The adapting lanes of one mixer (R.30): each says what its path needs, and every one
/// holds the largest, so the sources mixed together stay in line (two microphones on one
/// voice a few ms apart would comb). A lane that stops saying is forgotten after
/// [`SYNC_FORGET`].
#[derive(Default)]
pub(crate) struct SyncGroup {
    wants: Mutex<Vec<(usize, f64, Instant)>>,
    /// The site's floor for this mixer's receiver (its sync group across machines,
    /// idn-routing.md §A.14), ms as f64 bits; 0: none.
    pub(super) floor: AtomicU64,
}

pub(super) const SYNC_FORGET: Duration = Duration::from_secs(5);

impl SyncGroup {
    /// Lane `id` needs `want_ms`: the group's target (the largest fresh need, at least
    /// the site's floor).
    pub(crate) fn publish(&self, id: usize, want_ms: f64, now: Instant) -> f64 {
        let mut w = self.wants.lock_ok();
        w.retain(|(i, _, at)| *i != id && now.duration_since(*at) < SYNC_FORGET);
        w.push((id, want_ms, now));
        let floor = f64::from_bits(self.floor.load(Ordering::Relaxed));
        w.iter().map(|(_, m, _)| *m).fold(want_ms.max(floor), f64::max)
    }

    /// The largest fresh need of its lanes (the floor left out): what the receiver tells
    /// the site. `None`: no adapting lane.
    pub(crate) fn need(&self, now: Instant) -> Option<f64> {
        let w = self.wants.lock_ok();
        w.iter()
            .filter(|(_, _, at)| now.duration_since(*at) < SYNC_FORGET)
            .map(|(_, m, _)| *m)
            .reduce(f64::max)
    }
}
