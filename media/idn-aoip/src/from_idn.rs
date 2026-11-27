//! IDN → network: the routable receiver `NAME` on the Node, its routes in a mixer of lanes
//! by site time (as IDN Return), written into the transmit rings the engine sends from.
//!
//! The engine sends the packet for frames T… when the media clock reaches T
//! (idn-aoip-device.md §5.3), so the mixer's block is written at now + the lead
//! ([`lead`]): room for the largest packet a receiver asked for, a block, and the thread
//! waking late. The block half a ring away is silenced with each one written, so a stall,
//! a clock step or the writer's end plays silence, not sound from a ring ago. While a
//! desk on this machine writes the rings itself (R.33), the channels it claims are left
//! to it (the routes' sound there taken and left out); the routes keep playing on the
//! others.

use crate::clock::{SiteClock, rate};
use idn_aoip_device::engine::MediaClock;
use idn_common::netrings::{Claim, LEN as RING, Rings};
use idn_common::sync::LockOk as _;
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::routing::{AudioReceiver, Route, Router, RouterConfig, RouterEvent};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Blocks taken from the mixer: about 1 ms (44, 48 or 96 samples).
fn block() -> usize {
    crate::clock::ms() as usize
}

/// The smallest packet the lead is counted for (what this device advertises as its
/// largest, 32 frames: a receiver keeping to it never asks for more).
const FPP_USUAL: usize = 32;
/// Room for the writing thread waking late: 2 ms.
fn tx_margin() -> usize {
    2 * block()
}
/// A receiver lane's latency unless the site sets the receiver's (§A.12).
const LANE_LATENCY_MS: u32 = 20;

/// How far ahead of the media clock the sound for the network is written, for packets of
/// up to `fpp` frames. the engine reads a packet's frames when the clock reaches its first,
/// so `fpp` of them must be there; the writer adds a block at each block boundary, and
/// may wake tx_margin() late. (It was 8 ms whatever the packets, for the largest allowed,
/// 256 frames: review 2026-11-16. For 32-frame packets this is 3.7 ms.)
pub fn lead(fpp: usize) -> usize {
    fpp.max(FPP_USUAL) + block() + tx_margin()
}

/// How long after the block after the mixer's current one it is heard (the mixer's model:
/// a block filled at `cb` is heard at `cb` + a block + this): a block written at its
/// boundary starts `lead` − a block ahead, the engine stamps it 0.5 ms early, then the
/// receiving devices' latency (`receivers_latency`, samples). Lanes playing by site time
/// count it in (§C.4).
pub fn output_delay(lead: usize, receivers_latency: usize) -> Duration {
    Duration::from_micros(
        ((lead - block() - block() / 2 + receivers_latency) as u64) * 1_000_000 / u64::from(rate()),
    )
}

/// The least latency of a lane into the mixer, in ms: the output delay (sound can't be
/// heard sooner), a block, and room for the source's own blocks (up to 4 ms).
pub fn lane_floor_ms(lead: usize, receivers_latency: usize) -> u32 {
    output_delay(lead, receivers_latency)
        .as_micros()
        .div_ceil(1000) as u32
        + 1
        + 4
}

/// What the writer does at a wake.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Step {
    /// On from the target, silence around it (the start, a stall, or the clock stepped).
    Resync,
    /// This many blocks from where it is.
    Write(usize),
}

/// The writer's step: write up to `target` (media time, `lead` ahead of the clock) from
/// `written`. Behind by more than the lead and four blocks (the engine has read past what
/// is written, so catching up through the mixer only delays it), or far off either way:
/// a resync.
pub fn plan(target: usize, written: Option<usize>, lead: usize) -> Step {
    let Some(w) = written else {
        return Step::Resync;
    };
    let ahead = target.wrapping_sub(w) as isize;
    if ahead > (lead + 4 * block()) as isize || ahead < -((RING / 2) as isize) {
        Step::Resync
    } else {
        Step::Write(ahead.max(0) as usize / block())
    }
}

/// Whether the engine may have read past what was written before this wake: its packet of
/// `fpp` frames starting at the clock (`target` − `lead`) reaches past `written`.
pub fn late(target: usize, written: usize, lead: usize, fpp: usize) -> bool {
    (written.wrapping_sub(target.wrapping_sub(lead)) as isize) < fpp.max(FPP_USUAL) as isize
}

