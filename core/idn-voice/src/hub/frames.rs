//! Sound waiting for one session's WebSocket (§V.8), apart from its JSON: each frame kept
//! for its window (a live link's `ws_queue_s`; a handset's the longer of that and its
//! channel's store-and-forward window, §V.13.6), the oldest going first past it, counted.
//! A handset's frames are kept after they are sent too, for their window: a link that
//! died without a word (cellular: no FIN, noticed only at the idle timeout) swallowed
//! what was written into it, and a resume sends again whatever the client says it didn't
//! get (`have`). Frame bodies are shared by every listener (`Arc<[u8]>`), and every
//! queue on the site counts against one byte budget (`frame_budget_mb`).

use super::Sid;
use idn_common::sync::LockOk as _;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Frames one session's queue holds at most, whatever its windows.
pub const FRAME_QUEUE_MAX: usize = 20_000;
/// What a queued frame costs the budget beyond its body (its header, its place).
const OVERHEAD: u64 = 64;

/// The site's byte budget for queued sound (all sessions together).
#[derive(Debug, Default)]
pub struct Budget {
    used: AtomicU64,
    limit: AtomicU64,
    /// Frames dropped site-wide because the budget was spent.
    pub dropped: AtomicU64,
}

impl Budget {
    pub fn new(limit_bytes: u64) -> Budget {
        let b = Budget::default();
        b.limit.store(limit_bytes, Ordering::Relaxed);
        b
    }

    pub fn set_limit(&self, bytes: u64) {
        self.limit.store(bytes, Ordering::Relaxed);
    }

    /// Bytes queued now.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn over(&self, more: u64) -> bool {
        self.used.load(Ordering::Relaxed) + more > self.limit.load(Ordering::Relaxed)
    }
}

/// One frame for one listener.
pub struct Queued {
    pub kind: u8,
    pub talker: Sid,
    pub channel: u32,
    pub seq: u32,
    /// The Opus packet, shared by every listener of it.
    pub body: Arc<[u8]>,
    pub at: Instant,
    pub keep: Duration,
    /// Kept after it is sent, for a resume to send again (a handset's).
    pub retain: bool,
    pub sent: bool,
}

impl Queued {
    fn cost(&self) -> u64 {
        self.body.len() as u64 + OVERHEAD
    }

    fn expired(&self) -> bool {
        self.at.elapsed() > self.keep
    }

    /// As the client gets it: kind, talker, channel, seq (little-endian), the packet.
    fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(13 + self.body.len());
        b.push(self.kind);
        b.extend_from_slice(&self.talker.to_le_bytes());
        b.extend_from_slice(&self.channel.to_le_bytes());
        b.extend_from_slice(&self.seq.to_le_bytes());
        b.extend_from_slice(&self.body);
        b
    }
}

#[derive(Default)]
struct Ring {
    q: VecDeque<Queued>,
    /// Every frame before it is sent; from it on, some may be (a resume's `have`).
    cursor: usize,
    bytes: u64,
}

/// One session's queue (one per connection: a resumed handset's moves to a new one).
pub struct FrameQueue {
    r: Mutex<Ring>,
    budget: Arc<Budget>,
    pub ready: tokio::sync::Notify,
    /// Frames this session lost: past their window unsent, or the budget spent.
    pub dropped: AtomicU64,
}

impl FrameQueue {
    pub fn new(budget: Arc<Budget>) -> FrameQueue {
        FrameQueue {
            r: Mutex::default(),
            budget,
            ready: tokio::sync::Notify::new(),
            dropped: AtomicU64::new(0),
        }
    }

    /// Takes the front frame off; whether it went unsent (lost).
    fn pop(&self, r: &mut Ring) -> Option<bool> {
        let e = r.q.pop_front()?;
        r.cursor = r.cursor.saturating_sub(1);
        r.bytes -= e.cost();
        self.budget.used.fetch_sub(e.cost(), Ordering::Relaxed);
        Some(!e.sent)
    }

    /// Off the front: what is past its window, and what was sent and isn't kept. Frames
    /// lost (unsent).
    fn prune(&self, r: &mut Ring) -> u64 {
        let mut lost = 0;
        while r
            .q
            .front()
            .is_some_and(|e| e.expired() || (e.sent && !e.retain))
        {
            if self.pop(r) == Some(true) {
                lost += 1;
            }
        }
        lost
    }

    fn count(&self, lost: u64) -> u64 {
        if lost > 0 {
            self.dropped.fetch_add(lost, Ordering::Relaxed);
        }
        lost
    }

    /// Queues a frame: frames lost for it (past their windows, the queue's bound, the
    /// site's budget; this one too if the budget has no room even then).
    pub fn push(&self, e: Queued) -> u64 {
        let mut r = self.r.lock_ok();
        let mut lost = self.prune(&mut r);
        let c = e.cost();
        while !r.q.is_empty() && (r.q.len() >= FRAME_QUEUE_MAX || self.budget.over(c)) {
            if self.budget.over(c) {
                self.budget.dropped.fetch_add(1, Ordering::Relaxed);
            }
            if self.pop(&mut r) == Some(true) {
                lost += 1;
            }
        }
        if self.budget.over(c) {
            // (Others' queues hold the budget: this frame goes.)
            self.budget.dropped.fetch_add(1, Ordering::Relaxed);
            drop(r);
            return self.count(lost + 1);
        }
        r.bytes += c;
        self.budget.used.fetch_add(c, Ordering::Relaxed);
        r.q.push_back(e);
        drop(r);
        self.ready.notify_one();
        self.count(lost)
    }

