//! One talker's jitter buffer (VOICE-CLIENT.md §7, idn-voice.md §V.13.5): packets kept by
//! `seq` until their turn, one taken a pull. Its clock is the frames it gives (20 ms
//! each), so it follows the phone's sound output and needs no timer.
//!
//! - **Start**: playing begins once the frames waiting reach the target (the channel's
//!   `jitter_start_ms`), or once the first has waited that long (a talk shorter than the
//!   target is still heard), or at once with a backlog (a resume's: played straight away).
//! - **A gap, later frames here**: over a reliable link a missing frame never comes (the
//!   server dropped it for a slow link): play jumps to the next frame here, concealing
//!   once with that frame's FEC; the frames skipped are counted lost.
//! - **Nothing here (an underrun)**: late, not lost: concealed without moving on, so the
//!   buffer holds that much more once the frames come. That is a stall: the target grows
//!   by the wait (at most [`GROW_MAX`] a stall), up to `jitter_max_ms`; play goes on from
//!   the first frame that came. Nothing for [`IDLE_FRAMES`] is a talk that ended: quiet,
//!   the target halfway back to the start, and a fresh start later.
//! - **Steady**: the target shrinks a frame every [`SHRINK_EVERY`] frames played without a
//!   stall (4 ms a second), not below the start. Frames beyond the target are caught up
//!   by the speaker ([`Jitter::excess`]): silent ones skipped, others two played as one.
//! - Duplicates (a resume's `have` was older than what arrived) and frames after their
//!   turn are dropped, counted. Beyond [`MAX_FRAMES`] waiting the oldest go.
//! - **Numbers starting again** (a new replay's count from 0, on the same stream): a frame
//!   before the next one's turn while nothing waits and the stream is quiet, after a
//!   replay's end, or more than [`MAX_FRAMES`] before it, starts the stream afresh.
//!
//! `seq` is the talker's u32 count, compared as serial numbers (it may wrap): kept as a
//! 64-bit count extended from the frame expected next.

use std::collections::BTreeMap;

/// Frames waiting at most (60 s: a resume's backlog is the site's `store_forward_s`, 30
/// s by default, at most 300).
pub const MAX_FRAMES: usize = 3_000;
/// Quiet this long (frames) after an underrun is the end of a talk, not a stall: 1 s.
pub const IDLE_FRAMES: u32 = 50;
/// What one stall grows the target by at most (frames): 100 ms. (A talk that ends and
/// another that starts within a second reads as a stall; this bounds what that costs.)
pub const GROW_MAX: u32 = 5;
/// Frames played without a stall per frame the target shrinks: 5 s.
pub const SHRINK_EVERY: u32 = 250;

/// What to play next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pop {
    /// Nothing (not started, or the talk ended).
    Silence,
    /// This packet.
    Play(Vec<u8>),
    /// Frames lost; the packet after them (its FEC rebuilds the last one lost).
    Lost(Option<Vec<u8>>),
    /// Nothing yet: concealed, play waits (the `n`th frame of this wait).
    Stall(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Frames came; pulls since the first.
    Priming(u32),
    Playing,
    /// An underrun; pulls since.
    Stalled(u32),
}

/// Counters since the start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub packets: u64,
    pub duplicates: u64,
    pub late: u64,
    pub lost: u64,
    pub stalls: u64,
    pub dropped: u64,
}

#[derive(Debug)]
pub struct Jitter {
    /// By extended `seq`.
    q: BTreeMap<u64, Vec<u8>>,
    /// The next to play (once started), and the last one taken in (for extending).
    next: Option<u64>,
    last: Option<u64>,
    state: State,
    /// Frames.
    target: u32,
    start: u32,
    max: u32,
    steady: u32,
    /// A replay ended: the next frame may start the numbers again.
    ended: bool,
    pub counters: Counters,
}

/// Milliseconds as frames (at least one).
fn frames(ms: u32) -> u32 {
    (ms / crate::FRAME_MS).max(1)
}

impl Jitter {
    /// Starting at `start_ms`, growing to `max_ms` at most.
    pub fn new(start_ms: u32, max_ms: u32) -> Jitter {
        let start = frames(start_ms);
        Jitter {
            q: BTreeMap::new(),
            next: None,
            last: None,
            state: State::Idle,
            target: start,
            start,
            max: frames(max_ms).max(start),
            steady: 0,
            ended: false,
            counters: Counters::default(),
        }
    }

