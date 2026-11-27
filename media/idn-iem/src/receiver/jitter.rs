//! The jitter buffer (idn-iem.md §I.8.2): frames kept by seq until their turn, played one
//! a pull. Its clock is the frames it plays (5 ms each), so it follows the phone's sound
//! output and needs no timer of its own.
//!
//! - **Start:** playing begins once the frames waiting reach the target (`start_ms`,
//!   ~15 ms).
//! - **Missing frame, later ones here:** lost; concealed, and play moves on.
//! - **Nothing here yet (an underrun):** the frame is late, not lost: a concealed frame is
//!   played *without* moving on, so the buffer grows by a frame, and the target's margin
//!   by one too (up to `max_ms`). After 200 ms of nothing the flow has stopped: silence,
//!   and a fresh start when frames come again.
//! - **Too much buffered:** when, over a whole second, a frame more than the target was
//!   always waiting, one frame is taken out (two played as one, crossfaded): this follows
//!   the desk's clock drifting from the phone's. Three frames or more over it for 200 ms
//!   (what a burst added): one out every 200 ms. A margin unused for 10 s shrinks by
//!   1 ms (not below `min_ms`). Far too much (past `max_ms`) jumps back to the target at
//!   once, as does the start: playing begins with the newest frames.
//! - Frames again (a duplicate, the same packet by unicast and multicast) and frames
//!   after their turn (late) are dropped and counted.

use crate::wire::Audio;
use std::collections::BTreeMap;

/// One frame's channels, as far as they came.
#[derive(Debug, Clone, Default)]
pub(crate) struct Frame {
    /// Its sequence number (read by the tests only: the order frames come out in).
    #[cfg_attr(not(test), allow(dead_code))]
    pub seq: u32,
    /// When its first sample was taken at the desk (ns), and the AUDIO flags.
    pub time: u64,
    pub flags: u8,
    pub samples: usize,
    pub entries: Vec<(u16, Vec<u8>)>,
    /// The packets it came in, and those seen (a bit each, parts 0–63).
    parts: u8,
    seen: u64,
}

impl Frame {
    /// The share of its packets that never came (0 to 1).
    fn missing(&self) -> f32 {
        let parts = u32::from(self.parts.clamp(1, 64));
        1.0 - (self.seen.count_ones().min(parts) as f32 / parts as f32)
    }
}

/// What to play next.
#[derive(Debug)]
pub(crate) enum Pop {
    /// Nothing (not started, or the flow stopped).
    Silence,
    Play(Frame),
    /// A frame lost: concealed, play moved on.
    Lost,
    /// No frame yet: concealed, play waits (the buffer grows a frame).
    Stall,
    /// Two frames as one, the first crossfaded into the second (the buffer shrinks one).
    Splice(Frame, Frame),
}

/// Counters since the start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// AUDIO packets taken into the buffer.
    pub packets: u64,
    /// Packets dropped as seen before.
    pub duplicates: u64,
    /// Packets that came after their frame's turn.
    pub late: u64,
    /// Frames missing at their turn (later ones there): concealed.
    pub lost: u64,
    /// Frames played concealed (lost and underruns).
    pub concealed: u64,
    /// Times nothing was there at a frame's turn.
    pub underruns: u64,
    /// Frames taken out to shrink the buffer (crossfaded).
    pub drops: u64,
    /// Frames jumped over (far too much buffered, or a fresh start).
    pub skips: u64,
    /// Fresh starts (the flow stopped and came back, or jumped).
    pub restarts: u64,
}

/// After this long with nothing to play, the flow has stopped.
const STOPPED_MS: f32 = 200.0;
/// The window over which the least waiting is looked at (frames: 1 s at 5 ms).
const WINDOW: u32 = 200;
/// Frames without an underrun before the margin shrinks by 1 ms (10 s).
const CALM: u32 = 2000;
/// Frames kept at most (5 s).
const MAX_SLOTS: usize = 1000;

