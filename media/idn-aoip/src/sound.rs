//! The device's sound: the shared rings (R.33, `idn_common::netrings`), so a desk on this
//! machine works in them directly. The engine writes what it receives into the receive
//! rings at its play-out time and reads what it sends from the transmit rings; nothing is
//! copied in between.
//!
//! Unwritten time must be silence, not a ring-old sample: every millisecond the receive
//! rings are cleared from half a ring ahead of the clock (what is received is written
//! a few milliseconds ahead, so the clearing never meets it).

use crate::clock::SiteClock;
use idn_aoip_device::engine::{MediaClock, Sound};
use idn_common::netrings::{LEN, Rings};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The rings as the engine's sound, with each channel's loudest sample since the last
/// heartbeat (its meters, idn-aoip-device.md §4.8).
pub struct RingSound {
    pub rings: &'static Rings,
    pub tx_peaks: Vec<std::sync::atomic::AtomicU32>,
    pub rx_peaks: Vec<std::sync::atomic::AtomicU32>,
}

impl RingSound {
    pub fn new(rings: &'static Rings, rx: usize, tx: usize) -> RingSound {
        let v = |n| {
            (0..n)
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect()
        };
        RingSound {
            rings,
            tx_peaks: v(tx),
            rx_peaks: v(rx),
        }
    }

    /// The peaks since the last call as the heartbeat's level bytes: TX, then RX.
    pub fn take_levels(&self) -> (Vec<u8>, Vec<u8>) {
        let take = |v: &[std::sync::atomic::AtomicU32]| {
            v.iter()
                .map(|p| {
                    idn_aoip_device::settings::level_byte(f32::from_bits(
                        p.swap(0, Ordering::Relaxed),
                    ))
                })
                .collect()
        };
        (take(&self.tx_peaks), take(&self.rx_peaks))
    }
}

/// A raw peak (a left-justified magnitude) into `slot` as a float's bits.
fn peak_raw(slot: Option<&std::sync::atomic::AtomicU32>, most: u32) {
    if let Some(p) = slot {
        let v = (f64::from(most) / 2_147_483_648.0) as f32;
        p.fetch_max(v.to_bits(), Ordering::Relaxed);
    }
}

fn peak(slot: Option<&std::sync::atomic::AtomicU32>, samples: &[f32]) {
    if let Some(p) = slot {
        let m = samples.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        p.fetch_max(m.to_bits(), Ordering::Relaxed);
    }
}

impl Sound for RingSound {
    fn read(&self, tx_channel: usize, at: u64, out: &mut [f32]) {
        match tx_channel
            .checked_sub(1)
            .and_then(|c| self.rings.ring(false, c))
        {
            Some(r) => {
                for (i, o) in out.iter_mut().enumerate() {
                    *o = r[(at as usize).wrapping_add(i) & (LEN - 1)].load(Ordering::Relaxed)
                        as f32
                        / 2_147_483_648.0;
                }
            }
            None => out.fill(0.0),
        }
        peak(
            tx_channel.checked_sub(1).and_then(|c| self.tx_peaks.get(c)),
            out,
        );
    }

    fn read_raw(&self, tx_channel: usize, at: u64, out: &mut [i32]) {
        match tx_channel
            .checked_sub(1)
            .and_then(|c| self.rings.ring(false, c))
        {
            Some(r) => {
                let mut most = 0u32;
                for (i, o) in out.iter_mut().enumerate() {
                    *o = r[(at as usize).wrapping_add(i) & (LEN - 1)].load(Ordering::Relaxed);
                    most = most.max(o.unsigned_abs());
                }
                peak_raw(
                    tx_channel.checked_sub(1).and_then(|c| self.tx_peaks.get(c)),
                    most,
                );
            }
            None => out.fill(0),
        }
    }

