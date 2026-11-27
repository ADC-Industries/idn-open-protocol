//! Sound from the network played against another clock: a sound card's, OBS's, a
//! mixer's. Each source has its own lane: a small buffer absorbs the network's timing,
//! and a servo holds it at its target by resampling a few parts per million, so the
//! lane's clock and the consumer's never drift apart. A [`Mixer`] sums its lanes into
//! the blocks its consumer pulls (spec/extensions/idn-routing.md §A.7). `idn-audio`
//! plays a mixer on a sound card; the OBS plugin pulls one at OBS's audio rate.

use super::resample::{Rate, map_channels};
use super::servo::Servo;
use crate::ptp::{Mapping, Ns};
use idn_common::sync::LockOk as _;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A lane's buffer and drift correction.
#[derive(Debug, Clone)]
pub struct LaneConfig {
    /// Sound held before the consumer, in milliseconds: the network's timing jitter must
    /// fit in it. Default 60; 10 in the low-latency mode, on a wired network.
    pub latency_ms: u32,
    /// The largest drift correction, in parts per million (0 switches it off).
    pub max_correction_ppm: f64,
    /// Adapt to the path (roadmap R.30): start at `latency_ms`, then hold what the path's
    /// measured jitter needs (the latest arrival of the last 30 s against the earliest, a
    /// fifth more; the largest among the mixer's adapting lanes, so that sources mixed
    /// together stay in line): grown at once when the path gets worse, brought back 1 ms at a time
    /// (at most every 5 s, after 30 calm seconds) when it is better. Off: `latency_ms`
    /// as it is (underruns still widen it). A latency set for the receiver
    /// ([`Lane::follow_site`]) is never adapted.
    pub adaptive: bool,
    /// Adapting, never under this many ms (OBS's 21 ms ticks need 30).
    pub min_ms: u32,
    /// After a fresh start, the level is left to the servo (under 1 ms a second) rather
    /// than set right once by a cut or a pad (at a quiet moment, else at the front after
    /// 5 s): for a consumer that stamps its sound by the buffer's level (the switcher's
    /// program), where that one step showed as a jump in its stamps and a gap ~7 s after
    /// every cut (lab 467-B).
    pub keep_start_level: bool,
}

impl Default for LaneConfig {
    fn default() -> Self {
        LaneConfig {
            latency_ms: 60,
            max_correction_ppm: 1000.0,
            adaptive: false,
            min_ms: 2,
            keep_start_level: false,
        }
    }
}

/// A lane's counters and drift correction, from [`Lane::stats`].
#[derive(Debug, Clone, Default)]
pub struct LaneStats {
    /// The consumer (a sound card's name, `OBS`, …).
    pub device: String,
    /// The consumer's rate.
    pub device_rate: u32,
    /// The consumer's channels.
    pub device_channels: u16,
    /// Frames the device takes per callback (its buffer), once known.
    pub device_block: u32,
    /// The source's rate (of the last block).
    pub source_rate: u32,
    /// The source's channels (of the last block).
    pub source_channels: u16,
    /// Blocks pushed.
    pub blocks_in: u64,
    /// Frames played (silence included).
    pub frames_out: u64,
    /// Sound in the buffer, in ms: the mean over the last second of what the device
    /// found after taking its block (the low points of the cycle); the target asked for;
    /// and the level the servo holds that mean at, which is the target plus half a
    /// source block minus half a device block (where the mean sits when the ring never
    /// falls below the target between arrivals).
    pub fill_ms: f64,
    /// The target asked for ([`LaneConfig::latency_ms`]).
    pub target_ms: u32,
    /// What underruns have added to the target (2 ms each; 1 ms given back per ten clean
    /// minutes, never down to where one happened).
    pub margin_ms: f64,
    /// Adapting to the path ([`LaneConfig::adaptive`]): `target_ms` follows the need.
    pub adaptive: bool,
    /// The path's measured jitter: the latest arrival of the last 30 s against the
    /// earliest of the two seconds before it, ms (adapting lanes).
    pub jitter_ms: f64,
    /// Playing by time (a site time on this machine's grandmaster, idn-clock.md §C.4).
    pub by_time: bool,
    /// By time: how late the sound played against its time + the target, ms (the mean
    /// the servo last saw; it holds it at zero).
    pub late_ms: f64,
    /// The level the servo holds the mean fill at.
    pub hold_ms: f64,
    /// The source's block length, in ms (what the ring steps by per arrival).
    pub source_block_ms: f64,
    /// The resampling correction in use, in ppm (positive: the source's clock runs
    /// faster than the device's, so more device samples are made per source sample).
    pub correction_ppm: f64,
    /// Times the buffer ran dry (the rest of that block was silence, and the buffer
    /// refilled to its target before playing on), not counting `pauses`.
    pub underruns: u64,
    /// Times it ran dry because the source paused (nothing for over 300 ms: a sender
    /// stopped or restarted, a file at its end).
    pub pauses: u64,
    /// Sound dropped because the buffer grew too far past its target (a clock step or a
    /// burst), in ms.
    pub dropped_ms: f64,
    /// Device errors reported by the audio system.
    pub errors: u64,
    /// Times the source's rate or channel count changed (the resampler and the learned
    /// drift start over).
    pub streams: u64,
    /// The samples pass through unresampled: by time, at the device's rate, and no drift
    /// to correct (`IDN_LANE_THROUGH`).
    pub through: bool,
    /// While passing through: the few samples cut or put in, in a quiet moment, to hold
    /// the lateness (what drift is left), in ms.
    pub through_fixed_ms: f64,
    /// By time, filtering: the drift measured over the last steady stretch, ppm (under
    /// 10 the samples pass through, with `IDN_LANE_THROUGH`).
    pub calm_drift_ppm: f64,
    /// Why passing through last ended: `pause`, `jump`, `drift` (two fixes within 5
    /// minutes) or `measure` (site time came or went); empty if it never has.
    pub through_ended: &'static str,
    /// By time: steps of more than 5 ms put right at once (a cut, or silence put in).
    pub jumps: u64,
    /// The last of them, ms: + late (cut), − early (silence put in).
    pub last_jump_ms: f64,
}