pub(crate) struct Jitter {
    frame_ms: f32,
    min_ms: f32,
    max_ms: f32,
    /// What the buffer keeps beyond the frame playing (the target is a frame and this).
    margin_ms: f32,
    slots: BTreeMap<u64, Frame>,
    /// The newest seq seen, unwrapped (from 2^32 so earlier ones stay positive).
    last_ext: Option<u64>,
    newest: Option<u64>,
    play: Option<u64>,
    stalls: u32,
    window: u32,
    window_min: f32,
    calm: u32,
    level_ema: f32,
    loss_ema: f32,
    pub counters: Counters,
}

impl Jitter {
    /// `start_ms` to start with (and the target), adapting within `min_ms`..`max_ms`.
    pub fn new(frame_ms: f32, start_ms: f32, min_ms: f32, max_ms: f32) -> Jitter {
        let max_ms = max_ms.max(frame_ms * 2.0);
        let min_ms = min_ms.clamp(frame_ms, max_ms);
        let start = start_ms.clamp(min_ms, max_ms);
        Jitter {
            frame_ms,
            min_ms,
            max_ms,
            margin_ms: start - frame_ms,
            slots: BTreeMap::new(),
            last_ext: None,
            newest: None,
            play: None,
            stalls: 0,
            window: 0,
            window_min: f32::MAX,
            calm: 0,
            level_ema: 0.0,
            loss_ema: 0.0,
            counters: Counters::default(),
        }
    }

    /// What it holds on average (ms ahead of the frame playing, the frame included).
    pub fn level_ms(&self) -> f32 {
        if self.play.is_some() {
            self.level_ema
        } else {
            0.0
        }
    }

    /// What it aims to keep (ms).
    pub fn target_ms(&self) -> f32 {
        self.frame_ms + self.margin_ms
    }

    /// The share of packets lost lately (0 to 1; about the last second).
    pub fn loss(&self) -> f32 {
        self.loss_ema
    }

    #[cfg(test)]
    pub fn playing(&self) -> bool {
        self.play.is_some()
    }

    /// Forgets everything (a new flow): starts afresh.
    pub fn reset(&mut self) {
        self.slots.clear();
        self.last_ext = None;
        self.newest = None;
        self.play = None;
        self.stalls = 0;
        self.window = 0;
        self.window_min = f32::MAX;
    }

    fn ext(&self, seq: u32) -> u64 {
        match self.last_ext {
            None => (1u64 << 32) + u64::from(seq),
            Some(l) => {
                let d = i64::from(seq.wrapping_sub(l as u32) as i32);
                (l as i64 + d).max(0) as u64
            }
        }
    }

    /// An AUDIO packet in.
    pub fn insert(&mut self, a: &Audio) {
        let mut e = self.ext(a.seq);
        if let Some(n) = self.newest
            && (e > n + 400 || e + 400 < n)
        {
            // A jump of seconds (the desk's flow started again): afresh.
            self.counters.restarts += 1;
            self.reset();
            e = self.ext(a.seq);
        }
        if self.play.is_some_and(|p| e < p) {
            self.counters.late += 1;
            return;
        }
        let slot = self.slots.entry(e).or_insert_with(|| Frame {
            seq: a.seq,
            time: a.time,
            flags: a.flags,
            samples: usize::from(a.frame_samples).clamp(120, crate::MAX_TALK_SAMPLES),
            entries: Vec::new(),
            parts: a.parts,
            seen: 0,
        });
        let bit = 1u64 << (a.part % 64);
        if slot.seen & bit != 0 {
            self.counters.duplicates += 1;
            return;
        }
        slot.seen |= bit;
        if a.part == 0 {
            slot.time = a.time;
        }
        slot.entries
            .extend(a.entries.iter().map(|x| (x.id, x.data.clone())));
        self.counters.packets += 1;
        self.last_ext = Some(self.last_ext.map_or(e, |l| l.max(e)));
        self.newest = Some(self.newest.map_or(e, |n| n.max(e)));
        while self.slots.len() > MAX_SLOTS {
            self.slots.pop_first();
        }
    }

    fn note_loss(&mut self, x: f32) {
        self.loss_ema += (x - self.loss_ema) * 0.005;
    }

