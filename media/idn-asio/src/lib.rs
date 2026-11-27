//! IDN as an ASIO driver (spec/extensions/idn-node.md §N.14): an ASIO program (a DAW,
//! vMix, Reaper, …) gets IDN channels both ways, with no sound card and no kernel driver.
//!
//! - Its **outputs** are an IDN source named after the program (`HOST (Reaper)`), which
//!   the routing matrix routes anywhere.
//! - Its **inputs** are a routable receiver of the same name on this machine's IDN Node:
//!   whatever the matrix routes to it plays there, each transmitter through its own lane
//!   (buffer and drift correction), by site time where the site clock runs.
//! - Its **clock** is this machine's: a thread of its own calls the program's
//!   `bufferSwitch` every buffer and sends the outputs the moment the program has written
//!   them.
//!
//! The ASIO interface itself (the COM object hosts load, and its factory) is `object.rs`,
//! on IDN's own interface definitions (idn-asio-abi, from cwASIO's MIT headers; no
//! Steinberg SDK); it hands this crate plain values only (the `idn_asio_*` functions
//! below). Everything here also runs, and is tested, without it.
//!
//! # Safety
//!
//! Every `idn_asio_*` entry point has one contract: `p` is a driver from
//! [`idn_asio_new`] not yet freed, and each pointer argument is valid for the call (the
//! shim's own variables and buffers).
#![allow(clippy::missing_safety_doc)] // the contract is stated once, above

use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::routing::{AudioReceiver, Router, RouterConfig, RouterEvent};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod com;
/// The library's entry points on Linux (cwASIO's): `instantiateDriver`, `registerDriver`, …
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(any(windows, target_os = "linux"))]
mod object;

/// The rates offered.
pub const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
/// Buffer sizes: smallest, largest, preferred (frames; powers of two between).
pub const BUFFER: (u32, u32, u32) = (32, 2048, 256);

/// The driver's settings (`asio.json` in the IDN state folder; all optional).
#[derive(Debug, Clone)]
pub struct Config {
    /// The name on the network (default: the program's, `Reaper`).
    pub name: String,
    pub inputs: u16,
    pub outputs: u16,
    /// Each input lane's buffer, ms (sound from the network).
    pub latency_ms: u32,
}

/// The driver's own instance (its registration's name): the one hosts without instances
/// load, set by `asio.json`'s top level.
pub const DEFAULT_INSTANCE: &str = "IDN";

impl Config {
    /// From `dir/asio.json` if there is one; defaults (16 × 16, 10 ms) otherwise.
    pub fn load(dir: Option<&std::path::Path>, program: &str) -> Config {
        Config::load_instance(dir, program, DEFAULT_INSTANCE)
    }

    /// Instance `instance`'s (a cwASIO host's registration name, §A.6): the default
    /// instance from `asio.json`'s top level; another from `instances.NAME`, what it
    /// leaves out from the top level, and named on the network `PROGRAM INSTANCE` (two
    /// instances in one program are two sources, not one name twice). `asio.json` is
    /// written by hand, never by a program: a damaged one is logged (its name, line and
    /// column) and left where it is for the person to mend; the defaults then.
    pub fn load_instance(dir: Option<&std::path::Path>, program: &str, instance: &str) -> Config {
        let v: Value =
            match dir.map(|d| idn_common::fsutil::load_json_readonly(&d.join("asio.json"))) {
                Some(Ok(Some(v))) => v,
                Some(Err(e)) => {
                    idn_open_protocol::log::log(&format!("idn-asio: {e}; the defaults used"));
                    Value::Null
                }
                _ => Value::Null,
            };
        // (`instances.IDN` counts too, over the top level.)
        let own = &v["instances"][instance];
        let n = |k: &str, d: u64, max: u64| {
            own[k]
                .as_u64()
                .or_else(|| v[k].as_u64())
                .unwrap_or(d)
                .clamp(0, max)
        };
        let named = |x: &Value| {
            x.as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        };
        let name = named(&own["name"]).unwrap_or_else(|| {
            if instance == DEFAULT_INSTANCE {
                named(&v["name"]).unwrap_or_else(|| program.to_owned())
            } else {
                format!("{program} {instance}")
            }
        });
        Config {
            name,
            inputs: n("inputs", 16, 64) as u16,
            outputs: n("outputs", 16, 64) as u16,
            latency_ms: n("latency_ms", 10, 500) as u32,
        }
    }
}

/// The program's buffer switch, called by the clock: (its context, the half to process,
/// samples since start, nanoseconds of this machine's clock).
pub type Switch = unsafe extern "C" fn(*mut c_void, i32, u64, u64);

/// The network side, made at `init`.
struct Net {
    _publisher: Publisher,
    out: Option<Output>,
    _router: Option<Router>,
    /// What the router feeds (at the rate chosen), shared with its callback.
    input: Arc<Mutex<Arc<Mixer>>>,
    lanes: Arc<Mutex<BTreeMap<String, Lane>>>,
    /// The desk on this machine the driver's clock follows (roadmap R.26).
    desk: Arc<DeskClock>,
}

