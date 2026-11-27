//! The desk itself (spec/extensions/idn-mixer.md §M.2): settings and processing, block by
//! block, with no network in it, so it can be tested and embedded on its own.

use crate::dsp::{DeEss, Dynamics, Filter, KeyFilter, Meter, Shape, SlopeFilter, db};
use idn_plugin_host::protocol::ChainAudio;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

mod dsp;
mod settings_of;
use dsp::*;
pub use settings_of::*;
mod roundtrip;
mod types;
pub use roundtrip::{RoundTrip, roundtrip_give_up_s};
pub use types::*;

mod banks;
mod dynamics;
mod mix;
mod processing;

/// The rate when nothing says otherwise (`node::rate::DEFAULT`).
pub const DEFAULT_RATE: f32 = 48_000.0;

static RATE_HZ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(48_000);

/// The rate the desk runs at (44.1, 48 or 96 kHz; `idn-node.md` §N.40): set once, before
/// the desk is made ([`set_rate`]); the same for the program's life (a change starts the
/// program again).
pub fn rate() -> f32 {
    rate_hz() as f32
}

/// [`rate`] in Hz.
pub fn rate_hz() -> u32 {
    RATE_HZ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets the desk's rate (one of `node::rate::RATES`; others are left as they were).
pub fn set_rate(hz: u32) {
    if idn_open_protocol::node::rate::valid(hz) {
        RATE_HZ.store(hz, std::sync::atomic::Ordering::Relaxed);
    }
}
/// Samples per block when nothing chose one: 128 (2.67 ms). A machine may set its own
/// default ([`default_block`]), a desk its own ([`BLOCKS`], [`Desk::with_block`]).
pub const BLOCK: usize = 128;
/// The block sizes a desk runs: 32 (0.67 ms), 64 (1.33 ms), 128 (2.67
/// ms) or 256 samples (5.33 ms). Powers of two, as sound cards' buffers are: a desk on a
/// card whose buffer is its block has no buffering between them.
pub const BLOCKS: [usize; 4] = [32, 64, 128, 256];

/// This machine's default block: `IDN_DESK_BLOCK` (one of [`BLOCKS`]), else [`BLOCK`].
/// For a desk with none saved (its Setup, `--block` win).
pub fn default_block() -> usize {
    // The tests' desks are the built-in block whatever the machine sets (a lab's 32
    // failed twenty of them; bug run 5, 3-7b); one wanting another says so
    // (`Desk::with_block`).
    if cfg!(test) {
        return BLOCK;
    }
    static D: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *D.get_or_init(|| {
        idn_common::switches::value("IDN_DESK_BLOCK")
            .and_then(|v| v.trim().parse().ok())
            .filter(|b| BLOCKS.contains(b))
            .unwrap_or(BLOCK)
    })
}
/// Blocks of exact silence before a desk rests ([`Desk::process`]): 10 s, past the
/// longest hold (the ducker's 5 s) and the meters' and loudness windows.
pub const REST_AFTER: u32 = (10.0 * DEFAULT_RATE) as u32 / BLOCK as u32;
/// Aux buses (16 from 2026-11-08, was 8: console parity with an X32's).
pub const AUX: usize = 16;
/// DCA groups.
pub const DCA: usize = 16;
/// Group buses (§M.11): stereo, fed from channels after their faders, into the main bus.
pub const GROUPS: usize = 8;
/// Matrix mixes (§M.11): stereo mixes of the main, aux and group buses (8, was 4).
pub const MATRIX: usize = 8;
/// FX buses (§M.11): stereo, fed by channels' FX sends, their plugins (a reverb, a
/// delay) and return into the main bus (8, was 4).
pub const FX: usize = 8;
/// Mute groups (8 from 2026-11-18, was 6; a desk saved with 6 gets 7 and 8 off).
pub const MUTE_GROUPS: usize = 8;
/// User-assignable buttons.
pub const BUTTONS: usize = 8;
/// Bands in every parametric EQ, a channel's and a bus's (§M.15: eight from 2026-11-18;
/// six before, four before that). A band at 0 dB costs nothing.
pub const EQ_BANDS: usize = 8;
/// Duckers (§M.12).
pub const DUCKERS: usize = 4;
/// The monitor pair's first output channel.
pub const MONITOR: usize = 2 + 2 * AUX;
/// Output channels: main L/R, each aux L/R (a mono aux on both), monitor L/R, then each
/// group's L/R, each matrix's L/R, each FX return's L/R and the stream bus's L/R (after
/// the first 20, so routes made to those stay).
pub const OUTPUTS: usize = MONITOR + 2 + 2 * GROUPS + 2 * MATRIX + 2 * FX + 2;
/// The stream bus's first internal channel (a dedicated stereo bus for
/// the livestream, beside main; its own channel sends, EQ, dynamics and limiter): after
/// the FX returns, so every channel before it keeps its place.
pub const STREAM: usize = fx_out(FX);
/// A matrix's sources: the main bus, then each aux bus, each group, each FX return.
pub const MATRIX_SOURCES: usize = 1 + AUX + GROUPS + FX;

/// The bus counts before 2026-11-08 (8 aux, 4 matrix, 4 FX): the outside numbering
/// ([`bus_out`]) keeps their channels where they were.
const AUX_V1: usize = 8;
const MATRIX_V1: usize = 4;
const FX_V1: usize = 4;
/// The outside numbering's channels for the buses added after them: aux 9–16, then matrix
/// 5–8, then FX 5–8, after the 52 there were.
const ADDED_AT: usize = 2 + 2 * AUX_V1 + 2 + 2 * GROUPS + 2 * MATRIX_V1 + 2 * FX_V1;

/// Internal bus channel `i` (the processing's layout: main, aux 1–16, headphones,
/// groups, matrix 1–8, FX 1–8) as the outside numbers it: the output patch's `bus:J`, the
/// output's labels, the switchers' feeds. The first 52 keep the numbers they had with 8
/// aux, 4 matrix and 4 FX (matrix 1 at 37 from 1); the added buses follow (aux 9 at 53,
/// matrix 5 at 69, FX 5 at 77), so routes and patches made before still mean the same.
pub const fn bus_out(i: usize) -> usize {
    let aux_end = 2 + 2 * AUX;
    let groups_at = aux_end + 2;
    let matrix_at = groups_at + 2 * GROUPS;
    let fx_at = matrix_at + 2 * MATRIX;
    if i < 2 {
        i
    } else if i < aux_end {
        let a = i - 2;
        if a < 2 * AUX_V1 {
            2 + a
        } else {
            ADDED_AT + a - 2 * AUX_V1
        }
    } else if i < groups_at {
        2 + 2 * AUX_V1 + (i - aux_end)
    } else if i < matrix_at {
        2 + 2 * AUX_V1 + 2 + (i - groups_at)
    } else if i < fx_at {
        let a = i - matrix_at;
        if a < 2 * MATRIX_V1 {
            2 + 2 * AUX_V1 + 2 + 2 * GROUPS + a
        } else {
            ADDED_AT + 2 * (AUX - AUX_V1) + a - 2 * MATRIX_V1
        }
    } else if i >= fx_at + 2 * FX {
        // The stream bus (added 2026-11-18): the last two outside numbers.
        OUTPUTS - 2 + (i - fx_at - 2 * FX)
    } else {
        let a = i - fx_at;
        if a < 2 * FX_V1 {
            2 + 2 * AUX_V1 + 2 + 2 * GROUPS + 2 * MATRIX_V1 + a
        } else {
            ADDED_AT + 2 * (AUX - AUX_V1) + 2 * (MATRIX - MATRIX_V1) + a - 2 * FX_V1
        }
    }
}

/// [`bus_out`] backwards: the internal channel of outside number `j`.
const BUS_IN: [usize; OUTPUTS] = {
    let mut t = [0usize; OUTPUTS];
    let mut i = 0;
    while i < OUTPUTS {
        t[bus_out(i)] = i;
        i += 1;
    }
    t
};

/// The internal bus channel of the outside number `j` (`bus:J`); `None` past the last.
pub fn bus_in(j: usize) -> Option<usize> {
    BUS_IN.get(j).copied()
}

/// Group `k`'s first output channel (`k` from 0).
pub const fn group_out(k: usize) -> usize {
    MONITOR + 2 + 2 * k
}

/// Matrix `k`'s first output channel (`k` from 0).
pub const fn matrix_out(k: usize) -> usize {
    MONITOR + 2 + 2 * GROUPS + 2 * k
}

/// FX return `k`'s first output channel (`k` from 0): after the matrix mixes.
pub const fn fx_out(k: usize) -> usize {
    MONITOR + 2 + 2 * GROUPS + 2 * MATRIX + 2 * k
}

/// The first output channel of a matrix's source `j` (main, aux 1–8, groups 1–8,
/// FX 1–4).
const fn source_out(j: usize) -> usize {
    if j == 0 {
        0
    } else if j <= AUX {
        2 * j
    } else if j <= AUX + GROUPS {
        group_out(j - 1 - AUX)
    } else {
        fx_out(j - 1 - AUX - GROUPS)
    }
}

/// Where target gain `j` of a channel goes (main L/R, each aux's two, each group's two,
/// each FX bus's two): its bus channel.
const fn gain_target(j: usize) -> usize {
    if j < MONITOR {
        j
    } else if j < MONITOR + 2 * GROUPS {
        j + 2
    } else {
        fx_out(0) + j - MONITOR - 2 * GROUPS
    }
}

/// What the desk sends its insert chains' blocks to: the plugin host, or a stand-in.
pub trait Inserts: Send + Sync {
    /// Sends block `seq` of these chains.
    fn submit(&self, seq: u64, chains: &[ChainAudio]);
    /// Block `seq`'s results, waiting up to `wait` for them.
    fn take(&self, seq: u64, wait: Duration) -> Option<Vec<ChainAudio>>;
}

/// Samples the EQ analyzer looks at (170 ms at 48 kHz).
pub const SCOPE: usize = 8192;

/// The chain number of the main bus's inserts; aux bus `k` (from 0) is one more than this
/// plus `k`.
pub const MAIN_CHAIN: u32 = 1000;
/// Group `k`'s chain is this plus `k`.
pub const GROUP_CHAIN: u32 = 1100;
/// FX bus `k`'s chain is this plus `k` (in the groups' stage: both return into main).
pub const FX_CHAIN: u32 = 1150;
/// Matrix `k`'s chain is this plus `k`.
pub const MATRIX_CHAIN: u32 = 1200;
/// The headphone bus's chain.
pub const MONITOR_CHAIN: u32 = 1300;
/// The stream bus's chain.
pub const STREAM_CHAIN: u32 = 1350;

/// The bus strips, in the order of [`Meters::bus_late`]: the main bus, the aux buses, the
/// groups, the matrix mixes, the FX returns, the stream bus, the headphones.
pub const BUS_STRIPS: usize = 1 + AUX + GROUPS + MATRIX + FX + 1 + 1;
/// The stream bus's strip number.
pub const STREAM_STRIP: usize = 1 + AUX + GROUPS + MATRIX + FX;

/// Bus strip `k`'s chain and first internal bus channel.
pub const fn bus_strip(k: usize) -> (u32, usize) {
    if k <= AUX {
        (MAIN_CHAIN + k as u32, 2 * k)
    } else if k <= AUX + GROUPS {
        let g = k - 1 - AUX;
        (GROUP_CHAIN + g as u32, group_out(g))
    } else if k <= AUX + GROUPS + MATRIX {
        let m = k - 1 - AUX - GROUPS;
        (MATRIX_CHAIN + m as u32, matrix_out(m))
    } else if k <= AUX + GROUPS + MATRIX + FX {
        let f = k - 1 - AUX - GROUPS - MATRIX;
        (FX_CHAIN + f as u32, fx_out(f))
    } else if k == STREAM_STRIP {
        (STREAM_CHAIN, STREAM)
    } else {
        (MONITOR_CHAIN, MONITOR)
    }
}

/// How long the desk waits for the last block's plugin results before passing it dry: at
/// most 0.5 ms, and a quarter of its block (a fixed 0.5 ms was three quarters of a
/// 32-sample block).
fn plugin_wait(block: usize) -> Duration {
    Duration::from_micros((block as u64 * 1_000_000 / u64::from(rate_hz()) / 4).min(500))
}

/// A channel coming on fades its input in over this many samples (10 ms).
fn channel_fade() -> u32 {
    rate_hz() / 100
}

/// A channel's strip as it starts: filters set from its settings, everything else
/// silent.
fn fresh_strip(c: &Channel) -> Strip {
    let filter = |shape, f: f32, slope: u8| {
        let mut x = SlopeFilter::default();
        x.tune(shape, rate(), f, slope);
        x
    };
    Strip {
        hpf: filter(Shape::HighPass, c.hpf, c.hpf_slope),
        lpf: filter(Shape::LowPass, c.lpf, c.lpf_slope),
        eq: eq_filters(&effective(&c.eq, c.eq_on)),
        eq_now: effective(&c.eq, c.eq_on),
        gate: Dynamics::default(),
        comp: Dynamics::default(),
        gate_key: KeyFilter::default(),
        comp_key: KeyFilter::default(),
        lim: Dynamics::default(),
        deess: DeEss::default(),
        delay: Delay::default(),
        denoise: None,
        nr_dry: Delay::default(),
        nr_on: c.denoise,
        nr_gain: 1.0,
        nr_wait: 0,
        gains: vec![0.0; 2 + 2 * AUX + 2 * GROUPS + 2 * FX + 2],
        fade_in: 0,
        trim_now: None,
        dyn_mix: [
            if c.gate.on { 1.0 } else { 0.0 },
            if c.comp.on { 1.0 } else { 0.0 },
            if c.limiter.on { 1.0 } else { 0.0 },
        ],
        post_gain: None,
    }
}

/// Channels rest in banks of this many (a linked pair never straddles two).
pub const BANK: usize = 4;
/// A muted bank rests after this many blocks muted (0.5 s: not on a quick toggle).
#[cfg_attr(not(test), allow(dead_code))] // (the default block's: the tests use it)
const MUTE_REST: u32 = (0.5 * DEFAULT_RATE) as u32 / BLOCK as u32;

/// The processing state of a channel.
#[derive(Clone)]
struct Strip {
    hpf: SlopeFilter,
    lpf: SlopeFilter,
    eq: [Filter; EQ_BANDS],
    /// The bands as the filters play them now, gliding to the settings (see [`glide`]).
    eq_now: [Band; EQ_BANDS],
    gate: Dynamics,
    comp: Dynamics,
    /// The gate's and the compressor's key filters (§M.28).
    gate_key: KeyFilter,
    comp_key: KeyFilter,
    lim: Dynamics,
    deess: DeEss,
    /// The input delay.
    delay: Delay,
    /// Its noise suppression, while `denoise` is on.
    denoise: Option<idn_dsp::denoise::RateDenoiser>,
    /// The sound as it came, delayed as the suppression delays it (`denoise_amount`).
    nr_dry: Delay,
    /// The suppression in the channel's path now. It follows the setting through a short
    /// fade (`nr_gain`, 1 when not switching): out, the switch, and in again, after
    /// `nr_wait` samples of silence when switched on (what the suppressor still holds
    /// from before). Switched at once, the channel jumped 20 ms in its sound and
    /// clicked, 18 times of 20 (lab 498-A).
    nr_on: bool,
    nr_gain: f32,
    nr_wait: usize,
    /// Gains last block (ramped from to the new ones): main L, main R, each aux's two,
    /// each group's two.
    gains: Vec<f32>,
    /// Samples left of its fade-in after it starts (a routed input, a channel put on the
    /// desk): the lane fades a new route in, but the channel can come on up to a routing
    /// check later, at full level.
    fade_in: u32,
    /// The trim (with its polarity) as it plays now, ramped to the settings (a step was a
    /// click; bug round F, F7-7). `None` until the first block.
    trim_now: Option<f32>,
    /// The gate's, compressor's and limiter's share of the signal (F7-7): 1 on, 0 off,
    /// faded between.
    dyn_mix: [f32; 3],
    /// Its post-fader gain as it plays now (F7-7). `None` until the first block.
    post_gain: Option<f32>,
}

/// The processing state of a bus (two sides).
#[derive(Clone)]
struct BusState {
    eq: [[Filter; EQ_BANDS]; 2],
    /// The bands as the filters play them now, gliding to the settings (see [`glide`]).
    eq_now: [Band; EQ_BANDS],
    comp: Dynamics,
    lim: Dynamics,
    gain: f32,
    /// The main's dim gain and mono amount (0 stereo, 1 mono) as they play now, ramped to
    /// the settings (a 20 dB step on air clicked; bug round F, F7-7).
    dim: f32,
    mono: f32,
    /// A matrix mix's sources' gains as they play now (F7-7).
    src_gain: [f32; MATRIX_SOURCES],
    /// The compressor's and limiter's share (F7-7).
    dyn_mix: [f32; 2],
    /// Its built-in effect and graphic EQ (§M.14).
    fx: crate::fx::FxState,
    geq: crate::fx::GeqState,
}

impl BusState {
    fn new(b: &Bus, gain: f32) -> BusState {
        let now = effective(&b.eq, b.eq_on);
        BusState {
            eq: [eq_filters(&now), eq_filters(&now)],
            eq_now: now,
            comp: Dynamics::default(),
            lim: Dynamics::default(),
            gain,
            dim: if b.dim { db(b.dim_db) } else { 1.0 },
            mono: if b.mono { 1.0 } else { 0.0 },
            src_gain: [0.0; MATRIX_SOURCES],
            dyn_mix: [
                if b.comp.on { 1.0 } else { 0.0 },
                if b.limiter.on { 1.0 } else { 0.0 },
            ],
            fx: crate::fx::FxState::default(),
            geq: crate::fx::GeqState::default(),
        }
    }
}

/// Chains' latencies as the desk takes them: at most a second (a plugin saying -1 as
/// 4 294 967 295 samples had the clock thread ask for a 16 GiB delay line; bug run
/// 2026-11-24).
fn bounded_latency(mut chains: BTreeMap<u32, u32>) -> BTreeMap<u32, u32> {
    let most = rate_hz();
    for l in chains.values_mut() {
        *l = (*l).min(most);
    }
    chains
}

/// A delay line of a settable length (latency compensation, the input delay). A new
/// length crossfades from the old over [`Delay::FADE`] samples: taken at once it left a
/// gap of silence (longer) or skipped sound (shorter), a click on every bus, e.g. every
/// channel's line-up when noise suppression went on anywhere (bug round F, F7-1).
#[derive(Clone, Default)]
struct Delay {
    /// A ring of the last samples, `buf.len()` a power of two (or empty).
    buf: Vec<f32>,
    pos: usize,
    /// The length now, and the one being faded from (with the samples of fade left).
    len: usize,
    from: usize,
    fade: usize,
}

impl Delay {
    /// 2 ms at the desk's rate (bug run J2-9: 96 samples at every rate).
    fn fade_len() -> usize {
        (rate_hz() / 500) as usize
    }

    fn run(&mut self, x: &mut [f32], len: usize) {
        if self.buf.is_empty() {
            // Its first block: nothing before it to fade from (a set delay from the start).
            (self.len, self.from, self.fade) = (len, len, 0);
        } else if len != self.len {
            // (Mid-fade: from the length being faded to.)
            self.from = self.len;
            self.len = len;
            self.fade = Self::fade_len();
        }
        // (Run at 0 too, so the samples a longer delay needs are there when it comes.)
        let need = self.len.max(self.from) + 1;
        if self.buf.len() < need {
            self.grow(need);
        }
        let mask = self.buf.len() - 1;
        for v in x {
            self.buf[self.pos] = *v;
            let to = self.buf[(self.pos + self.buf.len() - self.len) & mask];
            *v = if self.fade > 0 {
                let from = self.buf[(self.pos + self.buf.len() - self.from) & mask];
                let n = Self::fade_len().max(self.fade).max(1);
                let t = (n - self.fade) as f32 / n as f32;
                self.fade -= 1;
                from + (to - from) * t
            } else {
                to
            };
            self.pos = (self.pos + 1) & mask;
        }
    }

    /// A longer ring, the samples so far kept in order (only when a delay grows).
    fn grow(&mut self, need: usize) {
        // At least the suppression's line-up at the desk's rate from the first block, so
        // turning it on doesn't grow every channel's ring on the clock thread (bug run
        // K6-7: 960 fit 1024 at 48 kHz, not 1370 at 44.1 or 2400 at 96).
        let least = (idn_dsp::denoise::latency(rate_hz()) + 1)
            .next_power_of_two()
            .max(1024);
        let cap = need.next_power_of_two().max(least);
        let mut buf = vec![0.0f32; cap];
        let old = self.buf.len();
        // The oldest sample first: at `pos` (the next to be written over).
        for k in 0..old {
            buf[cap - old + k] = self.buf[(self.pos + k) % old];
        }
        self.buf = buf;
        self.pos = 0;
    }
}

/// A stage of bus insert chains (§M.11): each block, a bus's signal goes to its chain and
/// comes back from the last block's. While any bus of the stage has plugins, every bus of
/// it comes out one block (and the longest chain's latency) later, those without plugins
/// (or whose result was late) delayed to line up.
#[derive(Default)]
struct Stage {
    /// Per bus channel: the last block's signal, its dry delay, its line-up delay.
    prev: Vec<Vec<f32>>,
    dry: Vec<Delay>,
    delay: Vec<Delay>,
}

/// Delays a set of channels by a stage's latency, to stay in line with it.
#[derive(Default)]
struct Lag(Vec<Delay>);

impl Lag {
    fn run(&mut self, chans: &mut [Vec<f32>], len: usize) {
        self.0.resize(chans.len(), Delay::default());
        for (d, c) in self.0.iter_mut().zip(chans) {
            d.run(c, len);
        }
    }
}

/// The desk: settings, processing state, meters.
pub struct Desk {
    /// Samples a block (one of [`BLOCKS`]), and in blocks: rest after 10 s of silence,
    /// a muted channel after 0.5 s ([`REST_AFTER`], [`MUTE_REST`] at the default block).
    block: usize,
    rest_after: u32,
    mute_rest: u32,
    /// Behind an Arc: readers take it in an instant and clone or serialise it with the
    /// desk's lock let go, which the clock thread takes every 2 ms.
    settings: Arc<Settings>,
    /// Blocks in a row with every input and output exactly silent, no plugins and nothing
    /// fading, on the settings at `quiet_settings` (their address): past [`REST_AFTER`],
    /// the desk rests (outputs silence without running its chains).
    quiet_blocks: u32,
    /// A soloed channel was mixed this block (the headphones then carry the solo).
    solo_heard: bool,
    quiet_settings: usize,
    /// Desk inputs (the input patch's choices) and outputs (the output patch's).
    inputs: usize,
    outputs: usize,
    /// The desk inputs something is routed to (§M.11): a channel whose input has nothing
    /// is parked (not processed).
    routed: Vec<bool>,
    /// The desk inputs sound arrives on now (a backup input's failover, §M.11).
    live: Vec<bool>,
    /// Each channel's input B in use (last block).
    on_b: Vec<bool>,
    /// The channel whose sound the EQ analyzer shows, and its last samples (a ring).
    analyze: Option<usize>,
    scope: Vec<f32>,
    scope_at: usize,
    strips: Vec<Strip>,
    main: BusState,
    aux: Vec<BusState>,
    groups: Vec<BusState>,
    matrix: Vec<BusState>,
    fx: Vec<BusState>,
    monitor: BusState,
    stream: BusState,
    /// The main bus's loudness (EBU R128, §M.11), and the stream bus's.
    loudness: crate::dsp::Loudness,
    stream_loudness: crate::dsp::Loudness,
    /// Talkback's gain last block.
    talk: f32,
    /// Blocks a gain change is ramped over now (1: within the block; more after a snapshot).
    ramp: usize,
    /// The next slow change's ramp, seconds (a snapshot's `fade_s`; 0.2 s when none).
    fade: Option<f32>,
    meters: Meters,
    // Insert chains (§M.6): where blocks go, the chains built there (by number, with
    // their latency in samples), and the pipeline's state.
    plugins: Option<Arc<dyn Inserts>>,
    chains: BTreeMap<u32, u32>,
    seq: u64,
    /// Which channels run this block: kept between blocks (no allocation per block).
    active_buf: Vec<bool>,
    /// Banks of four channels at rest (idn-mixer.md §M.24): per channel,
    /// blocks its input and output have been exactly silent, blocks it has been muted,
    /// whether its bank rests now and whether it rested muted (its strip starts afresh
    /// when it wakes); which channels the output patch taps (never at rest); and which
    /// run this block.
    ch_silent: Vec<u32>,
    ch_muted: Vec<u32>,
    resting: Vec<bool>,
    rested_muted: Vec<bool>,
    /// Per channel: its strip holds sound from before its rest (it began muted, with
    /// sound): it wakes afresh, faded in.
    rest_stale: Vec<bool>,
    tapped: (u64, Vec<bool>),
    run_buf: Vec<bool>,
    /// A round-trip test (§M.28.2), and whether its input was quiet a block.
    round_trip: RoundTrip,
    round_trip_quiet: bool,
    /// The channels whose inserts come first this block, and the rest (§M.28).
    pre_ins_buf: Vec<bool>,
    post_ins_buf: Vec<bool>,
    /// The chains' blocks to the plugins: their buffers kept between blocks.
    outgoing: Vec<ChainAudio>,
    /// Counts settings changes (the parsed output patch is for one of them).
    settings_gen: u64,
    /// The output patch as parsed, for the settings of that count.
    patch: (u64, Vec<Tap>),
    /// Whether the last block was sent, and the chains it held.
    sent: bool,
    expect: Vec<u32>,
    /// Each channel's signal at the insert point, this block and the last (the last is
    /// what goes on dry), and its latency compensation.
    at: Vec<Vec<f32>>,
    prev: Vec<Vec<f32>>,
    delay: Vec<Delay>,
    /// The dry signal delayed by its chain's latency, so that a late block stays in line.
    dry: Vec<Delay>,
    /// The bus stages: groups; main and aux; matrix mixes and headphones.
    group_stage: Stage,
    bus_stage: Stage,
    out_stage: Stage,
    /// Line-up delays: the channels' main and aux sums while the groups' stage runs; the
    /// groups while the main/aux stage runs; the rest while the last stage runs.
    lag_sums: Lag,
    lag_groups: Lag,
    lag_rest: Lag,
    lag_rest_groups: Lag,
    lag_fx: Lag,
    lag_rest_fx: Lag,
    /// The stream bus alike: while the groups' stage runs, and while the last stage runs.
    lag_stream_sums: Lag,
    lag_rest_stream: Lag,
    // Work buffers, kept to avoid allocating per block.
    post: Vec<f32>,
    /// Each channel after the inserts (before its fader), and after its fader (direct out).
    pre: Vec<Vec<f32>>,
    direct: Vec<Vec<f32>>,
    /// Each channel's tap points for direct outs (§M.11): its input as it came, before its
    /// EQ (after trim, delay, filters, gate), after its EQ (before the compressor).
    tap_in: Vec<Vec<f32>>,
    tap_pre_eq: Vec<Vec<f32>>,
    tap_post_eq: Vec<Vec<f32>>,
    /// The channels another's gate or compressor is keyed from (§M.28): their sound
    /// before processing, this block (only theirs is copied).
    keys: Vec<Vec<f32>>,
    /// A block's room for the dry sound of a channel's noise suppression.
    nr_scratch: Vec<f32>,
    /// The channels in the IEM flow (§M.26), in order, for the settings now (made with
    /// them, off the clock thread).
    iem: Arc<Vec<usize>>,
    /// Among them, the talkback microphone's (`talkback.to_iem`, §M.28).
    iem_talkback: Option<usize>,
    /// Soloed channels, for the headphones.
    solo: Vec<Vec<f32>>,
    buses: Vec<Vec<f32>>,
    /// The automix, duckers and audio-follows-video (§M.12).
    auto: AutoState,
    /// Each desk output's delay line (§M.15).
    out_delay: Vec<Delay>,
    /// What the switcher has on air (its inputs' labels), when it is heard from.
    on_air: Option<std::collections::BTreeSet<String>>,
}

/// The automix's, duckers' and audio-follows-video's running state (§M.12).
#[derive(Default)]
struct AutoState {
    /// Each channel's level, power, smoothed: for the automix (30 ms) and the keys (10 ms).
    share_pow: Vec<f32>,
    key_pow: Vec<f32>,
    /// Each ducker's gain now, dB, and how long it still holds, s.
    duck_db: Vec<f32>,
    duck_hold: Vec<f32>,
    /// Each channel's audio-follows-video gain now (0–1).
    afv: Vec<f32>,
    /// Each channel's gain from all three this block, dB.
    gain_db: Vec<f32>,
    /// The block's working gains (kept, so none is allocated per block).
    gain_scratch: Vec<f32>,
}

impl AutoState {
    fn new(n: usize) -> AutoState {
        AutoState {
            share_pow: vec![0.0; n],
            key_pow: vec![0.0; n],
            duck_db: vec![0.0; DUCKERS],
            duck_hold: vec![0.0; DUCKERS],
            afv: vec![1.0; n],
            gain_db: vec![0.0; n],
            gain_scratch: Vec::new(),
        }
    }
}

impl Desk {
    /// A desk of `n` input channels with default settings, `n` desk inputs and the buses
    /// on its outputs in order.
    pub fn new(n: usize) -> Desk {
        Desk::with_io(n, n, OUTPUTS)
    }

    /// Samples a block.
    pub fn block(&self) -> usize {
        self.block
    }

    /// A desk of `n` channels, `inputs` desk inputs (the input patch chooses among them)
    /// and `outputs` desk outputs (the output patch fills them).
    pub fn with_io(n: usize, inputs: usize, outputs: usize) -> Desk {
        Desk::with_block(n, inputs, outputs, default_block())
    }

    /// [`Desk::with_io`] with `block` samples a block (one of [`BLOCKS`]; another is taken
    /// as the default).
    pub fn with_block(n: usize, inputs: usize, outputs: usize, block: usize) -> Desk {
        let block = if BLOCKS.contains(&block) {
            block
        } else {
            default_block()
        };
        let settings = Settings::new(n).normalised(n, outputs);
        let mut d = Desk {
            block,
            rest_after: (10.0 * rate()) as u32 / block as u32,
            mute_rest: (0.5 * rate()) as u32 / block as u32,
            inputs,
            outputs,
            routed: vec![true; inputs],
            live: vec![true; inputs],
            on_b: Vec::new(),
            analyze: None,
            scope: vec![0.0; SCOPE],
            scope_at: 0,
            strips: Vec::new(),
            main: BusState::new(&settings.main, 1.0),
            aux: Vec::new(),
            groups: Vec::new(),
            matrix: Vec::new(),
            fx: Vec::new(),
            monitor: BusState::new(&settings.monitor, 1.0),
            stream: BusState::new(&settings.stream, 1.0),
            loudness: crate::dsp::Loudness::default(),
            stream_loudness: crate::dsp::Loudness::default(),
            talk: 0.0,
            ramp: 1,
            fade: None,
            quiet_blocks: 0,
            solo_heard: false,
            quiet_settings: 0,
            meters: Meters::default(),
            plugins: None,
            chains: BTreeMap::new(),
            seq: 0,
            active_buf: Vec::new(),
            ch_silent: Vec::new(),
            ch_muted: Vec::new(),
            resting: Vec::new(),
            rested_muted: Vec::new(),
            rest_stale: Vec::new(),
            tapped: (0, Vec::new()),
            run_buf: Vec::new(),
            pre_ins_buf: Vec::new(),
            round_trip: RoundTrip::Idle,
            round_trip_quiet: false,
            post_ins_buf: Vec::new(),
            outgoing: Vec::new(),
            settings_gen: 1,
            patch: (0, Vec::new()),
            sent: false,
            expect: Vec::new(),
            at: Vec::new(),
            prev: Vec::new(),
            delay: Vec::new(),
            dry: Vec::new(),
            group_stage: Stage::default(),
            bus_stage: Stage::default(),
            out_stage: Stage::default(),
            lag_sums: Lag::default(),
            lag_groups: Lag::default(),
            lag_rest: Lag::default(),
            lag_rest_groups: Lag::default(),
            lag_fx: Lag::default(),
            lag_rest_fx: Lag::default(),
            lag_stream_sums: Lag::default(),
            lag_rest_stream: Lag::default(),
            post: vec![0.0; block],
            pre: Vec::new(),
            direct: Vec::new(),
            tap_in: Vec::new(),
            keys: Vec::new(),
            nr_scratch: Vec::new(),
            tap_pre_eq: Vec::new(),
            tap_post_eq: Vec::new(),
            iem: Arc::default(),
            iem_talkback: None,
            solo: vec![vec![0.0; block]; 2],
            buses: vec![vec![0.0; block]; OUTPUTS],
            auto: AutoState::new(n),
            out_delay: vec![Delay::default(); outputs],
            on_air: None,
            settings: Arc::new(settings),
        };
        d.rebuild();
        d
    }

    fn rebuild(&mut self) {
        let s = &self.settings;
        let filter = |shape, f: f32| {
            if f > 0.0 {
                Filter::new(shape, rate(), f, 0.0, 0.707)
            } else {
                Filter::Flat
            }
        };
        let _ = filter;
        self.strips = s.channels.iter().map(fresh_strip).collect();
        let n0 = s.channels.len();
        self.ch_silent = vec![0; n0];
        self.ch_muted = vec![0; n0];
        self.resting = vec![false; n0];
        self.rested_muted = vec![false; n0];
        self.rest_stale = vec![false; n0];
        self.aux = s.aux.iter().map(|b| BusState::new(b, 0.0)).collect();
        self.groups = s.groups.iter().map(|b| BusState::new(b, 0.0)).collect();
        self.matrix = s.matrix.iter().map(|b| BusState::new(b, 0.0)).collect();
        self.fx = s.fx.iter().map(|b| BusState::new(b, 0.0)).collect();
        let n = s.channels.len();
        let block = self.block;
        self.pre = vec![vec![0.0; block]; n];
        self.on_b = vec![false; n];
        self.direct = vec![vec![0.0; block]; n];
        self.tap_in = vec![vec![0.0; block]; n];
        self.keys = vec![vec![0.0; block]; n];
        self.nr_scratch = vec![0.0; block];
        self.tap_pre_eq = vec![vec![0.0; block]; n];
        self.tap_post_eq = vec![vec![0.0; block]; n];
        self.at = vec![vec![0.0; block]; n];
        self.prev = vec![vec![0.0; block]; n];
        self.delay = vec![Delay::default(); n];
        self.dry = vec![Delay::default(); n];
        self.meters = Meters {
            channel_in: vec![Meter::default(); n],
            channel_out: vec![Meter::default(); n],
            channel_gr: vec![(0.0, 0.0, 0.0, 0.0); n],
            outputs: vec![Meter::default(); OUTPUTS],
            channel_late: vec![0; n],
            bus_late: vec![0; BUS_STRIPS],
            bus_gr: vec![(0.0, 0.0); BUS_STRIPS],
            lufs_m: -144.0,
            lufs_s: -144.0,
            lufs_i: -144.0,
            stream_lufs: (-144.0, -144.0, -144.0),
            channel_auto: vec![0.0; n],
        };
        self.auto = AutoState::new(n);
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Starts the main mix's integrated loudness again (§M.15).
    pub fn reset_loudness(&mut self) {
        self.loudness.reset_integrated();
        self.meters.lufs_i = -144.0;
        self.stream_loudness.reset_integrated();
        self.meters.stream_lufs.2 = -144.0;
    }

    /// What the switcher has on air (its inputs' labels), for audio follows video
    /// (§M.12); `None` while it isn't heard from: following channels then stay up.
    pub fn set_on_air(&mut self, on_air: Option<std::collections::BTreeSet<String>>) {
        self.on_air = on_air;
    }

    /// Desk inputs and outputs.
    pub fn io(&self) -> (usize, usize) {
        (self.inputs, self.outputs)
    }

    /// Which desk inputs sound arrives on (all, until told): a backup input takes over
    /// from a channel's first one that has none.
    pub fn set_live(&mut self, live: Vec<bool>) {
        self.live = live;
        self.live.resize(self.inputs, false);
    }

    /// Whether channel `i` takes its input B now.
    pub fn uses_b(&self, i: usize) -> bool {
        self.on_b.get(i).copied().unwrap_or(false)
    }

    /// The channel (from 0) the EQ analyzer listens to, or none. Returns whether that
    /// changed (the scope starts empty).
    pub fn set_analyze(&mut self, ch: Option<usize>) -> bool {
        let changed = self.analyze != ch;
        if changed {
            self.scope.fill(0.0);
        }
        self.analyze = ch;
        changed
    }

    /// The analyzer channel's last [`SCOPE`] samples, oldest first.
    pub fn scope(&self) -> Vec<f32> {
        let mut v = self.scope[self.scope_at..].to_vec();
        v.extend_from_slice(&self.scope[..self.scope_at]);
        v
    }

    /// Which desk inputs something is routed to (all, until told).
    pub fn set_routed(&mut self, routed: Vec<bool>) {
        self.routed = routed;
        self.routed.resize(self.inputs, false);
    }

    /// Whether channel `i` (from 0) is in use: its input is routed (or, for a linked pair,
    /// either side's) and it isn't hidden. A parked channel isn't processed and sends
    /// nothing anywhere; its settings stay.
    pub fn active(&self, i: usize) -> bool {
        let s = &self.settings;
        let routed = |c: usize| {
            s.channels
                .get(c)
                .map(|ch| ch.source.unwrap_or(c + 1))
                .and_then(|src| self.routed.get(src.wrapping_sub(1)))
                .copied()
                .unwrap_or(false)
        };
        let l = lead(s, i);
        if s.channels[l].shown == Some(false) {
            return false;
        }
        // Its backup input counts too.
        let routed_b = |c: usize| {
            s.channels
                .get(c)
                .map(|ch| ch.input_b)
                .filter(|&b| b > 0)
                .and_then(|b| self.routed.get(b - 1))
                .copied()
                .unwrap_or(false)
        };
        let pair = s.channels[l].link && l + 1 < s.channels.len();
        routed(l) || routed_b(l) || pair && (routed(l + 1) || routed_b(l + 1))
    }

    /// New settings: filters retuned in place, EQ bands gliding to theirs (no click);
    /// gains ramp within a block, or over 0.2 s with `slow` (a snapshot recall).
    /// The next slow change (a snapshot's recall) ramped over `secs` (§M.28).
    pub fn next_fade(&mut self, secs: f32) {
        self.fade = Some(secs.clamp(0.0, 10.0));
    }

    pub fn set(&mut self, s: Settings, slow: bool) {
        let (n, outputs) = self.shape();
        drop(self.set_normalised(s.normalised(n, outputs), slow));
    }

    /// The settings' shape, for [`Settings::normalised`] done before [`Desk::set_normalised`].
    pub fn shape(&self) -> (usize, usize) {
        (self.settings.channels.len(), self.outputs)
    }

    /// The settings as they are now, shared: cloned or serialised by the caller with the
    /// desk let go.
    pub fn settings_arc(&self) -> Arc<Settings> {
        self.settings.clone()
    }

    /// As [`Desk::set`], for settings normalised already to [`Desk::shape`] (the work of it
    /// done by the caller, with the desk let go). The settings before, to be dropped there
    /// too.
    pub fn set_normalised(&mut self, s: Settings, slow: bool) -> Arc<Settings> {
        self.set_normalised_with(s, slow, Vec::new())
    }

    /// The channels whose noise suppression `s` turns on and that have none made: made by
    /// the caller with the desk let go ([`Desk::set_normalised_with`]).
    pub fn denoisers_needed(&self, s: &Settings) -> Vec<usize> {
        (0..self.strips.len().min(s.channels.len()))
            .filter(|&i| s.channels[lead(s, i)].denoise && self.strips[i].denoise.is_none())
            .collect()
    }

    /// [`Desk::set_normalised`] with suppressors made already for some channels (bug run
    /// K6-4: made under the desk's lock, a snapshot turning it on for 16 channels at 96 kHz
    /// held the clock thread 10–20 ms).
    pub fn set_normalised_with(
        &mut self,
        s: Settings,
        slow: bool,
        made: Vec<(usize, idn_dsp::denoise::RateDenoiser)>,
    ) -> Arc<Settings> {
        for (i, d) in made {
            if let Some(st) = self.strips.get_mut(i)
                && st.denoise.is_none()
            {
                st.denoise = Some(d);
            }
        }
        // A linked pair's filters are its first channel's, as the rest of its strip (each
        // side took its own: an 80 Hz cut set on the pair filtered one side; bug round F,
        // F7-3).
        for (i, st) in self.strips.iter_mut().enumerate().take(s.channels.len()) {
            let c = &s.channels[lead(&s, i)];
            // Its noise suppression made here, not on the clock thread (bug run J2-4: at
            // 44.1 and 96 kHz it makes two resamplers); mostly made already (K6-4).
            if c.denoise && st.denoise.is_none() {
                st.denoise = Some(idn_dsp::denoise::RateDenoiser::new(rate_hz()));
            }
            st.hpf.tune(Shape::HighPass, rate(), c.hpf, c.hpf_slope);
            st.lpf.tune(Shape::LowPass, rate(), c.lpf, c.lpf_slope);
        }
        // (EQ changes glide there in the blocks that follow: see `glide`.)
        if slow {
            // 0.2 s at any block size (100 blocks was 67 ms at 32 samples, 0.53 s at 256),
            // or a recall's own fade (§M.28).
            let secs = f64::from(self.fade.take().unwrap_or(0.2));
            self.ramp = ((secs * f64::from(rate_hz())) as usize / self.block).max(1);
        } else {
            // Any change over about 2 ms at the least: 3 blocks at 32, 2 at 64, 1 from 128.
            self.ramp = self.ramp.max(dsp::ramp_samples().div_ceil(self.block));
        }
        // Effects of a new kind made here, not on the clock thread (F7-8).
        for (st, f) in self.fx.iter_mut().zip(&s.fx) {
            st.fx.prepare(&f.effect, rate());
        }
        self.settings_gen += 1;
        // The IEM flow's channels (§M.26), each linked pair's two sides.
        let iem = crate::iem::marked(&s);
        if *self.iem != iem {
            self.iem = Arc::new(iem);
        }
        self.iem_talkback = crate::iem::talkback_lane(&s);
        // The output patch read here, with the desk's lock (not on the clock thread, where
        // it was parsed into a new list after every change; bug round F, F7-8).
        self.patch = (
            self.settings_gen,
            (0..self.outputs)
                .map(|o| Tap::parse(s.outputs.get(o).map_or("", String::as_str)))
                .collect(),
        );
        std::mem::replace(&mut self.settings, Arc::new(s))
    }

    /// Where insert chains' blocks go, and the chains built there now (chain number →
    /// its latency in samples). Strips without a chain pass on unchanged, delayed to line
    /// up with the others.
    pub fn set_plugins(&mut self, plugins: Option<Arc<dyn Inserts>>, chains: BTreeMap<u32, u32>) {
        self.plugins = plugins;
        self.chains = if self.plugins.is_some() {
            bounded_latency(chains)
        } else {
            BTreeMap::new()
        };
    }

    /// The chains' latency changed (a parameter); where blocks go stays.
    pub fn set_chains(&mut self, chains: BTreeMap<u32, u32>) {
        if self.plugins.is_some() {
            self.chains = bounded_latency(chains);
        }
    }

    /// A stage's latency now, in samples: a block and its longest chain, while it has any.
    fn stage_latency(&self, r: std::ops::Range<u32>) -> usize {
        let l: Vec<u32> = self.chains.range(r).map(|(_, &l)| l).collect();
        if l.is_empty() {
            0
        } else {
            self.block + l.into_iter().max().unwrap_or(0) as usize
        }
    }

    /// The latency the insert chains add now, in samples (for each stage that has any
    /// plugins, a block and its longest chain: channels, groups, main and aux, matrix and
    /// headphones).
    /// A round-trip test is waiting to send or listening.
    pub fn round_trip_active(&self) -> bool {
        matches!(
            self.round_trip,
            RoundTrip::Asked { .. } | RoundTrip::Sent { .. }
        )
    }

    pub fn plugin_latency(&self) -> u32 {
        (self.stage_latency(0..MAIN_CHAIN)
            + self.stage_latency(GROUP_CHAIN..MATRIX_CHAIN)
            + self.stage_latency(MAIN_CHAIN..GROUP_CHAIN)
            + self.stage_latency(MATRIX_CHAIN..u32::MAX)) as u32
    }

    pub fn meters(&self) -> &Meters {
        &self.meters
    }

    /// The channels in the IEM flow (§M.26), from 0, in order.
    pub fn iem_channels(&self) -> Arc<Vec<usize>> {
        self.iem.clone()
    }

    /// How many channels the IEM flow has now.
    pub fn iem_len(&self) -> usize {
        self.iem.len()
    }

    /// The last block's IEM channels (§M.26): each after its processing and inserts,
    /// before its fader, mute and automix (the talkback's tap), into `out` (one buffer a
    /// channel, kept between calls). Which channels they are.
    pub fn iem_tap(&self, out: &mut Vec<Vec<f32>>) -> Arc<Vec<usize>> {
        out.resize_with(self.iem.len(), Vec::new);
        // At rest (every input silent for 10 s) the chains didn't run: silence.
        let resting = self.quiet_blocks >= self.rest_after;
        // The talkback microphone's lane (§M.28): silent while talkback is off.
        let quiet_tb = self.iem_talkback.filter(|_| !self.settings.talkback.on);
        for (o, &i) in out.iter_mut().zip(self.iem.iter()) {
            o.clear();
            if quiet_tb == Some(i) {
                o.resize(self.block, 0.0);
                continue;
            }
            match self.pre.get(i) {
                // A channel parked or never run this block: its buffer may hold an old
                // block (silence instead).
                Some(p) if !resting && self.active(i) => o.extend_from_slice(p),
                _ => o.resize(self.block, 0.0),
            }
        }
        self.iem.clone()
    }
}

/// What a desk output carries (the output patch's text, [`Settings::outputs`], parsed).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tap {
    None,
    Bus(usize),
    Pre(usize),
    Input(usize),
    PreEq(usize),
    PostEq(usize),
    Post(usize),
    In(usize),
}

impl Tap {
    fn parse(p: &str) -> Tap {
        let Some((kind, v)) = p.split_once(':') else {
            return Tap::None;
        };
        let Ok(n) = v.parse::<usize>() else {
            return Tap::None;
        };
        match kind {
            // `bus:J` in the outside numbering (see `bus_out`).
            "bus" => bus_in(n).map_or(Tap::None, Tap::Bus),
            "pre" => Tap::Pre(n),
            "input" => Tap::Input(n),
            "preeq" => Tap::PreEq(n),
            "posteq" => Tap::PostEq(n),
            "post" => Tap::Post(n),
            "in" => Tap::In(n),
            _ => Tap::None,
        }
    }
}

#[cfg(test)]
mod tests;
