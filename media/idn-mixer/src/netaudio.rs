//! A network-audio device on this machine as the desk's sound card (roadmap R.33): the
//! desk works in the device's rings directly (`idn_common::netrings`, made by `idn-aoip`),
//! one block a tick of the site clock. The network's channels arrive on the desk's
//! channels with no lane between (nothing to buffer or resample: the device and the desk
//! count the same clock), and the desk's outputs go out a lead ahead of the clock.
//!
//! What it costs in time, mic to speaker: the device's receive latency, a millisecond and
//! a half for what it closes each millisecond, one block of the desk, the lead (the
//! network's largest packet and 2 ms for a late tick), and the receiving device's latency.
//!
//! The desk has the transmit channels its outputs go on (from the device's first, as many
//! as it has outputs, or [`NetAudioConfig::outputs`]), and says so in the rings' head
//! before its first block; the device's routes keep playing on the others. (A device too
//! old to share leaves the desk every channel, so the desk keeps them all silent but its
//! own, as it always did.)
//!
//! The desk only maps memory and reads the Node's clock: it links nothing of the device's.

use idn_common::netrings::{Claim, LEN, Place, Rings, Tick, media_at};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Samples of `ms` milliseconds at the desk's rate (§N.40).
fn ms(n: usize) -> usize {
    crate::desk::rate_hz() as usize * n / 1000
}
/// How far behind the clock the block read ends: the device closes its rings up to the
/// clock every millisecond; 2 ms covers that and a tick's lateness.
fn margin() -> usize {
    ms(2)
}
/// Room for a late tick ahead of the largest packet: 2 ms.
fn late() -> usize {
    ms(2)
}
/// The smallest packet the lead is counted for (what such a device advertises).
const PACKET: usize = 32;
/// What a late block adds to the lead (1 ms), and the most it is added to (8 ms).
fn step() -> usize {
    ms(1)
}
fn extra_most() -> usize {
    8 * step()
}

/// Whether a block written at `write_at` with the media clock at `now` came too late:
/// less than a packet of it still ahead of the clock.
fn wrote_late(write_at: usize, now: usize, packet: usize) -> bool {
    (write_at.wrapping_sub(now) as isize) < packet as isize
}

/// Which device, and where its channels land on the desk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetAudioConfig {
    /// The device's name on its network (`idn-aoip --device-name`).
    pub device: String,
    /// The desk channel its first receive channel arrives on (from 0).
    pub first: usize,
    /// How many of the desk's outputs go to the device, on its transmit channels from the
    /// first; `None`: as many as both have. The device's other transmit channels stay its
    /// IDN routes' (R.33).
    pub outputs: Option<usize>,
}

/// The desk's side of the rings, used on its clock thread: nothing here blocks or
/// allocates once the rings are open (opening is tried once a second, off the block's
/// path in time: a failed look takes microseconds).
pub struct NetAudio {
    cfg: NetAudioConfig,
    block: usize,
    /// The desk's outputs.
    outputs: usize,
    rings: Option<Rings>,
    /// The transmit channels the desk writes, claimed in the rings' head: the only ones
    /// it keeps silent ahead of the clock (the device's routes play on the others).
    claim: Claim,
    place: Place,
    looked: Option<Instant>,
    /// The device's beats, and when they last moved.
    beats: (u64, Instant),
    resyncs: u64,
    /// Ticks so late that the device sent a little silence.
    late: u64,
    /// Blocks that landed too late though their tick was on time ([`NetAudio::write`]),
    /// and what they have added to the lead, samples.
    late_writes: u64,
    extra: usize,
    /// When the lead last grew or came back down: after [`EXTRA_DECAY`] without a late
    /// block it comes down a step (bug run I4-7: one slow block kept it up for good).
    extra_since: Instant,
    state: &'static str,
    /// The rings' descriptor last refused for its rate, and the desk's rate then.
    refused: Option<(String, u32)>,
}

/// How long without a late block before the added lead comes down a step (as a lane's
/// margin, 600 s).
const EXTRA_DECAY: std::time::Duration = std::time::Duration::from_secs(600);

impl NetAudio {
    /// For a desk of `block` samples a block and `outputs` outputs.
    pub fn new(cfg: NetAudioConfig, block: usize, outputs: usize) -> NetAudio {
        NetAudio {
            cfg,
            block,
            outputs,
            rings: None,
            claim: Claim::default(),
            place: Place::new(block, margin(), PACKET + late(), PACKET),
            looked: None,
            beats: (0, Instant::now()),
            resyncs: 0,
            late: 0,
            late_writes: 0,
            extra: 0,
            extra_since: Instant::now(),
            state: "waiting for the device",
            refused: None,
        }
    }