    /// The next frame to send, marked sent (frames past their window skipped, counted).
    pub fn take(&self) -> Option<Vec<u8>> {
        let mut r = self.r.lock_ok();
        let mut lost = self.prune(&mut r);
        let mut out = None;
        while r.cursor < r.q.len() {
            let i = r.cursor;
            r.cursor += 1;
            let e = &mut r.q[i];
            if e.sent {
                continue;
            }
            e.sent = true;
            if e.expired() {
                e.retain = false;
                lost += 1;
                continue;
            }
            out = Some(e.bytes());
            break;
        }
        lost += self.prune(&mut r);
        drop(r);
        self.count(lost);
        out
    }

    /// Frames not yet sent.
    pub fn pending(&self) -> usize {
        let r = self.r.lock_ok();
        r.q.iter().skip(r.cursor).filter(|e| !e.sent).count()
    }

    /// A resumed handset's new queue with what `old` holds, past-window frames dropped.
    /// `have` (the client's last `seq` from each talker): what it got is sent, the rest —
    /// written into a link that died, or never sent — is sent again, in order. Without it,
    /// what was sent counts as received.
    pub fn take_from(old: &FrameQueue, have: Option<&HashMap<Sid, u32>>) -> FrameQueue {
        let q = FrameQueue::new(old.budget.clone());
        let mut ring = std::mem::take(&mut *old.r.lock_ok());
        let lost = old.prune(&mut ring);
        if let Some(have) = have {
            for e in ring.q.iter_mut() {
                e.sent = have.get(&e.talker).is_some_and(|s| e.seq <= *s);
            }
            ring.cursor = 0;
        }
        q.dropped.store(
            old.dropped.load(Ordering::Relaxed) + lost,
            Ordering::Relaxed,
        );
        *q.r.lock_ok() = ring;
        // (What waits goes out as soon as the new connection's writer starts.)
        q.ready.notify_one();
        q
    }
}

impl Drop for FrameQueue {
    fn drop(&mut self) {
        let r = self.r.lock_ok();
        self.budget.used.fetch_sub(r.bytes, Ordering::Relaxed);
    }
}

/// A resume's `have`: `TALKER:SEQ,…` (at most 256 talkers); anything else left out.
pub fn parse_have(s: &str) -> HashMap<Sid, u32> {
    s.split(',')
        .take(256)
        .filter_map(|p| {
            let (t, q) = p.trim().split_once(':')?;
            Some((t.parse().ok()?, q.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(talker: Sid, seq: u32, keep_ms: u64, retain: bool) -> Queued {
        Queued {
            kind: 1,
            talker,
            channel: 3,
            seq,
            body: Arc::from(&[0xF8u8, seq as u8][..]),
            at: Instant::now(),
            keep: Duration::from_millis(keep_ms),
            retain,
            sent: false,
        }
    }

    /// Frames past their window are dropped when taken and when moved to a resumed
    /// connection, not only when another is queued (bug run: stale sound on resume).
    #[test]
    fn past_their_window_frames_are_never_sent() {
        let b = Arc::new(Budget::new(1 << 20));
        let q = FrameQueue::new(b.clone());
        q.push(frame(1, 1, 30, true));
        q.push(frame(1, 2, 60_000, true));
        std::thread::sleep(Duration::from_millis(60));
        let f = q.take().unwrap();
        assert_eq!(f[9..13], 2u32.to_le_bytes());
        assert_eq!(q.dropped.load(Ordering::Relaxed), 1);
        let q2 = FrameQueue::new(b.clone());
        q2.push(frame(1, 1, 30, true));
        std::thread::sleep(Duration::from_millis(60));
        let moved = FrameQueue::take_from(&q2, None);
        assert_eq!(moved.pending(), 0);
        assert!(moved.take().is_none());
        drop((q, q2, moved));
        assert_eq!(b.used(), 0);
    }

    /// A handset's sent frames are kept: a resume with `have` sends again what the client
    /// didn't get, in order.
    #[test]
    fn a_resume_sends_again_what_the_client_did_not_get() {
        let b = Arc::new(Budget::new(1 << 20));
        let q = FrameQueue::new(b.clone());
        for s in 1..=4 {
            q.push(frame(7, s, 60_000, true));
        }
        q.push(frame(9, 1, 60_000, true));
        for _ in 0..5 {
            q.take().unwrap();
        }
        assert_eq!(q.pending(), 0);
        let have = parse_have("7:2, 9:1, junk");
        let r = FrameQueue::take_from(&q, Some(&have));
        assert_eq!(r.pending(), 2);
        assert_eq!(r.take().unwrap()[9..13], 3u32.to_le_bytes());
        assert_eq!(r.take().unwrap()[9..13], 4u32.to_le_bytes());
        assert!(r.take().is_none());
    }

    /// The site's budget: past it the queue's oldest go, counted.
    #[test]
    fn the_budget_holds() {
        let b = Arc::new(Budget::new(3 * (OVERHEAD + 2)));
        let q = FrameQueue::new(b.clone());
        for s in 1..=5 {
            q.push(frame(1, s, 60_000, false));
        }
        assert_eq!(q.pending(), 3);
        assert_eq!(q.dropped.load(Ordering::Relaxed), 2);
        assert!(b.dropped.load(Ordering::Relaxed) >= 2);
        assert_eq!(q.take().unwrap()[9..13], 3u32.to_le_bytes());
    }
}
