//! Media time from the Node's site clock (idn-aoip-device.md §8.1: the device needs no
//! PTP of its own; media time = site time × rate). The Node's mapping is followed every
//! 100 ms; the clock unusable for 2 s is "lost": nothing is sent and the heartbeat says
//! no clock until it is back.

use idn_aoip_device::engine::MediaClock;
use idn_open_protocol::ptp::Mapping;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

static RATE_HZ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(48_000);

/// The device's sample rate (44.1, 48 or 96 kHz, from IDN: `--rate`, `idn-node.md`
/// §N.40, idn-aoip-device.md §1.5): set once at the start.
pub fn rate() -> u32 {
    RATE_HZ.load(Ordering::Relaxed)
}

/// Sets the device's rate (one of IDN's; others are left as they were).
pub fn set_rate(hz: u32) {
    if idn_open_protocol::node::rate::valid(hz) {
        RATE_HZ.store(hz, Ordering::Relaxed);
    }
}

/// Samples of a millisecond at the device's rate (44, 48 or 96: blocks of about 1 ms).
pub fn ms() -> u64 {
    u64::from(rate()) / 1000
}

/// How long the site clock may be unusable before both ways go silent.
pub const LOST_AFTER: Duration = Duration::from_secs(2);

/// The site clock as this program follows it.
#[derive(Default)]
pub struct SiteClock {
    mapping: RwLock<Option<Mapping>>,
    pub lost: AtomicBool,
    /// The leader's PTP clock identity (for the clock status a controller asks for).
    pub leader: RwLock<Option<[u8; 8]>>,
}

impl SiteClock {
    /// Site time now (TAI, ns), `None` without a usable clock.
    pub fn site_ns(&self) -> Option<i128> {
        if self.lost.load(Ordering::Relaxed) {
            return None;
        }
        let m = (*self.mapping.read().ok()?)?;
        Some(m.site(idn_open_protocol::ptp::mono_ns()))
    }

    /// Follows the Node's clock until `stop` (a thread).
    pub fn follow(self: &Arc<SiteClock>, stop: Arc<AtomicBool>) -> std::io::Result<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("idn-aoip-clock".into())
            .spawn(move || {
                let mut last_usable = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(m) = idn_open_protocol::node::clock::status()
                        .filter(|c| c.usable())
                        .and_then(|c| c.mapping)
                    {
                        last_usable = Instant::now();
                        if let Ok(mut w) = me.mapping.write() {
                            *w = Some(m);
                        }
                        if let Ok(mut l) = me.leader.write() {
                            *l = idn_open_protocol::node::clock::ptp_grandmaster()
                                .and_then(|gm| clock_identity(&gm));
                        }
                        if me.lost.swap(false, Ordering::Relaxed) {
                            println!("site clock: back; sound on again");
                        }
                    } else if last_usable.elapsed() >= LOST_AFTER
                        && !me.lost.swap(true, Ordering::Relaxed)
                    {
                        eprintln!(
                            "idn-aoip: the Node's site clock isn't usable (the Node stopped, or it has no leader): silent both ways until it is back"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
            .map(|_| ())
    }

    /// Waits up to 30 s for a usable clock (at start).
    pub fn wait_usable(&self) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(m) = idn_open_protocol::node::clock::status()
                .filter(|c| c.usable())
                .and_then(|c| c.mapping)
            {
                if let Ok(mut w) = self.mapping.write() {
                    *w = Some(m);
                }
                return Ok(());
            }
            if Instant::now() > until {
                return Err(
                    "the Node's site clock isn't usable (not locked to a leader within 30 s)"
                        .into(),
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl MediaClock for SiteClock {
    fn now(&self) -> Option<u64> {
        self.site_ns().map(media_at)
    }
}

/// Media time (samples) at site time `site_ns`.
pub fn media_at(site_ns: i128) -> u64 {
    ((site_ns.max(0) as u128) * u128::from(rate()) / 1_000_000_000) as u64
}

/// An IDN timestamp (100 ns since 1970, UTC) for media time `t`.
pub fn idn_timestamp(t: u64) -> i64 {
    let site = (u128::from(t) * 1_000_000_000 / u128::from(rate())) as i64;
    (site - idn_open_protocol::ptp::UTC_OFFSET_S as i64 * 1_000_000_000) / 100
}

/// A PTP clock identity ("aa-bb-…" or hex) as its 8 bytes.
pub fn clock_identity(text: &str) -> Option<[u8; 8]> {
    let hex: String = text.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 16 {
        return None;
    }
    let mut id = [0u8; 8];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_time_and_timestamps() {
        let now: i128 = 1_700_000_000_123_456_789;
        let t = media_at(now);
        assert_eq!(t, (now as u128 * 48_000 / 1_000_000_000) as u64);
        // The timestamp is the media time's site time as UTC.
        let back =
            idn_timestamp(t) * 100 + idn_open_protocol::ptp::UTC_OFFSET_S as i64 * 1_000_000_000;
        assert!((now as i64 - back).abs() < 20_834);
        assert_eq!(
            clock_identity("00-1d-c1-ff-fe-0a-0b-0c"),
            Some([0, 0x1d, 0xc1, 0xff, 0xfe, 0x0a, 0x0b, 0x0c])
        );
        assert_eq!(clock_identity("nope"), None);
    }
}
