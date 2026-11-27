//! The rings a network-audio device shares with a desk on the same machine (roadmap
//! R.33): one ring a channel, each sample at the index of its media time (samples of site
//! time, `& (len - 1)`), in shared memory ([`crate::shm`]).
//!
//! The device (`idn-aoip`) makes them and hands them to its network code: what the
//! network sends it lands in the receive rings at each sample's time plus the device's
//! latency; what is in the transmit rings when the media clock reaches it goes out. A
//! desk maps the same rings and works in them directly, one block a tick of the site
//! clock: it reads the block that just became final and writes its outputs a lead ahead
//! of the clock. No connection, buffer or resampler lies between them: they share one
//! clock, so there is nothing to correct.
//!
//! A page of numbers comes first ([`HEAD`] bytes): what the rings are, and two counters
//! each side keeps moving so the other knows it is there (the device steps aside from the
//! transmit rings while a desk writes them, and plays its routes into them again when the
//! desk stops). The desk says there which transmit channels it writes ([`Claim`]); the
//! device steps aside from those alone, and its routes keep the rest.
//!
//! Samples are `i32`, left-justified (a full-scale float is `i32::MAX`), as the device's
//! network code takes them.

use crate::err::invalid_data as invalid;
use crate::shm::Region;
use std::io;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

/// The head's size: one page.
pub const HEAD: usize = 4096;
/// "IDNRINGS".
const MAGIC: u64 = u64::from_le_bytes(*b"IDNRINGS");
/// The head's layout. 2 (2026-11-26): the claim at [`AT_CLAIM`], the device's features
/// word gone. Rings of another version aren't opened: a desk and a device must be the
/// same build.
const VERSION: u32 = 2;
/// Samples a ring: 1.4 s at 48 kHz (a power of two).
pub const LEN: usize = 1 << 16;
/// The most channels each way.
pub const MAX_CHANNELS: usize = 512;

// The head's fields (byte offsets).
const AT_MAGIC: usize = 0;
const AT_VERSION: usize = 8;
const AT_RATE: usize = 12;
const AT_LEN: usize = 16;
const AT_RX: usize = 20;
const AT_TX: usize = 24;
const AT_RX_LATENCY: usize = 28;
const AT_PACKET: usize = 32;
const AT_DEVICE_BEAT: usize = 40;
const AT_DESK_BEAT: usize = 48;
/// The desk's claim ([`Claim`]), one word so the device never reads half of one; 0: none.
const AT_CLAIM: usize = 56;

/// Set in a claim's word: a claim is there (a desk claiming no channel isn't 0).
const CLAIMED: u64 = 1 << 63;

/// The transmit channels a desk writes: `first..first + count` (from 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Claim {
    pub first: usize,
    pub count: usize,
}

impl Claim {
    /// Every one of `tx` channels: what a desk that claims none has.
    pub fn all(tx: usize) -> Claim {
        Claim {
            first: 0,
            count: tx,
        }
    }

    /// Whether channel `c` is the desk's.
    pub fn has(&self, c: usize) -> bool {
        c >= self.first && c - self.first < self.count
    }

    /// Its channels.
    pub fn channels(&self) -> std::ops::Range<usize> {
        self.first..self.first + self.count
    }

    /// As one word (the head's, or a counter's): never 0.
    pub fn word(&self) -> u64 {
        CLAIMED
            | ((self.first.min((u32::MAX >> 1) as usize) as u64) << 32)
            | self.count.min(u32::MAX as usize) as u64
    }

    /// From [`Claim::word`]; `None` for 0.
    pub fn of_word(w: u64) -> Option<Claim> {
        (w & CLAIMED != 0).then_some(Claim {
            first: ((w >> 32) as u32 & (u32::MAX >> 1)) as usize,
            count: w as u32 as usize,
        })
    }
}

impl std::fmt::Display for Claim {
    /// As people count them: "1–8", "3", or "none".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.count {
            0 => write!(f, "none"),
            1 => write!(f, "{}", self.first + 1),
            n => write!(f, "{}–{}", self.first + 1, self.first + n),
        }
    }
}

/// The shared rings, made ([`Rings::create`]) or mapped ([`Rings::open`]).
pub struct Rings {
    region: Region,
    rx: usize,
    tx: usize,
    rate: u32,
}