impl LaneStats {
    /// Passing through, as status lines say it (`IDN_LANE_THROUGH`): `through, fixed N
    /// ms` while it does; otherwise the drift last measured and why it last stopped, if
    /// either is known; empty with neither.
    pub fn through_line(&self) -> String {
        if self.through {
            return format!("through, fixed {:.0} ms", self.through_fixed_ms);
        }
        let mut s = Vec::new();
        if self.calm_drift_ppm != 0.0 {
            s.push(format!("drift {:.1} ppm", self.calm_drift_ppm));
        }
        if !self.through_ended.is_empty() {
            s.push(format!("through ended: {}", self.through_ended));
        }
        if s.is_empty() {
            String::new()
        } else {
            format!("filtering ({})", s.join(", "))
        }
    }
}

/// Lane identities for their sync group.
static LANE_IDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Sums lanes into the blocks a consumer pulls with [`Mixer::fill`]: a sound card's
/// callback, OBS's audio, a mixing engine. Its rate and channel count are the consumer's.
pub struct Mixer {
    /// The lanes playing on the mixer, and a count of its changes.
    lanes: Mutex<Vec<Arc<Shared>>>,
    lanes_gen: AtomicU64,
    /// Lanes the callback's copy let go of, freed here by the next lane made or dropped,
    /// not on the callback's thread (a lane's ring can be megabytes).
    grave: Mutex<Vec<Arc<Shared>>>,
    /// The consumer's copy of `lanes` and the change it was copied at: the callback
    /// plays its copy and copies again only when the list changed and is free (a lane
    /// being made or let go never holds a sound card's callback up).
    cb_lanes: Mutex<(u64, Vec<Arc<Shared>>)>,
    /// Its adapting lanes' common target.
    sync: Arc<SyncGroup>,
    /// The largest block the device has asked for in the last 10–20 s, in frames (a
    /// device that moves to a smaller period is followed: the largest ever kept the
    /// target too high for good).
    block: Arc<AtomicU32>,
    /// The largest in this 10 s window, and when the window started (ms).
    block_window: AtomicU32,
    window_at: AtomicU64,
    frames_out: AtomicU64,
    errors: Arc<AtomicU64>,
    dev_ch: usize,
    dev_rate: u32,
    name: String,
    /// When the last block was taken ([`crate::ptp::mono_ns`]), and how long after its
    /// callback a block is heard (the device's buffer), ns: when a lane's next sample
    /// plays, for playing by time.
    cb_mono: Arc<AtomicI64>,
    out_delay_ns: Arc<AtomicI64>,
    /// Lanes may pass their samples through unresampled (`IDN_LANE_THROUGH`; on by
    /// default, `off` turns it off).
    through: bool,
}

/// A lane's sound waiting to play: device-rate frames of `cols.len()` samples, column `k`
/// for the device's channel `cols[k]`. A routed lane's columns are the channels its routes
/// have fed (only added to: a route taken away leaves silence in its column), not the whole
/// device's: 16 stereo sources on a 32-channel desk kept, and summed, 32 channels each.
/// Any other lane's are all of the device's, in order.
struct Ring {
    data: VecDeque<f32>,
    cols: Vec<usize>,
    /// Frames the pushing side cut from the front, silence it put in front (frames), and
    /// its other changes of where frames are (a quiet stretch cut or padded, a start's),
    /// all counted up: the callback's copy (see [`Spare`]) knows by them where the frames
    /// it played already are. Changes in place (fades, columns added) move nothing.
    cut: u64,
    padded: u64,
    moved: u64,
}

impl std::ops::Deref for Ring {
    type Target = VecDeque<f32>;
    fn deref(&self) -> &VecDeque<f32> {
        &self.data
    }
}

impl std::ops::DerefMut for Ring {
    fn deref_mut(&mut self) -> &mut VecDeque<f32> {
        &mut self.data
    }
}

impl Ring {
    fn new(capacity: usize) -> Ring {
        Ring {
            data: VecDeque::with_capacity(capacity),
            cols: Vec::new(),
            cut: 0,
            padded: 0,
            moved: 0,
        }
    }

    /// Samples a frame (at least 1).
    fn width(&self) -> usize {
        self.cols.len().max(1)
    }

    /// Columns for the channels of `chans` it hasn't, silent in what waits already.
    fn add_cols(&mut self, chans: &[usize]) {
        let new: Vec<usize> = chans
            .iter()
            .copied()
            .filter(|c| !self.cols.contains(c))
            .collect();
        if new.is_empty() {
            return;
        }
        // (With no columns yet, what waits is silence a sample a frame: kept as as many
        // silent frames, so the lane's level doesn't drop when its first route comes.)
        let w = self.cols.len();
        let frames = self.data.len() / w.max(1);
        let mut data = VecDeque::with_capacity(frames * (w + new.len()));
        for f in 0..frames {
            data.extend(self.data.range(f * w..(f + 1) * w).copied());
            data.extend(std::iter::repeat_n(0.0, new.len()));
        }
        self.data = data;
        self.cols.extend(new);
    }
}

/// The consumer's own copy of what a lane plays next (taken only by the consumer's
/// callback, so never waited for). The ring's lock is also the pushing side's, held
/// through its level keeping; a callback finding it held plays this copy instead of
/// waiting (a pushing thread preempted with the lock held kept a sound card waiting,
/// 2026-11-26), and what it played comes off the ring's front the next time.
#[derive(Default)]
struct Spare {
    /// Frames as the ring held them (its columns, `cols`), copied after the last block
    /// taken with the ring locked: the next block's.
    data: Vec<f32>,
    cols: Vec<usize>,
    frames: usize,
    /// Frames of `data` played while the ring was held: still at the ring's front, to
    /// come off it when the callback next has it, less what the pushing side cut from
    /// the front meanwhile (gone already).
    owed: usize,
    /// The ring's `cut`, `padded` and `moved` when copied.
    cut_at: u64,
    padded_at: u64,
    moved_at: u64,
    /// The last frame played, by column: a ring that runs dry just as a block ends has
    /// nothing left to fade, and the next block ramps these down instead of a step.
    played_last: Vec<f32>,
}