/// The sound of one transmitter on this machine (through shared memory: the desk) as it
/// arrives: its frames, when, and its rate. The driver times the program's buffers by it,
/// so the desk's clock (its card, or site time) runs the program too and nothing drifts
/// between them (roadmap R.26). The first such transmitter is taken.
#[derive(Default)]
struct DeskClock {
    tx: Mutex<Option<String>>,
    /// The change count (see `write`).
    seq: AtomicU64,
    frames: AtomicU64,
    at: AtomicI64,
    rate: AtomicU32,
    /// The latency for the desk's sound once it clocks the program: one buffer of the
    /// program's, the desk's 2 ms block and 1 ms (ms; 0 before the clock starts).
    want_ms: AtomicU32,
}

impl DeskClock {
    /// A change of (frames, at, rate), as one: the count odd while it is written (one
    /// writer: the `tx` lock is held). Read as three atomics, a new time could come with
    /// the old count: the clock off by a block now and then (bug run 2026-11-25).
    fn write(&self, f: impl FnOnce()) {
        self.seq.fetch_add(1, Ordering::Relaxed);
        // (A fence: the odd count seen before any of the new values, on every CPU.)
        std::sync::atomic::fence(Ordering::Release);
        f();
        self.seq.fetch_add(1, Ordering::Release);
    }

    fn arrived(&self, tx: &str, frames: usize, rate: u32) {
        let mut t = self.tx.lock_ok();
        if t.is_none() {
            *t = Some(tx.to_owned());
        }
        if t.as_deref() == Some(tx) {
            let now = idn_open_protocol::ptp::mono_ns();
            self.write(|| {
                self.at.store(now, Ordering::Relaxed);
                self.frames.fetch_add(frames as u64, Ordering::Relaxed);
                self.rate.store(rate, Ordering::Relaxed);
            });
        }
    }

    fn gone(&self, tx: &str) {
        let mut t = self.tx.lock_ok();
        if t.as_deref() == Some(tx) {
            *t = None;
            self.write(|| {
                self.at.store(0, Ordering::Relaxed);
                self.frames.store(0, Ordering::Relaxed);
            });
        }
    }

    /// As the pacer reads a reference clock: the three from one change (never waiting on
    /// the writer: a few tries, then nothing this time).
    fn read(&self) -> Option<(u64, i64, u32)> {
        for _ in 0..8 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let (at, frames, rate) = (
                self.at.load(Ordering::Relaxed),
                self.frames.load(Ordering::Relaxed),
                self.rate.load(Ordering::Relaxed),
            );
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return (at != 0).then_some((frames, at, rate));
            }
        }
        None
    }
}

/// The buffers, two halves per channel, as the program reads and writes them.
struct Buffers {
    frames: usize,
    /// Per input channel, then per output channel: two halves of `frames` each.
    inputs: Vec<[Vec<f32>; 2]>,
    outputs: Vec<[Vec<f32>; 2]>,
}