impl Rings {
    /// New rings, all silent: `rx` channels from the network, `tx` to it, at `rate` Hz;
    /// the device's receive latency in samples, for whoever reads them.
    pub fn create(rx: usize, tx: usize, rate: u32, rx_latency: u32) -> io::Result<Rings> {
        if rx > MAX_CHANNELS || tx > MAX_CHANNELS {
            return Err(invalid("too many channels"));
        }
        let region = Region::create("idn-rings", HEAD + (rx + tx) * LEN * 4)?;
        let put32 = |at: usize, v: u32| region.write(at, &v.to_le_bytes());
        region.write(AT_MAGIC, &MAGIC.to_le_bytes());
        put32(AT_VERSION, VERSION);
        put32(AT_RATE, rate);
        put32(AT_LEN, LEN as u32);
        put32(AT_RX, rx as u32);
        put32(AT_TX, tx as u32);
        put32(AT_RX_LATENCY, rx_latency);
        Ok(Rings {
            region,
            rx,
            tx,
            rate,
        })
    }

    /// What another program opens them by ([`Rings::open`]): the region's name and size.
    pub fn descriptor(&self) -> String {
        format!("{} {}", self.region.name(), self.region.len())
    }

    /// The rings another program made, by their [`Rings::descriptor`].
    pub fn open(descriptor: &str) -> io::Result<Rings> {
        let (name, len) = descriptor
            .trim()
            .rsplit_once(' ')
            .and_then(|(n, l)| Some((n, l.parse::<usize>().ok()?)))
            .ok_or_else(|| invalid("not a rings descriptor"))?;
        if !(HEAD..=HEAD + 2 * MAX_CHANNELS * LEN * 4).contains(&len) {
            return Err(invalid("a rings descriptor with an impossible size"));
        }
        let region = Region::open(name, len)?;
        let get32 = |at: usize| {
            let mut b = [0u8; 4];
            region.read(at, &mut b);
            u32::from_le_bytes(b)
        };
        let mut magic = [0u8; 8];
        region.read(AT_MAGIC, &mut magic);
        if u64::from_le_bytes(magic) != MAGIC || get32(AT_VERSION) != VERSION {
            return Err(invalid("not this build's rings"));
        }
        let (rx, tx) = (get32(AT_RX) as usize, get32(AT_TX) as usize);
        if get32(AT_LEN) as usize != LEN
            || rx > MAX_CHANNELS
            || tx > MAX_CHANNELS
            || HEAD + (rx + tx) * LEN * 4 != len
        {
            return Err(invalid("rings of another shape than their head says"));
        }
        let rate = get32(AT_RATE);
        Ok(Rings {
            region,
            rx,
            tx,
            rate,
        })
    }

    /// Channels from the network, and to it.
    pub fn channels(&self) -> (usize, usize) {
        (self.rx, self.tx)
    }

