//! Protecting live video (§E.12.5): transfers slow down while live sessions report
//! trouble; a shared token bucket caps them, and LEDBAT backs them off as queueing grows.

use super::*;

/// When live media sessions last reported trouble (losses, skipped frames), in
/// milliseconds since the Unix epoch: any session, and by the peer device's fingerprint
/// where it is known (link sessions with enrolled devices).
pub(super) struct Trouble {
    pub(super) any: u64,
    pub(super) by_device: BTreeMap<String, u64>,
}

pub(super) static TROUBLE: Mutex<Trouble> = Mutex::new(Trouble {
    any: 0,
    by_device: BTreeMap::new(),
});

/// How long transfers stay slowed after the last trouble report.
pub(super) const TROUBLE_HOLD: Duration = Duration::from_secs(5);

/// A live media session lost or skipped frames, with the device `peer` (its
/// fingerprint) if known: transfers slow down, as [`TroubleResponse`] says (§E.12.5).
pub(crate) fn video_trouble(peer: Option<&str>) {
    let now = idn_common::epoch::millis();
    let mut t = TROUBLE.lock_ok();
    t.any = now;
    if let Some(fp) = peer {
        t.by_device.insert(fp.to_owned(), now);
    }
    let hold = TROUBLE_HOLD.as_millis() as u64;
    t.by_device.retain(|_, at| now.saturating_sub(*at) < hold);
}

/// Whether trouble was reported within [`TROUBLE_HOLD`]: with device `peer`, or with
/// any session for `None`.
pub(super) fn trouble_with(peer: Option<&str>) -> bool {
    let t = TROUBLE.lock_ok();
    let at = match peer {
        None => t.any,
        Some(fp) => t.by_device.get(fp).copied().unwrap_or(0),
    };
    idn_common::epoch::millis().saturating_sub(at) < TROUBLE_HOLD.as_millis() as u64
}

/// A token bucket shared by all transfers: the application's cap.
#[derive(Default)]
pub(super) struct TokenBucket {
    pub(super) state: tokio::sync::Mutex<Option<(f64, Instant)>>, // (tokens, last refill)
}

impl TokenBucket {
    pub(super) async fn take(&self, n: u64, rate: f64) {
        loop {
            let wait = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let (tokens, last) = st.get_or_insert((0.0, now));
                let dt = now.duration_since(*last).as_secs_f64();
                *last = now;
                // At most a second's worth saved, but always room for one take: a cap
                // below one chunk per second used to wait forever.
                *tokens = (*tokens + dt * rate).min(rate.max(n as f64));
                if *tokens >= n as f64 {
                    *tokens -= n as f64;
                    return;
                }
                Duration::from_secs_f64((n as f64 - *tokens) / rate)
            };
            tokio::time::sleep(wait.min(Duration::from_millis(200))).await;
        }
    }
}

/// One transfer's own pacing: its recent speed, and while slowed, half the speed it had
/// when the trouble began (at least 256 kB/s).
pub(super) struct Pace {
    pub(super) started: Instant,
    pub(super) sent: u64,
    /// Bytes per second while slowed, and that slowdown's own bucket.
    pub(super) slowed: Option<(f64, TokenBucket)>,
}

impl Default for Pace {
    fn default() -> Self {
        Pace {
            started: Instant::now(),
            sent: 0,
            slowed: None,
        }
    }
}

impl Pace {
    pub(super) async fn next(&mut self, n: u64, slow: bool) {
        if !slow {
            self.slowed = None;
        } else if self.slowed.is_none() {
            let speed = self.sent as f64 / self.started.elapsed().as_secs_f64().max(0.1);
            self.slowed = Some(((speed / 2.0).max(256.0 * 1024.0), TokenBucket::default()));
        }
        if let Some((rate, bucket)) = &self.slowed {
            bucket.take(n, *rate).await;
        }
        // The speed over the last few seconds, for the next slowdown.
        if self.started.elapsed() > Duration::from_secs(3) && self.slowed.is_none() {
            (self.started, self.sent) = (Instant::now(), 0);
        }
        self.sent += n;
    }
}

/// LEDBAT (RFC 6817), a background congestion controller: it fills an idle path but
/// backs off as soon as queueing delay grows, leaving the capacity to interactive and
/// live traffic. The queueing delay is taken as the smoothed RTT above the minimum RTT.
#[derive(Debug, Clone)]
pub(super) struct Ledbat {
    pub(super) window: u64,
    pub(super) mtu: u64,
    /// Slow start until the first sign of queueing or loss.
    pub(super) slow_start: bool,
    pub(super) recovery_start: Instant,
}

/// Extra queueing delay LEDBAT aims at (RFC 6817 allows up to 100 ms; a LAN needs less).
pub(super) const LEDBAT_TARGET: Duration = Duration::from_millis(25);

impl Controller for Ledbat {
    fn on_ack(
        &mut self,
        _now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &quinn_proto::RttEstimator,
    ) {
        if app_limited || sent <= self.recovery_start {
            return;
        }
        let queueing = rtt.get().saturating_sub(rtt.min());
        let target = LEDBAT_TARGET.as_secs_f64();
        if self.slow_start {
            if queueing.as_secs_f64() < target / 2.0 {
                self.window += bytes;
                return;
            }
            self.slow_start = false;
        }
        // RFC 6817 §2.4.2: cwnd += GAIN * off_target * bytes * MSS / cwnd.
        let off_target = (target - queueing.as_secs_f64()) / target;
        let delta = off_target * bytes as f64 * self.mtu as f64 / self.window as f64;
        let w = (self.window as f64 + delta.min(bytes as f64)).max(2.0 * self.mtu as f64);
        self.window = w as u64;
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        _persistent: bool,
        _lost_bytes: u64,
    ) {
        if sent <= self.recovery_start {
            return;
        }
        self.recovery_start = now;
        self.slow_start = false;
        self.window = (self.window / 2).max(2 * self.mtu);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu as u64;
        self.window = self.window.max(2 * self.mtu);
    }

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        10 * self.mtu
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

pub(super) struct LedbatConfig;

impl ControllerFactory for LedbatConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        let mtu = current_mtu as u64;
        Box::new(Ledbat {
            window: 10 * mtu,
            mtu,
            slow_start: true,
            recovery_start: now,
        })
    }
}
