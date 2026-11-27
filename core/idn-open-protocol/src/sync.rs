//! `sync/1` (spec/extensions/idn-extensions.md §E.14): frame-accurate presentation.
//! Displays in the same sync group show each frame at the same moment: at its sender's
//! timestamp, converted to the receiver's clock, plus the group's delay.
//!
//! Turn it on per input with [`InputConfig::sync`](crate::input::InputConfig::sync):
//!
//! ```no_run
//! use idn_open_protocol::input::{Input, InputConfig};
//! use idn_open_protocol::sync::{SyncConfig, SyncDelay};
//! # fn main() -> std::io::Result<()> {
//! let left = Input::open(InputConfig {
//!     sync: Some(SyncConfig::new("stage", SyncDelay::Auto)),
//!     ..InputConfig::new("SERVER (Stage left)")
//! })?;
//! let right = Input::open(InputConfig {
//!     sync: Some(SyncConfig::new("stage", SyncDelay::Auto)),
//!     ..InputConfig::new("SERVER (Stage right)")
//! })?;
//! // Each input hands over every frame at its presentation time: show what arrives.
//! # Ok(())
//! # }
//! ```

use crate::input::InputEvent;
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The extension token (§E.14).
pub const TOKEN: &str = "sync/1";
/// Longest sync group name, in characters.
pub const MAX_GROUP: usize = 64;

/// How long a synced display waits before showing a frame (§E.14.2). Every display of
/// a group should use the same setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDelay {
    /// A fixed number of frame periods of the source (for example 2, 3 or 5).
    Frames(u32),
    /// A fixed delay in milliseconds.
    Millis(u32),
    /// The smallest delay that lets every display of the group show every frame on
    /// time, agreed through the sender (§E.14.4).
    Auto,
}

/// How the sender's clock is related to this machine's (§E.14.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncClock {
    /// Measured with pings (works anywhere).
    #[default]
    Measured,
    /// The machines' clocks are synchronised already (PTP, or good NTP): no offset.
    System,
}

/// Frame-accurate presentation for one input (§E.14). `None` in
/// [`InputConfig::sync`](crate::input::InputConfig::sync) is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// The group this display belongs to (`stage`, `screens`, …): displays of a group are
    /// shown in step; groups are independent. At most [`MAX_GROUP`] characters.
    pub group: String,
    /// How long to wait before showing a frame.
    pub delay: SyncDelay,
    /// How the sender's clock is known.
    pub clock: SyncClock,
}

impl SyncConfig {
    /// Sync in `group` with `delay`, measuring the clock offset.
    pub fn new(group: &str, delay: SyncDelay) -> Self {
        SyncConfig {
            group: group.chars().take(MAX_GROUP).collect(),
            delay,
            clock: SyncClock::Measured,
        }
    }
}

/// Sync figures for [`InputStats::sync`](crate::input::InputStats::sync) (§E.14.6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncStats {
    /// `fixed`, `auto`, or `unclocked` (the source doesn't do `sync/1`: frames wait the
    /// fixed delay from their arrival).
    pub mode: &'static str,
    /// The group.
    pub group: String,
    /// The delay in use, in milliseconds.
    pub delay_ms: f64,
    /// The sender's clock minus this one, in microseconds, once measured.
    pub offset_us: Option<i64>,
    /// The spread of the offset samples used, in microseconds: the measurement's
    /// uncertainty.
    pub offset_spread_us: Option<i64>,
    /// What this display needs (auto mode's input), in milliseconds.
    pub need_ms: Option<f64>,
    /// Frames ready after their presentation time, dropped to stay in step.
    pub late: usize,
}

/// Microseconds since the Unix epoch, now, on the clock frames are stamped with (site
/// time while the machine's site clock is usable, else the system's): the system's clock
/// here measured the offset between two system clocks while frames carried site time, and
/// a sender's system clock off the site's had every frame dropped as late (bug run
/// 2026-11-25).
fn wall_us() -> i64 {
    crate::api::timestamp_now() / 10
}