    /// New settings (a channel's changed); the target kept within them.
    pub fn set_limits(&mut self, start_ms: u32, max_ms: u32) {
        self.start = frames(start_ms);
        self.max = frames(max_ms).max(self.start);
        self.target = self.target.clamp(self.start, self.max);
    }

    /// `seq` as a 64-bit count, the nearest to the one expected (serial numbers: within
    /// 2³¹ either way).
    fn extend(&self, seq: u32) -> u64 {
        match self.next.or(self.last) {
            // (Far from 0, so a count before it never goes below.)
            None => (1u64 << 32) + u64::from(seq),
            Some(r) => {
                let d = seq.wrapping_sub(r as u32) as i32;
                (r as i64 + i64::from(d)) as u64
            }
        }
    }

    /// The stream afresh: a new count (a replay again).
    fn reset(&mut self) {
        self.q.clear();
        self.next = None;
        self.last = None;
        self.state = State::Idle;
        self.ended = false;
    }

    /// A replay's last frame came: the next one may count from 0 again.
    pub fn end(&mut self) {
        self.ended = true;
    }

    /// A packet came.
    pub fn push(&mut self, seq: u32, packet: Vec<u8>) {
        if self.ended && self.q.is_empty() {
            self.reset();
        }
        let mut s = self.extend(seq);
        if let Some(n) = self.next
            && s < n
        {
            let quiet = self.q.is_empty() && matches!(self.state, State::Idle | State::Stalled(_));
            if quiet || n - s > MAX_FRAMES as u64 {
                // Numbers starting again: a fresh stream.
                self.reset();
                s = self.extend(seq);
            } else {
                self.counters.late += 1;
                return;
            }
        }
        if self.q.contains_key(&s) {
            self.counters.duplicates += 1;
            return;
        }
        self.counters.packets += 1;
        self.q.insert(s, packet);
        self.last = Some(self.last.map_or(s, |l| l.max(s)));
        while self.q.len() > MAX_FRAMES {
            if let Some((k, _)) = self.q.pop_first() {
                self.counters.dropped += 1;
                if self.next.is_some_and(|n| n <= k) {
                    self.next = Some(k + 1);
                }
            }
        }
        if self.state == State::Idle {
            self.state = State::Priming(0);
        }
    }

    /// The next frame's turn.
    pub fn pop(&mut self) -> Pop {
        match self.state {
            State::Idle => Pop::Silence,
            State::Priming(waited) => {
                let waited = waited + 1;
                if self.q.len() as u32 >= self.target || waited >= self.target {
                    self.next = self.q.keys().next().copied();
                    self.state = State::Playing;
                    self.play()
                } else {
                    self.state = State::Priming(waited);
                    Pop::Silence
                }
            }
            State::Playing => self.play(),
            State::Stalled(waited) => {
                let Some(&first) = self.q.keys().next() else {
                    let waited = waited + 1;
                    if waited > IDLE_FRAMES {
                        // The talk ended: quiet, the target halfway back to its start.
                        self.state = State::Idle;
                        self.next = None;
                        self.target = self.start + (self.target - self.start) / 2;
                        return Pop::Silence;
                    }
                    self.state = State::Stalled(waited);
                    return Pop::Stall(waited);
                };
                // Frames again within the second: a stall. Play on from the first that
                // came (any before it never will), keeping more.
                self.counters.stalls += 1;
                self.target = (self.target + waited.min(GROW_MAX)).min(self.max);
                self.steady = 0;
                if let Some(n) = self.next
                    && first > n
                {
                    self.counters.lost += first - n;
                }
                self.next = Some(first);
                self.state = State::Playing;
                self.play()
            }
        }
    }

