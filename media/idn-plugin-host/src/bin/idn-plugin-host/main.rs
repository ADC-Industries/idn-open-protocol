//! `idn-plugin-host`: CLAP and VST3 plugins in a process of their own, driven by the
//! mixing desk over standard input and output (`idn_plugin_host::protocol`). Each chain
//! has two threads: its main thread, where its plugins are made, told, saved and where
//! their own windows live (with the timers and descriptors those register), and its audio
//! thread, which only processes. A plugin's window can then repaint without making its
//! sound late. The chains of a block run at once on their audio threads (
//! on one thread, ten heavy plugins filled the block).

use idn_common::sync::LockOk as _;
mod clap;
use idn_common::rt as priority;
mod runloop;
mod scan;
mod vst3;
mod window;

use idn_plugin_host::protocol::{
    ChainAudio, Msg, SHM_BYTES, SHM_ITEM_BYTES, SHM_SLOT_BYTES, SHM_SLOTS, parse_chains,
    put_chains, read_msg, shm_get, shm_inputs, shm_put, shm_results, write_msg,
};
use runloop::{Fired, RunLoop};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Write};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use window::{HostWindow, Parent};

/// The desk's rate, as its chains are made at (one desk, one rate).
static RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(48_000);

/// A plugin's parameter as the panel shows it.
pub struct Param {
    pub id: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
    pub text: String,
}

/// A loaded plugin's main-thread side, whatever its format.
pub trait Plugin {
    fn name(&self) -> String;
    /// Samples of latency it adds.
    fn latency(&mut self) -> u32;
    fn params(&mut self) -> Vec<Param>;
    /// Takes effect at the next block.
    fn set_param(&mut self, id: u32, value: f64);
    fn save(&mut self) -> Vec<u8>;
    fn load(&mut self, state: &[u8]) -> Result<(), String>;
    /// Its audio side, taken once, for the chain's audio thread.
    fn processor(&mut self) -> Box<dyn Processor>;
    /// Parameters moved in its own window since last asked: (id, value).
    fn edits(&mut self) -> Vec<(u32, f64)>;
    /// Whether it has an interface of its own that can go in a window here.
    fn has_editor(&mut self) -> bool;
    /// Shows its own interface in `win`; its size.
    fn open_editor(&mut self, win: &HostWindow) -> Result<(u32, u32), String>;
    /// The size its interface wants before a window is made for it (default 640 × 480).
    fn editor_size(&mut self) -> Option<(u32, u32)> {
        None
    }
    fn close_editor(&mut self);
    /// A size its interface asked for since last asked.
    fn take_resize(&mut self) -> Option<(u32, u32)> {
        None
    }
    /// It closed its interface itself.
    fn editor_gone(&mut self) -> bool {
        false
    }
    /// A timer or descriptor it registered (CLAP).
    fn on_timer(&mut self, _id: u32) {}
    fn on_fd(&mut self, _fd: i32, _flags: u32) {}
}

/// A loaded plugin's audio side: lives on the chain's audio thread.
pub trait Processor: Send {
    /// Processes `io` (planar, `frames` per channel) in place.
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize);
}

/// What a chain's main thread gets when a plugin is made: its run loop and its number.
pub struct Ui {
    pub runloop: Rc<RunLoop>,
    pub owner: u64,
}

/// Opens a plugin by its key (`clap:PATH#ID`, `vst3:PATH#CLASS`) for `channels` channels.
fn open(
    key: &str,
    channels: usize,
    rate: f64,
    block: usize,
    ui: Ui,
) -> Result<Box<dyn Plugin>, String> {
    let (format, rest) = key
        .split_once(':')
        .ok_or("a plugin key is FORMAT:PATH#ID")?;
    let (path, id) = rest
        .rsplit_once('#')
        .ok_or("a plugin key is FORMAT:PATH#ID")?;
    match format {
        "clap" => clap::open(path, id, channels, rate, block, ui),
        "vst3" => vst3::open(path, id, channels, rate, block, ui),
        f => Err(format!("unknown plugin format {f:?}")),
    }
}

/// A chain's plugins on its main thread.
struct Chain {
    plugins: Vec<Slot>,
}

struct Slot {
    plugin: Box<dyn Plugin>,
    owner: u64,
    bypass: bool,
    window: Option<HostWindow>,
}