/// The receiver `name` with `channels` channels on the Node, its routes into `mixer`. Its
/// lanes' latency is at least `floor` ms ([`lane_floor_ms`]), adapting too (`min_ms`):
/// the mixer's output is heard its output delay later, which a lane playing by time must
/// cover (one below it waited for sound from the future and played silence: profile round
/// 12; an adapting lane came back down below it within seconds: bug run H).
pub fn receiver(
    node: &Arc<Node>,
    name: &str,
    channels: u16,
    mixer: &Arc<Mixer>,
    floor: Arc<AtomicU32>,
) -> Result<(Router, Arc<AudioReceiver>), String> {
    let rx = Arc::new(
        AudioReceiver::register_as(
            node,
            name,
            channels,
            &[],
            idn_open_protocol::node::routing::ReceiverKind::NetworkDevice,
        )
        .map_err(|e| format!("receiver {name:?}: {e}"))?,
    );
    rx.attach_mixer(mixer);
    let lanes: Arc<Mutex<HashMap<String, Lane>>> = Arc::default();
    let m = mixer.clone();
    // The floor at the start; it grows if a receiver asks for larger packets (the writer
    // then leads by more), and lanes are then held at it.
    let floor_at_start = floor.load(Ordering::Relaxed);
    let router = Router::start(
        node.clone(),
        rx.clone(),
        RouterConfig { lossless: true },
        move |ev| match ev {
            RouterEvent::Block(b) => {
                let floor_ms = floor.load(Ordering::Relaxed);
                let mut l = lanes.lock_ok();
                let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                    m.lane(&LaneConfig {
                        latency_ms: b
                            .latency_ms
                            .unwrap_or(b.path.start_ms().min(LANE_LATENCY_MS))
                            .max(floor_ms),
                        max_correction_ppm: 1000.0,
                        adaptive: true,
                        min_ms: floor_ms,
                        ..Default::default()
                    })
                });
                lane.follow_site(if floor_ms > floor_at_start {
                    Some(b.latency_ms.unwrap_or(0).max(floor_ms))
                } else {
                    b.latency_ms.map(|l| l.max(floor_ms))
                });
                let at = idn_open_protocol::audio::site_time(
                    b.audio.header.timestamp,
                    b.audio.header.metadata.as_deref(),
                );
                lane.push_timed(
                    b.audio.header.sample_rate,
                    &b.audio.channels,
                    Some(b.map),
                    at,
                );
            }
            RouterEvent::Gone(tx) => {
                lanes.lock_ok().remove(tx);
            }
        },
    )
    .map_err(|e| format!("routes: {e}"))?;
    Ok((router, rx))
}

/// Counters.
#[derive(Default)]
pub struct Counts {
    pub blocks: AtomicU64,
    pub skipped: AtomicU64,
    /// Wakes so late that the engine may have read past what was written.
    pub late: AtomicU64,
    pub peak: AtomicU32,
    /// The transmit channels a desk on this machine has now ([`Claim::word`]; 0: no desk).
    pub desk: AtomicU64,
}

/// The transmit channels a desk has: those it claims; `None` while no desk writes (a desk
/// claims before its first beat, so a beat without a claim leaves the routes every one).
fn desks_channels(rings: &Rings, there: bool) -> Option<Claim> {
    if there { rings.claim() } else { None }
}

/// A block of the routes' sound (`buf`, `tx` channels interleaved) into the transmit rings
/// at `w`, but for the channels the desk has (`taken`); the block half a ring behind
/// silenced on the same channels. The loudest sample written.
fn write_block(
    rings: &Rings,
    buf: &[f32],
    tx: usize,
    w: usize,
    taken: Option<Claim>,
    chan: &mut [f32],
) -> f32 {
    let mut peak = 0.0f32;
    for c in (0..tx).filter(|&c| taken.is_none_or(|k| !k.has(c))) {
        for (i, s) in chan.iter_mut().enumerate() {
            *s = buf[i * tx + c];
            peak = peak.max(s.abs());
        }
        rings.write(c, w, chan);
        rings.silence_channel(c, w.wrapping_sub(RING / 2), chan.len());
    }
    peak
}