/// One lane: a source's buffer, played into the device's mix.
struct Shared {
    /// Device-rate samples waiting to play, a frame of the ring's columns at a time.
    ring: Mutex<Ring>,
    /// The consumer's copy of the next block (see [`Spare`]).
    spare: Mutex<Spare>,
    /// Playing (the buffer reached its target); cleared by an underrun to refill first.
    ready: AtomicBool,
    /// Frames per channel left in the ring after each callback took its block, summed,
    /// and the number of callbacks: the mean fill over the servo's interval.
    fill_sum: AtomicU64,
    fill_n: AtomicU64,
    /// The device's (shared by the lanes).
    block: Arc<AtomicU32>,
    underruns: AtomicU64,
    /// Of those, once the source came back: the times it had paused (stopped,
    /// restarted), not the path's fault, and the others, the stats' underruns (counted
    /// when the sound resumes, as only then is it known which it was: subtracting the
    /// pauses from all, a restart showed as an underrun until the source was back).
    pauses: AtomicU64,
    jitter_underruns: AtomicU64,
    errors: Arc<AtomicU64>,
    dev_ch: usize,
    dev_rate: u32,
    /// The mixer's (see [`Mixer`]).
    cb_mono: Arc<AtomicI64>,
    out_delay_ns: Arc<AtomicI64>,
    through: bool,
    /// Its [`Lane`] dropped: what was left of its sound fades out and plays, then the
    /// mixer lets it go.
    ending: AtomicBool,
    /// Adapting: its mixer's lanes' common target, and its identity there.
    sync: Arc<SyncGroup>,
    sync_id: usize,
    /// Before it plays: the level it will start at (ns of sound: the prime level) and when
    /// its last block came ([`crate::ptp::mono_ns`]), so a sample pushed now is said to be
    /// heard when it will be, not as soon as the little it holds yet.
    start_ns: AtomicI64,
    pushed_mono: AtomicI64,
}

/// One source on a [`Mixer`], from [`Mixer::lane`]: its own buffer and drift servo, its
/// channels placed by a map and summed with the other lanes'. Removed when dropped.
pub struct Lane {
    shared: Arc<Shared>,
    device: Arc<Mixer>,
    cfg: Mutex<LaneConfig>,
    /// The configuration it was made with (what it goes back to when no latency is set
    /// for its receiver).
    own: LaneConfig,
    inner: Mutex<Inner>,
    stats: Mutex<LaneStats>,
}

/// The pushing side's state.
struct Inner {
    rate: Option<(u32, u16, Rate)>,
    servo: Servo,
    last: Option<Instant>,
    /// The servo waits until then after a refill, an underrun or a drop; in that time
    /// the buffer is trimmed to its target rather than bled by the servo.
    hold_until: Option<Instant>,
    underruns_seen: u64,
    /// When the servo last ran: it runs once a second on the mean fill.
    servo_at: Option<Instant>,
    /// When the ring was last trimmed (recovery trims are spaced out).
    last_trim: Option<Instant>,
    /// While recovering: recent (time, fill) readings, to tell a burst still arriving
    /// (the fill keeps growing) from one that is over (steady, and cut once).
    growth: Vec<(Instant, f64)>,
    /// The channel count of the last block.
    channels_seen: usize,
    /// Added to the target after underruns (the path's jitter is more than the target
    /// covers), and when it last changed.
    margin_ms: f64,
    margin_at: Option<Instant>,
    /// 1 ms above the margin the last underrun happened at: the give-back stops there.
    margin_floor: f64,
    /// Playing by time: the last block came with a site time on our grandmaster.
    timed: bool,
    /// By time, not started: since when every block has come older than the target can
    /// play (the source's sound is that old when it gets here), and the least of those
    /// ages past the target, ms.
    too_old: Option<(Instant, f64, Instant)>,
    /// Adapting, by time: the target the source's age needs (`too_old`); adapting never
    /// goes back under it (it would run dry and start over, every time it settled).
    age_floor: f64,
    /// By time: how late the next sample plays against its time + latency, ns, summed
    /// since the servo last ran, and the count.
    late_sum: f64,
    late_n: u32,
    /// By time: the drift learned from the lateness's slope, the correction applied, and
    /// the last second's mean lateness and when, ms.
    drift_ppm: f64,
    applied_ppm: f64,
    prev_late: Option<(Instant, f64)>,
    /// A fresh start (the first sound, or after the source paused): its level is
    /// measured once the start has settled and set right once, at a quiet moment.
    starting: bool,
    /// That correction while it waits for a quiet moment: ms to cut (positive) or to
    /// add as silence (negative), and since when.
    relevel: Option<(f64, Instant)>,
    /// The re-level (by its `since`) whose look for a quiet moment found none on the last
    /// push: this one need only look at what came since.
    quiet_looked: Option<Instant>,
    /// The resampled block's buffer, kept between pushes.
    fresh_buf: Vec<f32>,
    /// A lower latency was asked for: its surplus is cut at once.
    cut_asked: bool,
    /// Whether the samples may pass through, and holding them while they do.
    watch: Watch,
    /// A fade-in longer than the ring held when it began: (frames faded, its length),
    /// the rest for the frames that come next.
    fade_left: Option<(usize, usize)>,
    /// Ms of its source heard before it first played, and whether it has: a new lane
    /// hears [`NEW_LANE_MS`] before its first start.
    heard_ms: f64,
    ever_ready: bool,
    /// Before it first plays: when its first block came (and its length, ms), and how many
    /// have come. A source whose sound comes as fast as it plays (evenly, in real time)
    /// starts at once, not [`NEW_LANE_MS`] later.
    heard_from: Option<(Instant, f64)>,
    /// It played before and its source paused and came back (its level is then set right
    /// at the usual 2 ms, not with a first start's allowance).
    restarted: bool,
    heard_n: u32,
    /// Adapting (R.30): the arrivals' lateness, the target it holds, when that last
    /// changed and when the last underrun was.
    jitter: super::jitter::Jitter,
    auto_ms: f64,
    auto_at: Option<Instant>,
    underrun_at: Option<Instant>,
    /// The target the last underrun happened at: not given back below it (less half a
    /// millisecond) for [`ADAPT_MEMORY`].
    underran_ms: f64,
    /// When the source last started or came back after a pause: its first seconds come
    /// in a rush (a backlog), not as the path's jitter.
    resumed_at: Option<Instant>,
    /// Whether the target has come from the path (a rise to what was measured, an
    /// underrun, a fall to the measured need): until then the target is the start guess,
    /// the lane's own, not published to its sync group.
    measured: bool,
    /// The first settling's start guess and when: an underrun soon after it goes back
    /// there at once (the path needed more than its first seconds showed; growing a
    /// little at a time from the settled level was a burst of underruns, lab 464(a)).
    settled_from: Option<(f64, Instant)>,
    /// When this lane last published to its sync group, and what.
    published: (Option<Instant>, f64),
    /// The target the mixer's adapting lanes hold together (at least `auto_ms`).
    group_ms: f64,
}

