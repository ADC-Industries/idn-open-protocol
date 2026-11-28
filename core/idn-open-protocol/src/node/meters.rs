//! Levels for the routing matrix's meters (idn-routing.md §A.15): each
//! output this process registered with its Node keeps each channel's peak, and every
//! 100 ms tells the Node, only while the Node says someone watches (`<meters secs/>`,
//! renewed every 2 s while a matrix is open). Nothing is measured or sent otherwise, and
//! nothing changes on the network between transmitters and receivers.

use idn_common::sync::LockOk as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// How often a watched output's levels go to the Node.
pub const EVERY: Duration = Duration::from_millis(100);
/// The quietest level reported, dBFS (a channel at or below it reads this).
pub const FLOOR_DB: i32 = -90;

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// Until when (ms from [`epoch`]) the Node asked for levels; 0: never.
static UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// Whether levels are wanted now.
pub fn on() -> bool {
    let until = UNTIL_MS.load(Ordering::Relaxed);
    until != 0 && (epoch().elapsed().as_millis() as u64) < until
}

/// The Node's `<meters secs/>`: levels wanted for the next `secs` seconds (at most 30).
pub(crate) fn wanted_for(secs: u64) {
    // Off until now: peaks kept from the end of the last watch (minutes ago, maybe) aren't
    // shown as the first levels.
    if !on() {
        for m in registry().lock_ok().iter().filter_map(Weak::upgrade) {
            for p in &m.peaks {
                p.store(0, Ordering::Relaxed);
            }
        }
    }
    let until = epoch().elapsed().as_millis() as u64 + secs.clamp(1, 30) * 1000;
    UNTIL_MS.store(until, Ordering::Relaxed);
    start_reporter();
}

/// One output's peaks since the last report, per channel (as `f32` bits: for values of 0
/// or more their order is the numbers' order, so `fetch_max` keeps the larger).
pub(crate) struct Meter {
    peaks: [AtomicU32; super::routing::MAX_CHANNELS as usize],
    channels: AtomicU32,
    conn: Weak<super::Conn>,
    handle: String,
}

impl Meter {
    pub(crate) fn new(conn: Weak<super::Conn>, handle: String) -> Arc<Meter> {
        let m = Arc::new(Meter {
            peaks: std::array::from_fn(|_| AtomicU32::new(0)),
            channels: AtomicU32::new(0),
            conn,
            handle,
        });
        let mut r = registry().lock_ok();
        // (Outputs closed meanwhile let go of here too: else only while watched.)
        r.retain(|w| w.strong_count() > 0);
        r.push(Arc::downgrade(&m));
        drop(r);
        m
    }

    /// A block's channels (planar): each one's peak kept. Call only when [`on`].
    pub(crate) fn add(&self, channels: &[Vec<f32>]) {
        let n = channels.len().min(self.peaks.len());
        self.channels.store(n as u32, Ordering::Relaxed);
        for (p, c) in self.peaks.iter().zip(&channels[..n]) {
            // (`max` passes over NaN; a value past 1e6 is clamped, not taken as loud.)
            let m = c.iter().fold(0f32, |m, v| m.max(v.abs())).min(1e6);
            p.fetch_max(m.to_bits(), Ordering::Relaxed);
        }
    }

    /// The peaks since the last call, in whole dBFS from [`FLOOR_DB`] (`-12,-90,…`), and
    /// back to silence; `None` before any block.
    fn take(&self) -> Option<String> {
        let n = self.channels.load(Ordering::Relaxed) as usize;
        if n == 0 {
            return None;
        }
        Some(
            self.peaks[..n]
                .iter()
                .map(|p| db(f32::from_bits(p.swap(0, Ordering::Relaxed))).to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
    }
}

/// A peak (0..) as whole dBFS, from [`FLOOR_DB`] up.
pub fn db(peak: f32) -> i32 {
    if peak <= 0.0 || !peak.is_finite() {
        return FLOOR_DB;
    }
    ((20.0 * peak.log10()).round() as i32).clamp(FLOOR_DB, 24)
}

/// `-12,-90,…` back to levels (dBFS); a bad entry reads [`FLOOR_DB`].
pub fn parse(levels: &str) -> Vec<i32> {
    levels
        .split(',')
        .filter(|s| !s.is_empty())
        .take(super::routing::MAX_CHANNELS as usize)
        .map(|s| {
            s.trim()
                .parse::<i32>()
                .map_or(FLOOR_DB, |v| v.clamp(FLOOR_DB, 24))
        })
        .collect()
}

/// A Node's `<levels><tx name db/>…</levels>` as the matrix pages take it: `{name: [dB,
/// …]}`.
pub fn levels_json(levels: &crate::xml::Element) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for t in levels.children_named("tx") {
        if let Some(name) = t.attr("name") {
            m.insert(
                name.to_owned(),
                serde_json::json!(parse(t.attr("db").unwrap_or(""))),
            );
        }
    }
    serde_json::Value::Object(m)
}

fn registry() -> &'static Mutex<Vec<Weak<Meter>>> {
    static R: OnceLock<Mutex<Vec<Weak<Meter>>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

/// How long the reporter sleeps at a time while no levels are wanted; a request wakes it
/// at once.
const IDLE: Duration = Duration::from_secs(1);

/// The thread that reports, started the first time levels are wanted; it sleeps while
/// they aren't, and is woken by each request.
fn start_reporter() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    static REPORTER: OnceLock<std::thread::Thread> = OnceLock::new();
    if STARTED.swap(true, Ordering::Relaxed) {
        if let Some(t) = REPORTER.get() {
            t.unpark();
        }
        return;
    }
    let started = std::thread::Builder::new()
        .name("idn-meters".into())
        .spawn(|| {
            loop {
                if !on() {
                    std::thread::park_timeout(IDLE);
                    continue;
                }
                std::thread::sleep(EVERY);
                if !on() {
                    continue;
                }
                let live: Vec<Arc<Meter>> = {
                    let mut r = registry().lock_ok();
                    r.retain(|w| w.strong_count() > 0);
                    r.iter().filter_map(Weak::upgrade).collect()
                };
                for m in live {
                    let (Some(levels), Some(c)) = (m.take(), m.conn.upgrade()) else {
                        continue;
                    };
                    c.send(
                        crate::xml::Element::new("levels")
                            .with_attr("handle", &m.handle)
                            .with_attr("db", &levels),
                    );
                }
            }
        });
    if let Ok(h) = started {
        let _ = REPORTER.set(h.thread().clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_round_trip() {
        assert_eq!(db(1.0), 0);
        assert_eq!(db(0.5), -6);
        assert_eq!(db(0.0), FLOOR_DB);
        assert_eq!(db(f32::NAN), FLOOR_DB);
        assert_eq!(db(1e-9), FLOOR_DB);
        assert_eq!(parse("-12,-90,x,3"), vec![-12, -90, FLOOR_DB, 3]);
    }

    #[test]
    fn a_meter_keeps_the_peak_until_taken() {
        let m = Meter::new(Weak::new(), "h".into());
        assert_eq!(m.take(), None);
        m.add(&[vec![0.1, -0.5], vec![0.0; 2]]);
        m.add(&[vec![0.25, 0.0], vec![f32::NAN, 0.001]]);
        assert_eq!(m.take().as_deref(), Some("-6,-60"));
        assert_eq!(m.take().as_deref(), Some("-90,-90"));
    }
}