thread_local! {
    /// The clock this thread is (its number, 0: none): the program's callback runs on it.
    /// Per clock, not just "a clock": a program holding two IDN drivers may stop one from
    /// the other's callback (bug run 2026-11-25).
    static ON_CLOCK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// On an IDN driver's clock thread (any driver's: inside a program's callback).
pub(crate) fn on_a_clock() -> bool {
    ON_CLOCK.with(|c| c.get()) != 0
}

/// On another IDN driver's clock thread than `p`'s: a call to `p` that would wait for its
/// clock, or change what its clock uses, is refused there (§A.4).
pub(crate) fn on_another_clock(p: *mut c_void) -> bool {
    // SAFETY: the module's contract (`p` from `idn_asio_new`, not freed).
    on_a_clock() && !unsafe { dr(p) }.on_my_clock()
}

/// `p` has no buffers and no clock: nothing of it in use yet.
pub(crate) fn idle(p: *mut c_void) -> bool {
    // SAFETY: as above.
    let d = unsafe { dr(p) };
    !d.running() && d.buffers.lock_ok().is_none() && d.sender.lock_ok().is_none()
}

/// Clock numbers (never 0).
static CLOCKS: AtomicU64 = AtomicU64::new(1);

/// A driver handed to a thread to be dropped there.
struct SendDriver {
    _d: Box<Driver>,
}
// SAFETY: the driver is shared across threads already (its state behind locks and
// atomics); here it only moves to be dropped.
unsafe impl Send for SendDriver {}

/// The running clock.
struct Clock {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The driver. Every entry point takes it shared, its changing state behind locks and
/// atomics: the program may call in from its own threads while another call runs (its
/// bufferSwitch reads the position while `stop` joins the clock), and a `&mut Driver`
/// alongside would be two live aliases.
pub struct Driver {
    pub config: Config,
    rate: AtomicU32,
    net: Mutex<Option<Net>>,
    error: Mutex<String>,
    buffers: Arc<Mutex<Option<Buffers>>>,
    clock: Mutex<Option<Clock>>,
    /// The thread sending the outputs (it hands the output back when it ends).
    sender: Mutex<Option<std::thread::JoinHandle<Option<Output>>>>,
    /// Frames a half, while there are buffers (read without their lock: a program may ask
    /// for the latency from inside its callback).
    frames: AtomicU32,
    /// The output's name and port on the network (kept: the output itself moves to the
    /// sender while running).
    out_addr: Mutex<Option<(String, u16)>>,
    /// Samples since start and the time of the last buffer (ns since `epoch`).
    position: Arc<(AtomicU64, AtomicU64)>,
    /// The last clock's number (its thread's `ON_CLOCK`), kept until its sender is joined.
    clock_id: AtomicU64,
    /// The lanes' largest latency target now (ms; 0: none yet), kept by the router so a
    /// host asking from its time-critical callback reads it without a lock (bug run
    /// 2026-11-25).
    target_ms: Arc<AtomicU32>,
    /// Start, stop, buffers made or let go, the rate: one at a time (registered "Both",
    /// a program may call them from several threads at once; bug run 2026-11-25). Never
    /// taken on the clock's own thread.
    control: Mutex<()>,
    epoch: Instant,
    /// The system's time at `epoch` (Windows: `timeGetTime`; Linux: the monotonic clock;
    /// in ns), so the positions' times are on the clock hosts compare them with: they were
    /// from the driver's start (deep bug run 2026-11-09).
    base_ns: u64,
}

impl Driver {
    pub fn new(config: Config) -> Driver {
        Driver {
            config,
            rate: AtomicU32::new(48_000),
            net: Mutex::new(None),
            error: Mutex::default(),
            buffers: Arc::default(),
            clock: Mutex::new(None),
            sender: Mutex::new(None),
            frames: AtomicU32::new(BUFFER.2),
            out_addr: Mutex::new(None),
            position: Arc::default(),
            clock_id: AtomicU64::new(0),
            target_ms: Arc::default(),
            control: Mutex::new(()),
            epoch: Instant::now(),
            base_ns: system_time_ns(),
        }
    }

    /// Joins the network: the output (always) and the routable receiver (with the Node).
    /// `node`: the machine's Node, if it runs.
    pub fn init(&self, node: Option<Arc<Node>>, mdns: Option<bool>) -> Result<(), String> {
        let c = &self.config;
        let publisher = Publisher::new(PublisherConfig {
            secure: node.as_ref().map(|n| n.secure_config()),
            mdns,
            ..Default::default()
        })
        .map_err(|e| format!("IDN: {e}"))?;
        let out = (c.outputs > 0)
            .then(|| {
                Output::new(
                    &publisher,
                    OutputConfig {
                        audio_labels: (1..=c.outputs).map(|i| format!("Out {i}")).collect(),
                        ..OutputConfig::new(&c.name)
                    },
                )
            })
            .transpose()
            .map_err(|e| format!("IDN output: {e}"))?;
        let input = Arc::new(Mutex::new(Mixer::new(
            self.rate(),
            c.inputs.max(1),
            &c.name,
        )));
        let lanes: Arc<Mutex<BTreeMap<String, Lane>>> = Arc::default();
        let desk: Arc<DeskClock> = Arc::default();
        let target_ms = self.target_ms.clone();
        let router = match (&node, c.inputs) {
            (Some(n), k) if k > 0 => {
                let labels: Vec<String> = (1..=k).map(|i| format!("In {i}")).collect();
                let rx = Arc::new(
                    AudioReceiver::register_as(
                        n,
                        &c.name,
                        k,
                        &labels,
                        idn_open_protocol::node::routing::ReceiverKind::AsioDriver,
                    )
                    .map_err(|e| format!("IDN inputs: {e}"))?,
                );
                let (input, lanes2, latency) = (input.clone(), lanes.clone(), c.latency_ms);
                let (desk2, target2) = (desk.clone(), target_ms.clone());
                Some(
                    Router::start(n.clone(), rx, RouterConfig { lossless: true }, move |ev| {
                        match ev {
                            RouterEvent::Block(b) => {
                                let mut l = lanes2.lock_ok();
                                // Its own, unless the site sets the receiver's (§A.12).
                                let want = b.latency_ms.unwrap_or(latency);
                                let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                                    input.lock_ok().lane(&LaneConfig {
                                        latency_ms: want,
                                        max_correction_ppm: 1000.0,
                                        ..Default::default()
                                    })
                                });
                                // The desk on this machine shares the program's clock (R.26):
                                // its sound needs only a buffer and a block of room.
                                let near = desk2.want_ms.load(Ordering::Relaxed);
                                let want = if b.audio.via == "machine"
                                    && near > 0
                                    && b.latency_ms.is_none()
                                {
                                    want.min(near)
                                } else {
                                    want
                                };
                                lane.follow_latency(want);
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
                                // A transmitter on this machine (shared memory): the desk,
                                // whose clock the program's buffers follow.
                                if b.audio.via == "machine" {
                                    desk2.arrived(
                                        b.tx,
                                        b.audio.channels.first().map_or(0, Vec::len),
                                        b.audio.header.sample_rate,
                                    );
                                }
                                let most = l.values().map(|x| x.stats().target_ms).max();
                                target2.store(most.unwrap_or(0), Ordering::Relaxed);
                            }
                            RouterEvent::Gone(tx) => {
                                let mut l = lanes2.lock_ok();
                                l.remove(tx);
                                let most = l.values().map(|x| x.stats().target_ms).max();
                                target2.store(most.unwrap_or(0), Ordering::Relaxed);
                                drop(l);
                                desk2.gone(tx);
                            }
                        }
                    })
                    .map_err(|e| format!("IDN inputs: {e}"))?,
                )
            }
            _ => None,
        };
        *self.out_addr.lock_ok() = out.as_ref().map(|o| (o.name().to_owned(), o.port()));
        *self.net.lock_ok() = Some(Net {
            _publisher: publisher,
            out,
            _router: router,
            input,
            lanes,
            desk,
        });
        Ok(())
    }

    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }

    /// On this driver's own clock thread (inside the program's callback).
    fn on_my_clock(&self) -> bool {
        let id = self.clock_id.load(Ordering::Acquire);
        id != 0 && ON_CLOCK.with(|c| c.get()) == id
    }

    /// The control lock (see `control`); none on any IDN driver's clock thread. Not only
    /// this driver's: a program holding two IDN drivers that stops each from the other's
    /// callback had each clock wait for the other to end, for ever. A clock thread never
    /// waits on a driver (two-driver guard, 2026-11-26).
    fn control(&self) -> Result<std::sync::MutexGuard<'_, ()>, String> {
        if on_a_clock() {
            return Err("not from inside a buffer switch".into());
        }
        Ok(self.control.lock_ok())
    }

    /// A new rate: the inputs' lanes start again at it (not while running).
    pub fn set_rate(&self, rate: u32) -> Result<(), String> {
        if !RATES.contains(&rate) {
            return Err(format!("{rate} Hz: 44.1, 48, 88.2 or 96 kHz"));
        }
        let _c = self.control()?;
        if self.running() {
            return Err("not while running".into());
        }
        if self.rate.swap(rate, Ordering::Relaxed) != rate
            && let Some(n) = self.net.lock_ok().as_ref()
        {
            // The mixer replaced under the lanes' lock: a lane made meanwhile on the old
            // mixer would stay silent.
            let mut lanes = n.lanes.lock_ok();
            *n.input.lock_ok() = Mixer::new(rate, self.config.inputs.max(1), &self.config.name);
            lanes.clear();
        }
        Ok(())
    }

    /// The buffers for `frames` a half (made for every channel).
    pub fn create_buffers(&self, frames: u32) -> Result<(), String> {
        let _c = self.control()?;
        // Made again while running (a host after a reset request, without stopping): the
        // clock first, or it keeps writing the old buffers the host no longer has, sized
        // for the old frame count. Also when refused below: the host's callbacks are gone,
        // and a clock left running sent its last buffers on and on (bug run 2026-11-25).
        self.stop_locked();
        if !(BUFFER.0..=BUFFER.1).contains(&frames) {
            return Err(format!("buffer of {frames}: {} to {}", BUFFER.0, BUFFER.1));
        }
        let half = || [vec![0f32; frames as usize], vec![0f32; frames as usize]];
        *self.buffers.lock_ok() = Some(Buffers {
            frames: frames as usize,
            inputs: (0..self.config.inputs).map(|_| half()).collect(),
            outputs: (0..self.config.outputs).map(|_| half()).collect(),
        });
        self.frames.store(frames, Ordering::Relaxed);
        Ok(())
    }

    pub fn dispose_buffers(&self) {
        let Ok(_c) = self.control() else { return };
        self.stop_locked();
        *self.buffers.lock_ok() = None;
    }

    /// Where the program finds channel `ch`'s half `half` (input or output); null when
    /// there is no such buffer. Valid until the buffers are disposed of.
    pub fn buffer(&self, input: bool, ch: usize, half: usize) -> *mut f32 {
        let mut b = self.buffers.lock_ok();
        let Some(b) = b.as_mut() else {
            return std::ptr::null_mut();
        };
        let list = if input { &mut b.inputs } else { &mut b.outputs };
        list.get_mut(ch)
            .and_then(|h| h.get_mut(half))
            .map_or(std::ptr::null_mut(), |v| v.as_mut_ptr())
    }

    /// Latency in frames: inputs (a buffer and the lanes' target), outputs (a buffer).
    pub fn latencies(&self) -> (u32, u32) {
        let frames = self.frames.load(Ordering::Relaxed);
        // The lanes' target as they run (a desk on this machine, a latency the site set,
        // adapting), not the configured one: hosts line recordings up by it (deep bug run
        // 2026-11-09).
        // (Atomics only: a host may ask from its time-critical callback.)
        let ms = self.target_ms.load(Ordering::Relaxed);
        let ms = if ms > 0 { ms } else { self.config.latency_ms };
        let lanes = (self.rate() as u64 * ms as u64 / 1000).min(i32::MAX as u64 / 2) as u32;
        (frames.saturating_add(lanes), frames)
    }

    /// Samples since start and the time they were reached (ns).
    pub fn position(&self) -> (u64, u64) {
        (
            self.position.0.load(Ordering::Relaxed),
            self.position.1.load(Ordering::Relaxed),
        )
    }

    /// Nanoseconds of this driver's clock now.
    pub fn now_ns(&self) -> u64 {
        self.base_ns + self.epoch.elapsed().as_nanos() as u64
    }

    /// Starts the clock: every buffer, the inputs filled, `switch(ctx, half, …)` called,
    /// the outputs sent.
    pub fn start(&self, switch: Switch, ctx: *mut c_void) -> Result<(), String> {
        // (From inside the callback: refused; a stop there waits for it to return.)
        let _c = self
            .control()
            .map_err(|_| "start the clock again after the buffer switch returns".to_owned())?;
        if self.running() {
            return Ok(());
        }
        // A sender left by a stop from inside the callback: its output back first.
        if self.sender.lock_ok().is_some() {
            self.stop_locked();
        }
        let frames = self
            .buffers
            .lock_ok()
            .as_ref()
            .map(|b| b.frames)
            .ok_or("no buffers")?;
        let (rate, outputs) = (self.rate(), self.config.outputs as usize);
        let stop = Arc::new(AtomicBool::new(false));
        let (buffers, position, epoch) = (self.buffers.clone(), self.position.clone(), self.epoch);
        let base_ns = self.base_ns;
        // What times the buffers (R.26): the desk on this machine, else site time, else
        // this machine's clock.
        let desk = self.net.lock_ok().as_ref().map(|n| n.desk.clone());
        if let Some(d) = &desk {
            let buffer_ms = (frames as u64 * 1000).div_ceil(rate.max(1) as u64) as u32;
            d.want_ms.store(buffer_ms + 3, Ordering::Relaxed);
        }
        let reference: Option<idn_open_protocol::audio::pace::ClockRef> =
            desk.map(|d| Arc::new(move || d.read()) as _);
        let input = self
            .net
            .lock_ok()
            .as_ref()
            .map(|n| n.input.lock_ok().clone());
        // The program's sound out: through a channel to the sender, so a slow network never
        // holds up the program's next buffer. Bounded: a sender stuck that long loses
        // blocks rather than the machine its memory (12 MB/s grew).
        // (The clock thread fills buffers handed back by the sender, which makes the blocks:
        // it allocated one per channel each buffer, ~1500 a second at 32 frames, on the
        // raised-priority thread; bug run 2026-11-25.)
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u32, Vec<Vec<f32>>)>(64);
        let (spare_tx, spare_rx) = std::sync::mpsc::sync_channel::<Vec<Vec<f32>>>(64);
        // Blocks made now, for the clock to fill (it never allocates): about 100 ms of
        // them (8 were 5 ms at 32 frames: a sender held off by a busy program lost
        // buffers), each channel with its room.
        let blocks = (rate as usize / 10).div_ceil(frames.max(1)).clamp(8, 64);
        for _ in 0..blocks {
            let _ = spare_tx.try_send((0..outputs).map(|_| Vec::with_capacity(frames)).collect());
        }
        // The output handed over once the sender runs (a sender that didn't start kept
        // it, and every later start had none).
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<Option<Output>>(1);
        let sender = std::thread::Builder::new()
            .name("idn-asio-send".into())
            .spawn(move || {
                let out = out_rx.recv().ok().flatten();
                while let Ok((sample_rate, b)) = rx.recv() {
                    if let Some(o) = &out {
                        // (A panic here ended the sender, the output with it.)
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            o.send_audio(
                                AudioBlock {
                                    sample_rate,
                                    channels: b.clone(),
                                },
                                None,
                            )
                        }));
                    }
                    let _ = spare_tx.try_send(b);
                }
                out
            })
            .map_err(|e| e.to_string())?;
        let _ = out_tx.send(self.net.lock_ok().as_mut().and_then(|n| n.out.take()));
        let ctx = ctx as usize;
        let stop2 = stop.clone();
        let id = CLOCKS.fetch_add(1, Ordering::Relaxed);
        self.clock_id.store(id, Ordering::Release);
        // There before its thread starts: a stop from its very first callback finds it.
        *self.clock.lock_ok() = Some(Clock {
            stop: stop.clone(),
            thread: None,
        });
        let thread = std::thread::Builder::new()
            .name("idn-asio-clock".into())
            .spawn(move || {
                ON_CLOCK.with(|c| c.set(id));
                raise_priority();
                // Started again after a panic in the driver's own work (the program's
                // callback can't unwind into here: a panic there ends the host anyway).
                let beat = idn_common::heartbeat::Heartbeat::new();
                let mut runs = 0u32;
                // A block taken back from a full channel (kept across restarts), and the
                // inputs' interleaving room (made here, not in the loop).
                let mut spare: Option<Vec<Vec<f32>>> = None;
                let mut interleaved: Vec<f32> = Vec::with_capacity(
                    frames * input.as_ref().map_or(0, |m| m.channels() as usize),
                );
                idn_common::heartbeat::keep_running(
                    "the IDN ASIO driver's clock",
                    &stop2,
                    &beat,
                    || {
                        runs += 1;
                        let period =
                            Duration::from_nanos(frames as u64 * 1_000_000_000 / rate as u64);
                        let mut pacer = idn_open_protocol::audio::pace::Pacer::new(
                            "asio: the driver's",
                            "desk",
                            frames as u32,
                            rate,
                            reference.clone(),
                        );
                        let (base, base_mono) = (Instant::now(), idn_open_protocol::ptp::mono_ns());
                        // After a restart, a block in place of one the failed run held.
                        if runs > 1 && spare.is_none() {
                            spare =
                                Some((0..outputs).map(|_| Vec::with_capacity(frames)).collect());
                        }
                        // Buffers since this run started (the pacer's count), and the first
                        // one's number: 0 at the start; after a restart, on from the last
                        // position (the host's time never runs back, and the halves keep
                        // alternating).
                        let mut k: u64 = 0;
                        let k0 = if runs == 1 {
                            position.0.store(0, Ordering::Relaxed);
                            0
                        } else {
                            idn_open_protocol::log::log(&format!(
                                "asio: the driver's clock was started again after an error ({}×)",
                                runs - 1
                            ));
                            position.0.load(Ordering::Relaxed) / frames as u64 + 1
                        };
                        while !stop2.load(Ordering::Relaxed) {
                            beat.beat();
                            // In 64 bits: `period * k as u32` wrapped after 2³² buffers (33 days
                            // of 32-frame buffers), and the clock then spun without a buffer.
                            let due_mono = pacer.due(k);
                            let due = base
                                + Duration::from_nanos(
                                    due_mono.saturating_sub(base_mono).max(0) as u64
                                );
                            if let Some(d) = due.checked_duration_since(Instant::now()) {
                                if !idn_open_protocol::audio::pace::sleep_unless_stopped(d, &stop2)
                                {
                                    break;
                                }
                            } else if Instant::now() - due > period * 8 {
                                // Far behind (a stall): on from now rather than rushing.
                                k += ((Instant::now() - due).as_nanos() / period.as_nanos()) as u64;
                                continue;
                            }
                            let half = ((k0 + k) % 2) as usize;
                            let mut g = buffers.lock_ok();
                            // Gone, or made again at another size: this clock is done.
                            let Some(b) = g.as_mut().filter(|b| b.frames == frames) else {
                                break;
                            };
                            // The inputs from the routing matrix.
                            if let Some(m) = &input
                                && m.channels() > 0
                            {
                                let n = m.channels() as usize;
                                interleaved.resize(frames * n, 0.0);
                                m.fill(&mut interleaved);
                                for (c, ch) in b.inputs.iter_mut().enumerate() {
                                    for (i, v) in ch[half].iter_mut().enumerate() {
                                        *v = interleaved[i * n + c.min(n - 1)];
                                    }
                                }
                            }
                            // Let go while the program runs: it may call back into the driver
                            // (`idn_asio_buffer`, creating or disposing buffers, which stop this
                            // clock first, so the buffers can't change meanwhile), and no Rust
                            // reference to them may be live while it writes them.
                            drop(g);
                            let samples = (k0 + k) * frames as u64;
                            let now = base_ns + epoch.elapsed().as_nanos() as u64;
                            position.0.store(samples, Ordering::Relaxed);
                            position.1.store(now, Ordering::Relaxed);
                            // The program reads the inputs and writes the outputs of this half.
                            // SAFETY: `ctx` is the driver object (object.rs), whose last
                            // Release stops this clock before it goes; no Rust reference to the
                            // buffers' memory is live while the program writes them.
                            unsafe { switch(ctx as *mut c_void, half as i32, samples, now) };
                            if outputs > 0 {
                                let g = buffers.lock_ok();
                                let Some(b) = g.as_ref() else { break };
                                // A block handed back by the sender (made beforehand: no
                                // allocation on this thread); none free (the network
                                // stalled): this buffer isn't sent.
                                if let Some(mut channels) =
                                    spare.take().or_else(|| spare_rx.try_recv().ok())
                                {
                                    for (to, h) in channels.iter_mut().zip(&b.outputs) {
                                        to.clear();
                                        to.extend_from_slice(&h[half]);
                                    }
                                    if let Err(e) = tx.try_send((rate, channels)) {
                                        // Full or gone: kept for the next buffer.
                                        let (std::sync::mpsc::TrySendError::Full((_, c))
                                        | std::sync::mpsc::TrySendError::Disconnected((_, c))) = e;
                                        spare = Some(c);
                                    }
                                }
                            }
                            k += 1;
                        }
                    },
                );
                drop(tx);
            });
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                self.clock.lock_ok().take();
                // (Its channel went with the closure: the sender ends, the output back.)
                if let Ok(out) = sender.join()
                    && let Some(n) = self.net.lock_ok().as_mut()
                {
                    n.out = out;
                }
                return Err(e.to_string());
            }
        };
        // Its thread, unless a stop from its first callback took the clock already (then it
        // ends by itself; the sender is joined by the next stop or start).
        if let Some(c) = self.clock.lock_ok().as_mut()
            && Arc::ptr_eq(&c.stop, &stop)
        {
            c.thread = Some(thread);
        }
        *self.sender.lock_ok() = Some(sender);
        Ok(())
    }

    pub fn stop(&self) {
        // From inside a callback (this driver's, or another IDN driver's in the same
        // program): only told to end, never waited for (it ends when its callback returns;
        // its sender is joined by the next stop or start, from another thread). Waiting
        // here, two drivers stopping each other from their callbacks hung both.
        if on_a_clock() {
            if let Some(c) = self.clock.lock_ok().take() {
                c.stop.store(true, Ordering::Relaxed);
            }
            return;
        }
        let _c = self.control.lock_ok();
        self.stop_locked();
    }

    /// `stop`, the control lock held (or not needed).
    fn stop_locked(&self) {
        // Taken out, then joined with no other lock held (the clock's thread calls the
        // program, which may call in here).
        let clock = self.clock.lock_ok().take();
        if let Some(mut c) = clock {
            c.stop.store(true, Ordering::Relaxed);
            if let Some(t) = c.thread.take() {
                // Called from the program's callback, on the clock's own thread: it ends
                // when the callback returns (joining itself would hang or panic). The
                // sender then ends with it, and is joined by the next stop or start.
                if self.on_my_clock() {
                    return;
                }
                let _ = t.join();
            }
        }
        // Not from the clock's own thread, even when the clock was already taken (a second
        // stop from the callback): the sender ends only when the clock's thread does, so
        // joining it there waited for ever (deep bug run 2026-11-09).
        if self.on_my_clock() {
            return;
        }
        // The output back from the sender thread, for the next start.
        let sender = self.sender.lock_ok().take();
        if let Some(s) = sender
            && let Ok(out) = s.join()
            && let Some(n) = self.net.lock_ok().as_mut()
        {
            n.out = out;
        }
    }

    pub fn running(&self) -> bool {
        self.clock.lock_ok().is_some()
    }

    /// The output's name on the network (`HOST (Reaper)`) and its port.
    pub fn output(&self) -> Option<(String, u16)> {
        self.out_addr.lock_ok().clone()
    }

    pub fn error(&self) -> String {
        self.error.lock_ok().clone()
    }

    pub fn set_error(&self, e: String) {
        *self.error.lock_ok() = e;
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The clock thread above the program's own threads (it is the program's sound card).
fn raise_priority() {
    #[cfg(windows)]
    // SAFETY: the current thread's pseudo-handle; no other preconditions.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

// ---------------------------------------------------------------------------------------
// What the C++ shim calls (plain values only). `d` is the pointer `idn_asio_new` gave.

/// The program's name (its executable's), for the defaults.
fn program_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .map(|s| {
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        })
        .unwrap_or_else(|| "ASIO".into())
}

