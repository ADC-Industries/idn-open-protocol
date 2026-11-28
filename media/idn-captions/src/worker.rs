//! whisper in a worker process at low priority ([`crate::Priority`]; an integrator's bug run
//! H05-1): caption passes must never take CPU from the frames of the program that runs
//! them. whisper.cpp starts its own compute threads on every pass; on Windows a new thread
//! doesn't inherit its creator's thread priority, and ggml leaves its threads at NORMAL,
//! so a lowered calling thread covered only its own share. A process of its own takes a
//! priority for all its threads: on Windows its priority class (below normal, or idle),
//! which every thread's priority is relative to; on Linux and other Unix systems its nice
//! value (10, or 19), set before any thread exists, so whisper's threads inherit it.
//!
//! Only whisper's work moves there: loading the model, choosing its device and the passes.
//! What the engine makes of a pass (sound, music, lines, filters, load) stays in the
//! engine's own process.
//!
//! # The worker
//!
//! `PROGRAM --idn-captions-worker low|lowest`, its standard input and output pipes to the
//! engine; its errors go where the engine's go. A program serves as one by calling
//! [`serve_if_asked`] first thing in its `main` (`idn` does). It says hello first (before
//! it lowers its priority, so a busy machine doesn't hold up the greeting), sets its
//! priority, then answers until its input closes, then exits.
//!
//! # Frames
//!
//! Each way the same: a `u32` little-endian length, then that many bytes: a JSON object
//! on one line ending `\n`, then (a pass's sound) the rest, raw. At most [`MAX_FRAME`]
//! bytes either way; a longer one, or one that can't be read, ends the conversation (a
//! pass is at most 30 s of 16 kHz `f32`, 1.9 MB).
//!
//! # Messages
//!
//! - First, from the worker unasked: `{"op":"hello","program":"idn-captions",
//!   "version":V,"protocol":1}` ([`PROTOCOL`]). A program that doesn't say it within
//!   [`HELLO_WITHIN`] isn't a captions worker (GNU libidn's `idn`, say) and isn't used.
//! - Each request carries an `"id"` (counting up from 1 within a worker's life) and its
//!   answer the same `"id"`; the engine discards answers to requests it gave up on.
//! - `{"op":"load","path":P,"gpu":B}` → `{"ok":true,"device":NAME,"priority":WHAT}`
//!   (`"priority_error":WHY` instead when the priority couldn't be set) or
//!   `{"error":WHY}`. The device is chosen in the worker, as in-process (the best GPU
//!   when `gpu`).
//! - `{"op":"devices","gpu":B}` → `{"device":NAME,"gpu":{"index","name","discrete",
//!   "memory_mb"}|null}`: the device a load would choose ("auto" keys its choice on it, and
//!   only the worker opens the GPU's driver).
//! - `{"op":"pass","threads":T,"lang":L,"prompt":S}` (and when measuring
//!   `"max_tokens":M`, `"temperature_inc":X`), the sound after it as `f32` little-endian
//!   16 kHz mono → `{"segments":[{"text":S,"no_speech":X,"probs":[X…]}…]}` or
//!   `{"error":WHY}`. Each segment's `probs` leaves out the special tokens (at or above
//!   end-of-text).
//! - `{"op":"abort","id":N}`, no answer of its own: pass `N` (under way, or still to
//!   come) stops as soon as whisper asks, answering `{"error":"stopped"}`
//!   ([`crate::STOPPED`]). Read on a thread of its own while a pass runs.
//!
//! # Slow, stopped, ended
//!
//! A pass not answered within [`crate::EngineConfig::worker_timeout`] is given up on
//! (aborted, its answer discarded when it comes); the worker lives on: slow is not dead,
//! a machine busy with frames is what low priority is for. Only a worker that ends, sends
//! a frame that can't be read, or whose process is gone is stopped; the engine starts
//! another after [`backoff`] (1 s, 5 s, 30 s, then every 60 s, afresh after 10 minutes
//! up), on the CPU after one that ended loading the model on the GPU, and never runs
//! whisper in its own process for it. Stopping the engine's process waits for nothing:
//! a worker is ended (its input closed, killed a second later) on a thread of its own.

use crate::whisper::{Abort, Local, PassParams, Segment};
use crate::{Gpu, Priority, STOPPED};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// The argument that makes a program a captions worker ([`serve_if_asked`]).
pub const FLAG: &str = "--idn-captions-worker";
/// The version of these messages, in the worker's hello.
pub const PROTOCOL: u64 = 1;
/// The longest frame either side takes (8 MiB).
pub const MAX_FRAME: usize = 8 << 20;
/// How long a started worker has to say hello.
pub const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// How long a pass is given to end after a stop (measuring) before its worker is killed.
const STOP_WITHIN: Duration = Duration::from_secs(1);
/// How long a worker whose input is closed has to end before it is killed (on a thread
/// of its own).
const END_WITHIN: Duration = Duration::from_secs(1);
/// A worker up this long before it ends starts the [`backoff`] afresh.
const BACKOFF_RESET: Duration = Duration::from_secs(600);
/// The longest one wait on the caller's thread between looks at the worker.
const POLL: Duration = Duration::from_millis(100);

/// A frame read: its JSON header and its payload.
type Frame = (Value, Vec<u8>);

/// The program's name: `idn`, `idn.exe` on Windows.
fn idn_name() -> &'static str {
    if cfg!(windows) { "idn.exe" } else { "idn" }
}

