//! Network → IDN: the receive rings read behind the media clock, out as the IDN source
//! `HOST (NAME)`, each block stamped with its site time.
//!
//! The engine writes a sample stamped T at T + the flow's latency; what didn't come is
//! silence (cleared ahead, [`crate::sound::keep`]); this reads rx_margin() behind the clock.

use crate::clock::{SiteClock, idn_timestamp, rate};
use idn_aoip_device::engine::MediaClock;
use idn_common::netrings::Rings;
use idn_open_protocol::api::{AudioBlock, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Blocks handed to IDN: about 1 ms (44, 48 or 96 samples).
pub fn block() -> u64 {
    crate::clock::ms()
}
/// How far behind the media clock the network's sound is read (3 ms).
pub fn rx_margin() -> u64 {
    3 * block()
}
/// The most it catches up block by block after a stall (50 ms); past it, on from the
/// limit (a burst of old blocks helps no receiver).
pub fn rx_catch_up() -> u64 {
    50 * block()
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Step {
    Resync,
    Read(u64),
}

/// The reader's step: read up to `limit` (media time) from `pos`.
pub fn plan(limit: u64, pos: Option<u64>) -> Step {
    let Some(p) = pos else { return Step::Resync };
    let ready = limit as i128 - p as i128;
    if ready.unsigned_abs() > u128::from(rx_catch_up()) {
        Step::Resync
    } else {
        Step::Read(ready.max(0) as u64 / block())
    }
}

/// Counters.
#[derive(Default)]
pub struct Counts {
    pub blocks: AtomicU64,
    pub skipped: AtomicU64,
    pub peak: std::sync::atomic::AtomicU32,
}

/// The reading thread, until `stop`: `rx` channels, the device's `latency` (samples).
pub fn spawn(
    rings: &'static Rings,
    rx: usize,
    out: Arc<Output>,
    latency: u64,
    clock: Arc<SiteClock>,
    stop: Arc<AtomicBool>,
    counts: Arc<Counts>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("idn-aoip-in".into())
        .spawn(move || {
            idn_common::rt::audio_this_thread();
            let mut pos: Option<u64> = None;
            while !stop.load(Ordering::Relaxed) {
                // Until its next block is final.
                let wait = match (clock.now(), pos) {
                    (Some(now), Some(p)) => {
                        let due = p + block() + rx_margin();
                        Duration::from_nanos(
                            due.saturating_sub(now) * 1_000_000_000 / u64::from(rate()),
                        )
                        .clamp(Duration::from_micros(50), Duration::from_millis(2))
                    }
                    _ => Duration::from_millis(1),
                };
                std::thread::sleep(wait);
                let Some(now) = clock.now() else { continue };
                let limit = now.saturating_sub(rx_margin());
                let blocks = match plan(limit, pos) {
                    Step::Resync => {
                        if pos.is_some() {
                            counts.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        pos = Some(limit);
                        continue;
                    }
                    Step::Read(n) => n,
                };
                let mut p = pos.unwrap_or(limit);
                for _ in 0..blocks {
                    let channels: Vec<Vec<f32>> = (0..rx)
                        .map(|c| {
                            let mut v = vec![0.0f32; block() as usize];
                            rings.read(c, p as usize, &mut v);
                            v
                        })
                        .collect();
                    let peak = channels
                        .iter()
                        .flatten()
                        .fold(0.0f32, |m, x| m.max(x.abs()));
                    counts.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
                    // Taken `latency` before it was due here.
                    let ts = idn_timestamp(p.saturating_sub(latency));
                    let _ = out.send_audio_at(
                        AudioBlock {
                            sample_rate: rate(),
                            channels,
                        },
                        None,
                        ts,
                    );
                    counts.blocks.fetch_add(1, Ordering::Relaxed);
                    p += block();
                }
                pos = Some(p);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_keeps_up_and_gives_up_on_a_long_stall() {
        let limit = 10_000_000u64;
        assert_eq!(plan(limit, None), Step::Resync);
        assert_eq!(plan(limit, Some(limit - block())), Step::Read(1));
        assert_eq!(plan(limit, Some(limit - block() + 1)), Step::Read(0));
        assert_eq!(plan(limit, Some(limit + 5)), Step::Read(0));
        assert_eq!(plan(limit, Some(limit - 20 * block())), Step::Read(20));
        assert_eq!(plan(limit, Some(limit - rx_catch_up() - 1)), Step::Resync);
        assert_eq!(plan(limit, Some(limit + rx_catch_up() + 1)), Step::Resync);
    }
}
