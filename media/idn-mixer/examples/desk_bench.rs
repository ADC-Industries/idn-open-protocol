//! The desk's processing time per block: a busy 32-channel desk (gate, EQ, compressor,
//! de-esser, sends to aux, FX and groups on every channel; bus dynamics), the default
//! desk, and the busy desk on near-silence (denormals). No plugins, no network.
//!
//! Also the heap allocations made per block on the desk's thread (the clock thread must
//! not wait on the allocator), and the clock thread's whole block: the desk, then its
//! outputs handed to the network (`send_audio`), as the engine does.
//!
//!   [IDN_DESK_BLOCK=32|64|128|256] cargo run --release -p idn-mixer --example desk_bench [-- SECONDS]

use idn_mixer::desk::{Desk, SendTo, default_block};

/// The block the desks here are made with: the machine's default (`IDN_DESK_BLOCK`, else
/// 128), so the buffers match the desk (`IDN_DESK_BLOCK=32 desk_bench`).
fn bs() -> usize {
    default_block()
}
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const IO: usize = 64;

/// Counts allocations made on a thread while its `COUNTING` is on.
struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static TRACE: Cell<bool> = const { Cell::new(false) };
}
// SAFETY: every block comes from and goes back to System with the caller's layout; the
// counting and tracing don't panic.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            // `IDN_BENCH_TRACE=1`: where the first one comes from (once).
            if TRACE.with(Cell::get) {
                TRACE.with(|t| t.set(false));
                COUNTING.with(|c| c.set(false));
                // Not `eprintln!`: an allocator must not panic (a failed write is ignored).
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    std::backtrace::Backtrace::force_capture()
                );
                COUNTING.with(|c| c.set(true));
            }
        }
        // SAFETY: the caller's layout, passed on to System.
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        // SAFETY: `p` came from System (every allocation here does) with layout `l`.
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `p` came from System (every allocation here does) with layout `l`.
        unsafe { std::alloc::System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn busy(d: &mut Desk) {
    let mut s = d.settings().clone();
    for (i, c) in s.channels.iter_mut().enumerate() {
        c.shown = Some(true);
        c.hpf = 80.0;
        c.gate.on = true;
        c.eq_on = true;
        for b in &mut c.eq {
            b.on = true;
            b.gain = 3.0;
        }
        c.comp.on = true;
        c.deesser.on = i % 2 == 0;
        for k in 0..4 {
            c.sends[k] = SendTo {
                on: true,
                level: -6.0,
                ..Default::default()
            };
        }
        c.fx_sends[i % 2] = SendTo {
            on: true,
            level: -10.0,
            ..Default::default()
        };
        c.groups[i % 4] = true;
    }
    for b in [&mut s.main].into_iter().chain(s.aux.iter_mut()) {
        b.comp.on = true;
        b.limiter.on = true;
    }
    for g in s.groups.iter_mut().take(4) {
        g.comp.on = true;
        g.to_main = true;
    }
    for f in s.fx.iter_mut() {
        f.to_main = true;
    }
    d.set(s, false);
}

/// [`busy`] with the step 9 processing on every channel (§M.28): the gate a ducker keyed
/// from channel 1 through key filters, with hold; the compressor parallel (50 %), RMS,
/// auto release, its key high-passed; 24 dB filters both ways; sends tapped at the
/// input, before and after the EQ.
fn busy_new(d: &mut Desk) {
    busy(d);
    let mut s = d.settings().clone();
    for (i, c) in s.channels.iter_mut().enumerate() {
        c.lpf = 16_000.0;
        (c.hpf_slope, c.lpf_slope) = (24, 24);
        c.gate.mode = idn_mixer::desk::GateMode::Ducker;
        c.gate.key = if i == 0 { 0 } else { 1 };
        (c.gate.key_hpf, c.gate.key_lpf, c.gate.hold) = (200.0, 5000.0, 50.0);
        c.comp.mix = 50.0;
        c.comp.detect = idn_mixer::desk::Detect::Rms;
        c.comp.auto_release = true;
        c.comp.key_hpf = 100.0;
        use idn_mixer::desk::SendTap;
        for (k, t) in [SendTap::Input, SendTap::PreEq, SendTap::PostEq]
            .iter()
            .enumerate()
        {
            c.sends[k].tap = Some(*t);
        }
    }
    d.set(s, false);
}

/// Runs `blocks` blocks (each also sent, as the clock does, with `out`); gives (mean, 99th
/// percentile, worst) in µs and allocations per block.
fn time(
    d: &mut Desk,
    blocks: usize,
    amp: f32,
    out_to: Option<&std::sync::Arc<idn_open_protocol::api::Output>>,
) -> (f64, f64, f64, f64) {
    // As the engine: the outputs handed to an output thread, which sends them and hands
    // back buffers for the next block.
    let (to_out, from_clock) = std::sync::mpsc::sync_channel::<Vec<Vec<f32>>>(8);
    let (to_clock, spare) = std::sync::mpsc::sync_channel::<Vec<Vec<f32>>>(8);
    for _ in 0..4 {
        let _ = to_clock.try_send(vec![vec![0.0; bs()]; IO]);
    }
    if let Some(o) = out_to.cloned() {
        std::thread::spawn(move || {
            while let Ok(block) = from_clock.recv() {
                let _ = to_clock.try_send(vec![vec![0.0; bs()]; block.len()]);
                let _ = o.send_audio(
                    idn_open_protocol::api::AudioBlock {
                        sample_rate: 48_000,
                        channels: block,
                    },
                    None,
                );
            }
        });
    }
    let mut seed = 1u32;
    let mut noise = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    let mut out = vec![vec![0.0; bs()]; IO];
    let mut input = vec![vec![0.0; bs()]; IO];
    let mut t = Vec::with_capacity(blocks);
    let mut allocs = 0u64;
    // The first blocks of a fresh desk fault its memory in: not counted.
    let warm = 100;
    for b in 0..blocks + warm {
        if b == warm + 1 && std::env::var("IDN_BENCH_TRACE").as_deref() == Ok("1") {
            TRACE.with(|t| t.set(true));
        }
        for (k, ch) in input.iter_mut().enumerate() {
            for (i, v) in ch.iter_mut().enumerate() {
                let tone =
                    (2.0 * std::f32::consts::PI * 110.0 * (k + 1) as f32 * (b * bs() + i) as f32
                        / 48_000.0)
                        .sin();
                *v = amp * (0.7 * tone + 0.3 * noise());
            }
        }
        let start = Instant::now();
        let a0 = ALLOCS.load(Ordering::Relaxed);
        COUNTING.with(|c| c.set(true));
        d.process(&input, &mut out);
        if out_to.is_some() {
            let next = spare
                .try_recv()
                .unwrap_or_else(|_| vec![vec![0.0; bs()]; IO]);
            let _ = to_out.try_send(std::mem::replace(&mut out, next));
        }
        COUNTING.with(|c| c.set(false));
        if b >= warm {
            allocs += ALLOCS.load(Ordering::Relaxed) - a0;
            t.push(start.elapsed().as_secs_f64() * 1e6);
        }
    }
    let mean = t.iter().sum::<f64>() / t.len() as f64;
    t.sort_by(f64::total_cmp);
    (
        mean,
        t[t.len() * 99 / 100],
        t[t.len() - 1],
        allocs as f64 / blocks as f64,
    )
}

fn main() {
    let secs: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30.0);
    let blocks = (secs * 48_000.0 / bs() as f64) as usize;
    // As the desk's clock thread runs (`IDN_BENCH_NO_FTZ=1`: without).
    if std::env::var("IDN_BENCH_NO_FTZ").as_deref() != Ok("1") {
        idn_common::rt::flush_denormals();
    }
    let budget = bs() as f64 / 48.0 * 1000.0;
    println!("{blocks} blocks of {} samples ({budget:.0} µs each)", bs());
    let report = |name: &str, (mean, p99, worst, allocs): (f64, f64, f64, f64)| {
        println!(
            "{name:<30} mean {mean:7.1} µs ({:4.1}% of a core)  p99 {p99:7.1}  worst {worst:7.1}  allocs/block {allocs:5.1}",
            mean / budget * 100.0
        );
    };
    let publisher =
        idn_open_protocol::api::Publisher::new(idn_open_protocol::api::PublisherConfig {
            mdns: Some(false),
            ..Default::default()
        })
        .unwrap();
    let output = std::sync::Arc::new(
        idn_open_protocol::api::Output::new(
            &publisher,
            idn_open_protocol::api::OutputConfig::new("Desk bench"),
        )
        .unwrap(),
    );
    for channels in [32, 64] {
        let mut d = Desk::with_io(channels, IO, IO);
        busy(&mut d);
        report(
            &format!("{channels} ch busy, −20 dBFS"),
            time(&mut d, blocks, 0.1, None),
        );
        // Banks of four at rest (§M.24): three quarters of the channels muted.
        let mut d = Desk::with_io(channels, IO, IO);
        busy(&mut d);
        let mut s = d.settings().clone();
        for c in s.channels.iter_mut().skip(channels / 4) {
            c.mute = true;
        }
        d.set(s, false);
        let _ = time(&mut d, blocks / 10, 0.1, None);
        report(
            &format!("{channels} ch busy, 3/4 muted"),
            time(&mut d, blocks, 0.1, None),
        );
        let mut d = Desk::with_io(channels, IO, IO);
        report(
            &format!("{channels} ch default desk"),
            time(&mut d, blocks, 0.1, None),
        );
        let mut d = Desk::with_io(channels, IO, IO);
        busy(&mut d);
        let _ = time(&mut d, blocks / 10, 0.1, None);
        report(
            &format!("{channels} ch busy, near-silence"),
            time(&mut d, blocks, 1e-30, None),
        );
        let mut d = Desk::with_io(channels, IO, IO);
        busy_new(&mut d);
        report(
            &format!("{channels} ch busy + §M.28"),
            time(&mut d, blocks, 0.1, None),
        );
        let mut d = Desk::with_io(channels, IO, IO);
        busy(&mut d);
        report(
            &format!("{channels} ch busy + send"),
            time(&mut d, blocks, 0.1, Some(&output)),
        );
    }
}
