//! Per-receiver picture state: changed regions sent (§E.15), `adapt/1` quality (§E.16.1)
//! and rate control.

use super::*;

impl Regions {
    /// After a frame: the receiver has its picture if the video was sent.
    pub(super) fn commit(&mut self, video_sent: bool) {
        if let Some(p) = self.pending.take().filter(|_| video_sent) {
            if p.whole {
                self.last_whole = Some(Instant::now());
                self.refresh.store(false, Ordering::Relaxed);
            }
            self.sent = Some(p);
        }
    }
}

impl Adapt {
    pub(super) fn new(quality: u8, floor: u8) -> Self {
        let max = quality as f64;
        Adapt {
            q: max,
            max,
            floor: (floor as f64).min(max),
            checked: Instant::now(),
            clean_since: Instant::now(),
            seen: (0, 0, 0),
        }
    }

    pub(super) fn quality(&self) -> u8 {
        self.q.round() as u8
    }

    /// Once a second: trouble since the last look lowers quality, a clean spell raises it.
    pub(super) fn update(&mut self, client: &ClientShared) {
        if self.checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.checked = Instant::now();
        let (lost, late) = client
            .report
            .lock_ok()
            .as_ref()
            .map_or((0, 0), |r| (r.v_lost, r.v_late));
        self.adjust(client.skipped.load(Ordering::Relaxed), lost, late);
    }

    pub(super) fn adjust(&mut self, skipped: u64, lost: u64, late: u64) {
        let now = (skipped, lost, late);
        let trouble = now.0 > self.seen.0 || now.1 > self.seen.1 || now.2 > self.seen.2;
        self.seen = now;
        if trouble {
            self.q = (self.q - 4.0).max(self.floor);
            self.clean_since = Instant::now();
        } else if self.clean_since.elapsed() >= Duration::from_secs(3) {
            self.q = (self.q + 1.0).min(self.max);
        }
    }
}

impl RateControl {
    const MIN_Q: f64 = 50.0;

    /// Adjusts after a frame of `bytes` against a per-frame budget. Bits fall roughly
    /// exponentially as Q drops, so the step is proportional to the log of the ratio.
    pub(super) fn update(&mut self, bytes: usize, budget: f64) {
        if budget <= 0.0 || bytes == 0 {
            return;
        }
        let ratio = bytes as f64 / budget;
        if !ratio.is_finite() {
            return;
        }
        // The floor never exceeds the configured quality (quality may be set below 50).
        self.q = (self.q - 6.0 * ratio.ln()).clamp(Self::MIN_Q.min(self.max), self.max);
    }

    pub(super) fn quality(&self) -> u8 {
        self.q.round() as u8
    }
}
