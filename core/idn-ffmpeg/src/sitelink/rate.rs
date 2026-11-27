//! The link's bandwidth: measured, the senders' rates adapted to it, its status.

use super::*;

impl Session {
    pub(super) fn measure(&self) {
        let mut r = self.rates.lock_ok();
        let dt = r.0.elapsed().as_secs_f64();
        if dt >= 2.0 {
            self.adapt();
            let (sent, _) = self.conn.stats();
            let got = self.received.load(Ordering::Relaxed);
            r.3 = (sent - r.1) as f64 * 8.0 / dt / 1e6;
            r.4 = (got - r.2) as f64 * 8.0 / dt / 1e6;
            *r = (Instant::now(), sent, got, r.3, r.4);
        }
    }

    /// Every 2 s: a send that blocked (the link can't carry the rate) or loss over 20 %
    /// brings the video down 25 %; 10 s without either, up 10 % towards the target
    /// (X.12.4). Loss alone below that is left to QUIC's resends: with 5 % random loss
    /// each way (QUIC counting 8–11 %, lost acknowledgements included) the pictures
    /// still came at 29.8 a second, and lowering the rate for it only made them worse.
    pub(super) fn adapt(&self) {
        let (sent, lost) = self.conn.stats();
        let mut a = self.adapt.lock_ok();
        // (Over little traffic, a few lost packets read as a large share: not counted.)
        let loss = if sent > a.0 + 100_000 {
            (lost.saturating_sub(a.1)) as f64 / (sent - a.0) as f64 * 100.0
        } else {
            0.0
        };
        (a.0, a.1, a.2) = (sent, lost, loss);
        let blocked = self.blocked.swap(false, Ordering::Relaxed);
        let scale = self.scale.load(Ordering::Relaxed);
        if blocked || loss > 20.0 {
            let to = (scale * 3 / 4).max(15);
            if to != scale {
                log(&format!(
                    "{}: loss {loss:.1} %{}: video at {to} % of its rate",
                    self.conn.peer_name,
                    if blocked { ", sends blocking" } else { "" }
                ));
            }
            self.scale.store(to, Ordering::Relaxed);
            a.3 = None;
        } else {
            let since = *a.3.get_or_insert_with(Instant::now);
            if scale < 100 && since.elapsed() >= Duration::from_secs(10) {
                self.scale
                    .store((scale * 11 / 10).min(100), Ordering::Relaxed);
                a.3 = Some(Instant::now());
            }
        }
    }

    pub(super) fn status(&self) -> LinkStatus {
        let r = self.rates.lock_ok();
        let offers = self.offers.lock_ok();
        let mut s = LinkStatus {
            peer: format!("{} ({})", self.conn.peer_name, self.conn.peer),
            connected: self.conn.closed().is_none(),
            note: "connected".into(),
            rtt_ms: self.conn.rtt().as_secs_f64() * 1000.0,
            out_mbps: r.3,
            in_mbps: r.4,
            limit_mbps: self.settings.limit_mbps,
            sending: self.subs.lock_ok().keys().cloned().collect(),
            offered: offers.keys().cloned().collect(),
            receiving: offers
                .iter()
                .filter(|(_, o)| o.receiving.load(Ordering::Relaxed))
                .map(|(n, _)| n.clone())
                .collect(),
            loss_pct: self.adapt.lock_ok().2,
            video_pct: self.scale.load(Ordering::Relaxed),
        };
        s.sending.sort();
        s.offered.sort();
        s.receiving.sort();
        s
    }

    /// Bits a second reserved by the sources being sent.
    pub(super) fn reserved(&self, subs: &HashMap<String, Sub>) -> u64 {
        subs.values()
            .map(|s| s.reserved.load(Ordering::Relaxed))
            .sum()
    }

    pub(super) fn limit(&self) -> u64 {
        (self.settings.limit_mbps * 1e6) as u64
    }

    /// The bit rate for a `w`×`h` picture, within what the link has left.
    pub(super) fn fit(&self, w: usize, h: usize, mine: &AtomicU64) -> u64 {
        let want = ((self.settings.mbps_1080p * 1e6) * (w * h) as f64 / (1920.0 * 1080.0))
            .max(MIN_VIDEO as f64) as u64;
        let others = self
            .reserved(&self.subs.lock_ok())
            .saturating_sub(mine.load(Ordering::Relaxed));
        let got = want.min(self.limit().saturating_sub(others)).max(MIN_VIDEO);
        mine.store(got, Ordering::Relaxed);
        got
    }
}