/// Silence from `at`, `len` samples, on the channels the desk hasn't (`taken`).
fn silence_free(rings: &Rings, tx: usize, at: usize, len: usize, taken: Option<Claim>) {
    for c in (0..tx).filter(|&c| taken.is_none_or(|k| !k.has(c))) {
        rings.silence_channel(c, at, len);
    }
}

/// The routes whose receiver channel the desk has (`taken`): left out while it does.
pub fn routes_taken(routes: &[Route], taken: Option<Claim>) -> Vec<Route> {
    let Some(k) = taken else {
        return Vec::new();
    };
    let mut r: Vec<Route> = routes
        .iter()
        .filter(|r| k.has(usize::from(r.ch)))
        .cloned()
        .collect();
    r.sort();
    r
}

/// Said once each time [`routes_taken`] changes.
pub fn routes_taken_line(taken: &[Route]) -> String {
    if taken.is_empty() {
        return "no IDN route plays on the desk's channels".into();
    }
    let each: Vec<String> = taken
        .iter()
        .map(|r| {
            format!(
                "{} {} on {}",
                r.tx,
                u32::from(r.tx_ch) + 1,
                u32::from(r.ch) + 1
            )
        })
        .collect();
    format!(
        "IDN routes left out while the desk has their channels: {}",
        each.join(", ")
    )
}

/// What the writing thread needs besides the rings and the mixer.
pub struct Writer {
    pub clock: Arc<SiteClock>,
    pub stop: Arc<AtomicBool>,
    pub counts: Arc<Counts>,
    /// The largest packet any receiver has asked for (0 before the first).
    pub max_fpp: Arc<AtomicUsize>,
    pub tx: usize,
    /// The receiving devices' latency, samples.
    pub receivers_latency: usize,
    /// The lanes' floor, raised when the lead grows.
    pub floor: Arc<AtomicU32>,
}