    fn write_raw(&self, rx_channel: usize, at: u64, samples: &[i32]) {
        if let Some(r) = rx_channel
            .checked_sub(1)
            .and_then(|c| self.rings.ring(true, c))
        {
            let mut most = 0u32;
            for (i, &x) in samples.iter().enumerate() {
                r[(at as usize).wrapping_add(i) & (LEN - 1)].store(x, Ordering::Relaxed);
                most = most.max(x.unsigned_abs());
            }
            peak_raw(
                rx_channel.checked_sub(1).and_then(|c| self.rx_peaks.get(c)),
                most,
            );
        }
    }

    fn write_slot(
        &self,
        rx_channel: usize,
        at: u64,
        audio: &[u8],
        channels: usize,
        bytes: usize,
        slot: usize,
    ) {
        let stride = channels * bytes;
        let Some(r) = rx_channel
            .checked_sub(1)
            .and_then(|c| self.rings.ring(true, c))
        else {
            return;
        };
        if stride == 0 || slot >= channels || !(2..=4).contains(&bytes) {
            return;
        }
        let mut most = 0u32;
        let o = slot * bytes;
        let at = at as usize;
        // Each width with fixed-size reads (a variable-length copy a sample cost more
        // than the buffer it saved).
        let mut put = |i: usize, x: i32| {
            r[at.wrapping_add(i) & (LEN - 1)].store(x, Ordering::Relaxed);
            most = most.max(x.unsigned_abs());
        };
        match bytes {
            3 => {
                for (i, f) in audio.chunks_exact(stride).enumerate() {
                    put(i, i32::from_be_bytes([f[o], f[o + 1], f[o + 2], 0]));
                }
            }
            2 => {
                for (i, f) in audio.chunks_exact(stride).enumerate() {
                    put(i, i32::from_be_bytes([f[o], f[o + 1], 0, 0]));
                }
            }
            _ => {
                for (i, f) in audio.chunks_exact(stride).enumerate() {
                    put(i, i32::from_be_bytes([f[o], f[o + 1], f[o + 2], f[o + 3]]));
                }
            }
        }
        peak_raw(
            rx_channel.checked_sub(1).and_then(|c| self.rx_peaks.get(c)),
            most,
        );
    }

    fn write(&self, rx_channel: usize, at: u64, samples: &[f32]) {
        peak(
            rx_channel.checked_sub(1).and_then(|c| self.rx_peaks.get(c)),
            samples,
        );
        if let Some(r) = rx_channel
            .checked_sub(1)
            .and_then(|c| self.rings.ring(true, c))
        {
            for (i, &x) in samples.iter().enumerate() {
                r[(at as usize).wrapping_add(i) & (LEN - 1)]
                    .store(idn_common::netrings::to_sample(x), Ordering::Relaxed);
            }
        }
    }
}

/// Clears the receive rings ahead of the clock and beats the device's beat (a desk
/// watches it), every millisecond, until `stop`.
pub fn keep(
    rings: &'static Rings,
    rx: usize,
    clock: Arc<SiteClock>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("idn-aoip-rings".into())
        .spawn(move || {
            let mut cleared: Option<usize> = None;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                rings.beat_device();
                let Some(now) = clock.now() else { continue };
                let ahead = (now as usize).wrapping_add(LEN / 2);
                let from = match cleared {
                    Some(c) if ahead.wrapping_sub(c) < LEN / 4 => c,
                    // The first time: the last block.
                    None => ahead.wrapping_sub(crate::clock::ms() as usize),
                    // A step back, or forward more than a quarter ring (a stall, a clock
                    // step): everything the reader can still reach, from 100 ms behind
                    // the new time (bug run I2-6: a step back replayed the last second;
                    // only one block was cleared). A packet written in the last
                    // millisecond may go with it.
                    Some(_) => (now as usize).wrapping_sub(crate::clock::rate() as usize / 10),
                };
                let n = ahead.wrapping_sub(from);
                for c in 0..rx {
                    if let Some(r) = rings.ring(true, c) {
                        for i in 0..n {
                            r[from.wrapping_add(i) & (LEN - 1)].store(0, Ordering::Relaxed);
                        }
                    }
                }
                cleared = Some(ahead);
            }
        })
        .map(|_| ())
}