/// Adapting: the need is the latest arrival of this many seconds (against the earliest
/// before it), with a fifth more and half a millisecond; a rise is taken from the last
/// `ADAPT_RISE_S` already. (Shorter in tests.)
#[cfg(not(test))]
const ADAPT_WINDOW_S: u64 = 30;
#[cfg(test)]
const ADAPT_WINDOW_S: u64 = 3;
#[cfg(not(test))]
const ADAPT_RISE_S: u64 = 3;
#[cfg(test)]
const ADAPT_RISE_S: u64 = 1;
/// Adapting: the target comes down 1 ms at a time, at most this often, after this long
/// without an underrun or a rise.
#[cfg(not(test))]
const ADAPT_STEP: Duration = Duration::from_secs(5);
#[cfg(test)]
const ADAPT_STEP: Duration = Duration::from_millis(300);
#[cfg(not(test))]
const ADAPT_CALM: Duration = Duration::from_secs(30);
#[cfg(test)]
const ADAPT_CALM: Duration = Duration::from_secs(2);
/// Adapting: how long the target that last underran is remembered (a path calm but for
/// a spike a minute otherwise came back down to it and underran again each minute).
#[cfg(not(test))]
const ADAPT_MEMORY: Duration = Duration::from_secs(600);
#[cfg(test)]
const ADAPT_MEMORY: Duration = Duration::from_secs(4);
/// Adapting: underruns in a row grow the target at most this often (inside the hold that
/// follows each, too: underruns under 2 s apart otherwise never grew it).
const ADAPT_UNDERRUN_STEP: Duration = Duration::from_millis(500);

/// How much of its source a new lane hears before it first plays (what is older than its
/// start needs goes unheard): a route's first blocks come unevenly (a few, then a gap of
/// a few ms), and a lane started on them ran dry at once and started again (lab 373: 3 ms
/// of sound, 5 ms of silence, a click). Not for a source whose first blocks come evenly
/// (an application pushing in real time): it starts at once, every sample heard.
const NEW_LANE_MS: f64 = 40.0;

/// A level to set right in a quiet moment: ms to cut (positive) or to add as silence
/// (negative), and since when.
type Relevel = Option<(f64, Instant)>;

impl Mixer {
    /// A mixer for a consumer taking `channels` channels at `rate`; `name` names the
    /// consumer in the lanes' stats.
    pub fn new(rate: u32, channels: u16, name: &str) -> Arc<Mixer> {
        Arc::new(Mixer {
            lanes: Mutex::new(Vec::new()),
            lanes_gen: AtomicU64::new(0),
            grave: Mutex::new(Vec::with_capacity(16)),
            cb_lanes: Mutex::new((u64::MAX, Vec::with_capacity(64))),
            sync: Arc::default(),
            block: Arc::new(AtomicU32::new(0)),
            block_window: AtomicU32::new(0),
            window_at: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            errors: Arc::new(AtomicU64::new(0)),
            dev_ch: channels.max(1) as usize,
            dev_rate: rate,
            name: name.to_owned(),
            cb_mono: Arc::new(AtomicI64::new(0)),
            out_delay_ns: Arc::new(AtomicI64::new(0)),
            through: idn_common::switches::on("IDN_LANE_THROUGH"),
        })
    }

    /// What its adapting lanes need together (the largest of their needs, ms; the
    /// site's floor left out): `None` while none adapts. A receiver reports it to the
    /// site, which keeps receivers of the same transmitters in line (idn-routing.md
    /// §A.14).
    pub fn need_ms(&self) -> Option<f64> {
        self.sync.need(Instant::now())
    }

    /// The least its adapting lanes hold, ms, as the site sets it for the receiver's sync
    /// group (other receivers of the same transmitters, on other machines); `None`:
    /// none.
    pub fn set_floor_ms(&self, floor: Option<f64>) {
        let v = floor.filter(|f| *f > 0.0).unwrap_or(0.0);
        self.sync.floor.store(v.to_bits(), Ordering::Relaxed);
    }

    /// How long after its callback a block is heard (the device's buffer and output
    /// latency), when the consumer knows (a sound card's callback timestamps): lanes
    /// playing by time count it in.
    pub fn set_output_delay(&self, delay: Duration) {
        self.out_delay_ns
            .store(delay.as_nanos() as i64, Ordering::Relaxed);
    }

    /// How long after its callback a block is heard, as the consumer last said
    /// ([`set_output_delay`](Self::set_output_delay)); zero until it does.
    pub fn output_delay(&self) -> Duration {
        Duration::from_nanos(self.out_delay_ns.load(Ordering::Relaxed).max(0) as u64)
    }

    /// How long until the consumer takes its next block: a block's length after the last
    /// was taken, less the time since (zero before the first, or when it is late).
    fn until_next_take(&self) -> Duration {
        let at = self.cb_mono.load(Ordering::Relaxed);
        if at == 0 {
            return Duration::ZERO;
        }
        let block_ns =
            self.block.load(Ordering::Relaxed) as i64 * 1_000_000_000 / self.dev_rate.max(1) as i64;
        let since = crate::ptp::mono_ns() - at;
        Duration::from_nanos((block_ns - since).clamp(0, block_ns) as u64)
    }

    /// A new lane on this mixer.
    pub fn lane(self: &Arc<Self>, cfg: &LaneConfig) -> Lane {
        Lane::new(self, cfg, self.name.clone())
    }

    /// The consumer's channel count.
    pub fn channels(&self) -> u16 {
        self.dev_ch as u16
    }

    /// The consumer's clock as its pulls show it: frames taken before the last pull, when
    /// that pull came ([`crate::ptp::mono_ns`]), and the consumer's nominal rate. A sound
    /// card's crystal, for a desk that follows its card (roadmap R.26). `None` before the
    /// first pull.
    pub fn clock(&self) -> Option<(u64, i64, u32)> {
        // The time is stored before the count grows: read the time on both sides, so a
        // pull in between is seen and read again.
        for _ in 0..4 {
            let at = self.cb_mono.load(Ordering::Acquire);
            let frames = self.frames_out.load(Ordering::Acquire);
            if at != 0 && self.cb_mono.load(Ordering::Acquire) == at {
                return Some((frames, at, self.dev_rate));
            }
        }
        None
    }