fn put_str(dst: *mut c_char, cap: usize, s: &str) {
    if dst.is_null() || cap == 0 {
        return;
    }
    let b: Vec<u8> = s.bytes().filter(|&c| c != 0).take(cap - 1).collect();
    // SAFETY: `dst` is non-null with `cap` bytes (checked above, the module's contract); at most
    // `cap - 1` bytes and the NUL are written.
    unsafe {
        std::ptr::copy_nonoverlapping(b.as_ptr() as *const c_char, dst, b.len());
        *dst.add(b.len()) = 0;
    }
}

/// The driver, shared: no entry point makes a `&mut` of it (the program calls from its own
/// threads while others run).
unsafe fn dr<'a>(p: *mut c_void) -> &'a Driver {
    // SAFETY: the module's contract: `p` came from `idn_asio_new` and is not yet freed.
    unsafe { &*(p as *const Driver) }
}

/// Runs a step that may fail: 1 on success; else 0, with the reason for
/// `idn_asio_error`. A panic (a bug) is a failure too: unwinding out of these functions
/// would end the program (the DAW) on the spot.
fn guarded(drv: &Driver, f: impl FnOnce(&Driver) -> Result<(), String>) -> i32 {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(drv)));
    match r {
        Ok(Ok(())) => 1,
        Ok(Err(e)) => {
            drv.set_error(e);
            0
        }
        Err(_) => {
            drv.set_error("internal error in the IDN driver".into());
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn idn_asio_new() -> *mut c_void {
    idn_asio_new_instance(DEFAULT_INSTANCE)
}

/// A driver for instance `instance` (see [`Config::load_instance`]).
pub fn idn_asio_new_instance(instance: &str) -> *mut c_void {
    // (A panic kept inside: unwinding out of an extern function aborts the host.)
    std::panic::catch_unwind(|| {
        // The IDN state folder (on Windows %LOCALAPPDATA%\idn-open-protocol, as the mixer's).
        let dir = idn_open_protocol::link::LinkConfig::default_dir().or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|d| std::path::PathBuf::from(d).join("idn-open-protocol"))
        });
        Box::into_raw(Box::new(Driver::new(Config::load_instance(
            dir.as_deref(),
            &program_name(),
            instance,
        )))) as *mut c_void
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_free(p: *mut c_void) {
    if !p.is_null() {
        // SAFETY: `p` came from `Box::into_raw` in `idn_asio_new`; freed once, last.
        let d = unsafe { Box::from_raw(p as *mut Driver) };
        // The last release from inside the program's callback: the network let go on a
        // thread of its own, not on the time-critical one (bug run 2026-11-25).
        if on_a_clock() {
            let d = SendDriver { _d: d };
            let _ = std::thread::Builder::new()
                .name("idn-asio-free".into())
                .spawn(move || drop(d));
        } else {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(d)));
        }
    }
}