impl Chain {
    /// Samples of latency the chain adds (bypassed plugins add none).
    fn latency(&mut self) -> u32 {
        self.plugins
            .iter_mut()
            .filter(|p| !p.bypass)
            .map(|p| p.plugin.latency())
            // (Saturating: two plugins' latencies past u32 panicked a debug build and
            // wrapped a release one; the desk takes at most a second.)
            .fold(0u32, u32::saturating_add)
    }
}

/// Each plugin's output peak (a level, not dB) since it was last read, by its place in
/// the chain: written by the chain's audio thread, read (and cleared) by its main thread
/// for the panel's per-slot meters.
type Levels = Arc<Mutex<Vec<f32>>>;

/// Work for a chain's audio thread.
enum AudioJob {
    /// Block `seq` of its sound, its result to the thread that gathers them.
    Block(u64, ChainAudio),
    /// New processors (old ones dropped there first), with the chain's channels.
    Install(usize, Vec<(Box<dyn Processor>, bool)>, Sender<()>),
    Bypass(usize, bool),
}

/// Starts a chain's audio thread.
fn audio_worker(id: u32, done: Sender<(u64, ChainAudio)>, levels: Levels) -> Sender<AudioJob> {
    let (tx, rx): (Sender<AudioJob>, Receiver<AudioJob>) = channel();
    std::thread::Builder::new()
        .name(format!("chain {id} audio"))
        .spawn(move || {
            priority::audio_this_thread();
            priority::flush_denormals();
            let mut channels = 0usize;
            let mut procs: Vec<(Box<dyn Processor>, bool)> = Vec::new();
            let mut buf: Vec<Vec<f32>> = Vec::new();
            while let Ok(job) = rx.recv() {
                match job {
                    AudioJob::Install(ch, p, ack) => {
                        procs = p;
                        channels = ch;
                        buf = vec![Vec::new(); ch];
                        *levels.lock_ok() = vec![0.0; procs.len()];
                        let _ = ack.send(());
                    }
                    AudioJob::Bypass(i, on) => {
                        if let Some(p) = procs.get_mut(i) {
                            p.1 = on;
                        }
                    }
                    AudioJob::Block(seq, b) => {
                        let out = if procs.is_empty() {
                            b
                        } else {
                            process(channels, &mut procs, &mut buf, b, &levels)
                        };
                        let _ = done.send((seq, out));
                    }
                }
            }
        })
        .expect("a thread for a chain");
    tx
}

/// A block through its chain.
fn process(
    channels: usize,
    procs: &mut [(Box<dyn Processor>, bool)],
    buf: &mut [Vec<f32>],
    b: ChainAudio,
    levels: &Levels,
) -> ChainAudio {
    let frames = b.frames as usize;
    // The block into the chain's buffers (missing channels silent).
    for (ch, x) in buf.iter_mut().enumerate() {
        x.clear();
        if ch < b.channels as usize {
            x.extend_from_slice(b.channel(ch));
        } else {
            x.resize(frames, 0.0);
        }
    }
    // Each plugin's output peak, kept until read (never waiting for the reader: a busy
    // lock skips this block's).
    let mut held = levels.try_lock_ok();
    for (i, (p, bypass)) in procs.iter_mut().enumerate() {
        if !*bypass {
            p.process(buf, frames);
        }
        if let Some(l) = held.as_mut().and_then(|l| l.get_mut(i)) {
            let peak = buf
                .iter()
                .flat_map(|x| x[..frames].iter())
                .fold(0f32, |m, v| m.max(v.abs()));
            *l = l.max(peak);
        }
    }
    drop(held);
    // Into the block's own buffer (no allocation per block when the channels match).
    let mut data = b.data;
    data.clear();
    for x in buf.iter() {
        data.extend_from_slice(&x[..frames]);
    }
    ChainAudio {
        chain: b.chain,
        channels: channels as u16,
        frames: b.frames,
        data,
    }
}

/// Work for a chain's main thread: a command, answered on the sender.
/// A chain's command and its request's id: the chain's thread answers it itself.
type Job = (Value, Value);