    /// The next frame to play.
    pub fn pop(&mut self) -> Pop {
        let Some(n) = self.newest else {
            return Pop::Silence;
        };
        let p = match self.play {
            Some(p) => p,
            None => {
                let Some(&first) = self.slots.keys().next() else {
                    return Pop::Silence;
                };
                // The target's worth of the newest frames: older ones (waiting while
                // nothing played yet) are passed over.
                let keep = ((self.target_ms() - 0.01) / self.frame_ms).ceil().max(1.0) as u64;
                if n - first + 1 < keep {
                    return Pop::Silence;
                }
                let start = n + 1 - keep;
                self.counters.skips += start - first;
                self.slots.retain(|&k, _| k >= start);
                self.play = Some(start);
                self.window = 0;
                self.window_min = f32::MAX;
                start
            }
        };
        let mut p = p;
        let mut ahead_ms = if n >= p {
            (n - p + 1) as f32 * self.frame_ms
        } else {
            0.0
        };
        // Far too much: back to the target at once.
        if ahead_ms > self.max_ms + self.frame_ms {
            let keep = (self.target_ms() / self.frame_ms).ceil() as u64;
            let to = n + 1 - keep.max(1);
            self.counters.skips += to - p;
            self.slots.retain(|&k, _| k >= to);
            p = to;
            self.play = Some(p);
            ahead_ms = (n - p + 1) as f32 * self.frame_ms;
            self.window = 0;
            self.window_min = f32::MAX;
        }
        self.level_ema += (ahead_ms - self.level_ema) * 0.02;
        self.window_min = self.window_min.min(ahead_ms);
        self.window += 1;
        self.calm += 1;
        if self.calm >= CALM {
            self.calm = 0;
            self.margin_ms = (self.margin_ms - 1.0).max(self.min_ms - self.frame_ms);
        }
        if let Some(f) = self.slots.remove(&p) {
            self.play = Some(p + 1);
            self.stalls = 0;
            self.note_loss(f.missing());
            // (Three frames spare or more for 200 ms: one out now; one spare for a whole
            // window: one out.)
            let spare = self.window_min - self.target_ms();
            if self.window >= WINDOW || (self.window >= WINDOW / 5 && spare >= 3.0 * self.frame_ms)
            {
                self.window = 0;
                self.window_min = f32::MAX;
                if spare >= self.frame_ms
                    && ahead_ms - self.frame_ms >= self.min_ms
                    && let Some(g) = self.slots.remove(&(p + 1))
                {
                    self.play = Some(p + 2);
                    self.counters.drops += 1;
                    self.note_loss(g.missing());
                    return Pop::Splice(f, g);
                }
            }
            return Pop::Play(f);
        }
        if n > p {
            self.play = Some(p + 1);
            self.stalls = 0;
            self.counters.lost += 1;
            self.counters.concealed += 1;
            self.note_loss(1.0);
            return Pop::Lost;
        }
        // Nothing here: late. Wait a frame (the buffer grows), and keep more from now on.
        self.stalls += 1;
        if self.stalls == 1 {
            self.counters.underruns += 1;
            self.margin_ms = (self.margin_ms + self.frame_ms).min(self.max_ms - self.frame_ms);
            self.calm = 0;
            self.window = 0;
            self.window_min = f32::MAX;
        }
        if self.stalls as f32 * self.frame_ms > STOPPED_MS {
            self.counters.restarts += 1;
            self.reset();
            return Pop::Silence;
        }
        self.counters.concealed += 1;
        Pop::Stall
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Entry;

    fn pkt(seq: u32) -> Audio {
        Audio {
            flow: 1,
            layout: 1,
            seq,
            time: u64::from(seq) * 5_000_000,
            frame_samples: 240,
            part: 0,
            parts: 1,
            flags: 0,
            entries: vec![Entry {
                id: 1,
                data: vec![seq as u8],
            }],
        }
    }

    fn seq_of(p: &Pop) -> Option<u32> {
        match p {
            Pop::Play(f) => Some(f.seq),
            _ => None,
        }
    }

    /// Starts at the target, plays in order across reordering and duplicates, conceals
    /// a lost frame and moves on, waits (grows) on an underrun.
    #[test]
    fn order_loss_and_underrun() {
        let mut j = Jitter::new(5.0, 15.0, 10.0, 80.0);
        j.insert(&pkt(10));
        assert!(matches!(j.pop(), Pop::Silence), "below the target");
        j.insert(&pkt(12));
        j.insert(&pkt(11));
        j.insert(&pkt(11));
        assert_eq!(j.counters.duplicates, 1);
        assert_eq!(seq_of(&j.pop()), Some(10));
        assert_eq!(seq_of(&j.pop()), Some(11));
        j.insert(&pkt(14));
        assert_eq!(seq_of(&j.pop()), Some(12));
        assert!(matches!(j.pop(), Pop::Lost));
        assert_eq!(seq_of(&j.pop()), Some(14));
        let target = j.target_ms();
        assert!(matches!(j.pop(), Pop::Stall));
        assert_eq!(j.target_ms(), target + 5.0, "an underrun widens the target");
        // The late frame isn't lost: play waited for it.
        j.insert(&pkt(15));
        assert_eq!(seq_of(&j.pop()), Some(15));
        // A frame after its turn: dropped.
        j.insert(&pkt(13));
        assert_eq!(j.counters.late, 1);
        assert_eq!(j.counters.lost, 1);
        assert_eq!(j.counters.underruns, 1);
    }

    /// Steady arrivals with a frame always spare: the buffer gives it back (crossfaded),
    /// not below its least; 200 ms of nothing is a stop and a fresh start.
    #[test]
    fn shrinks_when_spare_and_restarts_after_a_stop() {
        // At the target of 30 ms (six frames), then a burst of five more at once.
        let mut j = Jitter::new(5.0, 30.0, 10.0, 80.0);
        let mut seq = 0u32;
        for _ in 0..5 {
            j.insert(&pkt(seq));
            seq += 1;
        }
        let mut drops = 0;
        for k in 0..1500 {
            for _ in 0..if k == 10 { 6 } else { 1 } {
                j.insert(&pkt(seq));
                seq += 1;
            }
            if matches!(j.pop(), Pop::Splice(..)) {
                drops += 1;
            }
        }
        assert_eq!(drops, 5);
        assert_eq!(j.counters.skips, 0);
        assert!((j.level_ms() - 30.0).abs() < 1.0, "{}", j.level_ms());
        // Stopped.
        let mut silence = false;
        for _ in 0..60 {
            if matches!(j.pop(), Pop::Silence) {
                silence = true;
                break;
            }
        }
        assert!(silence && !j.playing());
        assert_eq!(j.counters.restarts, 1);
        // The seq wraps without a fuss.
        let mut j = Jitter::new(5.0, 10.0, 10.0, 80.0);
        j.insert(&pkt(u32::MAX - 1));
        j.insert(&pkt(u32::MAX));
        let mut got = vec![seq_of(&j.pop())];
        for s in [0, 1] {
            j.insert(&pkt(s));
            got.push(seq_of(&j.pop()));
        }
        got.push(seq_of(&j.pop()));
        assert_eq!(got, [Some(u32::MAX - 1), Some(u32::MAX), Some(0), Some(1)]);
    }

    /// Playing starts with the newest frames (those waiting before are passed over); far
    /// too much waiting later (a burst after a stall) jumps back to the target.
    #[test]
    fn far_too_much_jumps_back() {
        let mut j = Jitter::new(5.0, 15.0, 10.0, 50.0);
        for s in 0..40 {
            j.insert(&pkt(s));
        }
        assert_eq!(seq_of(&j.pop()), Some(37));
        assert_eq!(j.counters.skips, 37);
        for s in 40..=80 {
            j.insert(&pkt(s));
        }
        assert_eq!(seq_of(&j.pop()), Some(78));
        assert_eq!(j.counters.skips, 77);
    }

    /// A frame of two packets, one lost: played (its channel concealed), the loss seen.
    #[test]
    fn a_part_lost_counts() {
        let mut j = Jitter::new(5.0, 5.0, 5.0, 80.0);
        for s in 0..1500u32 {
            let mut a = pkt(s);
            a.parts = 2;
            j.insert(&a);
            if s % 2 == 0 {
                a.part = 1;
                j.insert(&a);
            }
            assert!(matches!(j.pop(), Pop::Play(_) | Pop::Splice(..)));
        }
        assert!((j.loss() - 0.25).abs() < 0.05, "{}", j.loss());
    }
}