    fn play(&mut self) -> Pop {
        let Some(n) = self.next else {
            self.state = State::Idle;
            return Pop::Silence;
        };
        if let Some(p) = self.q.remove(&n) {
            self.next = Some(n + 1);
            self.steady += 1;
            if self.steady >= SHRINK_EVERY {
                self.steady = 0;
                self.target = self.target.saturating_sub(1).max(self.start);
            }
            return Pop::Play(p);
        }
        if let Some(&k) = self.q.keys().next() {
            // A gap (none before n waits: those went late). On a reliable link it never
            // fills: jump to k, concealing once with its FEC.
            self.counters.lost += k - n;
            self.next = Some(k);
            return Pop::Lost(self.q.get(&k).cloned());
        }
        self.state = State::Stalled(1);
        Pop::Stall(1)
    }

    /// Frames waiting beyond the target (to catch up on).
    pub fn excess(&self) -> u32 {
        (self.q.len() as u32).saturating_sub(self.target)
    }

    /// Frames waiting.
    pub fn waiting(&self) -> u32 {
        self.q.len() as u32
    }

    /// The target, frames.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Nothing waiting and not playing.
    pub fn idle(&self) -> bool {
        self.state == State::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    fn seq_of(p: &Pop) -> Option<u32> {
        match p {
            Pop::Play(b) => Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            _ => None,
        }
    }

    #[test]
    fn it_starts_at_its_target_and_plays_in_order_whatever_the_arrival_order() {
        let mut j = Jitter::new(100, 2000);
        assert_eq!(j.target(), 5);
        for s in [12, 10, 11, 14, 13] {
            j.push(s, pkt(s));
        }
        let got: Vec<_> = (0..5).map(|_| seq_of(&j.pop())).collect();
        assert_eq!(got, [Some(10), Some(11), Some(12), Some(13), Some(14)]);
        // A frame after its turn is dropped, as is one again.
        j.push(11, pkt(11));
        j.push(16, pkt(16));
        j.push(16, pkt(16));
        assert_eq!((j.counters.late, j.counters.duplicates), (1, 1));
    }

    #[test]
    fn a_short_talk_is_played_after_the_target_s_time() {
        let mut j = Jitter::new(200, 2000);
        j.push(1, pkt(1));
        j.push(2, pkt(2));
        for _ in 0..9 {
            assert_eq!(j.pop(), Pop::Silence);
        }
        assert_eq!(seq_of(&j.pop()), Some(1));
        assert_eq!(seq_of(&j.pop()), Some(2));
    }

    #[test]
    fn a_gap_is_jumped_once_with_the_next_frame_s_fec() {
        let mut j = Jitter::new(60, 2000);
        for s in [1, 2, 4, 5] {
            j.push(s, pkt(s));
        }
        assert_eq!(seq_of(&j.pop()), Some(1));
        assert_eq!(seq_of(&j.pop()), Some(2));
        assert_eq!(j.pop(), Pop::Lost(Some(pkt(4))));
        assert_eq!(seq_of(&j.pop()), Some(4));
        // Two missing: one concealment, not two (they never come over a reliable link).
        j.push(8, pkt(8));
        assert_eq!(seq_of(&j.pop()), Some(5));
        assert_eq!(j.pop(), Pop::Lost(Some(pkt(8))));
        assert_eq!(seq_of(&j.pop()), Some(8));
        assert_eq!(j.counters.lost, 3);
        assert_eq!(j.target(), 3, "losses don't grow it");
    }

    #[test]
    fn a_stall_waits_without_moving_on_and_grows_the_target() {
        let mut j = Jitter::new(60, 200);
        for s in 0..3 {
            j.push(s, pkt(s));
        }
        for s in 0..3 {
            assert_eq!(seq_of(&j.pop()), Some(s));
        }
        // The link stalls 4 frames; then everything it held comes at once.
        for n in 1..=4 {
            assert_eq!(j.pop(), Pop::Stall(n));
        }
        for s in 3..9 {
            j.push(s, pkt(s));
        }
        assert_eq!(seq_of(&j.pop()), Some(3), "nothing skipped");
        assert_eq!(j.counters.stalls, 1);
        assert_eq!(j.target(), 3 + 4);
        // Another long stall: grown by GROW_MAX at most, and never past the maximum.
        for s in 4..9 {
            assert_eq!(seq_of(&j.pop()), Some(s));
        }
        for _ in 0..20 {
            j.pop();
        }
        j.push(9, pkt(9));
        assert_eq!(seq_of(&j.pop()), Some(9));
        assert_eq!(j.target(), 10, "200 ms at most");
        // Out of a stall with a gap: from the first frame that came.
        j.pop();
        j.push(12, pkt(12));
        assert_eq!(seq_of(&j.pop()), Some(12));
        assert_eq!(j.counters.lost, 2);
    }

    #[test]
    fn a_talk_that_ended_starts_afresh_and_steady_play_shrinks_the_target() {
        let mut j = Jitter::new(40, 2000);
        j.push(1, pkt(1));
        j.push(2, pkt(2));
        j.pop();
        j.pop();
        for _ in 0..=IDLE_FRAMES {
            j.pop();
        }
        assert!(j.idle());
        assert_eq!(j.counters.stalls, 0);
        // The next talk: primed again from its own first frame.
        j.push(100, pkt(100));
        assert_eq!(j.pop(), Pop::Silence);
        assert_eq!(seq_of(&j.pop()), Some(100));
        // Grown by a stall, then steady for a while: back down, not below the start.
        let mut j = Jitter::new(40, 2000);
        j.push(0, pkt(0));
        j.push(1, pkt(1));
        j.pop();
        j.pop();
        j.pop();
        j.pop();
        j.pop();
        let mut s = 2;
        j.push(s, pkt(s));
        j.pop();
        assert_eq!(j.target(), 2 + 3);
        for _ in 0..SHRINK_EVERY * 5 {
            s += 1;
            j.push(s, pkt(s));
            assert!(seq_of(&j.pop()).is_some());
        }
        assert_eq!(j.target(), 2);
        // Grown again, then the talk ends: halfway back.
        let mut j = Jitter::new(40, 2000);
        j.push(0, pkt(0));
        j.pop();
        j.pop();
        for _ in 0..5 {
            j.pop();
        }
        j.push(1, pkt(1));
        j.pop();
        assert_eq!(j.target(), 2 + 5);
        for _ in 0..=IDLE_FRAMES + 1 {
            j.pop();
        }
        assert!(j.idle());
        assert_eq!(j.target(), 2 + 2);
    }

    #[test]
    fn a_backlog_plays_at_once_and_is_bounded() {
        let mut j = Jitter::new(200, 2000);
        for s in 0..1500 {
            j.push(s, pkt(s));
        }
        assert_eq!(seq_of(&j.pop()), Some(0));
        assert_eq!(j.excess(), 1499 - 10);
        for s in 1500..(MAX_FRAMES as u32 + 1600) {
            j.push(s, pkt(s));
        }
        assert_eq!(j.waiting() as usize, MAX_FRAMES);
        assert!(j.counters.dropped > 0);
        // Play went on from the oldest kept.
        let first = seq_of(&j.pop()).unwrap();
        assert_eq!(first, MAX_FRAMES as u32 + 1600 - MAX_FRAMES as u32);
    }

    #[test]
    fn numbers_that_wrap_play_in_order() {
        let mut j = Jitter::new(60, 2000);
        for s in [u32::MAX - 1, u32::MAX, 0, 1] {
            j.push(s, pkt(s));
        }
        let got: Vec<_> = (0..4).map(|_| seq_of(&j.pop())).collect();
        assert_eq!(got, [Some(u32::MAX - 1), Some(u32::MAX), Some(0), Some(1)]);
        assert_eq!(j.counters.late, 0);
    }

    #[test]
    fn a_replay_counting_from_zero_again_is_played() {
        let mut j = Jitter::new(20, 2000);
        for s in 0..5 {
            j.push(s, pkt(s));
        }
        for s in 0..5 {
            assert_eq!(seq_of(&j.pop()), Some(s));
        }
        // The replay ends; the next one counts from 0 on the same stream.
        j.end();
        j.pop();
        j.push(0, pkt(0));
        j.push(1, pkt(1));
        assert_eq!(seq_of(&j.pop()), Some(0));
        assert_eq!(seq_of(&j.pop()), Some(1));
        assert_eq!(j.counters.late, 0);
        // Without the end, a stream gone quiet takes a count started again too.
        for _ in 0..3 {
            j.pop();
        }
        j.push(0, pkt(0));
        assert_eq!(seq_of(&j.pop()), Some(0));
    }
}