/// 1 when ready; else 0, with the reason in `idn_asio_error`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_init(p: *mut c_void) -> i32 {
    // SAFETY: the module's contract (`p` from `idn_asio_new`, not freed).
    guarded(unsafe { dr(p) }, |drv| {
        let node = Node::connect(&format!("idn-asio ({})", drv.config.name))
            .ok()
            .map(Arc::new);
        if node.is_none() {
            idn_open_protocol::log::log(
                "idn-asio: no IDN Node: outputs only, no inputs from the routing matrix",
            );
        }
        drv.init(node, None)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_error(p: *mut c_void, buf: *mut c_char, cap: i32) {
    // SAFETY: as above.
    put_str(buf, cap.max(0) as usize, &unsafe { dr(p) }.error());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_channels(p: *mut c_void, inputs: *mut i32, outputs: *mut i32) {
    // SAFETY: as above; `inputs` and `outputs` are the shim's own variables.
    let c = &unsafe { dr(p) }.config;
    unsafe {
        *inputs = c.inputs as i32;
        *outputs = c.outputs as i32;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_channel_name(
    p: *mut c_void,
    input: i32,
    ch: i32,
    buf: *mut c_char,
    cap: i32,
) -> i32 {
    // SAFETY: as above.
    let c = &unsafe { dr(p) }.config;
    let n = if input != 0 { c.inputs } else { c.outputs } as i32;
    if ch < 0 || ch >= n {
        return 0;
    }
    put_str(
        buf,
        cap.max(0) as usize,
        &format!("IDN {} {}", if input != 0 { "In" } else { "Out" }, ch + 1),
    );
    1
}

/// # Safety
///
/// The three pointers are valid for writes (the shim's own variables).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_buffer_sizes(min: *mut i32, max: *mut i32, preferred: *mut i32) {
    // SAFETY: the shim's own variables, valid for writes.
    unsafe {
        *min = BUFFER.0 as i32;
        *max = BUFFER.1 as i32;
        *preferred = BUFFER.2 as i32;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_latencies(p: *mut c_void, input: *mut i32, output: *mut i32) {
    // SAFETY: as above; `input` and `output` are the caller's own variables.
    let drv = unsafe { dr(p) };
    let (i, o) = drv.latencies();
    unsafe {
        *input = i as i32;
        *output = o as i32;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn idn_asio_can_rate(rate: f64) -> i32 {
    RATES.iter().any(|&r| (r as f64 - rate).abs() < 0.5) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_rate(p: *mut c_void) -> f64 {
    // SAFETY: the module's contract (`p` from `idn_asio_new`, not freed).
    unsafe { dr(p) }.rate() as f64
}

/// 1 on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_set_rate(p: *mut c_void, rate: f64) -> i32 {
    // SAFETY: as above.
    guarded(unsafe { dr(p) }, |drv| drv.set_rate(rate.round() as u32))
}

/// 1 on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_create_buffers(p: *mut c_void, frames: i32) -> i32 {
    // SAFETY: as above.
    guarded(unsafe { dr(p) }, |drv| {
        drv.create_buffers(frames.max(0) as u32)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_buffer(
    p: *mut c_void,
    input: i32,
    ch: i32,
    half: i32,
) -> *mut f32 {
    // SAFETY: as above.
    let drv = unsafe { dr(p) };
    // A channel or half that doesn't exist: none (two entries sharing channel 0's
    // buffers, bug run 2026-11-25).
    if ch < 0 || !(0..=1).contains(&half) {
        return std::ptr::null_mut();
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drv.buffer(input != 0, ch as usize, half as usize)
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// A step with nothing to answer, a panic in it kept from unwinding into the host.
fn quietly(drv: &Driver, f: impl FnOnce(&Driver)) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(drv))).is_err() {
        drv.set_error("internal error in the IDN driver".into());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_dispose_buffers(p: *mut c_void) {
    // SAFETY: as above.
    quietly(unsafe { dr(p) }, |drv| drv.dispose_buffers());
}

/// 1 on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_start(p: *mut c_void, switch: Switch, ctx: *mut c_void) -> i32 {
    // SAFETY: as above; `ctx` is the shim's object, alive until it frees the driver (which stops
    // the clock first).
    guarded(unsafe { dr(p) }, |drv| drv.start(switch, ctx))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_stop(p: *mut c_void) {
    // SAFETY: the module's contract (`p` from `idn_asio_new`, not freed).
    quietly(unsafe { dr(p) }, |drv| drv.stop());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_asio_position(p: *mut c_void, samples: *mut u64, nanos: *mut u64) {
    // SAFETY: as above; `samples` and `nanos` are the shim's own variables.
    let (s, n) = unsafe { dr(p) }.position();
    unsafe {
        *samples = s;
        *nanos = n;
    }
}

/// The program's "ASIO settings" button: the IDN Node's panel (routing, the machine).
#[unsafe(no_mangle)]
pub extern "C" fn idn_asio_control_panel() {
    #[cfg(windows)]
    com::open_node_panel();
    #[cfg(target_os = "linux")]
    linux::open_node_panel();
}

/// The system time hosts read for sample positions, in ns: `timeGetTime` on Windows, the
/// monotonic clock on Linux.
fn system_time_ns() -> u64 {
    #[cfg(windows)]
    {
        // SAFETY: no arguments; it can't fail.
        u64::from(unsafe { windows_sys::Win32::Media::timeGetTime() }) * 1_000_000
    }
    #[cfg(target_os = "linux")]
    {
        linux::monotonic_ns()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}