    /// The consumer's rate.
    pub fn rate(&self) -> u32 {
        self.dev_rate
    }

    /// The consumer's errors counted so far ([`note_error`](Self::note_error)).
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Counts an error of the consumer's (a device error), shown in the lanes' stats.
    pub fn note_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
}

impl Lane {
    fn new(device: &Arc<Mixer>, cfg: &LaneConfig, name: String) -> Lane {
        let shared = Arc::new(Shared {
            // A quarter of a second to start with (it grows if it must): a second of 64
            // channels was 12 MB per lane, committed memory on Windows.
            ring: Mutex::new(Ring::new(device.dev_rate as usize / 4 * 2)),
            // Room for a large block of every channel made here, not on the callback.
            spare: Mutex::new(Spare {
                data: Vec::with_capacity(1024 * device.dev_ch),
                cols: Vec::with_capacity(device.dev_ch),
                played_last: Vec::with_capacity(device.dev_ch),
                ..Spare::default()
            }),
            ready: AtomicBool::new(false),
            fill_sum: AtomicU64::new(0),
            fill_n: AtomicU64::new(0),
            block: device.block.clone(),
            underruns: AtomicU64::new(0),
            pauses: AtomicU64::new(0),
            jitter_underruns: AtomicU64::new(0),
            errors: device.errors.clone(),
            dev_ch: device.dev_ch,
            dev_rate: device.dev_rate,
            cb_mono: device.cb_mono.clone(),
            out_delay_ns: device.out_delay_ns.clone(),
            through: device.through,
            ending: AtomicBool::new(false),
            sync: device.sync.clone(),
            sync_id: LANE_IDS.fetch_add(1, Ordering::Relaxed),
            start_ns: AtomicI64::new(0),
            pushed_mono: AtomicI64::new(0),
        });
        {
            let mut lanes = device.lanes.lock_ok();
            lanes.push(shared.clone());
            device.lanes_gen.fetch_add(1, Ordering::Release);
            let mut grave = device.grave.lock_ok();
            grave.clear();
            grave.reserve(lanes.len());
        }
        Lane {
            shared,
            device: device.clone(),
            cfg: Mutex::new(cfg.clone()),
            own: cfg.clone(),
            inner: Mutex::new(Inner {
                rate: None,
                // Fed once a second with a mean: slower gains than the capture servo's.
                servo: Servo::with_gains(10.0, 0.08, 0.0, cfg.max_correction_ppm),
                last: None,
                hold_until: None,
                underruns_seen: 0,
                servo_at: None,
                last_trim: None,
                growth: Vec::new(),
                channels_seen: 0,
                margin_ms: 0.0,
                margin_at: None,
                margin_floor: 0.0,
                timed: false,
                too_old: None,
                age_floor: 0.0,
                late_sum: 0.0,
                late_n: 0,
                drift_ppm: 0.0,
                applied_ppm: 0.0,
                prev_late: None,
                starting: false,
                relevel: None,
                quiet_looked: None,
                fresh_buf: Vec::new(),
                cut_asked: false,
                watch: Watch::new(),
                fade_left: None,
                heard_ms: 0.0,
                ever_ready: false,
                heard_from: None,
                restarted: false,
                heard_n: 0,
                jitter: Default::default(),
                auto_ms: cfg.latency_ms as f64,
                auto_at: None,
                underrun_at: None,
                underran_ms: 0.0,
                resumed_at: None,
                measured: false,
                settled_from: None,
                published: (None, 0.0),
                group_ms: cfg.latency_ms as f64,
            }),
            stats: Mutex::new(LaneStats {
                device: name,
                device_rate: device.dev_rate,
                device_channels: device.dev_ch as u16,
                target_ms: cfg.latency_ms,
                adaptive: cfg.adaptive,
                ..Default::default()
            }),
        }
    }

    /// Queues a block of planar f32 sound: `map` says which device channel each of its
    /// channels plays on, as (index into `channels`, device channel); a channel may play
    /// on several, channels not in the map aren't played, and device channels beyond the
    /// device's are ignored. Never blocks for long.
    pub fn push(&self, sample_rate: u32, channels: &[Vec<f32>], map: &[(usize, u16)]) {
        self.push_inner(sample_rate, channels, Some(map));
    }

    /// Queues a block whose channels play one to one on the consumer's (a mono block on
    /// the first two): the one-source form.
    pub fn push_all(&self, sample_rate: u32, channels: &[Vec<f32>]) {
        self.push_inner(sample_rate, channels, None);
    }

    /// [`push`](Self::push) or [`push_all`](Self::push_all) (`map` `None`) for a block
    /// with a site time: `site_ns`, when its first sample was taken on this machine's
    /// grandmaster ([`site_time`]). While this machine's site clock is usable the lane
    /// plays it by time (spec/extensions/idn-clock.md §C.4): each sample at its time plus
    /// the latency, so receivers with the same latency play together and sources line up;
    /// otherwise by buffer level, as without one.
    pub fn push_timed(
        &self,
        sample_rate: u32,
        channels: &[Vec<f32>],
        map: Option<&[(usize, u16)]>,
        site_ns: Option<Ns>,
    ) {
        let mapping = crate::node::clock::status()
            .filter(|c| c.usable())
            .and_then(|c| c.mapping);
        let mut inner = self.inner.lock_ok();
        let at = site_ns.zip(mapping);
        let cfg = PushCfg::of(&self.cfg.lock_ok());
        push_inner(
            &self.shared,
            &mut inner,
            &cfg,
            &self.stats,
            sample_rate,
            channels,
            map,
            at,
        );
    }

    fn push_inner(&self, sample_rate: u32, channels: &[Vec<f32>], map: Option<&[(usize, u16)]>) {
        let mut inner = self.inner.lock_ok();
        let cfg = PushCfg::of(&self.cfg.lock_ok());
        push_inner(
            &self.shared,
            &mut inner,
            &cfg,
            &self.stats,
            sample_rate,
            channels,
            map,
            None,
        );
    }