    /// The sample rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    fn atomic_u32(&self, at: usize) -> &AtomicU32 {
        // SAFETY: `at` is one of the head's 4-aligned fields, within the mapping (at least
        // HEAD bytes), which lives as long as `self`.
        unsafe { &*(self.region.as_ptr().add(at) as *const AtomicU32) }
    }

    fn atomic_u64(&self, at: usize) -> &AtomicU64 {
        self.region.atomic_u64(at).expect("a field of the head")
    }

    /// The device's receive latency, samples: a sample stamped T is in its ring at T + this.
    pub fn rx_latency(&self) -> usize {
        self.atomic_u32(AT_RX_LATENCY).load(Ordering::Relaxed) as usize
    }

    /// The largest packet (samples) a receiver on the network has asked the device for:
    /// that much must be in the transmit rings ahead of the media clock at any moment.
    pub fn packet(&self) -> usize {
        self.atomic_u32(AT_PACKET).load(Ordering::Relaxed) as usize
    }

    /// The device: its largest packet so far ([`Rings::packet`]).
    pub fn set_packet(&self, samples: usize) {
        self.atomic_u32(AT_PACKET)
            .store(samples.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    }

    /// The device, every millisecond or so while it runs: it is there.
    pub fn beat_device(&self) {
        self.atomic_u64(AT_DEVICE_BEAT)
            .fetch_add(1, Ordering::Relaxed);
    }

    /// The device's beats so far (a desk: unchanged for a while, the device is gone).
    pub fn device_beats(&self) -> u64 {
        self.atomic_u64(AT_DEVICE_BEAT).load(Ordering::Relaxed)
    }

    /// The desk, every block it writes: it has the transmit rings.
    pub fn beat_desk(&self) {
        // (Release, and Acquire where it's read: a device seeing the beat sees the claim
        // set before it, on weakly ordered machines too.)
        self.atomic_u64(AT_DESK_BEAT)
            .fetch_add(1, Ordering::Release);
    }

    /// The desk's beats so far (the device: changing, a desk writes the transmit rings).
    pub fn desk_beats(&self) -> u64 {
        self.atomic_u64(AT_DESK_BEAT).load(Ordering::Acquire)
    }

    /// The desk, before its first beat: the transmit channels it writes.
    pub fn set_claim(&self, claim: Claim) {
        self.atomic_u64(AT_CLAIM)
            .store(claim.word(), Ordering::Release);
    }

    /// The desk's claim; `None` until a desk has set one.
    pub fn claim(&self) -> Option<Claim> {
        Claim::of_word(self.atomic_u64(AT_CLAIM).load(Ordering::Acquire))
    }

    /// Ring `c` from the network (`rx`) or to it: its samples, for the device's network
    /// code to take by address. `None` past the channels.
    pub fn ring(&self, rx: bool, c: usize) -> Option<&[AtomicI32]> {
        let k = if rx {
            (c < self.rx).then_some(c)?
        } else {
            self.rx + (c < self.tx).then_some(c)?
        };
        // SAFETY: ring `k` lies within the mapping (its size checked against the channel
        // counts when made or opened), 4-aligned (HEAD and each ring are multiples of 4
        // on a page-aligned mapping), and lives as long as `self`. Every program touches
        // it only through atomics.
        Some(unsafe {
            std::slice::from_raw_parts(
                self.region.as_ptr().add(HEAD + k * LEN * 4) as *const AtomicI32,
                LEN,
            )
        })
    }

    /// `out.len()` samples of receive channel `c` from media time `at`, as floats; silence
    /// for a channel there isn't.
    pub fn read(&self, c: usize, at: usize, out: &mut [f32]) {
        match self.ring(true, c) {
            Some(r) => {
                for (i, o) in out.iter_mut().enumerate() {
                    *o = r[at.wrapping_add(i) & (LEN - 1)].load(Ordering::Relaxed) as f32
                        / 2_147_483_648.0;
                }
            }
            None => out.fill(0.0),
        }
    }

    /// `samples` into transmit channel `c` at media time `at` (held to full scale).
    pub fn write(&self, c: usize, at: usize, samples: &[f32]) {
        if let Some(r) = self.ring(false, c) {
            for (i, &x) in samples.iter().enumerate() {
                r[at.wrapping_add(i) & (LEN - 1)].store(to_sample(x), Ordering::Relaxed);
            }
        }
    }

    /// Silence in every transmit ring from media time `at`, `len` samples.
    pub fn silence(&self, at: usize, len: usize) {
        for c in 0..self.tx {
            self.silence_channel(c, at, len);
        }
    }

    /// Silence in transmit ring `c` from media time `at`, `len` samples.
    pub fn silence_channel(&self, c: usize, at: usize, len: usize) {
        if let Some(r) = self.ring(false, c) {
            for i in 0..len {
                r[at.wrapping_add(i) & (LEN - 1)].store(0, Ordering::Relaxed);
            }
        }
    }
}