/// Starts a chain's main thread; it ends (closing windows, dropping its plugins there,
/// after their audio sides) when its sender is dropped.
fn main_worker(
    id: u32,
    audio: Sender<AudioJob>,
    events: Sender<Msg>,
    levels: Levels,
) -> Sender<Job> {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = channel();
    std::thread::Builder::new()
        .name(format!("chain {id}"))
        .spawn(move || {
            // Plugins' windows draw here: below the audio threads, so a busy interface
            // never keeps them waiting for a CPU.
            priority::lower_this_thread();
            let rl = Rc::new(RunLoop::default());
            let mut chain: Option<Chain> = None;
            let mut next_owner = 1u64;
            loop {
                let busy = rl.busy()
                    || chain
                        .as_ref()
                        .is_some_and(|c| c.plugins.iter().any(|p| p.window.is_some()));
                // With a window open: the run loop's timers, descriptors and window events
                // every few milliseconds between commands.
                let job = if busy {
                    match rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(j) => Some(j),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match rx.recv() {
                        Ok(j) => Some(j),
                        Err(_) => break,
                    }
                };
                if let Some((v, request)) = job {
                    let mut r =
                        match command(&v, &mut chain, &rl, &mut next_owner, &audio, id, &levels) {
                            Ok(r) => r,
                            Err(e) => json!({"error": e}),
                        };
                    r["id"] = request;
                    let _ = events.send(Msg::Json(r));
                }
                if let Some(c) = chain.as_mut() {
                    service(id, c, &rl, &events);
                }
            }
            // The audio side goes first, then the plugins (CLAP deactivates with it back).
            // An audio thread stuck in a plugin still holds their processors: they are
            // kept for good then, never unloaded under a running call.
            let (ack, done) = channel();
            let freed = audio.send(AudioJob::Install(0, Vec::new(), ack)).is_err()
                || done.recv_timeout(Duration::from_secs(2)).is_ok();
            if freed {
                drop(chain);
            } else {
                std::mem::forget(chain);
            }
        })
        .expect("a thread for a chain");
    tx
}

/// Timers, descriptors, windows closed or resized, and parameters moved in windows.
fn service(id: u32, c: &mut Chain, rl: &RunLoop, events: &Sender<Msg>) {
    for f in rl.due() {
        match f {
            Fired::Func(f) => f(),
            Fired::Timer(owner, t) => {
                if let Some(s) = c.plugins.iter_mut().find(|s| s.owner == owner) {
                    s.plugin.on_timer(t);
                }
            }
            Fired::Fd(owner, fd, flags) => {
                if let Some(s) = c.plugins.iter_mut().find(|s| s.owner == owner) {
                    s.plugin.on_fd(fd, flags);
                }
            }
        }
    }
    window::pump();
    for (i, s) in c.plugins.iter_mut().enumerate() {
        if let Some(w) = s.window.as_mut() {
            if w.closed() || s.plugin.editor_gone() {
                s.plugin.close_editor();
                s.window = None;
                let _ = events.send(Msg::Json(
                    json!({"event": "editor_closed", "chain": id, "index": i}),
                ));
            } else if let Some((width, height)) = s.plugin.take_resize() {
                w.resize(width, height);
            }
        }
        for (param, value) in s.plugin.edits() {
            let _ = events.send(Msg::Json(
                json!({"event": "param", "chain": id, "index": i, "param": param, "value": value}),
            ));
        }
    }
}

/// How long a block may take before its slow chains are left out of the answer: the desk
/// has long since gone on without them (it takes block k − 1's results with block k).
/// A chain that misses it is stuck: sent nothing until its late result comes, so that one
/// plugin hung in its processing doesn't hold up every block (a
/// 1 s limit made every block take 1 s and every insert on the desk play dry).
const BLOCK_LIMIT: Duration = Duration::from_millis(50);

/// The pipe to the desk, taken off standard output before any plugin loads: plugins print
/// (version banners, debug lines), and their bytes in the stream made the desk see a
/// broken host and start it again every 5 s. Their standard output is standard error from
/// here on.
fn protocol_out() -> std::fs::File {
    #[cfg(unix)]
    // SAFETY: plain descriptor calls on 1 and 2 before any other thread runs; the
    // duplicate is owned by the File alone. (Should dup2 fail, plugins' output still
    // goes into the stream, as before.)
    unsafe {
        use std::os::fd::FromRawFd;
        let fd = libc::dup(1);
        assert!(fd >= 0, "no standard output");
        libc::dup2(2, 1);
        std::fs::File::from_raw_fd(fd)
    }
    #[cfg(windows)]
    // SAFETY: the standard handles are this process's; the duplicate is owned by the File
    // alone. The C runtime's descriptor 1 goes to standard error too (closing the original
    // handle; the duplicate stays).
    unsafe {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        unsafe extern "C" {
            fn _dup2(from: i32, to: i32) -> i32;
        }
        let (me, out) = (GetCurrentProcess(), GetStdHandle(STD_OUTPUT_HANDLE));
        let mut ours = std::ptr::null_mut();
        if DuplicateHandle(me, out, me, &mut ours, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
            ours = out;
        } else {
            SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE));
            _dup2(2, 1);
        }
        std::fs::File::from_raw_handle(ours as _)
    }
}