    /// The latency asked for now, in ms ([`LaneConfig::latency_ms`]).
    pub fn latency_ms(&self) -> u32 {
        self.cfg.lock_ok().latency_ms
    }

    /// Follows the latency set for its receiver by the site (§A.12): `Some`, held at that
    /// (not adapted); `None`, the lane's own configuration again (adapting if it was
    /// made so, from where it is). Cheap when nothing changes: call it with every block.
    pub fn follow_site(&self, site_ms: Option<u32>) {
        match site_ms {
            Some(ms) => {
                let c = self.cfg.lock_ok();
                let same = !c.adaptive && c.latency_ms == ms;
                drop(c);
                if !same {
                    self.set_latency(ms);
                }
            }
            None => {
                let c = self.cfg.lock_ok().clone();
                if c.adaptive == self.own.adaptive
                    && (c.adaptive || c.latency_ms == self.own.latency_ms)
                {
                    return;
                }
                if self.own.adaptive {
                    // Adapting again from the latency it holds now (no jump), its
                    // measurements started afresh.
                    let now_ms = if c.adaptive { None } else { Some(c.latency_ms) };
                    {
                        let mut cfg = self.cfg.lock_ok();
                        *cfg = self.own.clone();
                    }
                    let mut inner = self.inner.lock_ok();
                    if let Some(ms) = now_ms {
                        inner.auto_ms = ms as f64;
                    }
                    inner.jitter.reset();
                    inner.auto_at = Some(Instant::now());
                    self.stats.lock_ok().adaptive = true;
                } else {
                    self.set_latency(self.own.latency_ms);
                }
            }
        }
    }

    /// Holds `latency_ms`, changing to it ([`set_latency`](Self::set_latency)) only when it
    /// differs: for each block of a routed receiver whose latency the site may set
    /// (idn-routing.md §A.12).
    pub fn follow_latency(&self, latency_ms: u32) {
        if self.latency_ms() != latency_ms {
            self.set_latency(latency_ms);
        }
    }

    /// Changes the latency while playing (an OBS source's low-latency box), without
    /// starting the source over: lower, the surplus is cut once (crossfaded); higher,
    /// silence goes in where the sound is quiet (by its fill), or the drift correction
    /// takes it up (by time). (Stopped to refill, the sound broke off mid-wave: a desk's
    /// target for a source on this machine grows with its blocks, lab 374.)
    pub fn set_latency(&self, latency_ms: u32) {
        let (old, was_adaptive) = {
            let mut c = self.cfg.lock_ok();
            // Asked for: held as it is, not adapted.
            let was = (c.latency_ms, c.adaptive);
            c.adaptive = false;
            c.latency_ms = latency_ms;
            was
        };
        // An adapting lane held its group's target (with the site's floor), not only its
        // own: that is the level it leaves. (Read with `cfg` let go: the pushes take
        // `inner`, then `cfg`.)
        let old = if was_adaptive {
            let i = self.inner.lock_ok();
            i.group_ms.max(i.auto_ms).round() as u32
        } else {
            old
        };
        {
            let mut st = self.stats.lock_ok();
            st.target_ms = latency_ms;
            st.adaptive = false;
        }
        let mut inner = self.inner.lock_ok();
        // A latency asked for starts without what underruns added to the old one (16 ms
        // of start-up margin made a site's 2 ms play at 18): it grows again
        // if this one can't be met.
        inner.margin_ms = 0.0;
        inner.margin_floor = 0.0;
        inner.margin_at = None;
        // And without the age a source's sound had before (I4-8: kept for the lane's life).
        inner.age_floor = 0.0;
        inner.too_old = None;
        if latency_ms > old {
            if !self.shared.ready.load(Ordering::Relaxed) {
                // Not playing yet: it starts at the new level.
            } else if inner.timed {
                // By time: the lateness check sees it early and the servo slews it in
                // (a step over 5 ms is put in at once, faded in, as a jump is).
                inner.prev_late = None;
            } else {
                // By its fill: the difference in silence, where the sound is quiet
                // (within 5 s, else at the front, faded).
                inner.relevel = Some((-f64::from(latency_ms - old), Instant::now()));
                inner.quiet_looked = None;
            }
        } else if latency_ms < old {
            inner.growth.clear();
            inner.cut_asked = true;
            if inner.timed {
                // By time: the lateness check cuts the surplus at its next look (within a
                // second), crossfaded; held off 2 s, it slewed at the servo's 1000 ppm
                // instead, a minute from 60 ms to 2.
                inner.hold_until = None;
                inner.prev_late = None;
            } else {
                // The surplus is cut once, crossfaded, as after a burst (recovering).
                inner.hold_until = Some(Instant::now() + Duration::from_secs(2));
            }
        }
    }

    /// The sound waiting in this lane (its buffer and what the resampler still holds),
    /// as time: the next sample taken is this much older than the end of the last block
    /// pushed. A consumer that knows that block's time knows its next sample's.
    pub fn queued(&self) -> Duration {
        let inner = self.inner.lock_ok();
        let ns_dev = 1e9 / self.shared.dev_rate.max(1) as f64;
        let mut ring = {
            let r = self.shared.ring.lock_ok();
            (r.len() / r.width()) as f64 * ns_dev
        };
        if !self.shared.ready.load(Ordering::Relaxed) {
            // Not playing yet: it starts at its prime level after its last block (silence
            // put before the sound makes up what it lacks), so that is when a sample pushed
            // now plays.
            let start = self.shared.start_ns.load(Ordering::Relaxed);
            let since = crate::ptp::mono_ns() - self.shared.pushed_mono.load(Ordering::Relaxed);
            if start > 0 {
                ring = ring.max((start - since.max(0)) as f64);
            }
        }
        let held = inner.rate.as_ref().map_or(0.0, |(sr, _, r)| {
            r.pending() as f64 * 1e9 / (*sr).max(1) as f64 + r.output_delay() as f64 * ns_dev
        });
        Duration::from_nanos((ring + held) as u64)
    }

    /// How long until a sample pushed now is heard: what the lane still holds
    /// ([`queued`](Self::queued)), the time until the device takes its next block (the
    /// block it took last is still playing out of its period), and the device's output
    /// delay (its buffer and latency, as its callback timestamps say:
    /// [`Mixer::output_delay`]). For lip-sync: picture `p` shows when the sound pushed
    /// with it is heard. (Without the time to the next block it read up to a device block
    /// low, by when in the device's period it was asked.)
    pub fn latency(&self) -> Duration {
        self.queued() + self.device.until_next_take() + self.device.output_delay()
    }