/// One frame: the header's JSON on a line, then `payload`.
pub(crate) fn encode(header: &Value, payload: &[u8]) -> Vec<u8> {
    let mut h = serde_json::to_vec(header).unwrap_or_else(|_| b"{}".to_vec());
    h.push(b'\n');
    let len = (h.len() + payload.len()) as u32;
    let mut out = Vec::with_capacity(4 + h.len() + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    out
}

/// The next frame from `r`: its header and payload; `None` when the input has ended.
/// One longer than `max`, or without a JSON object for its header, is an error.
pub(crate) fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    let bad = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
    if len > max {
        return Err(bad(format!("a frame of {len} bytes (at most {max})")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let nl = body
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| bad("a frame without its header's line end".into()))?;
    let payload = body.split_off(nl + 1);
    let header: Value =
        serde_json::from_slice(&body[..nl]).map_err(|e| bad(format!("a frame's header: {e}")))?;
    if !header.is_object() {
        return Err(bad("a frame's header isn't a JSON object".into()));
    }
    Ok(Some((header, payload)))
}

/// Sound as a frame's payload (`f32` little-endian).
pub(crate) fn audio_bytes(audio: &[f32]) -> Vec<u8> {
    audio.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A frame's payload as sound.
pub(crate) fn audio_from(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if !bytes.len().is_multiple_of(4) {
        return Err(format!("sound of {} bytes (not whole f32s)", bytes.len()));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect())
}

/// A number of a JSON answer as `f32` (JSON has no NaN: serde_json writes it as `null`,
/// read back as NaN so the engine judges it as before).
fn f32_of(v: &Value) -> f32 {
    v.as_f64().map_or(f32::NAN, |x| x as f32)
}

fn segment_json(s: &Segment) -> Value {
    json!({"text": s.text, "no_speech": s.no_speech, "probs": s.probs})
}

fn segment_of(v: &Value) -> Result<Segment, String> {
    Ok(Segment {
        text: v["text"]
            .as_str()
            .ok_or("a segment without its text")?
            .to_owned(),
        no_speech: f32_of(&v["no_speech"]),
        probs: v["probs"]
            .as_array()
            .map(|a| a.iter().map(f32_of).collect())
            .unwrap_or_default(),
    })
}

fn gpu_json(g: Option<&Gpu>) -> Value {
    g.map_or(Value::Null, |g| {
        json!({"index": g.index, "name": g.name, "discrete": g.discrete, "memory_mb": g.memory_mb})
    })
}

fn gpu_of(v: &Value) -> Option<Gpu> {
    Some(Gpu {
        index: v["index"].as_i64()? as i32,
        name: v["name"].as_str()?.to_owned(),
        discrete: v["discrete"].as_bool()?,
        memory_mb: v["memory_mb"].as_u64()?,
    })
}

/// A pass's request (its `id` set when sent).
fn pass_header(p: &PassParams) -> Value {
    let mut h = json!({
        "op": "pass",
        "threads": p.threads,
        "lang": p.lang,
        "prompt": p.prompt,
    });
    if let Some(n) = p.max_tokens {
        h["max_tokens"] = json!(n);
    }
    if let Some(t) = p.temperature_inc {
        h["temperature_inc"] = json!(t);
    }
    h
}

fn pass_params_of(h: &Value) -> PassParams {
    PassParams {
        threads: h["threads"].as_u64().unwrap_or(0) as usize,
        lang: h["lang"].as_str().unwrap_or("en").to_owned(),
        prompt: h["prompt"].as_str().unwrap_or_default().to_owned(),
        max_tokens: h["max_tokens"].as_i64().map(|n| n as i32),
        temperature_inc: h["temperature_inc"].as_f64().map(|x| x as f32),
    }
}

/// The worker's first frame.
fn hello() -> Value {
    json!({
        "op": "hello",
        "program": "idn-captions",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
    })
}

/// Whether `h` is a captions worker's hello in this protocol.
fn check_hello(h: &Value) -> Result<(), String> {
    if h["op"] != "hello" {
        return Err("not IDN's captions worker (it didn't say IDN's hello)".to_owned());
    }
    match h["protocol"].as_u64() {
        Some(PROTOCOL) => Ok(()),
        p => Err(format!(
            "a captions worker of protocol {p:?} ({}); this engine speaks {PROTOCOL}",
            h["version"].as_str().unwrap_or("?")
        )),
    }
}

// ---------------------------------------------------------------------------------------
// The worker's side.

/// Serves as a captions worker when this program was started as one (its first argument
/// [`FLAG`], by an [`crate::Engine`]), then exits the process once its input closes.
/// Otherwise returns at once. Call it first thing in `main`, before any thread is
/// started: on Linux the priority it sets is inherited only by threads started after.
///
/// `idn` calls it; an embedder whose own program should be the worker (an embedding application) calls it
/// in its `main` and sets [`crate::EngineConfig::worker`] to `std::env::current_exe()`.
pub fn serve_if_asked() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(FLAG)) {
        return;
    }
    let level = match args.next().and_then(|a| a.into_string().ok()).as_deref() {
        Some("lowest") => Priority::Lowest,
        _ => Priority::Low,
    };
    let Ok((input, mut output)) = protocol_pipes() else {
        std::process::exit(1)
    };
    // Hello first, at the priority it was started with (a busy machine would otherwise
    // hold up the greeting past the engine's patience). No thread exists yet.
    if output
        .write_all(&encode(&hello(), &[]))
        .and_then(|()| output.flush())
        .is_err()
    {
        std::process::exit(1)
    }
    // Then the priority: still before any thread exists.
    let priority = set_priority(level);
    serve(input, output, priority);
    std::process::exit(0)
}

/// This process's standard input and output for the frames, standard output then pointed
/// at standard error, so nothing whisper or a library prints can land among the frames.
fn protocol_pipes() -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        // SAFETY: plain descriptor calls on this process's own standard descriptors; the
        // new descriptor (close-on-exec, 3 or above) is owned by the `File` alone.
        let out = unsafe {
            let fd = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(2, 1) < 0 {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            std::fs::File::from_raw_fd(fd)
        };
        Ok((Box::new(io::stdin()), Box::new(out)))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        };
        // SAFETY: this process's standard handles. The original output handle is owned by
        // the `File` from here on: the standard output slot is given standard error's
        // handle, so Rust's and Windows' own writes to "standard output" go there. (The C
        // runtime's descriptor 1 keeps the original: whisper's and ggml's messages go to
        // their log hooks, not to it.)
        let out = unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                return Err(io::Error::other("no standard output"));
            }
            if SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE)) == 0 {
                return Err(io::Error::last_os_error());
            }
            std::fs::File::from_raw_handle(h as _)
        };
        Ok((Box::new(io::stdin()), Box::new(out)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok((Box::new(io::stdin()), Box::new(io::stdout())))
    }
}

/// What [`set_priority`] did: what this process runs at, or why it couldn't.
pub(crate) type PriorityGot = Result<String, String>;