/// A float sample as the rings hold it; a non-number is silence.
pub fn to_sample(x: f32) -> i32 {
    if x.is_nan() {
        0
    } else {
        (x.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32
    }
}

/// Media time (samples of site time, wrapping) at site time `site_ns` (TAI, ns), at `rate`
/// Hz: the index a sample of that moment has in the rings.
pub fn media_at(site_ns: i128, rate: u32) -> usize {
    ((site_ns.max(0) as u128) * rate as u128 / 1_000_000_000) as usize
}

/// A desk's place in the rings, block by block: where it reads the network's sound and
/// where it writes its own, from the media clock at each tick.
///
/// - **Read** the block that ended `margin` behind the clock: the device has closed
///   everything up to the clock by then (it does so every millisecond), so `margin` is
///   that millisecond and room besides.
/// - **Write** a block starting `lead` ahead of the clock: the device sends a packet's
///   samples when the clock reaches its first, so the largest packet (`packet`) must
///   still be ahead of the clock when a tick comes late; the rest of `lead` is that room.
///
/// Both run on from block to block without a gap. A tick that comes late finds them
/// behind where the clock says by its lateness; the ticks after it come at once and catch
/// up. Later than the room in `lead`, the device sends what isn't written yet (silence)
/// for as long as the tick was over ([`Tick::late`]); the places still run on, so that is
/// all that is lost. Only four blocks behind and more (a stall), or with the clock
/// stepped, do both start again from the clock ([`Tick::resynced`]).
///
/// (Starting again at every tick more than a block off, as this first did, was 800
/// times in 14 s on a desk without real-time priority, each one a hole in the sound;
/// starting again as soon as a packet was no longer covered turned a tick a fraction of
/// a millisecond too late into 3 ms of silence, three times in ten minutes: lab 515-A.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// Samples a block.
    pub block: usize,
    /// How far behind the clock the block read ends.
    pub margin: usize,
    /// How far ahead of the clock the block written starts.
    pub lead: usize,
    /// The largest packet the device sends: what must stay written ahead of the clock.
    pub packet: usize,
    read: Option<usize>,
    write: Option<usize>,
    /// Where the writing ended before [`Place::set_lead`] moved it on: the gap from there
    /// is made silent at the next tick.
    moved_from: Option<usize>,
}

/// What a tick does: where to read a block and where to write one (media times).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub read_at: usize,
    pub write_at: usize,
    /// Both started again from the clock (the start, a stall, a step of the clock).
    pub resynced: bool,
    /// The tick came so late that a packet was no longer covered: the device sent a
    /// little silence (the lateness past the room), and the places ran on.
    pub late: bool,
    /// Samples between the end of what was written before and `write_at`, when this tick
    /// starts further on than the last ended (0 otherwise): to be made silent, so what
    /// lay there from a ring ago isn't sent.
    pub skipped: usize,
}

impl Place {
    /// A desk of `block` samples a block, for packets of up to `packet` samples.
    pub fn new(block: usize, margin: usize, lead: usize, packet: usize) -> Place {
        Place {
            block,
            margin,
            lead,
            packet: packet.min(lead),
            read: None,
            write: None,
            moved_from: None,
        }
    }

    /// A new lead (and largest packet): the writing moves by the difference, the reading
    /// goes on as it was (bug run K6-6: a new place resynced both, a click on every
    /// input). Moved on, the gap is made silent; moved back, the newer block is written
    /// over what was written for that time and not yet sent.
    pub fn set_lead(&mut self, lead: usize, packet: usize) {
        if let Some(w) = self.write {
            if lead >= self.lead {
                self.moved_from.get_or_insert(w);
                self.write = Some(w.wrapping_add(lead - self.lead));
            } else {
                self.write = Some(w.wrapping_sub(self.lead - lead));
            }
        }
        self.lead = lead;
        self.packet = packet.min(lead);
    }