    /// When a sample pushed now will be heard ([`latency`](Self::latency) from now).
    pub fn heard_at(&self) -> Instant {
        Instant::now() + self.latency()
    }

    /// This lane's counters and drift correction.
    pub fn stats(&self) -> LaneStats {
        let mut s = self.stats.lock_ok().clone();
        s.frames_out = self.device.frames_out.load(Ordering::Relaxed);
        s.pauses = self.shared.pauses.load(Ordering::Relaxed);
        s.underruns = self.shared.jitter_underruns.load(Ordering::Relaxed);
        s.errors = self.device.errors.load(Ordering::Relaxed);
        s.device_block = self.device.block.load(Ordering::Relaxed);
        s
    }
}

impl Drop for Lane {
    /// Playing, what it holds fades out over up to 5 ms and plays (the rest goes): taken
    /// away at once, a desk's route removed stopped its sound dead (lab 372). The mixer
    /// lets it go once that has played.
    fn drop(&mut self) {
        let s = &self.shared;
        if s.ready.load(Ordering::Relaxed) {
            let mut ring = s.ring.lock_ok();
            let w = ring.width().max(1);
            let n = (ring.len() / w).min((s.dev_rate / 200) as usize);
            if n > 0 {
                ring.truncate(n * w);
                for j in 0..n {
                    let g = 1.0 - (j + 1) as f32 / n as f32;
                    for v in ring.range_mut(j * w..(j + 1) * w) {
                        *v *= g;
                    }
                }
                s.ending.store(true, Ordering::Relaxed);
                return;
            }
        }
        let mut lanes = self.device.lanes.lock_ok();
        lanes.retain(|l| !Arc::ptr_eq(l, &self.shared));
        self.device.lanes_gen.fetch_add(1, Ordering::Release);
        self.device.grave.lock_ok().clear();
    }
}

/// A received block's site time for [`Lane::push_timed`]: its timestamp (100 ns since
/// 1970, UTC) as site time, when its metadata says it was taken on the grandmaster this
/// machine follows (`<x_idn_clock gm>`, idn-clock.md §C.3); else `None`.
pub fn site_time(timestamp: i64, metadata: Option<&str>) -> Option<Ns> {
    let gm = crate::ext::parse_clock_metadata(metadata)?;
    (crate::node::clock::grandmaster()? == gm)
        .then(|| timestamp as Ns * 100 + crate::ptp::UTC_OFFSET_S as Ns * 1_000_000_000)
}

impl Mixer {
    /// The consumer's next block: `out` (interleaved, the mixer's channels) is filled with
    /// the lanes' sound, summed; silence where none plays. Call it at the consumer's pace
    /// (a sound card's callback, a timer at the consumer's rate).
    pub fn fill(&self, out: &mut [f32]) {
        let frames = out.len() / self.dev_ch.max(1);
        self.cb_mono.store(crate::ptp::mono_ns(), Ordering::Relaxed);
        self.frames_out.fetch_add(frames as u64, Ordering::Relaxed);
        let now_ms = (crate::ptp::mono_ns() / 1_000_000).max(0) as u64;
        let at = self.window_at.load(Ordering::Relaxed);
        if now_ms.saturating_sub(at) >= 10_000 {
            // A new window: the last one's largest and this block, the window before
            // that forgotten.
            let last = self.block_window.swap(frames as u32, Ordering::Relaxed);
            self.block.store(last.max(frames as u32), Ordering::Relaxed);
            self.window_at.store(now_ms, Ordering::Relaxed);
        } else {
            self.block_window
                .fetch_max(frames as u32, Ordering::Relaxed);
            self.block.fetch_max(frames as u32, Ordering::Relaxed);
        }
        out.fill(0.0);
        // (The consumer's alone: a second consumer would be a caller's mistake.)
        let mut cb = self.cb_lanes.lock_ok();
        if cb.0 != self.lanes_gen.load(Ordering::Acquire) {
            self.copy_lanes(&mut cb);
        }
        for lane in cb.1.iter() {
            lane.take_into(out);
        }
        // Lanes dropped while playing, once what was left of them has played (when the
        // list and their rings are free: else at a later block).
        if cb.1.iter().any(|l| l.ending.load(Ordering::Relaxed))
            && let Some(mut lanes) = self.lanes.try_lock_ok()
        {
            let before = lanes.len();
            lanes.retain(|l| {
                !l.ending.load(Ordering::Relaxed)
                    || l.ring.try_lock_ok().is_none_or(|r| !r.is_empty())
            });
            if lanes.len() != before {
                self.lanes_gen.fetch_add(1, Ordering::Release);
                drop(lanes);
                self.copy_lanes(&mut cb);
            }
        }
    }

    /// The callback's copy of the lane list made again, when the list is free (else at a
    /// later block): the lanes no longer on it handed to `grave`, to be freed elsewhere.
    fn copy_lanes(&self, cb: &mut (u64, Vec<Arc<Shared>>)) {
        let (Some(lanes), Some(mut grave)) = (self.lanes.try_lock_ok(), self.grave.try_lock_ok())
        else {
            return;
        };
        for l in cb.1.drain(..) {
            if !lanes.iter().any(|n| Arc::ptr_eq(n, &l)) {
                grave.push(l);
            }
        }
        cb.1.extend(lanes.iter().cloned());
        cb.0 = self.lanes_gen.load(Ordering::Acquire);
    }
}