/// This process at `level`, for itself and every thread it starts after: what it is now.
fn set_priority(level: Priority) -> PriorityGot {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, IDLE_PRIORITY_CLASS, SetPriorityClass,
        };
        let (class, name) = match level {
            Priority::Lowest => (IDLE_PRIORITY_CLASS, "idle priority class"),
            _ => (BELOW_NORMAL_PRIORITY_CLASS, "below normal priority class"),
        };
        // SAFETY: the current process's pseudo-handle and a documented class.
        if unsafe { SetPriorityClass(GetCurrentProcess(), class) } != 0 {
            Ok(name.to_owned())
        } else {
            Err(format!("{name}: {}", io::Error::last_os_error()))
        }
    }
    #[cfg(unix)]
    {
        let want: libc::c_int = match level {
            Priority::Lowest => 19,
            _ => 10,
        };
        // SAFETY: plain calls about this process (Linux: this thread, the only one yet;
        // threads started after inherit its nice value).
        unsafe {
            let now = libc::getpriority(libc::PRIO_PROCESS, 0);
            if now >= want {
                return Ok(format!("nice {now} (already as low)"));
            }
            if libc::setpriority(libc::PRIO_PROCESS, 0, want) == 0 {
                Ok(format!("nice {want}"))
            } else {
                Err(format!(
                    "nice {want}: {} (it stays at {now})",
                    io::Error::last_os_error()
                ))
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = level;
        Err("not on this system".to_owned())
    }
}

/// Answers frames from `input` on `output` until `input` ends. Passes run on the calling
/// thread; a thread of its own reads the input meanwhile (an abort comes during a pass).
pub(crate) fn serve(
    mut input: Box<dyn Read + Send>,
    mut output: Box<dyn Write + Send>,
    priority: PriorityGot,
) {
    // Passes up to this one are to stop (`abort`); all once the input has ended.
    let abort_upto = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel::<Frame>();
    {
        let abort_upto = abort_upto.clone();
        std::thread::spawn(move || {
            while let Ok(Some((h, payload))) = read_frame(&mut input, MAX_FRAME) {
                if h["op"] == "abort" {
                    abort_upto.fetch_max(h["id"].as_u64().unwrap_or(0), Ordering::Relaxed);
                } else if tx.send((h, payload)).is_err() {
                    break;
                }
            }
            // Ended (or a frame that can't be read: nothing after it can be): the pass
            // under way stops, and the loop below ends once it has answered what came.
            abort_upto.store(u64::MAX, Ordering::Relaxed);
        });
    }
    let mut local: Option<Local> = None;
    for (h, payload) in rx {
        let mut answer = match h["op"].as_str() {
            Some("load") => {
                // (A second load replaces the first: its memory let go first.)
                local = None;
                let path = PathBuf::from(h["path"].as_str().unwrap_or_default());
                match Local::load(&path, h["gpu"].as_bool().unwrap_or(true)) {
                    Ok(l) => {
                        let mut a = json!({"ok": true, "device": l.device});
                        match &priority {
                            Ok(p) => a["priority"] = json!(p),
                            Err(e) => a["priority_error"] = json!(e),
                        }
                        local = Some(l);
                        a
                    }
                    Err(e) => json!({"error": e}),
                }
            }
            Some("devices") => {
                let g = crate::device(h["gpu"].as_bool().unwrap_or(true));
                json!({"device": crate::device_name(g.as_ref()), "gpu": gpu_json(g.as_ref())})
            }
            Some("pass") => match (&mut local, audio_from(&payload)) {
                (None, _) => json!({"error": "no model loaded"}),
                (_, Err(e)) => json!({"error": e}),
                (Some(l), Ok(audio)) => {
                    let id = h["id"].as_u64().unwrap_or(u64::MAX);
                    let upto = abort_upto.clone();
                    let abort: Abort = Arc::new(move || upto.load(Ordering::Relaxed) >= id);
                    match l.pass(&pass_params_of(&h), &audio, Some(abort)) {
                        Ok(segs) => {
                            json!({"segments": segs.iter().map(segment_json).collect::<Vec<_>>()})
                        }
                        Err(e) => json!({"error": e}),
                    }
                }
            },
            other => json!({"error": format!("unknown op {other:?}")}),
        };
        answer["id"] = h["id"].clone();
        if output
            .write_all(&encode(&answer, &[]))
            .and_then(|()| output.flush())
            .is_err()
        {
            break;
        }
    }
}

// ---------------------------------------------------------------------------------------
// The engine's side: one worker process.

/// Where the worker program may be, in order: `given` ([`crate::EngineConfig::worker`])
/// alone, else the switch `IDN_CAPTIONS_WORKER` (`switch`) alone (`off`: none, whisper in
/// this process), else `idn` in `dir` (IDN's programs folder: next to this program), then
/// each `idn` on `path` (the `PATH` variable). The first that says hello is used. Why
/// there is none, else.
pub(crate) fn find_programs(
    given: Option<&Path>,
    switch: Option<String>,
    dir: Option<PathBuf>,
    path: Option<OsString>,
) -> Result<Vec<PathBuf>, String> {
    if let Some(p) = given {
        return Ok(vec![p.to_owned()]);
    }
    if let Some(v) = switch {
        if idn_common::switches::says_off(&v) {
            return Err("IDN_CAPTIONS_WORKER is off".to_owned());
        }
        return Ok(vec![PathBuf::from(v)]);
    }
    let name = idn_name();
    let mut found: Vec<PathBuf> = dir.map(|d| d.join(name)).into_iter().collect();
    if let Some(paths) = path {
        found.extend(std::env::split_paths(&paths).map(|d| d.join(name)));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in found {
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    if out.is_empty() {
        return Err(format!(
            "no worker program ({name} next to this program or on PATH; IDN_CAPTIONS_WORKER, or the engine's `worker`, names one)"
        ));
    }
    Ok(out)
}

/// [`find_programs`] with this process's own switch, folder and `PATH`.
pub(crate) fn find_programs_here(given: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    find_programs(
        given,
        idn_common::switches::value("IDN_CAPTIONS_WORKER"),
        idn_common::supervisor::programs_dir(),
        std::env::var_os("PATH"),
    )
}

/// What waiting for an answer came to, when not the answer.
#[derive(Debug, PartialEq)]
pub(crate) enum Fail {
    /// None within the time given: the request was aborted, the worker lives on.
    Slow,
    /// Stopped (measuring), and the pass didn't end within [`STOP_WITHIN`]: the worker was
    /// killed. Not a death: nothing went wrong with it.
    Stopped,
    /// It ended, sent a frame that can't be read, or its process is gone: stopped.
    Dead(String),
}

/// What one look at the worker's output found.
enum Got {
    Frame(Value),
    Nothing,
    Dead(String),
}

/// A worker process, from the engine's side.
pub(crate) struct Worker {
    /// Its process, until it is stopped (then reaped on a thread of its own).
    child: Option<Child>,
    /// To the thread writing its input (closed: the worker ends).
    tx: Option<mpsc::Sender<Vec<u8>>>,
    /// From the thread reading its output (in a mutex only so the engine stays `Sync`, as
    /// it was with whisper in-process; only ever reached through `&mut self`).
    rx: Mutex<mpsc::Receiver<io::Result<Frame>>>,
    /// When its output ended (set by the thread reading it): a worker killed ends it at
    /// once, so this is when it died, however late that is noticed.
    ended_at: Arc<OnceLock<Instant>>,
    next_id: u64,
    pub program: PathBuf,
}

impl Worker {
    /// Starts `program` as a worker at `level` (its hello not yet waited for).
    pub fn spawn(program: &Path, level: Priority) -> Result<Worker, String> {
        let mut cmd = Command::new(program);
        cmd.arg(FLAG)
            .arg(if level == Priority::Lowest {
                "lowest"
            } else {
                "low"
            })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window of its own (`idn.exe` is a console program; a host application may not be).
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("starting {}: {e}", program.display()))?;
        // (Windows: it ends with this process, a crash included. Unix: its input closes.)
        #[cfg(windows)]
        idn_common::supervisor::in_job(&child);
        let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
            reap(child, true);
            return Err("its pipes".to_owned());
        };
        let (tx, to_worker) = mpsc::channel::<Vec<u8>>();
        let (from_worker, rx) = mpsc::channel::<io::Result<Frame>>();
        let ended_at = Arc::new(OnceLock::new());
        let output_ended = ended_at.clone();
        let writer = std::thread::Builder::new()
            .name("captions-worker-in".into())
            .spawn(move || {
                for buf in to_worker {
                    if stdin.write_all(&buf).and_then(|()| stdin.flush()).is_err() {
                        break;
                    }
                }
            });
        let reader = match writer {
            Ok(_) => std::thread::Builder::new()
                .name("captions-worker-out".into())
                .spawn(move || {
                    loop {
                        match read_frame(&mut stdout, MAX_FRAME) {
                            Ok(Some(f)) => {
                                if from_worker.send(Ok(f)).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {
                                let _ = output_ended.set(Instant::now());
                                break;
                            }
                            Err(e) => {
                                let _ = output_ended.set(Instant::now());
                                let _ = from_worker.send(Err(e));
                                break;
                            }
                        }
                    }
                })
                .map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = reader {
            drop(tx);
            reap(child, true);
            return Err(format!("its threads: {e}"));
        }
        Ok(Worker {
            child: Some(child),
            tx: Some(tx),
            rx: Mutex::new(rx),
            ended_at,
            next_id: 1,
            program: program.to_owned(),
        })
    }

    /// When it died: when its output ended, else now (its death noticed another way).
    fn died_at(&self) -> Instant {
        self.ended_at.get().copied().unwrap_or_else(Instant::now)
    }

    /// Whether it has ended, without waiting: why, if so (it is stopped then). Frames
    /// waiting are discarded: called only with no request outstanding, they are answers
    /// given up on.
    fn ended(&mut self) -> Option<String> {
        loop {
            match self.next_frame(Duration::ZERO) {
                Got::Frame(_) => {}
                Got::Nothing => return None,
                Got::Dead(why) => {
                    self.kill();
                    return Some(why);
                }
            }
        }
    }

    /// One look at its output, waiting at most `wait`.
    fn next_frame(&mut self, wait: Duration) -> Got {
        let got = self
            .rx
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(wait);
        match got {
            Ok(Ok((h, _))) => Got::Frame(h),
            Ok(Err(e)) => Got::Dead(format!("a frame it sent: {e}")),
            Err(mpsc::RecvTimeoutError::Disconnected) => Got::Dead("it ended".to_owned()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match self.child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => Got::Dead(format!("it ended ({status})")),
                    Some(Err(e)) => Got::Dead(format!("its process: {e}")),
                    _ => Got::Nothing,
                }
            }
        }
    }

    /// Waits at most `within` for its hello.
    pub fn greet(&mut self, within: Duration) -> Result<(), String> {
        let end = Instant::now() + within;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.next_frame(left.min(POLL)) {
                Got::Frame(h) => return check_hello(&h),
                Got::Dead(why) => return Err(why),
                Got::Nothing if left.is_zero() => {
                    return Err(format!(
                        "no hello within {} s (not IDN's captions worker)",
                        within.as_secs_f32()
                    ));
                }
                Got::Nothing => {}
            }
        }
    }

    /// Its hello, if it has come, without waiting (`since`: when it was started).
    fn poll_hello(&mut self, since: Instant) -> Option<Result<(), String>> {
        match self.next_frame(Duration::ZERO) {
            Got::Frame(h) => Some(check_hello(&h)),
            Got::Dead(why) => Some(Err(why)),
            Got::Nothing if since.elapsed() >= HELLO_WITHIN => Some(Err(format!(
                "no hello within {} s (not IDN's captions worker)",
                HELLO_WITHIN.as_secs()
            ))),
            Got::Nothing => None,
        }
    }

    /// A frame to its input as it is.
    fn push(&mut self, header: &Value, payload: &[u8]) -> Result<(), String> {
        let frame = encode(header, payload);
        if frame.len() - 4 > MAX_FRAME {
            return Err(format!(
                "a frame of {} bytes for the worker (at most {MAX_FRAME})",
                frame.len() - 4
            ));
        }
        if self.tx.as_ref().is_some_and(|t| t.send(frame).is_ok()) {
            Ok(())
        } else {
            Err("its input closed".to_owned())
        }
    }

    /// Sends a request, its `id` set: the id.
    pub fn send(&mut self, mut header: Value, payload: &[u8]) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        header["id"] = json!(id);
        self.push(&header, payload)?;
        Ok(id)
    }

    fn abort(&mut self, id: u64) {
        let _ = self.push(&json!({"op": "abort", "id": id}), &[]);
    }

    /// The answer to request `id`, waiting at most `timeout` (answers to older requests,
    /// given up on, are discarded). Past it the request is aborted and the worker lives
    /// on ([`Fail::Slow`]). With `stop`: once it is set the request is aborted, and the
    /// worker killed if it hasn't answered [`STOP_WITHIN`] after ([`Fail::Stopped`]). A
    /// worker that ends is stopped ([`Fail::Dead`]). No single wait on the caller's thread
    /// is longer than [`POLL`] between these looks.
    pub fn wait(
        &mut self,
        id: u64,
        timeout: Duration,
        stop: Option<&AtomicBool>,
    ) -> Result<Value, Fail> {
        let end = Instant::now() + timeout;
        let mut aborted: Option<Instant> = None;
        loop {
            if aborted.is_none() && stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
                self.abort(id);
                aborted = Some(Instant::now());
            }
            if aborted.is_some_and(|t| t.elapsed() >= STOP_WITHIN) {
                self.kill();
                return Err(Fail::Stopped);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.abort(id);
                return Err(Fail::Slow);
            }
            match self.next_frame(left.min(POLL)) {
                Got::Frame(h) if h["id"].as_u64() == Some(id) => return Ok(h),
                // (An older request's answer, given up on: discarded.)
                Got::Frame(_) | Got::Nothing => {}
                Got::Dead(why) => {
                    self.kill();
                    return Err(Fail::Dead(why));
                }
            }
        }
    }

    /// The answer to request `id` if it has come, without waiting (older ones discarded);
    /// an error when the worker ended (it is stopped then).
    fn poll(&mut self, id: u64) -> Option<Result<Value, String>> {
        loop {
            match self.next_frame(Duration::ZERO) {
                Got::Frame(h) if h["id"].as_u64() == Some(id) => return Some(Ok(h)),
                Got::Frame(_) => {}
                Got::Nothing => return None,
                Got::Dead(why) => {
                    self.kill();
                    return Some(Err(why));
                }
            }
        }
    }

    /// Stopped now: killed, and reaped on a thread of its own (the caller waits for
    /// nothing).
    pub fn kill(&mut self) {
        self.tx = None;
        if let Some(c) = self.child.take() {
            reap(c, true);
        }
    }
}