    /// The tick at media time `now`.
    pub fn tick(&mut self, now: usize) -> Tick {
        // Where each would be, exactly on the clock.
        let read_due = now.wrapping_sub(self.margin + self.block);
        let write_due = now.wrapping_add(self.lead);
        // How far behind the clock's place it may run before starting again: four blocks
        // (the ticks after a late one catch up at once; past that it is a stall). Ahead
        // of it (ticks come early only by a timer's error): a block.
        let behind_most = (self.lead + 4 * self.block) as isize;
        // Late: what is written no longer reaches a packet past the clock.
        let late = self.write.is_some_and(|w| {
            (write_due.wrapping_sub(w) as isize) > (self.lead - self.packet) as isize
        });
        let off = |at: Option<usize>, due: usize| match at {
            None => true,
            Some(a) => {
                let behind = due.wrapping_sub(a) as isize;
                behind > behind_most || behind < -(self.block as isize)
            }
        };
        let resynced = off(self.write, write_due) || off(self.read, read_due);
        let before = self.moved_from.take().or(self.write);
        if resynced {
            (self.read, self.write) = (Some(read_due), Some(write_due));
        }
        let (read_at, write_at) = (
            self.read.unwrap_or(read_due),
            self.write.unwrap_or(write_due),
        );
        // On from further than the last block ended: that much lies unwritten (at most
        // half a ring is worth clearing; the rest is cleared as blocks are written).
        let skipped = before
            .map(|b| write_at.wrapping_sub(b) as isize)
            .filter(|g| *g > 0)
            .map_or(0, |g| (g as usize).min(LEN / 2));
        self.read = Some(read_at.wrapping_add(self.block));
        self.write = Some(write_at.wrapping_add(self.block));
        Tick {
            read_at,
            write_at,
            resynced,
            late: late && !resynced,
            skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new lead moves the writing alone: the reading runs on, the gap is said (K6-6).
    #[test]
    fn a_new_lead_moves_only_the_writing() {
        let mut p = Place::new(64, 96, 200, 32);
        let a = p.tick(10_000);
        let b = p.tick(10_064);
        assert!(!b.resynced && b.read_at == a.read_at + 64);
        p.set_lead(248, 32);
        let c = p.tick(10_128);
        assert!(!c.resynced, "{c:?}");
        assert_eq!(c.read_at, b.read_at + 64);
        assert_eq!(c.write_at, b.write_at + 64 + 48);
        assert_eq!(c.skipped, 48);
        p.set_lead(200, 32);
        let d = p.tick(10_192);
        assert!(!d.resynced);
        assert_eq!(d.read_at, c.read_at + 64);
        assert_eq!((d.write_at, d.skipped), (c.write_at + 64 - 48, 0));
    }

    #[test]
    fn rings_are_shared_between_two_mappings() {
        let made = Rings::create(2, 3, 48_000, 96).unwrap();
        let seen = Rings::open(&made.descriptor()).unwrap();
        assert_eq!(seen.channels(), (2, 3));
        assert_eq!((seen.rate(), seen.rx_latency()), (48_000, 96));
        // The device writes a receive ring (as its network code does, by the ring itself);
        // the desk reads it, across the wrap of the ring.
        let at = LEN - 2;
        let ring = made.ring(true, 1).unwrap();
        for (i, v) in [0.5f32, -0.5, 0.25, 1.0].into_iter().enumerate() {
            ring[(at + i) & (LEN - 1)].store(to_sample(v), Ordering::Relaxed);
        }
        let mut got = [0f32; 4];
        seen.read(1, at, &mut got);
        for (g, w) in got.iter().zip([0.5f32, -0.5, 0.25, 1.0]) {
            assert!((g - w).abs() < 1e-6, "{got:?}");
        }
        // The desk writes a transmit ring; the device finds it; too loud is held, a
        // non-number is silence.
        seen.write(2, 100, &[0.5, 2.0, f32::NAN]);
        let tx = made.ring(false, 2).unwrap();
        assert_eq!(tx[100].load(Ordering::Relaxed), to_sample(0.5));
        assert_eq!(tx[101].load(Ordering::Relaxed), i32::MAX);
        assert_eq!(tx[102].load(Ordering::Relaxed), 0);
        seen.silence(100, 2);
        assert_eq!(tx[100].load(Ordering::Relaxed), 0);
        // A channel there isn't: silence read, nothing written, no ring.
        seen.read(9, 0, &mut got);
        assert_eq!(got, [0.0; 4]);
        assert!(made.ring(true, 2).is_none() && made.ring(false, 3).is_none());
        // Each side sees the other's beats and the packet size.
        made.beat_device();
        made.set_packet(64);
        seen.beat_desk();
        assert_eq!(
            (seen.device_beats(), made.desk_beats(), seen.packet()),
            (1, 1, 64)
        );
    }

    /// The desk's claim crosses in the head; none until a desk sets one.
    #[test]
    fn a_desks_claim_is_seen_by_the_device() {
        let made = Rings::create(0, 8, 48_000, 96).unwrap();
        let seen = Rings::open(&made.descriptor()).unwrap();
        assert_eq!(made.claim(), None);
        let k = Claim { first: 3, count: 2 };
        seen.set_claim(k);
        assert_eq!(made.claim(), Some(k));
        assert!(!k.has(2) && k.has(3) && k.has(4) && !k.has(5));
        assert_eq!(k.to_string(), "4–5");
        assert_eq!(Claim::all(8).to_string(), "1–8");
        assert_eq!(Claim { first: 2, count: 1 }.to_string(), "3");
        // A desk claiming nothing is a claim (not "all").
        let none = Claim { first: 0, count: 0 };
        seen.set_claim(none);
        assert_eq!(made.claim(), Some(none));
        assert!(!none.has(0));
        assert_eq!(Claim::of_word(0), None);
        // One silenced channel leaves the others.
        seen.write(1, 10, &[0.5]);
        seen.write(2, 10, &[0.5]);
        seen.silence_channel(1, 10, 1);
        assert_eq!(made.ring(false, 1).unwrap()[10].load(Ordering::Relaxed), 0);
        assert_eq!(
            made.ring(false, 2).unwrap()[10].load(Ordering::Relaxed),
            to_sample(0.5)
        );
    }

    #[test]
    fn what_isnt_rings_isnt_opened() {
        assert!(Rings::open("").is_err());
        assert!(Rings::open("name notanumber").is_err());
        assert!(Rings::open("no-such-region-here 1052672").is_err());
        // A region of the right size that isn't rings.
        let other = Region::create("idn-other", HEAD + LEN * 4).unwrap();
        assert!(Rings::open(&format!("{} {}", other.name(), other.len())).is_err());
        // Rings of another build's layout (another version in the head).
        let made = Rings::create(1, 1, 48_000, 96).unwrap();
        assert!(made.region.write(AT_VERSION, &(VERSION - 1).to_le_bytes()));
        let e = Rings::open(&made.descriptor()).err().expect("refused");
        assert_eq!(e.to_string(), "not this build's rings");
    }

    #[test]
    fn a_desks_place_runs_on_and_starts_again_after_a_stall() {
        let (block, margin, lead, packet) = (64, 96, 128, 32);
        let mut p = Place::new(block, margin, lead, packet);
        let t0 = 1_000_000usize;
        // The first tick: from the clock.
        let a = p.tick(t0);
        assert!(a.resynced);
        assert_eq!(
            (a.read_at, a.write_at, a.skipped),
            (t0 - margin - block, t0 + lead, 0)
        );
        // On time, early by a little, late by up to the room (lead − packet = 2 ms): block
        // after block with no gap and no restart, and what is written always covers a
        // packet past the clock.
        let mut want = a;
        for (k, late) in [0isize, 20, -20, 96, 90, 30, 0].into_iter().enumerate() {
            let t = (t0 + (k + 1) * block).wrapping_add_signed(late);
            let x = p.tick(t);
            want.read_at += block;
            want.write_at += block;
            assert_eq!(
                (x.read_at, x.write_at, x.resynced, x.skipped),
                (want.read_at, want.write_at, false, 0),
                "late by {late}"
            );
            assert!(
                x.write_at >= t + packet,
                "a packet is covered, late by {late}"
            );
            assert!(x.read_at + block <= t, "what is read is behind the clock");
        }
        // Later than the room (a packet no longer covered): said, and the places run on
        // (the device sent silence for the lateness past the room; starting again here
        // made it two blocks and more: lab 515-A).
        let t = t0 + 8 * block + 97;
        let x = p.tick(t);
        want.read_at += block;
        want.write_at += block;
        assert_eq!(
            (x.read_at, x.write_at, x.resynced, x.late, x.skipped),
            (want.read_at, want.write_at, false, true, 0)
        );
        // Four blocks late: still running on.
        let x = p.tick(t0 + 9 * block + 4 * block);
        assert!(x.late && !x.resynced);
        // A stall of 100 blocks: again from the clock, the stretch between made silent
        // (bounded to half a ring).
        let t2 = t + 101 * block;
        let x = p.tick(t2);
        assert!(x.resynced && !x.late);
        assert_eq!((x.read_at, x.write_at), (t2 - margin - block, t2 + lead));
        assert_eq!(x.skipped, 99 * block + 97);
        assert!(!p.tick(t2 + block).resynced);
        // The clock steps back: again from it, nothing to clear.
        let x = p.tick(t2 - 5000);
        assert!(x.resynced);
        assert_eq!(x.skipped, 0);
        // Across the wrap of the count.
        let mut p = Place::new(block, margin, lead, packet);
        let x = p.tick(10);
        assert_eq!(x.read_at, 10usize.wrapping_sub(margin + block));
        assert!(!p.tick(10 + block).resynced);
    }

    #[test]
    fn media_time_counts_samples_of_site_time() {
        assert_eq!(media_at(1_000_000_000, 48_000), 48_000);
        assert_eq!(
            media_at(1_794_000_000_123_456_789, 48_000),
            86_112_000_005_925
        );
        assert_eq!(media_at(-5, 48_000), 0);
    }
}