fn main() {
    // A scan, in a process of its own.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // (The pipe taken first in a scan too: plugins' own printing went into its lines, and
    // a plugin was lost or reported as crashed; bug round E, E2-8.)
    let pipe = protocol_out();
    if args.first().is_some_and(|a| a == "--scan") {
        scan::run(&args[1..], pipe);
        return;
    }
    // This thread hands the blocks out and gathers them: as urgent as the audio threads.
    priority::audio_this_thread();
    let mut input = BufReader::new(std::io::stdin().lock());
    // Answers, results and events all go out through one writer.
    let (out_tx, out_rx) = channel::<Msg>();
    std::thread::spawn(move || {
        priority::audio_this_thread();
        let mut out = BufWriter::new(pipe);
        while let Ok(m) = out_rx.recv() {
            if write_msg(&mut out, &m).and_then(|()| out.flush()).is_err() {
                std::process::exit(0);
            }
        }
    });
    let (done_tx, done_rx) = channel::<(u64, ChainAudio)>();
    let mut workers: BTreeMap<u32, (Sender<Job>, Sender<AudioJob>)> = BTreeMap::new();
    // The desk's shared memory for the audio, once it offers one (`shm`).
    let mut shm: Option<idn_common::shm::Region> = None;
    // Chains that missed a block's limit, with that block: sent nothing until it's back.
    let mut stuck: BTreeMap<u32, u64> = BTreeMap::new();
    let mut shm_buf: Vec<u8> = Vec::new();
    while let Ok(m) = read_msg(&mut input) {
        let reply = match m {
            Msg::Json(v) => {
                let id = v["id"].clone();
                let cmd = v["cmd"].as_str().unwrap_or("").to_owned();
                let mut r = match v["chain"].as_u64().map(|n| n as u32) {
                    // A chain's command: on its main thread (a new chain gets its threads).
                    Some(n) => {
                        if cmd == "chain" && !workers.contains_key(&n) {
                            let levels = Levels::default();
                            let audio = audio_worker(n, done_tx.clone(), levels.clone());
                            let main = main_worker(n, audio.clone(), out_tx.clone(), levels);
                            workers.insert(n, (main, audio));
                            stuck.remove(&n);
                        }
                        match workers.get(&n) {
                            None => json!({"error": "no such chain"}),
                            Some((w, _)) => {
                                // Answered by the chain's thread when it is done: waiting
                                // here held up every chain's audio while one built a plugin
                                // or painted its editor (up to 30 s dry).
                                let sent = w.send((v, id.clone())).is_ok();
                                if cmd == "remove" {
                                    workers.remove(&n);
                                    stuck.remove(&n);
                                }
                                if sent {
                                    continue;
                                }
                                json!({"error": "the chain's thread stopped"})
                            }
                        }
                    }
                    _ => match cmd.as_str() {
                        // The audio through the desk's shared memory (protocol.rs).
                        "shm" => {
                            let fits = v["slots"].as_u64() == Some(SHM_SLOTS as u64)
                                && v["slot_bytes"].as_u64() == Some(SHM_SLOT_BYTES as u64);
                            match idn_common::shm::Region::open(
                                v["name"].as_str().unwrap_or(""),
                                SHM_BYTES,
                            ) {
                                Ok(r) if fits => {
                                    shm = Some(r);
                                    json!({"ok": true})
                                }
                                Ok(_) => json!({"error": "another layout of shared memory"}),
                                Err(e) => json!({"error": format!("shared memory: {e}")}),
                            }
                        }
                        other => json!({"error": format!("no command {other:?}")}),
                    },
                };
                r["id"] = id;
                Msg::Json(r)
            }
            Msg::Audio {
                seq,
                chains: blocks,
            } => Msg::Audio {
                seq,
                chains: run_block(seq, blocks, &workers, &done_rx, &mut stuck, &out_tx),
            },
            // In the shared memory: read, run, the results written back to the slot (on
            // the pipe if they don't fit). A slot whose stamp isn't this block's (the desk
            // came round to it again while this one waited) is answered with nothing: the
            // desk plays those chains dry.
            Msg::AudioShm { seq, slot, len } => {
                let slot = slot as usize;
                // (At most a slot, before anything is allocated; the buffer kept between
                // blocks.)
                let b = &mut shm_buf;
                b.clear();
                b.resize((len as usize).min(SHM_ITEM_BYTES + 1), 0);
                let blocks = match &shm {
                    Some(r)
                        if slot < SHM_SLOTS
                            && b.len() <= SHM_ITEM_BYTES
                            && shm_get(r, shm_inputs(slot), seq, b) =>
                    {
                        parse_chains(b)
                            .ok()
                            .filter(|(s, _)| *s == seq)
                            .map(|(_, c)| c)
                            .unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                let done = run_block(seq, blocks, &workers, &done_rx, &mut stuck, &out_tx);
                b.clear();
                put_chains(b, seq, &done);
                match &shm {
                    Some(r) if shm_put(r, shm_results(slot), seq, b) => Msg::AudioShm {
                        seq,
                        slot: slot as u32,
                        len: b.len() as u32,
                    },
                    _ => Msg::Audio { seq, chains: done },
                }
            }
        };
        if out_tx.send(reply).is_err() {
            break;
        }
    }
}

/// Every chain's block to its audio thread at once, then the results gathered (within
/// [`BLOCK_LIMIT`]; a chain late past it is left out, and plays dry). Chains in `stuck`
/// get nothing until their late result arrives; both changes are told to the desk
/// (`{"event": "stuck"|"running", "chain"}`).
fn run_block(
    seq: u64,
    blocks: Vec<ChainAudio>,
    workers: &BTreeMap<u32, (Sender<Job>, Sender<AudioJob>)>,
    done_rx: &std::sync::mpsc::Receiver<(u64, ChainAudio)>,
    stuck: &mut BTreeMap<u32, u64>,
    events: &Sender<Msg>,
) -> Vec<ChainAudio> {
    // What the desk was told of each chain: "stuck" at most once a second, "running" only
    // after a "stuck" (one just too slow for the block said both every block).
    thread_local! {
        static TOLD: std::cell::RefCell<BTreeMap<u32, Instant>> = Default::default();
    }
    let tell_stuck = |chain: u32| {
        TOLD.with(|t| {
            let mut t = t.borrow_mut();
            let due = t
                .get(&chain)
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
            if due {
                t.insert(chain, Instant::now());
            }
            due
        })
    };
    // (Told "stuck" since it last ran: the desk is to hear it runs again.)
    thread_local! {
        static SAID_STUCK: std::cell::RefCell<std::collections::BTreeSet<u32>> = Default::default();
    }
    let back = |chain: u32, stuck: &mut BTreeMap<u32, u64>| {
        if stuck.remove(&chain).is_some() && SAID_STUCK.with(|s| s.borrow_mut().remove(&chain)) {
            eprintln!("idn-plugin-host: chain {chain} answers again");
            let _ = events.send(Msg::Json(json!({"event": "running", "chain": chain})));
        }
    };
    // Late results since the last block: their chains run again.
    while let Ok((_, c)) = done_rx.try_recv() {
        back(c.chain, stuck);
    }
    // The answer goes when the desk needs it (a block's time, and a little more): a chain
    // not done by then is left out of this block alone (dry on its own strip), not every
    // chain held up to the 50 ms limit and the whole desk dry (bug round E, E2-4).
    let frames = blocks.iter().map(|b| b.frames).max().unwrap_or(0);
    // In the desk's own rate (bug run K6-3: in 48 kHz frames, at 96 kHz it waited past the
    // desk's own wait and the whole desk played dry).
    let rate = f64::from(RATE.load(std::sync::atomic::Ordering::Relaxed).max(8_000));
    let deadline = BLOCK_LIMIT
        .min(Duration::from_secs_f64(f64::from(frames) / rate) + Duration::from_micros(500));
    let mut waiting = Vec::new();
    for b in blocks {
        let chain = b.chain;
        if !stuck.contains_key(&chain)
            && let Some((_, a)) = workers.get(&chain)
            && a.send(AudioJob::Block(seq, b)).is_ok()
        {
            waiting.push(chain);
        }
    }
    let mut done = Vec::with_capacity(waiting.len());
    let end = Instant::now() + deadline;
    while done.len() < waiting.len() {
        let Some(left) = end.checked_duration_since(Instant::now()) else {
            break;
        };
        match done_rx.recv_timeout(left) {
            Ok((s, c)) if s == seq => done.push(c),
            // (A result for an earlier block, late past the limit: dropped.)
            Ok((_, c)) => back(c.chain, stuck),
            Err(_) => break,
        }
    }
    for chain in waiting {
        if !done.iter().any(|c| c.chain == chain) {
            stuck.insert(chain, seq);
            if !SAID_STUCK.with(|s| s.borrow().contains(&chain)) && tell_stuck(chain) {
                SAID_STUCK.with(|s| s.borrow_mut().insert(chain));
                eprintln!(
                    "idn-plugin-host: chain {chain} missed block {seq}: left out until it answers"
                );
                let _ = events.send(Msg::Json(json!({"event": "stuck", "chain": chain})));
            }
        }
    }
    done.sort_by_key(|c| c.chain);
    done
}

/// A chain's command on its main thread.
fn command(
    v: &Value,
    chain: &mut Option<Chain>,
    rl: &Rc<RunLoop>,
    next_owner: &mut u64,
    audio: &Sender<AudioJob>,
    id: u32,
    levels: &Levels,
) -> Result<Value, String> {
    let index = || {
        v["index"]
            .as_u64()
            .map(|i| i as usize)
            .ok_or("index: a number")
    };
    fn slot(chain: &mut Option<Chain>, i: usize) -> Result<&mut Slot, String> {
        let c = chain.as_mut().ok_or("no such chain")?;
        c.plugins
            .get_mut(i)
            .ok_or_else(|| "no such plugin".to_owned())
    }
    let latency = |chain: &mut Option<Chain>| chain.as_mut().map_or(0, Chain::latency);
    match v["cmd"].as_str().unwrap_or("") {
        "chain" => {
            let channels = v["channels"].as_u64().unwrap_or(2).clamp(1, 8) as usize;
            let rate = v["rate"].as_f64().unwrap_or(48_000.0);
            if (8_000.0..=384_000.0).contains(&rate) {
                RATE.store(rate as u32, std::sync::atomic::Ordering::Relaxed);
            }
            let block = v["block"].as_u64().unwrap_or(96).clamp(1, 8192) as usize;
            let mut plugins = Vec::new();
            for p in v["plugins"].as_array().cloned().unwrap_or_default() {
                let key = p["key"].as_str().ok_or("plugins: each with a key")?;
                let owner = *next_owner;
                *next_owner += 1;
                let mut plug = open(
                    key,
                    channels,
                    rate,
                    block,
                    Ui {
                        runloop: rl.clone(),
                        owner,
                    },
                )
                .map_err(|e| format!("{key}: {e}"))?;
                if let Some(s) = p["state"].as_str().filter(|s| !s.is_empty()) {
                    let bytes =
                        idn_common::text::unbase64_r(s).map_err(|e| format!("state: {e}"))?;
                    plug.load(&bytes)
                        .map_err(|e| format!("{key}: state: {e}"))?;
                }
                plugins.push(Slot {
                    plugin: plug,
                    owner,
                    bypass: p["bypass"].as_bool().unwrap_or(false),
                    window: None,
                });
            }
            let procs: Vec<(Box<dyn Processor>, bool)> = plugins
                .iter_mut()
                .map(|s| (s.plugin.processor(), s.bypass))
                .collect();
            // The new audio side in place (the old one dropped there), then the old plugins.
            let (ack, done) = channel();
            audio
                .send(AudioJob::Install(channels, procs, ack))
                .map_err(|_| "the chain's audio thread stopped")?;
            if done.recv_timeout(Duration::from_secs(5)).is_err() {
                // The audio thread is stuck in a plugin and takes the new processors when
                // it frees itself: their plugins must outlive them, so they are kept for
                // good rather than dropped here under a running processor. Rare, and
                // the desk builds the chain again.
                std::mem::forget(plugins);
                return Err("the chain's audio thread didn't answer".into());
            }
            let mut c = Chain { plugins };
            let latency = c.latency();
            let described: Vec<Value> = c
                .plugins
                .iter_mut()
                .map(|p| json!({"name": p.plugin.name(), "params": params_json(p.plugin.params())}))
                .collect();
            drop_chain(chain, rl);
            *chain = Some(c);
            let _ = id;
            Ok(json!({"latency": latency, "plugins": described}))
        }
        "remove" => {
            let (ack, done) = channel();
            let freed = audio.send(AudioJob::Install(0, Vec::new(), ack)).is_err()
                || done.recv_timeout(Duration::from_secs(5)).is_ok();
            if !freed {
                // Stuck in a plugin: the plugins outlive their processors, kept for good
                // (as for "chain").
                if let Some(c) = chain.take() {
                    for s in &c.plugins {
                        rl.remove_owner(s.owner);
                    }
                    std::mem::forget(c);
                }
                return Err("the chain's audio thread didn't answer".into());
            }
            drop_chain(chain, rl);
            Ok(json!({}))
        }
        // A chain's latency now: many plugins know it only once they run (
        // LSP's limiters said 0 when built, then delayed by their look-ahead).
        "latency" => Ok(json!({"latency": latency(chain)})),
        // Each plugin's output peak since the last time (cleared now), in dBFS.
        "levels" => {
            let mut l = levels.lock_ok();
            let db: Vec<f64> = l
                .iter()
                .map(|&v| {
                    if v > 1e-7 {
                        (20.0 * f64::from(v).log10() * 10.0).round() / 10.0
                    } else {
                        -144.0
                    }
                })
                .collect();
            l.iter_mut().for_each(|v| *v = 0.0);
            Ok(json!({ "levels": db }))
        }
        "params" => {
            let p = &mut slot(chain, index()?)?.plugin;
            Ok(json!({"name": p.name(), "params": params_json(p.params()), "latency": p.latency()}))
        }
        "set" => {
            let i = index()?;
            let p = &mut slot(chain, i)?.plugin;
            let id = v["param"].as_u64().ok_or("param: a parameter id")? as u32;
            p.set_param(id, v["value"].as_f64().ok_or("value: a number")?);
            Ok(json!({"latency": latency(chain)}))
        }
        "bypass" => {
            let i = index()?;
            let on = v["on"].as_bool().ok_or("on: true or false")?;
            slot(chain, i)?.bypass = on;
            let _ = audio.send(AudioJob::Bypass(i, on));
            Ok(json!({"latency": latency(chain)}))
        }
        "load" => {
            let i = index()?;
            let bytes = idn_common::text::unbase64_r(v["state"].as_str().unwrap_or(""))
                .map_err(|e| format!("state: {e}"))?;
            slot(chain, i)?.plugin.load(&bytes)?;
            Ok(json!({"latency": latency(chain)}))
        }
        "state" => {
            let p = &mut slot(chain, index()?)?.plugin;
            Ok(json!({"state": idn_common::text::base64(p.save())}))
        }
        // Its own interface: in a window of the application's (`parent`) or its own.
        "editor" => {
            let parent = Parent::parse(v["parent"].as_str())?;
            let s = slot(chain, index()?)?;
            if s.window.is_none() {
                if !s.plugin.has_editor() {
                    return Err("this plugin has no window of its own".into());
                }
                let title = v["title"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| s.plugin.name());
                let (w, h) = s.plugin.editor_size().unwrap_or((640, 480));
                let win = HostWindow::open(parent, &title, w, h)?;
                let (w, h) = s.plugin.open_editor(&win)?;
                win.resize(w, h);
                s.window = Some(win);
            }
            let handle = s.window.as_ref().map_or(0, HostWindow::handle);
            Ok(json!({"open": true, "platform": window::PLATFORM, "window": handle}))
        }
        "editor_close" => {
            let s = slot(chain, index()?)?;
            if s.window.is_some() {
                s.plugin.close_editor();
                s.window = None;
            }
            Ok(json!({"open": false}))
        }
        other => Err(format!("no command {other:?}")),
    }
}

/// Lets go of a chain's plugins (their audio side already gone), and of what they had
/// registered in the run loop.
fn drop_chain(chain: &mut Option<Chain>, rl: &RunLoop) {
    if let Some(old) = chain.take() {
        let owners: Vec<u64> = old.plugins.iter().map(|s| s.owner).collect();
        drop(old);
        for o in owners {
            rl.remove_owner(o);
        }
    }
}

fn params_json(ps: Vec<Param>) -> Value {
    Value::Array(
        ps.into_iter()
            .map(|p| json!({"id": p.id, "name": p.name, "min": p.min, "max": p.max, "default": p.default, "value": p.value, "text": p.text}))
            .collect(),
    )
}