impl Drop for Worker {
    /// Its input closed (it ends of itself, a pass under way stopping); a thread of its
    /// own kills it if it hasn't after [`END_WITHIN`], and reaps it.
    fn drop(&mut self) {
        self.tx = None;
        if let Some(c) = self.child.take() {
            reap(c, false);
        }
    }
}

/// A worker's process reaped on a thread of its own, never holding the caller: killed
/// at once (`now`), else given [`END_WITHIN`] to end (its input closed) first. This
/// worker starts no processes of its own, so killing it is enough (on Windows its job
/// ends it with this process too).
fn reap(mut child: Child, now: bool) {
    if now {
        let _ = child.kill();
    }
    // (A thread that can't be made: dropped unreaped; it ends of itself at its input's
    // end, or by the kill above.)
    let _ = std::thread::Builder::new()
        .name("captions-worker-end".into())
        .spawn(move || {
            let end = Instant::now() + END_WITHIN;
            while Instant::now() < end {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
}

/// The first of `programs` that starts and says hello, at `level`; else why each didn't.
pub(crate) fn connect(programs: &[PathBuf], level: Priority) -> Result<Worker, String> {
    let mut why = Vec::new();
    for p in programs {
        match Worker::spawn(p, level) {
            Ok(mut w) => match w.greet(HELLO_WITHIN) {
                Ok(()) => return Ok(w),
                Err(e) => {
                    w.kill();
                    why.push(format!("{}: {e}", p.display()));
                }
            },
            Err(e) => why.push(e),
        }
    }
    Err(why.join("; "))
}

/// The device a worker among `programs` would run whisper on with `gpu` (as
/// [`crate::device`] in this process): the GPU, if one, and its name. `Ok(None)` when no
/// worker starts.
pub(crate) fn probe_device(
    programs: &[PathBuf],
    level: Priority,
    gpu: bool,
    timeout: Duration,
) -> Result<Option<(Option<Gpu>, String)>, String> {
    let Ok(mut w) = connect(programs, level) else {
        return Ok(None);
    };
    let id = w.send(json!({"op": "devices", "gpu": gpu}), &[])?;
    match w.wait(id, timeout, None) {
        Ok(a) => Ok(Some((
            gpu_of(&a["gpu"]),
            a["device"].as_str().unwrap_or_default().to_owned(),
        ))),
        Err(Fail::Dead(why)) => Err(format!("the captions worker: {why}")),
        Err(_) => Err(format!(
            "the captions worker didn't say its device within {} s",
            timeout.as_secs()
        )),
    }
}

// ---------------------------------------------------------------------------------------
// The engine's side: its worker, kept running.

/// How long to wait before starting a worker again after `deaths` ends in a row: 1 s,
/// 5 s, 30 s, then every 60 s (none before the first).
pub(crate) fn backoff(deaths: u32) -> Duration {
    Duration::from_secs(match deaths {
        0 => 0,
        1 => 1,
        2 => 5,
        3 => 30,
        _ => 60,
    })
}

/// Why [`Remote::open`] gave no worker.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// No worker program, or none that starts and says hello: whisper may run in-process.
    NoWorker(String),
    /// A worker started but the model couldn't be loaded in it (refused, ended twice, or
    /// too slow): an error, never whisper in-process.
    Model(String),
}

enum State {
    Up {
        w: Worker,
        since: Instant,
    },
    /// Started, its hello awaited.
    Starting {
        w: Worker,
        since: Instant,
    },
    /// Its model being loaded (request `id`).
    Loading {
        w: Worker,
        id: u64,
    },
    /// None; the next start at `at`.
    Down {
        at: Instant,
    },
}

/// An engine's worker: started, its model loaded, started again after it ends.
pub(crate) struct Remote {
    state: State,
    program: PathBuf,
    level: Priority,
    /// The model's path (UTF-8: it goes as JSON).
    model: String,
    /// On the GPU (off after a worker ended loading it there).
    gpu: bool,
    timeout: Duration,
    /// Ends in a row ([`backoff`]).
    deaths: u32,
    /// Why there is no worker up now (a step's error meanwhile).
    why: String,
    /// What was logged last of its state (each change logged once).
    said: String,
    /// What whisper runs on, and the priority it got (normal when it couldn't be set).
    pub device: String,
    pub priority: Priority,
}

/// A load request.
fn load_header(model: &str, gpu: bool) -> Value {
    json!({"op": "load", "path": model, "gpu": gpu})
}

/// The device and priority from a load's answer.
fn loaded(a: &Value) -> (String, PriorityGot) {
    let device = a["device"].as_str().unwrap_or_default().to_owned();
    let priority = match (a["priority"].as_str(), a["priority_error"].as_str()) {
        (Some(p), _) => Ok(p.to_owned()),
        (None, Some(e)) => Err(e.to_owned()),
        (None, None) => Err("not said".to_owned()),
    };
    (device, priority)
}

impl Remote {
    /// A worker among `programs` (the first that says hello) with the model at `model`
    /// loaded, waiting at most `timeout` for that. One that ends loading it on the GPU is
    /// started again on the CPU.
    pub fn open(
        programs: &[PathBuf],
        level: Priority,
        model: &Path,
        gpu: bool,
        timeout: Duration,
    ) -> Result<Remote, OpenError> {
        let path = model
            .to_str()
            .ok_or_else(|| {
                OpenError::Model(format!(
                    "the model's path {} isn't UTF-8 (the captions worker takes it as text)",
                    model.display()
                ))
            })?
            .to_owned();
        let mut w = connect(programs, level).map_err(OpenError::NoWorker)?;
        let program = w.program.clone();
        let mut r = Remote {
            state: State::Down { at: Instant::now() },
            program,
            level,
            model: path,
            gpu,
            timeout,
            deaths: 0,
            why: String::new(),
            said: String::new(),
            device: String::new(),
            priority: Priority::Normal,
        };
        loop {
            let id = w
                .send(load_header(&r.model, r.gpu), &[])
                .map_err(|e| OpenError::Model(format!("the captions worker: {e}")))?;
            match w.wait(id, timeout, None) {
                Ok(a) => {
                    if let Some(e) = a["error"].as_str() {
                        return Err(OpenError::Model(e.to_owned()));
                    }
                    r.up(w, &a);
                    return Ok(r);
                }
                Err(Fail::Dead(why)) if r.gpu => {
                    r.gpu = false;
                    idn_open_protocol::log::log(&format!(
                        "captions: the worker ended loading the model on the GPU ({why}); trying the CPU"
                    ));
                    w = connect(std::slice::from_ref(&r.program), level)
                        .map_err(|e| OpenError::Model(format!("the captions worker: {e}")))?;
                }
                Err(Fail::Dead(why)) => {
                    return Err(OpenError::Model(format!(
                        "the captions worker ended loading the model: {why}"
                    )));
                }
                Err(_) => {
                    return Err(OpenError::Model(format!(
                        "the model didn't load in the captions worker within {} s",
                        timeout.as_secs()
                    )));
                }
            }
        }
    }

    /// `w` up with the model loaded (`a`: the load's answer); logged.
    fn up(&mut self, w: Worker, a: &Value) {
        let (device, priority) = loaded(a);
        let line = match &priority {
            Ok(p) => format!(
                "captions: whisper in a worker process ({}) at {} priority ({p}) on {device}",
                self.program.display(),
                self.level.name(),
            ),
            Err(e) => format!(
                "captions: whisper in a worker process ({}) on {device}, but its priority couldn't be set: {e}",
                self.program.display(),
            ),
        };
        self.priority = if priority.is_ok() {
            self.level
        } else {
            Priority::Normal
        };
        self.device = device;
        self.state = State::Up {
            w,
            since: Instant::now(),
        };
        self.why.clear();
        self.say("up", &line);
    }

    /// `line` logged when the state `key` differs from the last logged.
    fn say(&mut self, key: &str, line: &str) {
        if self.said != key {
            self.said = key.to_owned();
            idn_open_protocol::log::log(line);
        }
    }

    /// The worker ended (or couldn't be started) now: [`died_at`](Self::died_at).
    fn died(&mut self, why: &str, up_since: Option<Instant>) {
        self.died_at(why, up_since, Instant::now());
    }

    /// The worker ended (or couldn't be started) at `at`: the next start [`backoff`] after
    /// that, however late it was noticed; `up_since`: when the one that ended came up (up
    /// [`BACKOFF_RESET`] or longer: the count afresh).
    fn died_at(&mut self, why: &str, up_since: Option<Instant>, at: Instant) {
        if up_since.is_some_and(|t| at.saturating_duration_since(t) >= BACKOFF_RESET) {
            self.deaths = 0;
        }
        self.deaths = self.deaths.saturating_add(1);
        let wait = backoff(self.deaths);
        self.state = State::Down { at: at + wait };
        self.why = format!(
            "captions: the worker isn't running ({why}); whisper waits for it (started again every {} s at most)",
            backoff(u32::MAX).as_secs()
        );
        self.say(
            &format!("down: {why}"),
            &format!(
                "captions: the worker {}: {why}; starting it again (in {} s, then backing off to every {} s); whisper isn't run meanwhile",
                self.program.display(),
                wait.as_secs(),
                backoff(u32::MAX).as_secs()
            ),
        );
    }

    /// The worker looked after without waiting ([`ready`](Self::ready)): one that ended is
    /// noticed, and started again when due, between passes too.
    pub fn tend(&mut self) {
        let _ = self.ready();
    }

    /// Its worker, once up: the state moved on without waiting (an end looked for, started
    /// when due, its hello and its model's load looked for); why not, else.
    fn ready(&mut self) -> Result<&mut Worker, String> {
        let now = Instant::now();
        match std::mem::replace(&mut self.state, State::Down { at: now }) {
            State::Up { mut w, since } => match w.ended() {
                None => self.state = State::Up { w, since },
                Some(why) => {
                    let at = w.died_at();
                    self.died_at(&why, Some(since), at);
                }
            },
            State::Down { at } if now < at => self.state = State::Down { at },
            State::Down { .. } => match Worker::spawn(&self.program, self.level) {
                Ok(w) => {
                    self.say(
                        "starting",
                        &format!(
                            "captions: the worker {} is starting again",
                            self.program.display()
                        ),
                    );
                    self.state = State::Starting { w, since: now }
                }
                Err(why) => self.died(&why, None),
            },
            State::Starting { mut w, since } => match w.poll_hello(since) {
                None => self.state = State::Starting { w, since },
                Some(Err(why)) => {
                    let at = w.died_at();
                    w.kill();
                    self.died_at(&why, None, at);
                }
                Some(Ok(())) => match w.send(load_header(&self.model, self.gpu), &[]) {
                    Ok(id) => self.state = State::Loading { w, id },
                    Err(why) => {
                        w.kill();
                        self.died(&why, None);
                    }
                },
            },
            State::Loading { mut w, id } => match w.poll(id) {
                None => self.state = State::Loading { w, id },
                Some(Ok(a)) => match a["error"].as_str() {
                    None => self.up(w, &a),
                    Some(e) if self.gpu => {
                        w.kill();
                        self.gpu = false;
                        self.say(
                            "cpu",
                            &format!(
                                "captions: the worker couldn't load the model on the GPU ({e}); starting it on the CPU"
                            ),
                        );
                    }
                    Some(e) => {
                        w.kill();
                        self.died(&format!("it can't load the model: {e}"), None);
                    }
                },
                Some(Err(why)) if self.gpu => {
                    self.gpu = false;
                    self.say(
                        "cpu",
                        &format!(
                            "captions: the worker ended loading the model on the GPU ({why}); starting it on the CPU"
                        ),
                    );
                }
                Some(Err(why)) => {
                    let at = w.died_at();
                    self.died_at(&why, None, at);
                }
            },
        }
        match &mut self.state {
            State::Up { w, .. } => Ok(w),
            State::Down { .. } if !self.why.is_empty() => Err(self.why.clone()),
            _ => Err("captions: the worker is starting (whisper isn't running yet)".to_owned()),
        }
    }

    /// One pass in the worker (as [`Local::pass`]); with `stop`, ended when it is set (the
    /// error [`STOPPED`]). A pass past the timeout is skipped (an error) and the worker
    /// kept; one that ended is started again (meanwhile each pass is an error).
    pub fn pass(
        &mut self,
        p: &PassParams,
        audio: &[f32],
        stop: Option<&AtomicBool>,
    ) -> Result<Vec<Segment>, String> {
        let payload = audio_bytes(audio);
        if payload.len() + 4096 > MAX_FRAME {
            return Err(format!(
                "captions: {:.1} s of sound for one pass (at most 30 s)",
                audio.len() as f64 / crate::RATE as f64
            ));
        }
        let timeout = self.timeout;
        let got = {
            let w = self.ready()?;
            match w.send(pass_header(p), &payload) {
                Ok(id) => w.wait(id, timeout, stop),
                Err(why) => {
                    w.kill();
                    Err(Fail::Dead(why))
                }
            }
        };
        match got {
            Ok(a) => {
                if let Some(e) = a["error"].as_str() {
                    return Err(e.to_owned());
                }
                a["segments"]
                    .as_array()
                    .ok_or("captions: the worker's answer has no segments")?
                    .iter()
                    .map(segment_of)
                    .collect()
            }
            Err(Fail::Slow) => Err(format!(
                "captions: a pass took over {} s; skipped",
                timeout.as_secs()
            )),
            Err(Fail::Stopped) => {
                // (Killed for the stop, not ended: started again at once if wanted.)
                self.state = State::Down { at: Instant::now() };
                Err(STOPPED.to_owned())
            }
            Err(Fail::Dead(why)) => {
                let (since, at) = match &self.state {
                    State::Up { w, since } => (Some(*since), w.died_at()),
                    _ => (None, Instant::now()),
                };
                self.died_at(&why, since, at);
                Err(self.why.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Worker {
        /// A worker with no process: the test holds the other ends of its pipes (what it
        /// is sent; what it answers).
        fn fake() -> (
            Worker,
            mpsc::Receiver<Vec<u8>>,
            mpsc::Sender<io::Result<Frame>>,
        ) {
            let (tx, sent) = mpsc::channel();
            let (answers, rx) = mpsc::channel();
            let w = Worker {
                child: None,
                tx: Some(tx),
                rx: Mutex::new(rx),
                ended_at: Arc::new(OnceLock::new()),
                next_id: 1,
                program: PathBuf::from("fake"),
            };
            (w, sent, answers)
        }
    }

    fn header_of(frame: &[u8]) -> Value {
        read_frame(&mut io::Cursor::new(frame), MAX_FRAME)
            .unwrap()
            .unwrap()
            .0
    }

    #[test]
    fn frames_round_trip() {
        let audio = vec![0.0f32, 1.5, -0.25, f32::MIN_POSITIVE];
        let p = PassParams {
            threads: 3,
            lang: "es".into(),
            prompt: "Pastor Okafor\nEphesians".into(),
            max_tokens: Some(32),
            temperature_inc: Some(0.0),
        };
        let mut bytes = encode(&pass_header(&p), &audio_bytes(&audio));
        bytes.extend(encode(&json!({"op": "abort", "id": 7}), &[]));
        let mut r = io::Cursor::new(bytes);
        let (h, payload) = read_frame(&mut r, MAX_FRAME).unwrap().unwrap();
        assert_eq!(h["op"], "pass");
        assert_eq!(pass_params_of(&h), p);
        assert_eq!(audio_from(&payload).unwrap(), audio);
        let (h, payload) = read_frame(&mut r, MAX_FRAME).unwrap().unwrap();
        assert_eq!((h["op"].as_str(), payload.len()), (Some("abort"), 0));
        assert!(read_frame(&mut r, MAX_FRAME).unwrap().is_none(), "the end");
        // Measuring's settings only when given.
        let plain = PassParams {
            lang: "en".into(),
            ..Default::default()
        };
        let h = pass_header(&plain);
        assert!(h.get("max_tokens").is_none() && h.get("temperature_inc").is_none());
        assert_eq!(pass_params_of(&h), plain);
        // Segments, a NaN kept as NaN.
        let s = Segment {
            text: " Hello.".into(),
            no_speech: f32::NAN,
            probs: vec![0.5, 0.25],
        };
        let back = segment_of(&segment_json(&s)).unwrap();
        assert!(back.no_speech.is_nan());
        assert_eq!((back.text, back.probs), (s.text, s.probs));
        // A GPU, and none.
        let g = Gpu {
            index: 1,
            name: "Radeon 680M".into(),
            discrete: false,
            memory_mb: 512,
        };
        assert_eq!(gpu_of(&gpu_json(Some(&g))), Some(g));
        assert_eq!(gpu_of(&gpu_json(None)), None);
        // The hello: ours, another protocol's, and anything else.
        assert!(check_hello(&hello()).is_ok());
        let mut other = hello();
        other["protocol"] = json!(2);
        assert!(check_hello(&other).is_err());
        assert!(check_hello(&json!({"op": "pass"})).is_err());
    }

    #[test]
    fn bad_frames_are_refused() {
        let big = encode(&json!({"op": "pass"}), &[0u8; 64]);
        assert!(read_frame(&mut io::Cursor::new(big.clone()), 32).is_err());
        assert!(read_frame(&mut io::Cursor::new(big), 128).is_ok());
        // No header line; a header that isn't an object; cut short.
        let mut no_line = 3u32.to_le_bytes().to_vec();
        no_line.extend(b"{}x");
        assert!(read_frame(&mut io::Cursor::new(no_line), MAX_FRAME).is_err());
        let mut not_object = 3u32.to_le_bytes().to_vec();
        not_object.extend(b"[]\n");
        assert!(read_frame(&mut io::Cursor::new(not_object), MAX_FRAME).is_err());
        let mut short = 10u32.to_le_bytes().to_vec();
        short.extend(b"{}\n");
        assert!(read_frame(&mut io::Cursor::new(short), MAX_FRAME).is_err());
        assert!(audio_from(&[0u8; 5]).is_err());
    }

    /// The worker's loop without a model: a pass before a load, an unknown op, an abort
    /// (no answer); each answer carries its request's id; it ends with its input.
    #[test]
    fn the_worker_answers_without_a_model() {
        let mut pass = pass_header(&PassParams::default());
        pass["id"] = json!(1);
        let mut input = encode(&pass, &audio_bytes(&[0.0]));
        input.extend(encode(&json!({"op": "abort", "id": 1}), &[]));
        input.extend(encode(&json!({"op": "dance", "id": 2}), &[]));
        let out = Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        serve(
            Box::new(io::Cursor::new(input)),
            Box::new(Shared(out.clone())),
            Ok("test".into()),
        );
        let bytes = out.lock().unwrap().clone();
        let mut r = io::Cursor::new(bytes);
        let (a, _) = read_frame(&mut r, MAX_FRAME).unwrap().unwrap();
        assert_eq!(
            (a["error"].as_str(), a["id"].as_u64()),
            (Some("no model loaded"), Some(1))
        );
        let (a, _) = read_frame(&mut r, MAX_FRAME).unwrap().unwrap();
        assert!(a["error"].as_str().unwrap().contains("dance"), "{a}");
        assert_eq!(a["id"], 2);
        assert!(read_frame(&mut r, MAX_FRAME).unwrap().is_none());
    }

    /// A pass past its time is given up on (an abort sent for it) and the worker kept;
    /// its late answer is discarded while the next request's is awaited.
    #[test]
    fn a_slow_pass_is_skipped_and_stale_answers_discarded() {
        let (mut w, sent, answers) = Worker::fake();
        let id = w.send(pass_header(&PassParams::default()), &[]).unwrap();
        assert_eq!(
            w.wait(id, Duration::from_millis(150), None),
            Err(Fail::Slow)
        );
        assert!(w.tx.is_some(), "the worker is kept");
        assert_eq!(header_of(&sent.recv().unwrap())["id"], id);
        let abort = header_of(&sent.recv().unwrap());
        assert_eq!(
            (abort["op"].as_str(), abort["id"].as_u64()),
            (Some("abort"), Some(id))
        );
        // The next pass: the late answer (the old id) comes first and is discarded.
        let next = w.send(pass_header(&PassParams::default()), &[]).unwrap();
        assert_eq!(next, id + 1);
        answers
            .send(Ok((json!({"id": id, "error": STOPPED}), Vec::new())))
            .unwrap();
        answers
            .send(Ok((json!({"id": next, "segments": []}), Vec::new())))
            .unwrap();
        let a = w.wait(next, Duration::from_secs(5), None).unwrap();
        assert_eq!(a["id"], next);
        assert!(a["segments"].is_array());
        // Without waiting: nothing yet, then the answer, an old one skipped.
        let third = w.send(pass_header(&PassParams::default()), &[]).unwrap();
        assert!(w.poll(third).is_none());
        answers
            .send(Ok((json!({"id": next, "segments": []}), Vec::new())))
            .unwrap();
        answers
            .send(Ok((json!({"id": third, "segments": []}), Vec::new())))
            .unwrap();
        assert_eq!(w.poll(third).unwrap().unwrap()["id"], third);
        // Its output ending is a death.
        drop(answers);
        let fourth = w.send(pass_header(&PassParams::default()), &[]).unwrap();
        assert!(matches!(
            w.wait(fourth, Duration::from_secs(5), None),
            Err(Fail::Dead(_))
        ));
    }

    /// A stop while measuring: an abort at once, then the worker killed a second later
    /// if no answer came ([`Fail::Stopped`], not a death).
    #[test]
    fn a_stop_aborts_then_kills() {
        let (mut w, sent, _answers) = Worker::fake();
        let stop = AtomicBool::new(true);
        let id = w.send(pass_header(&PassParams::default()), &[]).unwrap();
        let started = Instant::now();
        assert_eq!(
            w.wait(id, Duration::from_secs(60), Some(&stop)),
            Err(Fail::Stopped)
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        let _pass = sent.recv().unwrap();
        assert_eq!(header_of(&sent.recv().unwrap())["op"], "abort");
        assert!(w.tx.is_none(), "killed");
    }

    #[test]
    fn restarts_back_off() {
        let secs: Vec<u64> = (0..7).map(|d| backoff(d).as_secs()).collect();
        assert_eq!(secs, [0, 1, 5, 30, 60, 60, 60]);
        assert_eq!(backoff(u32::MAX), Duration::from_secs(60));
    }

    /// A worker that ends while nothing is asked of it is noticed by a look alone (no
    /// pass), and its next start is due on the schedule counted from its death, not from
    /// when that was noticed: 1 s, then 5 s, 30 s, 60 s. Starts that fail count as deaths.
    #[test]
    fn a_worker_that_ends_between_passes_is_started_again_on_time() {
        let (w, _sent, answers) = Worker::fake();
        let since = Instant::now();
        let mut r = Remote {
            state: State::Up { w, since },
            program: PathBuf::from("/nowhere/idn-captions-worker-missing"),
            level: Priority::Low,
            model: "/nowhere/ggml-base.en.bin".into(),
            gpu: false,
            timeout: Duration::from_secs(60),
            deaths: 0,
            why: String::new(),
            said: "up".into(),
            device: String::new(),
            priority: Priority::Low,
        };
        r.tend();
        assert!(matches!(r.state, State::Up { .. }), "alive: kept");
        // It died 3 s before it was looked at.
        let died = Instant::now() - Duration::from_secs(3);
        if let State::Up { w, .. } = &r.state {
            w.ended_at.set(died).unwrap();
        }
        drop(answers);
        r.tend();
        match r.state {
            State::Down { at } => assert_eq!(at, died + Duration::from_secs(1)),
            _ => panic!("not noticed"),
        }
        assert_eq!(r.deaths, 1);
        assert!(r.why.contains("isn't running"), "{}", r.why);
        // Due: started (this program can't be: a death), each next start further off.
        for (deaths, wait) in [(2, 5), (3, 30), (4, 60), (5, 60)] {
            let before = Instant::now();
            r.tend();
            assert_eq!(r.deaths, deaths);
            let State::Down { at } = r.state else {
                panic!("down");
            };
            let left = at.saturating_duration_since(before);
            // (Counted from the failed start, a moment after `before`.)
            assert!(
                left <= Duration::from_secs(wait) + Duration::from_millis(200)
                    && left + Duration::from_secs(2) > Duration::from_secs(wait),
                "{deaths}: {left:?}"
            );
            // Not before it is due.
            r.tend();
            assert_eq!(r.deaths, deaths, "not started early");
            r.state = State::Down { at: Instant::now() };
        }
    }

    /// The worker programs: the one given alone, else the switch alone (off: none), else
    /// `idn` next to this program, then each on PATH.
    #[test]
    fn the_worker_programs_are_found_in_order() {
        let here = idn_common::testing::TempDir::new("captions-worker-here");
        let on_path = idn_common::testing::TempDir::new("captions-worker-path");
        let empty = idn_common::testing::TempDir::new("captions-worker-empty");
        std::fs::write(here.join(idn_name()), b"").unwrap();
        std::fs::write(on_path.join(idn_name()), b"").unwrap();
        let path = std::env::join_paths([empty.path(), on_path.path(), here.path()]).unwrap();
        let given = Path::new("/opt/hostapp/hostapp");
        let find = |given: Option<&Path>, switch: Option<&str>, dir: Option<&Path>| {
            find_programs(
                given,
                switch.map(str::to_owned),
                dir.map(Path::to_path_buf),
                Some(path.clone()),
            )
        };
        assert_eq!(
            find(Some(given), Some("/x/idn"), Some(here.path())).unwrap(),
            [given]
        );
        assert_eq!(
            find(None, Some("/x/idn"), Some(here.path())).unwrap(),
            [PathBuf::from("/x/idn")]
        );
        assert!(find(None, Some("off"), Some(here.path())).is_err());
        assert_eq!(
            find(None, None, Some(here.path())).unwrap(),
            [here.join(idn_name()), on_path.join(idn_name())],
            "next to it first, then PATH, once each"
        );
        assert_eq!(
            find(None, None, Some(empty.path())).unwrap(),
            [on_path.join(idn_name()), here.join(idn_name())]
        );
        assert!(
            find_programs(None, None, Some(empty.path().to_owned()), None).is_err(),
            "none anywhere"
        );
    }

    /// A program that isn't there, or doesn't say IDN's hello (one that ends at once, one
    /// that says nothing), isn't used: the next in order is tried.
    #[test]
    fn programs_without_the_hello_are_skipped() {
        let missing = PathBuf::from("/nowhere/idn-captions-worker-missing");
        let e = connect(std::slice::from_ref(&missing), Priority::Low)
            .err()
            .unwrap();
        assert!(e.contains("starting"), "{e}");
        #[cfg(unix)]
        {
            let first = |names: &[&str]| names.iter().map(PathBuf::from).find(|p| p.exists());
            if let (Some(t), Some(cat)) = (
                first(&["/bin/true", "/usr/bin/true"]),
                first(&["/bin/cat", "/usr/bin/cat"]),
            ) {
                let started = Instant::now();
                let e = connect(&[missing, t, cat], Priority::Low).err().unwrap();
                assert_eq!(e.matches("; ").count(), 2, "each tried: {e}");
                // (GNU cat ends at once on the unknown option; another cat would say
                // nothing until the hello's wait is over: skipped either way.)
                assert!(
                    e.contains("ended") || e.contains("no hello"),
                    "cat isn't a worker: {e}"
                );
                assert!(started.elapsed() < HELLO_WITHIN + Duration::from_secs(3));
            }
        }
    }

    /// The real worker (`idn`, named by `IDN_CAPTIONS_WORKER`; skipped when it isn't set):
    /// it says hello, names its device, and refuses a model it can't load.
    #[test]
    fn the_idn_program_serves_as_a_worker() {
        let Some(program) = idn_common::switches::value("IDN_CAPTIONS_WORKER")
            .filter(|v| !idn_common::switches::says_off(v))
        else {
            eprintln!("skipped: IDN_CAPTIONS_WORKER (the built idn) not set");
            return;
        };
        let programs = [PathBuf::from(program)];
        let (_, device) = probe_device(&programs, Priority::Low, false, Duration::from_secs(60))
            .unwrap()
            .expect("it says hello");
        assert!(device.starts_with("CPU"), "{device}");
        let e = Remote::open(
            &programs,
            Priority::Low,
            Path::new("/nowhere/ggml-base.en.bin"),
            false,
            Duration::from_secs(60),
        )
        .err()
        .unwrap();
        assert!(
            matches!(&e, OpenError::Model(w) if w.contains("the model")),
            "{e:?}"
        );
    }
}