/// The writing thread, until `stop`.
pub fn spawn(
    rings: &'static Rings,
    mixer: Arc<Mixer>,
    w: Writer,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let Writer {
        clock,
        stop,
        counts,
        max_fpp,
        tx,
        receivers_latency,
        floor,
    } = w;
    std::thread::Builder::new()
        .name("idn-aoip-out".into())
        .spawn(move || {
            idn_common::rt::audio_this_thread();
            let mut buf = vec![0.0f32; block() * tx];
            let mut chan = vec![0.0f32; block()];
            let mut written: Option<usize> = None;
            let mut lead_now = lead(0);
            let mut desk = DeskWatch::new(rings.desk_beats());
            // The desk's channels as last said (a desk can come back with others).
            let mut said: Option<Claim> = None;
            while !stop.load(Ordering::Relaxed) {
                let wait = match (clock.now(), written) {
                    (Some(now), Some(w)) => {
                        let due = w.wrapping_add(block()).wrapping_sub(lead_now) as u64;
                        Duration::from_nanos(due.saturating_sub(now) * 1_000_000_000 / u64::from(rate()))
                            .clamp(Duration::from_micros(50), Duration::from_millis(2))
                    }
                    _ => Duration::from_millis(1),
                };
                std::thread::sleep(wait);
                let fpp = max_fpp.load(Ordering::Acquire);
                rings.set_packet(fpp.max(FPP_USUAL));
                if desk.look(rings.desk_beats(), std::time::Instant::now()) == Some(false) {
                    println!("the desk left; IDN's routes play on all channels to the network again");
                    // From the clock again, all channels silent around it.
                    written = None;
                    said = None;
                }
                // Read at every wake (one word): a desk started again with other outputs
                // moves its claim without its beat ever stopping.
                let taken = desks_channels(rings, desk.there);
                counts.desk.store(taken.map_or(0, |k| k.word()), Ordering::Relaxed);
                if let Some(k) = taken.filter(|k| said != Some(*k)) {
                    said = Some(k);
                    if k.channels().len() >= tx && k.first == 0 {
                        println!(
                            "a desk on this machine plays to the network directly on all {tx} channels; IDN's routes to the receiver wait"
                        );
                    } else {
                        println!(
                            "a desk on this machine plays to the network directly on channels {k} of {tx}; IDN's routes keep the others"
                        );
                    }
                }
                let Some(now) = clock.now() else {
                    // No clock: the mixer still runs (its lanes don't pile up).
                    mixer.fill(&mut buf);
                    continue;
                };
                if lead(fpp) > lead_now {
                    lead_now = lead(fpp);
                    mixer.set_output_delay(output_delay(lead_now, receivers_latency));
                    floor.store(lane_floor_ms(lead_now, receivers_latency), Ordering::Relaxed);
                    println!(
                        "a receiver asked for packets of {fpp} samples: writing {:.1} ms ahead from now on",
                        lead_now as f64 * 1000.0 / f64::from(rate())
                    );
                }
                let target = (now as usize).wrapping_add(lead_now);
                let blocks = match plan(target, written, lead_now) {
                    Step::Resync => {
                        if written.is_some() {
                            counts.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        silence_free(
                            rings,
                            tx,
                            target.wrapping_sub(lead_now),
                            2 * lead_now + block(),
                            taken,
                        );
                        written = Some(target);
                        continue;
                    }
                    Step::Write(n) => n,
                };
                let mut w = written.unwrap_or(target);
                if blocks > 0 && late(target, w, lead_now, fpp) {
                    counts.late.fetch_add(1, Ordering::Relaxed);
                }
                // The desk has them all: the routes' sound is only taken (the lanes don't
                // pile up).
                let all = taken.is_some_and(|k| (0..tx).all(|c| k.has(c)));
                for _ in 0..blocks {
                    mixer.fill(&mut buf);
                    if all {
                        w = w.wrapping_add(block());
                        continue;
                    }
                    let peak = write_block(rings, &buf, tx, w, taken, &mut chan);
                    counts.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
                    counts.blocks.fetch_add(1, Ordering::Relaxed);
                    w = w.wrapping_add(block());
                }
                written = Some(w);
            }
        })
}

/// Whether a desk writes the transmit rings: its beat in the rings' head moved within the
/// last 100 ms.
struct DeskWatch {
    beats: u64,
    moved: Option<std::time::Instant>,
    there: bool,
}

impl DeskWatch {
    fn new(beats: u64) -> DeskWatch {
        DeskWatch {
            beats,
            moved: None,
            there: false,
        }
    }

    /// The desk's beats now: `Some(true)` when it came, `Some(false)` when it left.
    fn look(&mut self, beats: u64, now: std::time::Instant) -> Option<bool> {
        if beats != self.beats {
            self.beats = beats;
            self.moved = Some(now);
        }
        let there = self
            .moved
            .is_some_and(|m| now.duration_since(m) < Duration::from_millis(100));
        (there != self.there).then(|| {
            self.there = there;
            there
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desk_is_seen_coming_and_going() {
        let t0 = std::time::Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut d = DeskWatch::new(7);
        // The count as it was (a desk that ran before this program started): none.
        assert_eq!(d.look(7, ms(0)), None);
        assert!(!d.there);
        // Its beat moves: there; still moving: no news.
        assert_eq!(d.look(8, ms(1)), Some(true));
        assert_eq!(d.look(9, ms(3)), None);
        // Still for under 100 ms (a late block): there; then gone.
        assert_eq!(d.look(9, ms(90)), None);
        assert_eq!(d.look(9, ms(104)), Some(false));
        assert_eq!(d.look(9, ms(500)), None);
        assert_eq!(d.look(10, ms(501)), Some(true));
    }

    /// R.33: a desk claiming channels 3..5 of 8 (TX 4–5) has those; the routes' sound
    /// goes on the others, and their block half a ring behind is silenced, not the desk's.
    #[test]
    fn a_desk_has_only_the_channels_it_claims() {
        let tx = 8;
        let rings = Rings::create(0, tx, 48_000, 96).unwrap();
        let desk = Rings::open(&rings.descriptor()).unwrap();
        let n = 48;
        let w = 10_000usize;
        let tx_at = |c: usize, i: usize| {
            rings.ring(false, c).unwrap()[i & (RING - 1)].load(Ordering::Relaxed)
        };
        // The desk's sound on its channels, now and half a ring behind (what it wrote then).
        desk.set_claim(Claim { first: 3, count: 2 });
        for c in 3..5 {
            desk.write(c, w, &[0.7; 48]);
            desk.write(c, w.wrapping_sub(RING / 2), &[0.7; 48]);
        }
        // A ring ago, on a route's channel.
        rings.write(0, w.wrapping_sub(RING / 2), &[0.3; 48]);
        let routes = vec![0.25f32; n * tx];
        let mut chan = vec![0f32; n];
        let k = desks_channels(&rings, true);
        assert_eq!(k, Some(Claim { first: 3, count: 2 }));
        let peak = write_block(&rings, &routes, tx, w, k, &mut chan);
        assert!((peak - 0.25).abs() < 1e-6);
        for c in 0..tx {
            let want = if (3..5).contains(&c) { 0.7 } else { 0.25 };
            assert_eq!(
                tx_at(c, w + n - 1),
                idn_common::netrings::to_sample(want),
                "{c}"
            );
            let behind = if (3..5).contains(&c) {
                idn_common::netrings::to_sample(0.7)
            } else {
                0
            };
            assert_eq!(tx_at(c, w.wrapping_sub(RING / 2)), behind, "{c}");
        }
        // A resync silences the routes' channels only.
        silence_free(&rings, tx, w, n, k);
        assert_eq!(tx_at(0, w), 0);
        assert_eq!(tx_at(3, w), idn_common::netrings::to_sample(0.7));
        // No desk, or a beat before any claim: everything is the routes'.
        assert_eq!(desks_channels(&rings, false), None);
        let fresh = Rings::create(0, tx, 48_000, 96).unwrap();
        assert_eq!(desks_channels(&fresh, true), None);
    }

    #[test]
    fn routes_on_the_desks_channels_are_named() {
        let r = |ch: u16, tx: &str, tx_ch: u16| Route {
            ch,
            tx: tx.into(),
            tx_ch,
        };
        let routes = vec![r(0, "A (x)", 0), r(4, "B (y)", 1), r(3, "B (y)", 0)];
        let k = Some(Claim { first: 3, count: 2 });
        let t = routes_taken(&routes, k);
        assert_eq!(t, vec![r(3, "B (y)", 0), r(4, "B (y)", 1)]);
        assert_eq!(
            routes_taken_line(&t),
            "IDN routes left out while the desk has their channels: B (y) 1 on 4, B (y) 2 on 5"
        );
        assert!(routes_taken(&routes, None).is_empty());
        assert_eq!(
            routes_taken_line(&[]),
            "no IDN route plays on the desk's channels"
        );
    }

    #[test]
    fn the_lead_follows_the_packets() {
        // 32-frame packets (what the device advertises): 3.7 ms; the largest: 8.3 ms.
        assert_eq!(lead(0), 176);
        assert_eq!(lead(32), 176);
        assert_eq!(lead(256), 400);
        // Heard: the lead less a block and a half, and the receivers' latency.
        assert_eq!(
            output_delay(176, 96).as_micros(),
            (176 - 72 + 96) * 1_000_000 / 48_000
        );
        // A lane never under the output delay (rounded up), a block and 4 ms.
        assert_eq!(lane_floor_ms(176, 96), 5 + 1 + 4);
        assert!(lane_floor_ms(400, 480) as u128 > output_delay(400, 480).as_millis());
    }

    #[test]
    fn the_writer_keeps_ahead_and_resyncs_after_a_stall() {
        let (l, t) = (lead(32), 10_000_000usize);
        assert_eq!(plan(t, None, l), Step::Resync);
        // At the boundary: a block; before it: none.
        assert_eq!(plan(t, Some(t - block()), l), Step::Write(1));
        assert_eq!(plan(t, Some(t - block() + 1), l), Step::Write(0));
        assert_eq!(plan(t, Some(t), l), Step::Write(0));
        // 3 ms late: caught up. Past the lead and four blocks, or the clock stepped back
        // or far ahead: a resync.
        assert_eq!(plan(t, Some(t - 3 * block()), l), Step::Write(3));
        assert_eq!(plan(t, Some(t - l - 4 * block() - 1), l), Step::Resync);
        assert_eq!(plan(t, Some(t + RING / 2 + 1), l), Step::Resync);
        assert_eq!(plan(t, Some(t.wrapping_sub(RING)), l), Step::Resync);
        // Just ahead of the target (the lead grew less than what is written): it waits.
        assert_eq!(plan(t, Some(t + 100), l), Step::Write(0));
    }
}