impl Shared {
    /// Adds this lane's next block to `mix`: nothing when it isn't ready, and when it
    /// runs dry, what is left faded out. Never waits for the pushing side: with the ring
    /// held there, the block copied last time plays (see [`Spare`]); only without one
    /// (the first block, a larger block than the last) does it wait.
    fn take_into(&self, mix: &mut [f32]) {
        if !self.ready.load(Ordering::Relaxed) {
            return;
        }
        let dev_ch = self.dev_ch.max(1);
        let want = mix.len() / dev_ch;
        if want == 0 {
            return;
        }
        // (The consumer's alone.)
        let mut spare = self.spare.lock_ok();
        let mut ring = match self.ring.try_lock_ok() {
            Some(r) => r,
            None => {
                let sp = &mut *spare;
                let at = sp.owed;
                if sp.frames >= at + want {
                    let w = sp.cols.len().max(1);
                    let part = &sp.data[at * w..(at + want) * w];
                    add_frames(mix, part, &[], &sp.cols, dev_ch, want, 0);
                    sp.played_last.clear();
                    sp.played_last.extend_from_slice(&part[(want - 1) * w..]);
                    sp.owed += want;
                    return;
                }
                // No copy to play (its first block since it started, the copy used up, or
                // short of a block that grew): waited for, as before. (Its first block
                // skipped instead started a lane playing by time a block late: a jump.)
                self.ring.lock_ok()
            }
        };
        // What the last blocks played from the copy comes off the front: those frames,
        // less what the pushing side cut from the front meanwhile (gone already). Fades
        // and columns added meanwhile changed samples, not where frames are. After other
        // moves (rare: one landing in the very block the ring was held), only if the
        // front is still the copy, bit for bit.
        if spare.owed > 0 {
            let w = ring.width();
            let (cut, pad) = (ring.cut - spare.cut_at, ring.padded - spare.padded_at);
            let (from, frames) = if ring.moved != spare.moved_at || (cut > 0 && pad > 0) {
                (0, None)
            } else if pad > 0 {
                // Silence put in front of them: they come out from behind it (the silence
                // then plays after what was heard, as a delay should).
                (pad as usize, Some(spare.owed))
            } else {
                (0, Some(spare.owed.saturating_sub(cut as usize)))
            };
            let frames = frames.unwrap_or_else(|| {
                let n = spare.owed * spare.cols.len().max(1);
                let same = ring.cols == spare.cols
                    && ring.len() >= n
                    && ring
                        .iter()
                        .zip(&spare.data[..n])
                        .all(|(a, b)| a.to_bits() == b.to_bits());
                if same { spare.owed } else { 0 }
            });
            let have = ring.len() / w;
            let (a, b) = (from.min(have), (from + frames).min(have));
            ring.drain(a * w..b * w);
            spare.owed = 0;
        }
        let w = ring.width();
        let have = (ring.len() / w).min(want);
        let dry = have < want;
        // Dry: what is left fades out over its last 2 ms, the rest is silence, and the
        // buffer refills to its level before playing on.
        let fade = if dry { have.min(96) } else { 0 };
        {
            let (a, b) = ring.as_slices();
            add_frames(mix, a, b, &ring.cols, dev_ch, have, fade);
        }
        // Run dry with nothing left: what played last ramps down over 1 ms (a sender
        // stopped just as a block ended stopped dead, lab 373). Else the last frame kept.
        if dry && have == 0 && spare.played_last.len() == w {
            let n = want.min((self.dev_rate / 1000) as usize).max(1);
            for f in 0..n {
                let g = 1.0 - (f + 1) as f32 / n as f32;
                let row = &mut mix[f * dev_ch..(f + 1) * dev_ch];
                for (k, &c) in ring.cols.iter().enumerate() {
                    if let Some(d) = row.get_mut(c) {
                        *d += spare.played_last[k] * g;
                    }
                }
            }
        }
        if dry {
            spare.played_last.clear();
        } else if have > 0 {
            let at = (have - 1) * w;
            spare.played_last.clear();
            spare.played_last.extend(ring.range(at..at + w));
        }
        ring.drain(..have * w);
        if dry {
            self.ready.store(false, Ordering::Relaxed);
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.fill_sum
            .fetch_add((ring.len() / w) as u64, Ordering::Relaxed);
        self.fill_n.fetch_add(1, Ordering::Relaxed);
        // The next block, copied for a callback that finds the ring held (reused room:
        // no allocation once it has grown to a block).
        let sp = &mut *spare;
        sp.frames = if dry { 0 } else { (ring.len() / w).min(want) };
        (sp.cut_at, sp.padded_at, sp.moved_at) = (ring.cut, ring.padded, ring.moved);
        sp.cols.clone_from(&ring.cols);
        sp.data.clear();
        let (a, b) = ring.as_slices();
        let n = sp.frames * w;
        let na = a.len().min(n);
        sp.data.extend_from_slice(&a[..na]);
        sp.data.extend_from_slice(&b[..n - na]);
    }

    /// The callback as a lone lane sees it (tests).
    #[cfg(test)]
    fn fill(&self, data: &mut [f32]) {
        self.block
            .fetch_max((data.len() / self.dev_ch.max(1)) as u32, Ordering::Relaxed);
        self.cb_mono.store(crate::ptp::mono_ns(), Ordering::Relaxed);
        data.fill(0.0);
        self.take_into(data);
    }
}

/// Adds `have` frames of a lane (`a` then `b`, frames of `cols.len()` samples, column
/// `k` for the device's channel `cols[k]`) into `mix` (frames of `dev_ch`), the last
/// `fade` of them faded out.
fn add_frames(
    mix: &mut [f32],
    a: &[f32],
    b: &[f32],
    cols: &[usize],
    dev_ch: usize,
    have: usize,
    fade: usize,
) {
    let whole = cols.len() == dev_ch && cols.iter().enumerate().all(|(k, &c)| k == c);
    if fade == 0 && whole {
        // The usual case for a lane of all the device's channels: added a slice at a
        // time (the ring's two halves), not a sample at a time.
        let n = have * dev_ch;
        let na = a.len().min(n);
        let (ma, mb) = mix[..n].split_at_mut(na);
        for (d, s) in ma.iter_mut().zip(&a[..na]) {
            *d += *s;
        }
        for (d, s) in mb.iter_mut().zip(&b[..n - na]) {
            *d += *s;
        }
    } else {
        // Each column into its channel (a routed lane's few).
        let mut it = a.iter().chain(b.iter());
        for f in 0..have {
            let g = if have - f <= fade {
                (have - f) as f32 / fade as f32
            } else {
                1.0
            };
            let row = &mut mix[f * dev_ch..(f + 1) * dev_ch];
            for &c in cols {
                let s = *it.next().unwrap_or(&0.0);
                if let Some(d) = row.get_mut(c) {
                    *d += s * g;
                }
            }
        }
    }
}

mod sync_group;
mod watch;
pub(crate) use sync_group::*;
use watch::*;

mod edits;
mod push;
mod servo;
use edits::*;
use push::*;
use servo::*;

#[cfg(test)]
mod tests;