/// A clock step (§2.7): a frame more than this far from its expected time.
const STEP_US: i64 = 2_000_000;
/// The need is the 99th percentile of lateness over this window, plus a frame.
const NEED_WINDOW: Duration = Duration::from_secs(10);
/// Delay used in auto mode before anything is known: three frame periods.
const START_FRAMES: f64 = 3.0;
/// The longest group delay, in milliseconds (announced or reported).
const MAX_DELAY_MS: f64 = 10_000.0;
/// The default cap on one receiver's reported need (§E.14.4), in milliseconds.
const DEFAULT_MAX_NEED_MS: f64 = 2_000.0;
/// The configured cap, as f64 bits; `u64::MAX` means "read the environment first".
static MAX_NEED_BITS: AtomicU64 = AtomicU64::new(u64::MAX);

/// The largest need a receiver's report counts for (§E.14.4): one slow or broken display
/// must not hold its whole sync group far behind. Configurable with
/// `IDN_SYNC_MAX_NEED_MS` (or [`set_max_need_ms`]); the default is 2 s, and it is held to
/// the overall [`MAX_DELAY_MS`] ceiling. Set it to `MAX_DELAY_MS` for effectively no cap.
pub fn max_need_ms() -> f64 {
    let bits = MAX_NEED_BITS.load(Ordering::Relaxed);
    if bits != u64::MAX {
        return f64::from_bits(bits);
    }
    let v = std::env::var("IDN_SYNC_MAX_NEED_MS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map_or(DEFAULT_MAX_NEED_MS, |v| v.clamp(0.0, MAX_DELAY_MS));
    MAX_NEED_BITS.store(v.to_bits(), Ordering::Relaxed);
    v
}

/// Sets the per-receiver need cap (§E.14.4), in milliseconds, held to the [`MAX_DELAY_MS`]
/// ceiling. A Node or panel can call this to override `IDN_SYNC_MAX_NEED_MS`.
pub fn set_max_need_ms(ms: f64) {
    let v = if ms.is_finite() {
        ms.clamp(0.0, MAX_DELAY_MS)
    } else {
        MAX_DELAY_MS
    };
    MAX_NEED_BITS.store(v.to_bits(), Ordering::Relaxed);
}
/// A frame period from a source's frame rate, held to 0.5 ms–1 s: a source declaring
/// 1/4,000,000,000 fps made a synced display hold every frame for ages (memory).
fn frame_period_ms(n: u32, d: u32) -> f64 {
    (1000.0 * d as f64 / n as f64).clamp(0.5, 1000.0)
}

struct Held {
    at: Instant,
    seq: u64,
    event: InputEvent,
}

impl PartialEq for Held {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Held {}
impl PartialOrd for Held {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Held {
    /// Earliest first in a max-heap.
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

/// One synced input's state: clock offset, delay, held frames.
pub(crate) struct Receiver {
    cfg: SyncConfig,
    /// The source lists `sync/1`: its timestamps can be related to this clock.
    peer_clocked: bool,
    /// Pings sent: (id, this machine's wall clock then).
    pings: VecDeque<(u64, i64)>,
    /// (round trip µs, offset µs), newest last.
    samples: VecDeque<(i64, i64)>,
    /// The group's delay as the sender announced it (auto).
    announced_ms: Option<f64>,
    /// How late frames were when ready, in ms, with when.
    lateness: VecDeque<(Instant, f64)>,
    frame_ms: f64,
    held: BinaryHeap<Held>,
    /// Held events let go at a clock step, in order: handed over before the stepped one
    /// ([`Receiver::take_flushed`]; bug run 2026-11-24: they came after it).
    flushed: Vec<InputEvent>,
    seq: u64,
    late: usize,
    last_report: Instant,
    /// The last first field's timestamp, for the second field's half-frame offset.
    field1_ts: Option<i64>,
}

impl Receiver {
    pub(crate) fn new(cfg: SyncConfig) -> Self {
        Receiver {
            cfg,
            peer_clocked: false,
            pings: VecDeque::new(),
            samples: VecDeque::new(),
            announced_ms: None,
            lateness: VecDeque::new(),
            frame_ms: 1000.0 / 30.0,
            held: BinaryHeap::new(),
            flushed: Vec::new(),
            seq: 0,
            late: 0,
            last_report: Instant::now(),
            field1_ts: None,
        }
    }

    /// A new session: the source (and its clock) may have changed.
    pub(crate) fn new_session(&mut self) {
        self.peer_clocked = false;
        self.pings.clear();
        self.samples.clear();
        self.announced_ms = None;
        self.field1_ts = None;
    }

    /// The source's `ndi_version` said whether it does `sync/1`.
    pub(crate) fn set_peer(&mut self, clocked: bool) {
        self.peer_clocked = clocked;
    }

    fn clocked(&self) -> bool {
        self.peer_clocked
    }

    pub(crate) fn ping_sent(&mut self, id: u64) {
        self.pings.push_back((id, wall_us()));
        while self.pings.len() > 32 {
            self.pings.pop_front();
        }
    }

    /// An answer to ping `id` with round trip `rtt_us` and the responder's wall clock
    /// `rt_us` when it answered (§E.14.3).
    pub(crate) fn pong(&mut self, id: u64, rtt_us: i64, rt_us: i64) {
        let Some(&(_, sent)) = self.pings.iter().find(|(i, _)| *i == id) else {
            return;
        };
        // Numbers from the peer: one that overflows, or is hours off, is no sample
        // (i64::MIN panicked a debug build's input).
        let Some(offset) = sent
            .checked_add(rtt_us / 2)
            .and_then(|mid| rt_us.checked_sub(mid))
            .filter(|o| o.unsigned_abs() < 24 * 3600 * 1_000_000 && rtt_us >= 0)
        else {
            return;
        };
        self.samples.push_back((rtt_us, offset));
        while self.samples.len() > 16 {
            self.samples.pop_front();
        }
    }

    /// The offset (sender minus this clock) and its spread: the median of the eight
    /// samples with the shortest round trips, so that a slow one doesn't move it.
    fn offset(&self) -> Option<(i64, i64)> {
        if self.cfg.clock == SyncClock::System {
            return Some((0, 0));
        }
        if self.samples.is_empty() {
            return None;
        }
        let mut best: Vec<(i64, i64)> = self.samples.iter().copied().collect();
        best.sort_by_key(|(rtt, _)| *rtt);
        best.truncate(8);
        let mut offs: Vec<i64> = best.iter().map(|(_, o)| *o).collect();
        offs.sort_unstable();
        Some((offs[offs.len() / 2], offs[offs.len() - 1] - offs[0]))
    }

    /// The group's announced delay (auto mode, §E.14.4).
    pub(crate) fn announced(&mut self, group: &str, delay_ms: f64) {
        if group == self.cfg.group && delay_ms.is_finite() && delay_ms >= 0.0 {
            self.announced_ms = Some(delay_ms.min(MAX_DELAY_MS));
        }
    }

    /// What this display needs: the 99th percentile of lateness, plus a frame period.
    fn need_ms(&self) -> Option<f64> {
        if self.lateness.len() < 5 {
            return None;
        }
        let mut v: Vec<f64> = self.lateness.iter().map(|(_, l)| *l).collect();
        v.sort_by(f64::total_cmp);
        let p99 = v[((v.len() - 1) as f64 * 0.99).round() as usize];
        Some(p99.max(0.0) + self.frame_ms)
    }

    /// The delay in use, in milliseconds.
    fn delay_ms(&self) -> f64 {
        match self.cfg.delay {
            SyncDelay::Frames(n) => n as f64 * self.frame_ms,
            SyncDelay::Millis(m) => m as f64,
            SyncDelay::Auto if !self.clocked() => START_FRAMES * self.frame_ms,
            SyncDelay::Auto => self
                .announced_ms
                .or_else(|| self.need_ms())
                .unwrap_or(START_FRAMES * self.frame_ms),
        }
    }

    /// Decides when a decoded frame or audio block is shown: `Some(event)` to hand over
    /// now, `None` if it is held (released by [`Receiver::due`]) or dropped as late.
    /// `video`: a picture (late ones are dropped); otherwise audio (late is shown).
    pub(crate) fn schedule(
        &mut self,
        event: InputEvent,
        video: bool,
        timestamp: i64,
        frame_rate: Option<(u32, u32)>,
        field_id: u32,
        received: Instant,
    ) -> Option<InputEvent> {
        if let Some((n, d)) = frame_rate.filter(|(n, d)| *n > 0 && *d > 0) {
            self.frame_ms = frame_period_ms(n, d);
        }
        let now = Instant::now();
        let delay_us = (self.delay_ms() * 1000.0) as i64;
        let at = match self.offset().filter(|_| self.clocked()) {
            Some((offset, _)) => {
                let now_wall = wall_us();
                let mut local_ts = (timestamp / 10).saturating_sub(offset);
                // The second field of a pair with one timestamp: half a frame later.
                if field_id == 1 {
                    self.field1_ts = Some(timestamp);
                } else if field_id == 2 && self.field1_ts == Some(timestamp) {
                    local_ts += (self.frame_ms * 500.0) as i64;
                }
                if video {
                    self.lateness
                        .push_back((now, now_wall.saturating_sub(local_ts) as f64 / 1000.0));
                    while self
                        .lateness
                        .front()
                        .is_some_and(|(t, _)| now.duration_since(*t) > NEED_WINDOW)
                    {
                        self.lateness.pop_front();
                    }
                }
                let wait = local_ts.saturating_add(delay_us).saturating_sub(now_wall);
                if wait.unsigned_abs() > (STEP_US + delay_us).unsigned_abs() {
                    // A clock step (§2.7): show it, and measure again; what was held goes
                    // first, in order.
                    self.samples.clear();
                    self.lateness.clear();
                    while let Some(h) = self.held.pop() {
                        self.flushed.push(h.event);
                    }
                    return Some(event);
                }
                if wait <= 0 {
                    if video {
                        // Late: dropped so this display stays in step (§E.14.5).
                        self.late += 1;
                        return None;
                    }
                    return Some(event); // audio is never dropped: a gap is worse
                }
                now + Duration::from_micros(wait as u64)
            }
            // Unclocked, or no offset measured yet: the delay from arrival.
            None => received + Duration::from_micros(delay_us.max(0) as u64),
        };
        if at <= now {
            return Some(event);
        }
        self.seq += 1;
        self.held.push(Held {
            at,
            seq: self.seq,
            event,
        });
        None
    }

    /// The held events let go at a clock step, in order: hand them over before the event
    /// [`schedule`](Self::schedule) returned.
    pub(crate) fn take_flushed(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.flushed)
    }

    /// When the next held event is due.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.held.peek().map(|h| h.at)
    }

    /// The held events whose time has come, in order.
    pub(crate) fn due(&mut self) -> Vec<InputEvent> {
        let now = Instant::now();
        let mut out = Vec::new();
        while self.held.peek().is_some_and(|h| h.at <= now) {
            out.push(self.held.pop().expect("peeked").event);
        }
        out
    }

    /// The report to the sender (§E.14.4), about once a second, while it does `sync/1`.
    pub(crate) fn report(&mut self) -> Option<String> {
        if !self.clocked() || self.last_report.elapsed() < Duration::from_secs(1) {
            return None;
        }
        self.last_report = Instant::now();
        let mut e = crate::xml::Element::new("x_idn_sync")
            .with_attr("group", &self.cfg.group)
            .with_attr("delay_ms", &format!("{:.1}", self.delay_ms()))
            .with_attr("frame_ms", &format!("{:.3}", self.frame_ms));
        if self.cfg.delay == SyncDelay::Auto
            && let Some(n) = self.need_ms()
        {
            e = e.with_attr("need_ms", &format!("{n:.1}"));
        }
        Some(e.to_xml())
    }

    pub(crate) fn stats(&self) -> SyncStats {
        let offset = self.offset().filter(|_| self.clocked());
        SyncStats {
            mode: match (self.clocked(), self.cfg.delay) {
                (false, _) => "unclocked",
                (true, SyncDelay::Auto) => "auto",
                (true, _) => "fixed",
            },
            group: self.cfg.group.clone(),
            delay_ms: self.delay_ms(),
            offset_us: offset.map(|o| o.0),
            offset_spread_us: offset.map(|o| o.1),
            need_ms: self.need_ms(),
            late: self.late,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Sender side: one delay per group over all receivers of this process (§E.14.4).

struct Member {
    group: String,
    need_ms: Option<f64>,
    delay_ms: f64,
    frame_ms: f64,
    seen: Instant,
    writer: Arc<Mutex<TcpStream>>,
}

#[derive(Default)]
struct Group {
    delay_ms: Option<f64>,
    changed: Option<Instant>,
    /// The member with the largest need at the last tick: when it leaves (or stops
    /// reporting), the delay falls to the largest need left at once (§E.14.4).
    setter: Option<u64>,
}

#[derive(Default)]
struct Registry {
    members: BTreeMap<u64, Member>,
    groups: BTreeMap<String, Group>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);
static NEXT_MEMBER: AtomicU64 = AtomicU64::new(1);

/// A fresh id for a receiver session in the registry.
pub(crate) fn member_id() -> u64 {
    NEXT_MEMBER.fetch_add(1, Ordering::Relaxed)
}

/// Decreases wait this long after the last change (§E.14.4).
const DECREASE_AFTER: Duration = Duration::from_secs(30);
/// Reports older than this are ignored (the receiver stopped reporting).
const STALE: Duration = Duration::from_secs(15);

/// A receiver's `x_idn_sync` report. Starts the announcing thread on first use.
pub(crate) fn report(
    id: u64,
    group: &str,
    need_ms: Option<f64>,
    delay_ms: f64,
    frame_ms: f64,
    writer: Arc<Mutex<TcpStream>>,
) {
    static TICKER: OnceLock<()> = OnceLock::new();
    TICKER.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("idn-sync".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    tick();
                }
            });
    });
    let group: String = group.chars().take(MAX_GROUP).collect();
    // A receiver's figures, held to what a display can mean ("inf" parses as a number,
    // and a delay of infinity panicked under the registry's lock: every session's sync
    // then failed).
    let ms = |v: f64| v.is_finite().then(|| v.clamp(0.0, MAX_DELAY_MS));
    let (need_ms, delay_ms) = (
        need_ms.and_then(ms).map(|n| n.min(max_need_ms())),
        ms(delay_ms).unwrap_or(0.0),
    );
    let mut held = REGISTRY.lock_ok();
    let reg = held.get_or_insert_with(Registry::default);
    let fresh = !reg.members.contains_key(&id);
    reg.members.insert(
        id,
        Member {
            group: group.clone(),
            need_ms,
            delay_ms,
            frame_ms: if frame_ms.is_finite() && frame_ms > 0.0 {
                frame_ms.min(1000.0)
            } else {
                33.4
            },
            seen: Instant::now(),
            writer: writer.clone(),
        },
    );
    // A new member hears the group's delay at once (written once the registry is let go,
    // as `tick` does: a receiver not reading held every output's frames, which ask the
    // registry each frame, for up to the write's 5 s; bug review 3).
    let say = fresh
        .then(|| reg.groups.get(&group).and_then(|g| g.delay_ms))
        .flatten();
    drop(held);
    if let Some(d) = say {
        announce(&writer, &group, d);
    }
}

/// A receiver's session ended.
pub(crate) fn leave(id: u64) {
    if let Some(reg) = REGISTRY.lock_ok().as_mut() {
        reg.members.remove(&id);
    }
}

/// The delay a receiver last said it uses (for `frames/1` deadlines, §E.13.2).
pub(crate) fn member_delay(id: u64) -> Option<Duration> {
    let reg = REGISTRY.lock_ok();
    let m = reg.as_ref()?.members.get(&id)?;
    Duration::try_from_secs_f64(m.delay_ms / 1000.0).ok()
}

fn announce(w: &Arc<Mutex<TcpStream>>, group: &str, delay_ms: f64) {
    use std::io::Write;
    let xml = crate::xml::Element::new("x_idn_sync")
        .with_attr("group", group)
        .with_attr("delay_ms", &format!("{delay_ms:.1}"))
        .to_xml();
    let f = crate::frame::Frame::text(crate::frame::kind::CONTROL, 0, &xml).encode();
    // (Half written, the stream would be out of step mid-message: closed then, as
    // `send_text` does.)
    let mut s = w.lock_ok();
    if s.write_all(&f).is_err() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

/// Recomputes each group's delay from its members' needs and announces changes:
/// increases at once, decreases after 30 s and by one frame period at a time.
fn tick() {
    let mut to_send: Vec<(Arc<Mutex<TcpStream>>, String, f64)> = Vec::new();
    {
        let mut reg = REGISTRY.lock_ok();
        let Some(reg) = reg.as_mut() else { return };
        let now = Instant::now();
        reg.members
            .retain(|_, m| now.duration_since(m.seen) < STALE * 4);
        // Each group once; groups no member is in any more go (bug run 2026-11-25: kept for
        // the process's life, a new name each second grew them without end).
        let names: std::collections::BTreeSet<String> =
            reg.members.values().map(|m| m.group.clone()).collect();
        reg.groups.retain(|n, _| names.contains(n));
        for name in names {
            let live: Vec<(u64, &Member)> = reg
                .members
                .iter()
                .filter(|(_, m)| m.group == name && now.duration_since(m.seen) < STALE)
                .map(|(id, m)| (*id, m))
                .collect();
            let Some((top, need)) = live
                .iter()
                .filter_map(|(id, m)| m.need_ms.map(|n| (*id, n)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
            else {
                continue; // nobody in auto mode
            };
            let step = live.iter().map(|(_, m)| m.frame_ms).fold(0.0, f64::max);
            let g = reg.groups.entry(name.clone()).or_default();
            // The member whose need set the delay is gone: no reason to wait for it.
            let setter_gone = g
                .setter
                .is_some_and(|s| !live.iter().any(|(id, _)| *id == s));
            g.setter = Some(top);
            let new = match g.delay_ms {
                None => need,
                Some(d) if need > d => need,
                Some(d) if need < d && setter_gone => need,
                Some(d)
                    if need < d - 0.5
                        && g.changed
                            .is_none_or(|c| now.duration_since(c) >= DECREASE_AFTER) =>
                {
                    need.max(d - step)
                }
                Some(d) => d,
            };
            if g.delay_ms != Some(new) {
                g.delay_ms = Some(new);
                g.changed = Some(now);
                for (_, m) in &live {
                    to_send.push((m.writer.clone(), name.clone(), new));
                }
            }
        }
    }
    // Written outside the lock: one slow receiver must not hold up the others.
    for (w, g, d) in to_send {
        announce(&w, &g, d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video() -> InputEvent {
        InputEvent::Log("frame".into())
    }

    #[test]
    fn the_offset_ignores_slow_samples() {
        let mut r = Receiver::new(SyncConfig::new("g", SyncDelay::Frames(3)));
        for id in 0..12 {
            r.ping_sent(id);
        }
        // Fast samples say +5 ms; three slow ones (long round trips) say otherwise.
        let sent = |r: &Receiver, id: u64| r.pings.iter().find(|p| p.0 == id).unwrap().1;
        for id in 0..9 {
            let s = sent(&r, id);
            r.pong(id, 400, s + 200 + 5_000);
        }
        for id in 9..12 {
            let s = sent(&r, id);
            r.pong(id, 80_000, s + 40_000 + 60_000);
        }
        let (o, spread) = r.offset().unwrap();
        assert!((o - 5_000).abs() < 50, "{o}");
        assert!(spread < 100, "{spread}");
    }

    #[test]
    fn frames_are_held_until_their_time_and_late_ones_dropped() {
        let mut r = Receiver::new(SyncConfig {
            clock: SyncClock::System,
            ..SyncConfig::new("g", SyncDelay::Millis(100))
        });
        r.set_peer(true);
        let now_ts = wall_us() * 10;
        // Stamped now: shown 100 ms from now.
        assert!(
            r.schedule(video(), true, now_ts, Some((30, 1)), 0, Instant::now())
                .is_none()
        );
        let due = r.next_due().unwrap();
        let wait = due.duration_since(Instant::now());
        assert!(wait > Duration::from_millis(80) && wait <= Duration::from_millis(100));
        assert!(r.due().is_empty());
        std::thread::sleep(wait + Duration::from_millis(2));
        assert_eq!(r.due().len(), 1);
        // Stamped 150 ms ago: past its time (100 ms delay), dropped and counted.
        let old = now_ts - 1_500_000;
        assert!(
            r.schedule(video(), true, old, Some((30, 1)), 0, Instant::now())
                .is_none()
        );
        assert!(r.next_due().is_none());
        assert_eq!(r.stats().late, 1);
        // An hour off: a clock step, shown at once rather than dropped.
        let step = now_ts + 36_000_000_000;
        assert!(
            r.schedule(video(), true, step, Some((30, 1)), 0, Instant::now())
                .is_some()
        );
    }

    #[test]
    fn fixed_frames_follow_the_source_rate_and_unclocked_waits_from_arrival() {
        let mut r = Receiver::new(SyncConfig::new("g", SyncDelay::Frames(3)));
        let arrived = Instant::now();
        // Not clocked (an NDI source): 3 frames at 25 fps = 120 ms from arrival.
        assert!(
            r.schedule(video(), true, 0, Some((25, 1)), 0, arrived)
                .is_none()
        );
        let wait = r.next_due().unwrap().duration_since(arrived);
        assert_eq!(wait, Duration::from_millis(120));
        assert_eq!(r.stats().mode, "unclocked");
    }

    #[test]
    fn the_group_delay_rises_at_once_and_falls_slowly() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let w = Arc::new(Mutex::new(
            TcpStream::connect(l.local_addr().unwrap()).unwrap(),
        ));
        let (a, b, other) = (member_id(), member_id(), member_id());
        let group = format!("test-{a}");
        report(a, &group, Some(80.0), 80.0, 33.3, w.clone());
        report(b, &group, Some(120.0), 80.0, 33.3, w.clone());
        report(other, "elsewhere", Some(900.0), 900.0, 33.3, w.clone());
        tick();
        let delay = |g: &str| {
            REGISTRY.lock_ok().as_ref().unwrap().groups[g]
                .delay_ms
                .unwrap()
        };
        assert_eq!(delay(&group), 120.0, "the largest need of the group");
        assert_eq!(delay("elsewhere"), 900.0, "groups are independent");
        // The slow display improves: no change before 30 s.
        report(b, &group, Some(60.0), 120.0, 33.3, w.clone());
        tick();
        assert_eq!(delay(&group), 120.0);
        // After 30 s: one frame period down at a time.
        {
            let mut reg = REGISTRY.lock_ok();
            let g = reg.as_mut().unwrap().groups.get_mut(&group).unwrap();
            g.changed = Some(Instant::now() - DECREASE_AFTER);
        }
        tick();
        assert!((delay(&group) - 86.7).abs() < 0.1, "{}", delay(&group));
        // A larger need raises it at once.
        report(a, &group, Some(200.0), 86.7, 33.3, w);
        tick();
        assert_eq!(delay(&group), 200.0);
        for id in [a, b, other] {
            leave(id);
        }
    }

    /// One receiver's huge need counts as 2 s at most, and when it leaves the group's
    /// delay falls back at once (not 10 s, then hours to recover).
    #[test]
    fn a_rogue_need_is_capped_and_forgotten_when_it_leaves() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let w = Arc::new(Mutex::new(
            TcpStream::connect(l.local_addr().unwrap()).unwrap(),
        ));
        let (a, b, rogue) = (member_id(), member_id(), member_id());
        let group = format!("rogue-{a}");
        let delay = |g: &str| {
            REGISTRY.lock_ok().as_ref().unwrap().groups[g]
                .delay_ms
                .unwrap()
        };
        report(a, &group, Some(40.0), 40.0, 33.3, w.clone());
        report(b, &group, Some(36.0), 40.0, 33.3, w.clone());
        tick();
        assert_eq!(delay(&group), 40.0);
        report(rogue, &group, Some(1e300), 1e300, 33.3, w.clone());
        tick();
        assert_eq!(
            delay(&group),
            max_need_ms(),
            "held to the cap (2 s by default)"
        );
        // The cap is configurable: a fresh group with a lowered cap holds to it at once
        // (an increase applies immediately, unlike a decrease).
        set_max_need_ms(1000.0);
        let g2 = format!("cfg-{rogue}");
        let c = member_id();
        report(c, &g2, Some(1e300), 1e300, 33.3, w.clone());
        tick();
        assert_eq!(delay(&g2), 1000.0, "held to the configured cap");
        set_max_need_ms(5e9); // beyond the ceiling → clamped
        assert_eq!(max_need_ms(), MAX_DELAY_MS);
        set_max_need_ms(2000.0); // restore the default (other tests use tiny needs)
        leave(c);
        leave(rogue);
        tick();
        assert_eq!(delay(&group), 40.0, "back at once, not a frame per 30 s");
        // An honest member's need falling still steps down slowly.
        report(a, &group, Some(20.0), 40.0, 33.3, w.clone());
        tick();
        assert_eq!(delay(&group), 40.0);
        for id in [a, b] {
            leave(id);
        }
    }

    /// A receiver's absurd figures (or a source's absurd frame rate) are held to sense:
    /// nothing panics, nothing waits for ages.
    #[test]
    fn absurd_figures_are_held_to_sense() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let w = Arc::new(Mutex::new(
            TcpStream::connect(l.local_addr().unwrap()).unwrap(),
        ));
        let id = member_id();
        report(
            id,
            "absurd",
            Some(f64::INFINITY),
            f64::INFINITY,
            f64::NAN,
            w.clone(),
        );
        assert_eq!(member_delay(id), Some(Duration::ZERO));
        report(id, "absurd", Some(1e300), 1e300, 1e300, w);
        assert_eq!(member_delay(id), Some(Duration::from_secs(10)));
        tick();
        leave(id);
        // 1/4e9 fps, three frames: three seconds at most.
        let mut r = Receiver::new(SyncConfig::new("g", SyncDelay::Frames(3)));
        let arrived = Instant::now();
        assert!(
            r.schedule(video(), true, 0, Some((1, u32::MAX)), 0, arrived)
                .is_none()
        );
        assert!(r.next_due().unwrap().duration_since(arrived) <= Duration::from_secs(3));
    }
}