    /// Where the device says its rings are.
    fn rings_file(&self) -> PathBuf {
        idn_common::paths::program_dir("aoip")
            .join(&self.cfg.device)
            .join("rings")
    }

    /// This block's place in the rings, at site time `site_ns` (`None`: no usable site
    /// clock): `None` while the device isn't there.
    pub fn tick(&mut self, site_ns: Option<i128>, rate: u32) -> Option<Tick> {
        let now = Instant::now();
        // The device gone (stopped, or started again with new rings): its beat still for
        // half a second. Its rings are let go, and looked for again.
        if let Some(r) = &self.rings {
            let b = r.device_beats();
            if b != self.beats.0 {
                self.beats = (b, now);
            } else if now.duration_since(self.beats.1) > Duration::from_millis(500) {
                self.rings = None;
                self.state = "waiting for the device";
            }
        }
        if self.rings.is_none() {
            // Looked for five times a second (once a second added up to a second to the
            // device's return: sound back 3.15 s after its start, lab 515(e)); a small
            // file read while there is none.
            if self
                .looked
                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(200))
            {
                return None;
            }
            self.looked = Some(now);
            let text = std::fs::read_to_string(self.rings_file()).ok()?;
            // Rings refused for their rate aren't opened again (a share request, a map
            // of up to 256 MB and its unmap, on this thread, five times a second; bug run
            // N4-2) until the device makes new ones.
            if self
                .refused
                .as_ref()
                .is_some_and(|r| *r == (text.clone(), rate))
            {
                return None;
            }
            let found = Rings::open(&text).ok()?;
            if found.rate() != rate {
                self.state = "the device runs at another sample rate than the desk";
                self.refused = Some((text, rate));
                return None;
            }
            self.beats = (found.device_beats(), now);
            // The desk's channels, said before its first beat (the device reads them once
            // the beat moves).
            let tx = found.channels().1;
            self.claim = Claim::all(
                self.outputs
                    .min(self.cfg.outputs.unwrap_or(usize::MAX))
                    .min(tx),
            );
            found.set_claim(self.claim);
            self.rings = Some(found);
            // Placed afresh with them: the old writing place would have silenced half a
            // ring behind the clock, on this thread, in one block (bug run M6-3).
            self.place = Place::new(self.block, margin(), self.place.lead, self.place.packet);
        }
        let rings = self.rings.as_ref()?;
        let Some(site_ns) = site_ns else {
            self.state = "no site clock";
            return None;
        };
        // The lead for the largest packet asked for so far: a new one moves the writing
        // only (bug run K6-6: the place started again, the reading too, a click).
        let packet = rings.packet().max(PACKET);
        let lead = packet + late() + self.extra;
        if lead != self.place.lead {
            self.place.set_lead(lead, packet);
        }
        let t = self.place.tick(media_at(site_ns, rate));
        if t.resynced {
            self.resyncs += 1;
        }
        if t.late {
            self.late += 1;
        }
        // On from further than the last block ended: what lies between is from a ring
        // ago, and is made silent (never what was just written).
        if t.skipped > 0 {
            for c in self.claim.channels() {
                rings.silence_channel(c, t.write_at.wrapping_sub(t.skipped), t.skipped);
            }
        }
        self.state = "running";
        Some(t)
    }

    /// The network's block into the desk's inputs (`planar`, a channel each), added to
    /// what the matrix routed there.
    pub fn read(&self, t: &Tick, planar: &mut [Vec<f32>], scratch: &mut [f32]) {
        let Some(rings) = &self.rings else { return };
        for c in 0..rings.channels().0 {
            let Some(p) = planar.get_mut(self.cfg.first + c) else {
                break;
            };
            let n = p.len().min(scratch.len());
            rings.read(c, t.read_at, &mut scratch[..n]);
            for (x, s) in p.iter_mut().zip(&scratch[..n]) {
                *x += *s;
            }
        }
    }

    /// The desk's outputs into the device's transmit rings, in order (output 1, main L, on
    /// its first channel), and the block half a ring ahead silenced: whatever stops the
    /// desk, the device then sends silence, not sound from a ring ago. Only the desk's
    /// channels: the others are the device's routes'.
    ///
    /// `site_ns`: site time now, after the desk made the block. A block that lands with
    /// less than a packet between it and the clock came too late: the device has sent
    /// that stretch as silence already. The tick was on time and the block itself took
    /// too long (the desk waited for its own lock, or the machine was busy): two blocks
    /// of silence in ten minutes on a machine without real-time priority, with nothing
    /// counting them (lab 515-A2). Counted here, and the desk writes a millisecond
    /// further ahead from then on (up to 8 ms more): the smallest lead this machine
    /// keeps without holes, found by measuring.
    pub fn write(&mut self, t: &Tick, out: &[Vec<f32>], site_ns: Option<i128>, rate: u32) {
        let Some(rings) = &self.rings else { return };
        for (c, o) in out.iter().enumerate().take(self.claim.count) {
            rings.write(c, t.write_at, o);
        }
        for c in self.claim.channels() {
            rings.silence_channel(c, t.write_at.wrapping_add(LEN / 2), self.block);
        }
        rings.beat_desk();
        if let Some(now) = site_ns.map(|s| media_at(s, rate))
            && wrote_late(t.write_at, now, self.place.packet)
        {
            self.late_writes += 1;
            self.extra_since = Instant::now();
            let extra = (self.extra + step()).min(extra_most());
            if extra != self.extra {
                self.extra = extra;
                // Said as it moves (eight times at most): what the sound's way through
                // the desk grew by, and why.
                idn_open_protocol::log::log(&format!(
                    "mixer: network audio: a block reached the device late; writing {} ms further ahead from now on",
                    extra / step()
                ));
            }
        } else if self.extra > 0 && self.extra_since.elapsed() >= EXTRA_DECAY {
            self.extra -= step().min(self.extra);
            self.extra_since = Instant::now();
            idn_open_protocol::log::log(&format!(
                "mixer: network audio: no late block for 10 minutes; writing {} ms ahead of the usual now",
                self.extra / step()
            ));
        }
    }

    /// For the desk's state.
    pub fn status(&self) -> Value {
        self.snapshot().json(&self.cfg)
    }

    /// Its counters as they are, copied (no allocation: taken on the clock thread, the
    /// JSON made when the state is asked for; bug run K6-8).
    pub fn snapshot(&self) -> NetSnapshot {
        let (rx, tx) = self.rings.as_ref().map_or((0, 0), Rings::channels);
        NetSnapshot {
            state: self.state,
            rx,
            tx,
            claim: self.rings.as_ref().map(|_| self.claim),
            resyncs: self.resyncs,
            late: self.late,
            late_writes: self.late_writes,
            read_behind: margin() + self.block,
            lead: self.place.lead,
            rate: crate::desk::rate_hz(),
            rx_latency_ms: self
                .rings
                .as_ref()
                .map(|r| r.rx_latency() as f64 * 1000.0 / f64::from(r.rate().max(1))),
        }
    }
}

/// [`NetAudio`]'s counters at one moment.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetSnapshot {
    state: &'static str,
    rx: usize,
    tx: usize,
    /// The transmit channels the desk has (the device's routes keep the rest).
    claim: Option<Claim>,
    resyncs: u64,
    late: u64,
    late_writes: u64,
    read_behind: usize,
    lead: usize,
    rate: u32,
    rx_latency_ms: Option<f64>,
}

impl NetSnapshot {
    /// The device's receive channels (the desk's inputs from it).
    pub fn inputs(&self) -> usize {
        self.rx
    }

    /// As the desk's state gives it.
    pub fn json(&self, cfg: &NetAudioConfig) -> Value {
        let rate = f64::from(self.rate.max(1));
        json!({
            "device": cfg.device,
            "first": cfg.first + 1,
            "state": self.state,
            "in": self.rx,
            "out": self.tx,
            // The device's transmit channels the desk's outputs go on (from 1); its IDN
            // routes keep playing on the others.
            "out_first": self.claim.map(|k| k.first + 1),
            "out_count": self.claim.map(|k| k.count),
            "resyncs": self.resyncs,
            "late_ticks": self.late,
            "late_writes": self.late_writes,
            // What the desk adds between the network and its block, and after it, ms.
            "read_behind_ms": self.read_behind as f64 * 1000.0 / rate,
            "write_ahead_ms": self.lead as f64 * 1000.0 / rate,
            "receive_latency_ms": self.rx_latency_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// A device's rings in a state folder of the test's own, as idn-aoip leaves them.
    fn device(name: &str, rx: usize, tx: usize) -> (Rings, PathBuf) {
        let dir = idn_common::paths::program_dir("aoip").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let rings = Rings::create(rx, tx, 48_000, 96).unwrap();
        std::fs::write(dir.join("rings"), rings.descriptor()).unwrap();
        (rings, dir)
    }

    #[test]
    fn the_desk_reads_and_writes_the_devices_rings() {
        let name = format!("TEST-NET-{}", std::process::id());
        let (dev, dir) = device(&name, 2, 2);
        let block = 64;
        let mut net = NetAudio::new(
            NetAudioConfig {
                device: name.clone(),
                first: 1,
                outputs: None,
            },
            block,
            3,
        );
        let site = 1_794_000_000_000_000_000i128;
        let now = media_at(site, 48_000);
        dev.beat_device();
        // The network's sound, closed up to the clock: each sample its place's value.
        let val = |i: usize| ((i % 1000) as f32) / 2000.0;
        for c in 0..2 {
            let r = dev.ring(true, c).unwrap();
            for i in now - 1000..now {
                r[i & (LEN - 1)].store(
                    idn_common::netrings::to_sample(val(i) * (c + 1) as f32 / 2.0),
                    Ordering::Relaxed,
                );
            }
        }
        let t = net.tick(Some(site), 48_000).expect("the device is there");
        assert_eq!(t.read_at, now - margin() - block);
        assert_eq!(t.write_at, now + PACKET + late());
        // Onto desk channels 2 and 3 (first = 1), added to what was there.
        let mut planar = vec![vec![0.25f32; block]; 4];
        let mut scratch = vec![0f32; block];
        net.read(&t, &mut planar, &mut scratch);
        assert_eq!(planar[0], vec![0.25; block]);
        for (k, (a, b)) in planar[1].iter().zip(&planar[2]).enumerate() {
            let i = t.read_at + k;
            assert!((a - 0.25 - val(i) / 2.0).abs() < 1e-5, "{k}");
            assert!((b - 0.25 - val(i)).abs() < 1e-5, "{k}");
        }
        // The desk's outputs 1 and 2 into the transmit rings at the lead, its beat moved,
        // and the block half a ring ahead silent.
        let far = dev.ring(false, 0).unwrap();
        far[(t.write_at + LEN / 2) & (LEN - 1)].store(77, Ordering::Relaxed);
        let out = vec![vec![0.5f32; block], vec![-0.5; block], vec![0.9; block]];
        net.write(&t, &out, Some(site), 48_000);
        let tx =
            |c: usize, i: usize| dev.ring(false, c).unwrap()[i & (LEN - 1)].load(Ordering::Relaxed);
        assert_eq!(tx(0, t.write_at), idn_common::netrings::to_sample(0.5));
        assert_eq!(
            tx(1, t.write_at + block - 1),
            idn_common::netrings::to_sample(-0.5)
        );
        assert_eq!(tx(0, t.write_at + block), 0);
        assert_eq!(tx(0, t.write_at + LEN / 2), 0);
        assert_eq!(dev.desk_beats(), 1);
        assert_eq!(net.status()["state"], "running");
        // The next block follows on; a bigger packet asked for: further ahead, once.
        dev.beat_device();
        let t2 = net.tick(Some(site + 1_333_333), 48_000).unwrap();
        assert_eq!(
            (t2.read_at, t2.write_at, t2.resynced),
            (t.read_at + block, t.write_at + block, false)
        );
        // The reading runs on; the writing moves by the lead's growth (bug run K6-6: it
        // started both again).
        let lead_was = net.place.lead;
        dev.set_packet(256);
        dev.beat_device();
        let t3 = net.tick(Some(site + 2_666_666), 48_000).unwrap();
        assert!(!t3.resynced);
        assert_eq!(t3.read_at, t2.read_at + block);
        assert_eq!(t3.write_at, t2.write_at + block + (256 + late() - lead_was));
        // No site clock: nothing this block.
        assert!(net.tick(None, 48_000).is_none());
        assert_eq!(net.status()["state"], "no site clock");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// R.33: the desk claims the transmit channels its outputs go on, before its first
    /// beat, and keeps only those silent ahead of the clock; the device's routes keep
    /// the rest.
    #[test]
    fn the_desk_claims_only_the_channels_it_writes() {
        let name = format!("TEST-CLAIM-{}", std::process::id());
        let (dev, dir) = device(&name, 0, 8);
        let block = 64;
        // Three outputs, two of them to the device.
        let mut net = NetAudio::new(
            NetAudioConfig {
                device: name,
                first: 0,
                outputs: Some(2),
            },
            block,
            3,
        );
        let site = 1_794_000_000_000_000_000i128;
        dev.beat_device();
        let t = net.tick(Some(site), 48_000).unwrap();
        assert_eq!(dev.claim(), Some(Claim { first: 0, count: 2 }));
        assert_eq!(dev.desk_beats(), 0, "claimed before the first beat");
        // A route's sound the device wrote on channel 2 and 5, now and half a ring ahead.
        let route = idn_common::netrings::to_sample(0.25);
        for c in [2, 5] {
            let r = dev.ring(false, c).unwrap();
            r[t.write_at & (LEN - 1)].store(route, Ordering::Relaxed);
            r[(t.write_at + LEN / 2) & (LEN - 1)].store(route, Ordering::Relaxed);
        }
        dev.ring(false, 1).unwrap()[(t.write_at + LEN / 2) & (LEN - 1)]
            .store(77, Ordering::Relaxed);
        let out = vec![vec![0.5f32; block], vec![-0.5; block], vec![0.9; block]];
        net.write(&t, &out, Some(site), 48_000);
        let tx =
            |c: usize, i: usize| dev.ring(false, c).unwrap()[i & (LEN - 1)].load(Ordering::Relaxed);
        assert_eq!(tx(0, t.write_at), idn_common::netrings::to_sample(0.5));
        assert_eq!(tx(1, t.write_at), idn_common::netrings::to_sample(-0.5));
        assert_eq!(tx(1, t.write_at + LEN / 2), 0);
        // Its third output isn't written; the routes' channels are left as they were.
        for c in [2, 5] {
            assert_eq!(tx(c, t.write_at), route, "{c}");
            assert_eq!(tx(c, t.write_at + LEN / 2), route, "{c}");
        }
        let s = net.status();
        assert_eq!((&s["out_first"], &s["out_count"]), (&json!(1), &json!(2)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Lab 515-A2: a block that lands late is counted, and the lead grows by a
    /// millisecond for it, to 8 ms more at most.
    #[test]
    fn a_block_that_lands_late_moves_the_lead_out() {
        assert!(!wrote_late(1128, 1000, 32));
        assert!(!wrote_late(1032, 1000, 32));
        assert!(wrote_late(1031, 1000, 32));
        assert!(wrote_late(900, 1000, 32));
        let name = format!("TEST-LATE-{}", std::process::id());
        let (dev, dir) = device(&name, 1, 1);
        let mut net = NetAudio::new(
            NetAudioConfig {
                device: name,
                first: 0,
                outputs: None,
            },
            64,
            1,
        );
        let site = 1_794_000_000_000_000_000i128;
        let out = vec![vec![0.5f32; 64]];
        dev.beat_device();
        let t = net.tick(Some(site), 48_000).unwrap();
        // Made in time: nothing changes.
        net.write(&t, &out, Some(site + 500_000), 48_000);
        assert_eq!(net.status()["late_writes"], 0);
        assert_eq!(net.place.lead, PACKET + late());
        // The block took 3 ms: late, and from the next tick a millisecond further ahead.
        dev.beat_device();
        let t = net.tick(Some(site + 1_333_333), 48_000).unwrap();
        net.write(&t, &out, Some(site + 1_333_333 + 3_000_000), 48_000);
        assert_eq!(net.status()["late_writes"], 1);
        dev.beat_device();
        net.tick(Some(site + 5_000_000), 48_000).unwrap();
        assert_eq!(net.place.lead, PACKET + late() + step());
        // Never past 8 ms more.
        for k in 0..20i128 {
            dev.beat_device();
            let s = site + 5_000_000 + (k + 1) * 1_333_333;
            if let Some(t) = net.tick(Some(s), 48_000) {
                net.write(&t, &out, Some(s + 20_000_000), 48_000);
            }
        }
        dev.beat_device();
        net.tick(Some(site + 60_000_000), 48_000);
        assert_eq!(net.place.lead, PACKET + late() + extra_most());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_device_is_waited_for() {
        let mut net = NetAudio::new(
            NetAudioConfig {
                device: format!("NOT-THERE-{}", std::process::id()),
                first: 0,
                outputs: None,
            },
            64,
            2,
        );
        assert!(net.tick(Some(1_794_000_000_000_000_000), 48_000).is_none());
        assert_eq!(net.status()["state"], "waiting for the device");
        assert_eq!(net.status()["in"], 0);
    }
}
