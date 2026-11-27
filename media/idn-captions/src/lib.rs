//! Live captions on this machine (G2, spec/extensions/idn-captions.md): speech from an
//! IDN sound source turned into text by whisper.cpp (MIT, through whisper-rs), sent as
//! caption lines on a text-only IDN source (`text/1`, idn-extensions.md §E.22.3) — a
//! draft at about every second, corrected in place, final at a pause. Nothing leaves the
//! machine but the text.
//!
//! # Priority: whisper in a worker process
//!
//! Caption passes must never take CPU from a program's frames. whisper.cpp starts its own
//! threads on every pass; on Windows a new thread doesn't inherit its creator's thread
//! priority and ggml leaves them at NORMAL, so lowering the calling thread covered only its
//! own share. By default ([`EngineConfig::priority`] = [`Priority::Low`]) whisper runs in a worker
//! process of its own below normal priority (Windows: the below-normal priority class;
//! Unix: nice 10); [`Priority::Lowest`] puts it at the lowest (the idle class; nice 19),
//! [`Priority::Normal`] in this process as before. Only whisper's work moves there (the
//! model, its device, the passes); everything else of the [`Engine`] stays here.
//! [`Engine::in_worker`], [`Engine::priority`] and [`Engine::device`] say where and how
//! it runs. The pipe protocol is in [`worker`]'s documentation.
//!
//! The worker is a program run as `PROGRAM --idn-captions-worker low|lowest` that calls
//! [`worker::serve_if_asked`] first thing in its `main`; `idn` does. Found as
//! [`EngineConfig::worker`], else the switch `IDN_CAPTIONS_WORKER` (a path; `off`: none),
//! else `idn` (`idn.exe`) in IDN's programs folder (next to this program), else `idn` on
//! `PATH`. An embedder can be its own worker instead:
//!
//! ```no_run
//! // First thing in `main`: before any thread starts.
//! idn_captions::worker::serve_if_asked();
//! // … the program as usual; for its captions:
//! let _cfg = idn_captions::EngineConfig {
//!     worker: std::env::current_exe().ok(),
//!     ..Default::default()
//! };
//! ```
//!
//! whisper runs in this process only when no worker program is found, or none found starts
//! and says IDN's hello within [`worker::HELLO_WITHIN`] (so GNU libidn's `idn` on `PATH`
//! is passed over); the log says so once a process, with why ("captions: whisper runs in
//! this process at normal priority: …"). A worker that is slow, starved or ended never
//! brings whisper back here:
//!
//! - a pass not answered within [`EngineConfig::worker_timeout`] is skipped (that step's
//!   error "captions: a pass took over N s; skipped") and the worker kept;
//! - a worker that ends (while loading the model or later) is started again after 1 s,
//!   5 s, 30 s, then every 60 s (afresh once one has run 10 minutes), on the CPU after one
//!   that ended loading the model on the GPU; meanwhile each step is an error saying so,
//!   and each change is logged once;
//! - a model the worker can't load is [`Engine::new`]'s error.
//!
//! Dropping the engine waits for nothing: the worker's input is closed (it ends of
//! itself) and a thread of its own kills it a second later if not.
//!
//! **"auto" measures in the worker**, at its priority, under whatever else the machine is
//! doing at the time ([`auto_choose`]): a busy box picks a smaller model. The choice is kept
//! in `auto.txt` (English) or `auto-multilingual.txt` in the models folder, keyed on the
//! device's name (asked of the worker); deleting the file measures again.
//!
//! **On Linux, nice ranks threads only within their cgroup.** Under systemd an embedding
//! application's unit (or session scope) and the IDN Node's unit (which runs `idn
//! captions`) compete by their `CPUWeight`, whatever the worker's nice value: a worker
//! started by the application itself ranks below the application's frames, one started
//! by the Node only below the Node's own work.

pub mod program;
mod whisper;
pub mod worker;

use idn_open_protocol::text::CaptionLine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use whisper::{Local, PassParams, Segment};

/// whisper.cpp's sample rate.
pub const RATE: u32 = 16_000;
/// Where the models come from: whisper.cpp's own, on Hugging Face, at a fixed commit
/// (its last, 2024-10-29) so the files can't change under us; each is checked against
/// [`MODEL_PINS`] too (bug round E, E4-11). New pins: `scripts/dev/model-hashes.sh`.
pub const MODELS: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/";
/// Each model's exact size and SHA-256 (Hugging Face's own record of the file at that
/// commit, looked up 2026-11-10).
pub const MODEL_PINS: [(&str, u64, &str); 9] = [
    (
        "tiny.en",
        77_704_715,
        "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
    ),
    (
        "base.en",
        147_964_211,
        "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    ),
    (
        "small.en",
        487_614_201,
        "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
    ),
    (
        "medium.en",
        1_533_774_781,
        "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
    ),
    (
        "large-v3-turbo",
        1_624_555_275,
        "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    ),
    // Every language (Spanish or Korean services on a CPU-only box had
    // nothing that kept up), at the same commit.
    (
        "tiny",
        77_691_713,
        "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    ),
    (
        "base",
        147_951_465,
        "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    ),
    (
        "small",
        487_601_967,
        "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    ),
    (
        "medium",
        1_533_763_059,
        "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    ),
];
/// The models offered: name, size in MB (about), what it is for. Those ending `.en` hear
/// English only (and do it a little better than the others of their size); the rest hear
/// every language ([`model_speaks`]).
pub const MODEL_LIST: [(&str, u32, &str); 9] = [
    ("tiny.en", 75, "English: fastest, least accurate"),
    ("base.en", 142, "English: a small machine"),
    ("small.en", 466, "English: a good CPU (8 threads or more)"),
    ("medium.en", 1500, "English: a GPU, or a very fast CPU"),
    (
        "large-v3-turbo",
        1600,
        "every language: a GPU (RTX 3060 or newer), the most accurate",
    ),
    ("tiny", 75, "every language: fastest, least accurate"),
    ("base", 142, "every language: a small machine"),
    (
        "small",
        466,
        "every language: a good CPU (8 threads or more)",
    ),
    ("medium", 1500, "every language: a GPU, or a very fast CPU"),
];

/// Common languages of services, for a host's picker: whisper's code
/// ([`EngineConfig::lang`]) and the language's name in English. Whisper hears about 100;
/// any of its codes works.
pub const LANGUAGES: [(&str, &str); 24] = [
    ("en", "English"),
    ("es", "Spanish"),
    ("pt", "Portuguese"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
    ("ja", "Japanese"),
    ("vi", "Vietnamese"),
    ("tl", "Tagalog"),
    ("id", "Indonesian"),
    ("hi", "Hindi"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("pl", "Polish"),
    ("ro", "Romanian"),
    ("ar", "Arabic"),
    ("fa", "Persian"),
    ("sw", "Swahili"),
    ("af", "Afrikaans"),
    ("ht", "Haitian Creole"),
    ("am", "Amharic"),
];

/// Whether model `name` hears language `lang` (whisper's code): an English-only model
/// (`.en`) hears only `en`; the others every language.
pub fn model_speaks(name: &str, lang: &str) -> bool {
    !name.ends_with(".en") || lang.trim().eq_ignore_ascii_case("en")
}

/// A GPU whisper can use (through Vulkan: the `gpu` feature).
#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    /// Its place among the GPUs whisper counts (its `gpu_device`).
    pub index: i32,
    pub name: String,
    /// A card of its own (not built into the processor).
    pub discrete: bool,
    /// Its memory, MB (a built-in GPU's is the machine's, shared).
    pub memory_mb: u64,
}

/// The GPUs on this machine whisper can use, best first: a card of its own, then the most
/// memory. None without the `gpu` feature or a GPU driver with Vulkan.
pub fn gpus() -> Vec<Gpu> {
    use whisper_rs_sys::*;
    // (ggml's messages as it finds its devices to its log hooks, not standard error.)
    whisper_rs::install_logging_hooks();
    let mut out = Vec::new();
    // SAFETY: queries of ggml's device list; the description is a C string it owns.
    unsafe {
        for i in 0..ggml_backend_dev_count() {
            let d = ggml_backend_dev_get(i);
            let t = ggml_backend_dev_type(d);
            if t != ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                && t != ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
            {
                continue;
            }
            let (mut free, mut total) = (0usize, 0usize);
            ggml_backend_dev_memory(d, &mut free, &mut total);
            let name = ggml_backend_dev_description(d);
            out.push(Gpu {
                index: out.len() as i32,
                name: if name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(name)
                        .to_string_lossy()
                        .into_owned()
                },
                discrete: t == ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU,
                memory_mb: (total / 1_048_576) as u64,
            });
        }
    }
    out.sort_by_key(|g| (!g.discrete, std::cmp::Reverse(g.memory_mb)));
    out
}

/// What whisper runs on ([`EngineConfig::gpu`]): the best GPU, or none (the CPU).
fn device(gpu: bool) -> Option<Gpu> {
    gpu.then(|| gpus().into_iter().next()).flatten()
}

/// Its name in logs and in `auto.txt`.
fn device_name(gpu: Option<&Gpu>) -> String {
    gpu.map_or_else(
        || {
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            format!("CPU ({threads} threads)")
        },
        |g| g.name.clone(),
    )
}

/// The models "auto" tries, largest first: from large-v3-turbo on a card of its own with
/// 4 GB or more, from small(.en) on another GPU or a CPU of 8 threads or more, else from
/// base(.en); down to tiny(.en). The `.en` ones for English, the others for any other
/// language.
fn auto_candidates(gpu: Option<&Gpu>, lang: &str) -> &'static [&'static str] {
    const ENGLISH: [&str; 4] = ["large-v3-turbo", "small.en", "base.en", "tiny.en"];
    const ANY: [&str; 4] = ["large-v3-turbo", "small", "base", "tiny"];
    let order: &'static [&'static str] = if model_speaks("x.en", lang) {
        &ENGLISH
    } else {
        &ANY
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    match gpu {
        Some(g) if g.discrete && g.memory_mb >= 4000 => order,
        Some(_) => &order[1..],
        None if threads >= 8 => &order[1..],
        None => &order[2..],
    }
}

/// The longest one pass of whisper may take for a model to keep up: a pass runs for each
/// step while a line is said, so drafts come about this often.
pub const KEEPS_UP: Duration = Duration::from_millis(2500);

/// The model "auto" means on this machine, measured on it: the largest of
/// [`auto_candidates`] whose pass takes [`KEEPS_UP`] or less on the device `cfg` gives
/// (the best GPU first), downloaded into `dir` as tried. Kept in `dir/auto.txt` (English)
/// or `dir/auto-multilingual.txt` (other languages) with the device's name, so it is
/// measured once per device (deleting the file measures again); `log` hears what happens.
/// Measured where whisper will run: in the worker at its priority (the device asked of
/// it), under the machine's load at the time, so a busy machine chooses a smaller model.
pub fn auto_choose(
    dir: &Path,
    cfg: &EngineConfig,
    log: &mut dyn FnMut(&str),
) -> Result<String, String> {
    auto_choose_with(dir, cfg, log, &mut |_, _, _| {})
}

/// [`auto_choose`], telling each model's download as numbers too: `progress` gets the
/// model's name, the bytes so far and its size (when known), as [`ensure_model`]'s
/// callback does (an integrator's ask on Beta8: only log text had it).
pub fn auto_choose_with(
    dir: &Path,
    cfg: &EngineConfig,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
) -> Result<String, String> {
    auto_choose_stop(dir, cfg, log, progress, &Arc::new(AtomicBool::new(false)))
}

/// [`auto_choose_with`], ended when `stop` is set: within about a second (a download or a
/// measuring pass under way included), with the error [`STOPPED`]; nothing is kept as the
/// choice, and no model file is left half-written.
pub fn auto_choose_stop(
    dir: &Path,
    cfg: &EngineConfig,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
    stop: &Arc<AtomicBool>,
) -> Result<String, String> {
    let (gpu, name) = choose_device(cfg)?;
    // One choice kept for English and one for the other languages (two services on one
    // box in two languages measured over each other's).
    let kept = dir.join(if model_speaks("x.en", &cfg.lang) {
        "auto.txt"
    } else {
        "auto-multilingual.txt"
    });
    if let Ok(t) = std::fs::read_to_string(&kept) {
        let field = |k: &str| {
            t.lines()
                .find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_owned()))
        };
        if field("device ").as_deref() == Some(name.as_str())
            && let Some(m) = field("model ")
            // (One for its kind of language: an English choice for English, else a
            // multilingual one; Spanish's `small` was kept when switched to English; bug
            // run 2026-11-25.)
            && auto_candidates(gpu.as_ref(), &cfg.lang).contains(&m.as_str())
            && model_file(&m).is_some_and(|f| dir.join(f).exists())
        {
            return Ok(m);
        }
    }
    let candidates = auto_candidates(gpu.as_ref(), &cfg.lang);
    let (mut chosen, mut failed) = (None, String::from("no model to try"));
    for (i, model) in candidates.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Err(STOPPED.into());
        }
        log(&format!("measuring {model} on {name}"));
        let mut tenth = 0;
        // One that won't download or load (too little GPU memory): the next one down.
        let pass = ensure_model_stop(
            dir,
            model,
            &mut |n, total| {
                progress(model, n, total);
                if let Some(t) = total.filter(|t| *t > 0)
                    && n * 10 / t != tenth
                {
                    tenth = n * 10 / t;
                    log(&format!("downloading the {model} model: {}%", tenth * 10));
                }
            },
            stop,
        )
        .and_then(|path| Engine::new(&path, cfg.clone()))
        // (Loading a model takes a moment: a stop meanwhile ends it here.)
        .and_then(|e| {
            if stop.load(Ordering::Relaxed) {
                Err(STOPPED.to_owned())
            } else {
                Ok(e)
            }
        })
        .and_then(|mut e| e.measure_stop(stop));
        let pass = match pass {
            Ok(p) => p,
            // (Stopped: no next model, and nothing kept as the choice.)
            Err(e) if e == STOPPED => return Err(e),
            Err(e) => {
                log(&format!("{model}: {e}"));
                failed = e;
                continue;
            }
        };
        log(&format!(
            "{model} on {name}: {} ms a pass",
            pass.as_millis()
        ));
        if pass <= KEEPS_UP || i + 1 == candidates.len() {
            chosen = Some((*model, pass));
            break;
        }
    }
    let (model, pass) = chosen.ok_or(failed)?;
    let _ = idn_common::fsutil::write(
        &kept,
        format!(
            "device {name}\nmodel {model}\npass_ms {}\n",
            pass.as_millis()
        ),
    );
    Ok(model.to_owned())
}

/// A model's file name.
pub fn model_file(name: &str) -> Option<String> {
    MODEL_LIST
        .iter()
        .any(|(n, _, _)| *n == name)
        .then(|| format!("ggml-{name}.bin"))
}

/// Model `name` in `dir`, downloaded first if it isn't there (`progress`: bytes, size).
pub fn ensure_model(
    dir: &Path,
    name: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    ensure_model_stop(dir, name, progress, &Arc::new(AtomicBool::new(false)))
}

/// What a call ended by its stop flag returns as its error ([`ensure_model_stop`],
/// [`auto_choose_stop`]).
pub const STOPPED: &str = "stopped";

/// [`ensure_model`], ended when `stop` is set (an integrator's ask on v0.3.4-Beta10: the operator
/// switched captions off or chose another model): within about a second, with the error
/// [`STOPPED`]. A download is written to a part file of its own and renamed only whole and
/// checked, so nothing is left under the model's name; a later call starts it again. Two
/// calls for the same model in the same folder at once are safe: each downloads to its own
/// part file, and the second rename puts the same checked file in place.
pub fn ensure_model_stop(
    dir: &Path,
    name: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
    stop: &Arc<AtomicBool>,
) -> Result<PathBuf, String> {
    let file = model_file(name).ok_or(format!(
        "model {name:?}: one of {}",
        MODEL_LIST.map(|m| m.0).join(", ")
    ))?;
    let pin = model_pin(name).ok_or(format!("model {name:?}: no pinned hash"))?;
    let path = dir.join(&file);
    if path.exists() {
        match already_checked(&path, pin, stop) {
            Ok(true) => return Ok(path),
            Ok(false) => {
                // Not the pinned file (an older or changed download): set aside, said, and
                // downloaded again. Only the latest kept (a model can be 1.6 GB).
                if let Ok(rd) = std::fs::read_dir(dir) {
                    let old = format!("{file}.bad-");
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().starts_with(&old) {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
                let bad = path.with_extension(format!("bin.bad-{}", idn_common::epoch::secs()));
                let _ = std::fs::rename(&path, &bad);
                idn_open_protocol::log::log(&format!(
                    "idn-captions: {file} isn't the pinned model (size or SHA-256): set aside as {}; downloading it again",
                    bad.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Err(STOPPED.into()),
            Err(e) => return Err(format!("checking {file}: {e}")),
        }
    }
    idn_open_protocol::web::httpc::download_stoppable(
        &format!("{MODELS}{file}"),
        &path,
        Duration::from_secs(60),
        Some(pin),
        progress,
        Some(stop),
    )
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::Interrupted {
            STOPPED.to_owned()
        } else {
            format!("downloading {file}: {e}")
        }
    })?;
    let _ = mark_checked(&path, pin);
    Ok(path)
}

/// Model `name`'s pinned size and SHA-256.
pub fn model_pin(name: &str) -> Option<idn_open_protocol::web::httpc::Expected> {
    let (_, size, hex) = MODEL_PINS.iter().find(|(n, _, _)| *n == name)?;
    let sha256: [u8; 32] = idn_common::hex::unhex(hex)?.try_into().ok()?;
    Some(idn_open_protocol::web::httpc::Expected {
        size: *size,
        sha256,
    })
}

/// The marker beside a model checked against its pin: `MODEL.ok`.
fn marker(path: &Path) -> PathBuf {
    path.with_extension("bin.ok")
}

/// What the marker holds: the file's size and modified time, and the pin it matched.
fn marker_text(
    path: &Path,
    pin: idn_open_protocol::web::httpc::Expected,
) -> std::io::Result<String> {
    let m = std::fs::metadata(path)?;
    let mtime = m
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let hex = idn_common::hex::hex(&pin.sha256);
    Ok(format!("{} {mtime} {hex}\n", m.len()))
}

fn mark_checked(path: &Path, pin: idn_open_protocol::web::httpc::Expected) -> std::io::Result<()> {
    std::fs::write(marker(path), marker_text(path, pin)?)
}

/// Whether the model at `path` is the pinned file. Hashed once (a 1.6 GB model takes a
/// few seconds); after that a marker with its size and time stands for the check, until
/// the file or the pin changes.
fn already_checked(
    path: &Path,
    pin: idn_open_protocol::web::httpc::Expected,
    stop: &AtomicBool,
) -> std::io::Result<bool> {
    use sha2::Digest;
    use std::io::Read;
    if std::fs::metadata(path)?.len() != pin.size {
        return Ok(false);
    }
    let want = marker_text(path, pin)?;
    if std::fs::read_to_string(marker(path)).is_ok_and(|t| t == want) {
        return Ok(true);
    }
    let mut f = std::fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        // (A stop looked at each megabyte: hashing a 1.5 GB model held it up.)
        if stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                STOPPED,
            ));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let sum: [u8; 32] = hash.finalize().into();
    if sum != pin.sha256 {
        return Ok(false);
    }
    let _ = std::fs::write(marker(path), want);
    Ok(true)
}

/// 48 kHz (or any rate) mono to 16 kHz: a box filter of each output sample's span
/// (enough for speech recognition).
pub fn to_16k(samples: &[f32], rate: u32) -> Vec<f32> {
    idn_open_protocol::audio::resample::simple(samples, rate, RATE)
}

/// What whisper makes up from faint noise (it learned them from video endings).
const MADE_UP: [&str; 8] = [
    "thank you",
    "thank you.",
    "thanks for watching!",
    "thank you for watching.",
    "thank you for watching!",
    "bye.",
    "you",
    "so",
];

/// What whisper says over music or noise however loud, left out (a film's last words).
const ALWAYS_MADE_UP: [&str; 2] = ["the end", "the end."];

/// A video's sign-off, what whisper makes of music however sure of it (
/// "Please subscribe and like! Thanks for watching!"): a segment of these alone, with
/// "watching" or "subscribe" in it, is left out. Longest first.
const SIGN_OFF: [&str; 12] = [
    "thank you for watching",
    "thanks for watching",
    "please subscribe",
    "subscribe",
    "and like",
    "like",
    "thank you",
    "thanks",
    "bye",
    "please",
    "you",
    "so",
];

/// A line the same as the last one within this long is left out: whisper over music says
/// the same thing again and again ("Thank you." four times). Short: a
/// service says lines again too ("Amen." after each prayer; 30 s left those out); music
/// itself is told apart by [`Music`].
const REPEAT_WITHIN: Duration = Duration::from_secs(8);

/// Music or speech, from the last 3 s of sound: the share of its 20 ms
/// slices quieter than half its average. Speech stops between syllables and words
/// (`jfk.wav`: 0.43–0.75 a 3 s window, still 0.43–0.70 with a piano under it at a
/// quarter of its level); music hardly does (a band, a piano, a fade: 0.00–0.05, a fade's
/// last seconds 0.31). Music below [`Music::MUSIC`], speech again above
/// [`Music::SPEECH`]; very quiet sound changes nothing.
#[derive(Debug, Default)]
pub struct Music {
    /// The last [`Music::SPAN`]'s 20 ms slices' levels (RMS), oldest first, and the
    /// slice being filled (sum of squares, samples): kept as sound comes, not measured
    /// again from the samples each block.
    slices: std::collections::VecDeque<f32>,
    filling: (f32, usize),
    /// Music now.
    pub on: bool,
}

impl Music {
    /// The span judged, in 20 ms slices at 16 kHz.
    const SLICE: usize = RATE as usize / 50;
    const SPAN: usize = 150;
    const MUSIC: f32 = 0.15;
    const SPEECH: f32 = 0.25;

    /// More sound at 16 kHz; whether music is playing now.
    pub fn hear(&mut self, samples: &[f32], quiet: f32) -> bool {
        let mut changed = false;
        for &x in samples {
            self.filling.0 += x * x;
            self.filling.1 += 1;
            if self.filling.1 == Self::SLICE {
                self.slices
                    .push_back((self.filling.0 / Self::SLICE as f32).sqrt());
                if self.slices.len() > Self::SPAN {
                    self.slices.pop_front();
                }
                self.filling = (0.0, 0);
                changed = true;
            }
        }
        if !changed || self.slices.len() < Self::SPAN {
            return self.on;
        }
        let mean = self.slices.iter().sum::<f32>() / Self::SPAN as f32;
        if mean < quiet {
            return self.on;
        }
        let share =
            self.slices.iter().filter(|&&r| r < 0.5 * mean).count() as f32 / Self::SPAN as f32;
        if self.on && share > Self::SPEECH {
            self.on = false;
        } else if !self.on && share < Self::MUSIC {
            self.on = true;
        }
        self.on
    }
}

/// A text's words, lower case, without punctuation, one space apart.
fn plain(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a segment is a video's sign-off and nothing else ([`SIGN_OFF`]).
fn sign_off(text: &str) -> bool {
    let words = plain(text);
    if !(words.contains("watching") || words.contains("subscribe")) {
        return false;
    }
    let mut rest = format!(" {words} ");
    for p in SIGN_OFF {
        rest = rest.replace(&format!(" {p} "), " ");
        // Twice: " like like " leaves one between its spaces.
        rest = rest.replace(&format!(" {p} "), " ");
    }
    rest.trim().is_empty()
}

/// A segment of three words or fewer whose words whisper is on average less sure of than
/// this is left out: speech scored 0.77–0.91 (`jfk.wav`, base.en), and what it makes of
/// music is short and unsure.
const UNSURE: f32 = 0.5;

/// What whisper says when there was no speech, left out.
fn noise(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
        || t.starts_with('♪')
        || t == "."
}

/// The priority whisper's work runs at ([`EngineConfig::priority`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// In this process, as its other threads (whisper's own threads at normal priority
    /// on Windows, whatever the calling thread's).
    Normal,
    /// In a worker process below normal priority (Windows: the below-normal priority
    /// class; Linux and other Unix systems: nice 10): frames first, captions with what is
    /// left (an integrator's bug run H05-1).
    #[default]
    Low,
    /// In a worker process at the lowest priority (Windows: the idle priority class;
    /// Unix: nice 19): captions only when nothing else wants the CPU.
    Lowest,
}

impl Priority {
    /// Its name in logs: `normal`, `low`, `lowest`.
    pub fn name(self) -> &'static str {
        match self {
            Priority::Normal => "normal",
            Priority::Low => "low",
            Priority::Lowest => "lowest",
        }
    }
}

/// How long the engine waits for each answer of its worker (the model loaded, a pass)
/// by default ([`EngineConfig::worker_timeout`]): a large model's first pass on a GPU can
/// take a while.
pub const WORKER_TIMEOUT: Duration = Duration::from_secs(120);

/// Settings of the engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// `en`, `es`…
    pub lang: String,
    /// Names and words to spell right ("Pastor Okafor, Ephesians, Hillsong"): given to
    /// whisper as its prompt.
    pub words: String,
    /// CPU threads (0: all but two).
    pub threads: usize,
    /// A line is final after this much quiet…
    pub pause: Duration,
    /// …or once it is this long.
    pub longest: Duration,
    /// Below this level (RMS) is quiet.
    pub quiet: f32,
    /// On the best GPU when there is one (the `gpu` feature); else the CPU.
    pub gpu: bool,
    /// Caption sung lines too: whisper over music makes things up, so lines are left
    /// out while [`Music`] hears music, unless this is set.
    pub keep_music: bool,
    /// The priority whisper's work runs at: [`Priority::Low`] by default, in a worker
    /// process ([`worker`]) so that captions never take CPU from the program's frames;
    /// [`Priority::Normal`] runs it in this process. Only when no worker program is found
    /// or none starts and says hello does whisper run in this process (logged once a
    /// process, with why).
    pub priority: Priority,
    /// The program to run as the worker: one that calls [`worker::serve_if_asked`] first
    /// thing in its `main` (an embedder sets its own, `std::env::current_exe()`). `None`:
    /// the switch `IDN_CAPTIONS_WORKER` (`off`: none), else `idn` (`idn.exe`) in IDN's
    /// programs folder (next to this program), else `idn` on `PATH`.
    pub worker: Option<PathBuf>,
    /// The longest the engine waits for each answer of its worker ([`WORKER_TIMEOUT`]):
    /// a pass past it is skipped (aborted; the worker kept), a model's load past it is
    /// [`Engine::new`]'s error.
    pub worker_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            lang: "en".into(),
            words: String::new(),
            threads: 0,
            pause: Duration::from_millis(700),
            longest: Duration::from_secs(10),
            quiet: 0.01,
            gpu: true,
            keep_music: false,
            priority: Priority::default(),
            worker: None,
            worker_timeout: WORKER_TIMEOUT,
        }
    }
}

/// A pass's settings from `cfg` (`previous`: the line before, for context).
fn pass_params(cfg: &EngineConfig, previous: &str) -> PassParams {
    PassParams {
        threads: cfg.threads,
        lang: cfg.lang.clone(),
        prompt: [cfg.words.trim(), previous.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        max_tokens: None,
        temperature_inc: None,
    }
}

/// Where whisper runs: in this process, or in a worker.
enum Whisper {
    Local(Box<Local>),
    Worker(Box<worker::Remote>),
}

impl Whisper {
    /// One pass; with `stop`, ended when it is set (the error [`STOPPED`]).
    fn pass(
        &mut self,
        p: &PassParams,
        audio: &[f32],
        stop: Option<&Arc<AtomicBool>>,
    ) -> Result<Vec<Segment>, String> {
        match self {
            Whisper::Local(l) => {
                let abort = stop.map(|s| {
                    let s = s.clone();
                    Arc::new(move || s.load(Ordering::Relaxed)) as whisper::Abort
                });
                l.pass(p, audio, abort)
            }
            Whisper::Worker(r) => r.pass(p, audio, stop.map(|s| &**s)),
        }
    }
}

/// Said once a process: whisper in-process for want of a worker.
static RUNS_HERE_SAID: AtomicBool = AtomicBool::new(false);

/// whisper for the model at `path` with `cfg`: in a worker at `cfg.priority`, else (normal
/// priority asked, or no worker program that starts and says hello: logged once a
/// process, with why) in this process. With it, the device and the priority it runs at. A
/// model a worker can't load is an error, never whisper in this process instead.
fn open_whisper(path: &Path, cfg: &EngineConfig) -> Result<(Whisper, String, Priority), String> {
    let programs = (cfg.priority != Priority::Normal)
        .then(|| worker::find_programs_here(cfg.worker.as_deref()));
    open_whisper_with(path, cfg, programs, &mut |p, c| Local::load(p, c.gpu))
}

/// [`open_whisper`] with the worker programs found (`None`: normal priority asked) and
/// in-process whisper's loader given.
fn open_whisper_with(
    path: &Path,
    cfg: &EngineConfig,
    programs: Option<Result<Vec<PathBuf>, String>>,
    local: &mut dyn FnMut(&Path, &EngineConfig) -> Result<Local, String>,
) -> Result<(Whisper, String, Priority), String> {
    let why = match programs {
        None => None,
        Some(Err(why)) => Some(why),
        Some(Ok(programs)) => {
            match worker::Remote::open(&programs, cfg.priority, path, cfg.gpu, cfg.worker_timeout) {
                Ok(r) => {
                    let (device, priority) = (r.device.clone(), r.priority);
                    return Ok((Whisper::Worker(Box::new(r)), device, priority));
                }
                Err(worker::OpenError::Model(e)) => return Err(e),
                Err(worker::OpenError::NoWorker(why)) => Some(why),
            }
        }
    };
    if let Some(why) = why
        && !RUNS_HERE_SAID.swap(true, Ordering::Relaxed)
    {
        idn_open_protocol::log::log(&format!(
            "captions: whisper runs in this process at normal priority: {why}"
        ));
    }
    let l = local(path, cfg)?;
    let device = l.device.clone();
    Ok((Whisper::Local(Box::new(l)), device, Priority::Normal))
}

/// The device whisper would run on with `cfg` (the GPU, if one, and its name), asked of
/// the worker when there is one: only whisper in this process opens the GPU's driver here.
fn choose_device(cfg: &EngineConfig) -> Result<(Option<Gpu>, String), String> {
    if cfg.priority != Priority::Normal
        && let Ok(programs) = worker::find_programs_here(cfg.worker.as_deref())
        && let Some(d) = worker::probe_device(&programs, cfg.priority, cfg.gpu, cfg.worker_timeout)?
    {
        return Ok(d);
    }
    let g = device(cfg.gpu);
    let name = device_name(g.as_ref());
    Ok((g, name))
}

/// Speech to caption lines.
pub struct Engine {
    whisper: Whisper,
    cfg: EngineConfig,
    /// The line being heard, at 16 kHz.
    audio: Vec<f32>,
    /// Samples at its start kept from the last line (its end may hold the next word, but
    /// alone they are no reason to listen).
    kept: usize,
    line: u64,
    draft: String,
    /// The sound heard in all (samples at 16 kHz), and as much when the draft last
    /// changed.
    heard: u64,
    draft_at: u64,
    /// The last final line (context for the next), and when.
    previous: String,
    previous_at: Option<Instant>,
    /// Music or speech now.
    music: Music,
    /// `IDN_CAPTIONS_SCORES=1`: the lines sent logged too, with whisper's scores.
    log_sent: bool,
    /// Seconds of work for each second of audio, on average (below 1: it keeps up).
    pub load: f64,
    /// What whisper runs on: the GPU's name, or "CPU (N threads)".
    pub device: String,
    /// whisper runs in a worker process ([`EngineConfig::priority`]), not in this one.
    pub in_worker: bool,
    /// The priority whisper runs at: [`EngineConfig::priority`] in a worker,
    /// [`Priority::Normal`] in this process (or in a worker whose priority couldn't be
    /// set).
    pub priority: Priority,
}

impl Engine {
    /// Loads the model at `path`.
    /// An English-only model (`ggml-*.en.bin`) with another language is refused
    /// ([`model_speaks`]; bug run B: it gave English, or nothing, without a word).
    pub fn new(path: &Path, cfg: EngineConfig) -> Result<Engine, String> {
        let english_only = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".en.bin"));
        if english_only && !model_speaks("x.en", &cfg.lang) {
            return Err(format!(
                "{}: English only; for language {:?} take a model without .en",
                path.display(),
                cfg.lang
            ));
        }
        let (whisper, device, priority) = open_whisper(path, &cfg)?;
        Ok(Engine {
            in_worker: matches!(whisper, Whisper::Worker(_)),
            whisper,
            cfg,
            audio: Vec::new(),
            kept: 0,
            line: 1,
            draft: String::new(),
            heard: 0,
            draft_at: 0,
            previous: String::new(),
            previous_at: None,
            music: Music::default(),
            log_sent: idn_open_protocol::switches::on("IDN_CAPTIONS_SCORES"),
            load: 0.0,
            device,
            priority,
        })
    }

    /// One pass where whisper runs; the device and priority said again after (a worker
    /// started again may run elsewhere: on the CPU after one ended loading the model on
    /// the GPU).
    fn pass(
        &mut self,
        p: &PassParams,
        audio: &[f32],
        stop: Option<&Arc<AtomicBool>>,
    ) -> Result<Vec<Segment>, String> {
        let r = self.whisper.pass(p, audio, stop);
        if let Whisper::Worker(w) = &self.whisper
            && !w.device.is_empty()
        {
            self.device.clone_from(&w.device);
            self.priority = w.priority;
        }
        r
    }

    /// More sound (mono, at `rate`).
    pub fn push(&mut self, samples: &[f32], rate: u32) {
        let s = to_16k(samples, rate);
        let was = self.music.on;
        if self.music.hear(&s, self.cfg.quiet) != was && !self.cfg.keep_music {
            idn_open_protocol::log::log(if was {
                "captions: speech again"
            } else {
                "captions: music: lines left out until speech"
            });
        }
        self.heard += s.len() as u64;
        self.audio.extend(s);
        // Nothing heard for a long time: forget it (no line starts with minutes of quiet).
        // With a draft on screen, at most the last 30 s (whisper's most): sound whisper's
        // output is filtered from (applause, a murmur) was never drained, and each pass
        // ran over all of it.
        let keep = (RATE as usize) * 30;
        if self.audio.len() > keep {
            let cut = self.audio.len()
                - if self.draft.is_empty() {
                    RATE as usize
                } else {
                    keep
                };
            self.audio.drain(..cut);
            self.kept = self.kept.saturating_sub(cut);
        }
    }

    fn rms(s: &[f32]) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    /// How long one pass takes on this machine: 5 s of faint noise (whisper's work is
    /// nearly all the same for any 30 s or less) read as up to 32 tokens, the second of
    /// two passes (the first prepares the GPU).
    pub fn measure(&mut self) -> Result<Duration, String> {
        self.measure_stop(&Arc::new(AtomicBool::new(false)))
    }

    /// [`measure`](Self::measure), ended when `stop` is set (whisper asked to abort as it
    /// works): the error [`STOPPED`].
    pub fn measure_stop(&mut self, stop: &Arc<AtomicBool>) -> Result<Duration, String> {
        let mut x: u32 = 1;
        let audio: Vec<f32> = (0..RATE as usize * 5)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 8) as f32 / (1u32 << 24) as f32 * 0.04 - 0.02
            })
            .collect();
        let mut took = Duration::ZERO;
        for _ in 0..2 {
            let started = Instant::now();
            let p = PassParams {
                // As much as a line of speech gives (about 30 tokens in 10 s), once: noise
                // alone would have whisper go on to its limit and try again hotter.
                max_tokens: Some(32),
                temperature_inc: Some(0.0),
                ..pass_params(&self.cfg, &self.previous)
            };
            if stop.load(Ordering::Relaxed) {
                return Err(STOPPED.into());
            }
            let r = self.pass(&p, &audio, Some(stop));
            if stop.load(Ordering::Relaxed) {
                return Err(STOPPED.into());
            }
            r?;
            took = started.elapsed();
        }
        Ok(took)
    }

    /// Listens to what came since the last step: the draft of the line now and, at a pause
    /// or at its longest, the line made final (then the next line starts).
    pub fn step(&mut self) -> Result<Vec<CaptionLine>, String> {
        let rate = RATE as usize;
        let len = self.audio.len();
        if len < rate / 2 {
            return Ok(Vec::new());
        }
        let pause = (self.cfg.pause.as_secs_f64() * rate as f64) as usize;
        let tail_quiet = len >= pause && Self::rms(&self.audio[len - pause..]) < self.cfg.quiet;
        let any_speech = self.audio[self.kept.min(len)..]
            .chunks(rate / 10)
            .any(|c| Self::rms(c) >= self.cfg.quiet);
        if !any_speech {
            // Only quiet: nothing to say; keep the last second (a word may start in it).
            if len > rate {
                self.audio.drain(..len - rate);
                self.kept = self.kept.saturating_sub(len - rate);
            }
            return Ok(Vec::new());
        }
        if self.music.on && !self.cfg.keep_music {
            // Music: whisper isn't run (its lines would be left out; a service's songs
            // kept the GPU busy for nothing), and the last 3 s are kept: speech that comes
            // back is told from music over that span, its first words in it. A draft on
            // screens when music starts was heard before it (it takes about 3 s of music
            // to tell) and is made final: a sermon going straight into a song lost its
            // last line. One changed in the last 2 s, or of a line with no sound before
            // the music (a second short of the span told, as it can take 3.6 s), may be
            // whisper on the music: taken down (an empty draft of its line). After a
            // quiet pause, base.en's "Thank you." 1.8 s into a song went final.
            let mut out = Vec::new();
            if !self.draft.is_empty() {
                let text = std::mem::take(&mut self.draft);
                let before = len.saturating_sub(Music::SLICE * Music::SPAN + rate);
                let voiced = self.audio[self.kept.min(before)..before]
                    .chunks(rate / 10)
                    .any(|c| Self::rms(c) >= self.cfg.quiet);
                let heard = voiced && self.heard - self.draft_at >= 2 * RATE as u64;
                out.push(CaptionLine {
                    line: self.line,
                    text: if heard { text.clone() } else { String::new() },
                    is_final: heard,
                    lang: self.cfg.lang.clone(),
                    at_ms: idn_common::epoch::millis() as i64,
                    ..Default::default()
                });
                if heard {
                    self.previous = text;
                    self.previous_at = Some(Instant::now());
                    self.line += 1;
                } else {
                    idn_open_protocol::log::log(&format!("captions: left out (music): {text:?}"));
                }
            }
            let span = Music::SLICE * Music::SPAN;
            if len > span {
                self.audio.drain(..len - span);
                self.kept = self.kept.saturating_sub(len - span);
            }
            return Ok(out);
        }
        let started = Instant::now();
        let p = pass_params(&self.cfg, &self.previous);
        // whisper needs a second at least; quiet pads it.
        let mut audio = self.audio.clone();
        if audio.len() < rate + rate / 10 {
            audio.resize(rate + rate / 10, 0.0);
        }

        let segments = self.pass(&p, &audio, None)?;
        // Segments whisper itself thinks are not speech ("Thank you." in the quiet), and
        // short ones it is unsure of (what it makes of music: "The End"),
        // left out — each logged, with why.
        let log_sent = self.log_sent;
        let text: String = segments
            .into_iter()
            .filter_map(|s| {
                let (t, no_speech, probs) = (s.text, s.no_speech, s.probs);
                let sure = probs.iter().sum::<f32>() / probs.len().max(1) as f32;
                let short = t.split_whitespace().count() <= 3;
                let why = if no_speech >= 0.6 {
                    Some("not speech")
                } else if short && sure < UNSURE {
                    Some("unsure")
                } else if ALWAYS_MADE_UP.contains(&t.trim().to_lowercase().as_str()) {
                    Some("a stock phrase")
                } else if sign_off(&t) {
                    Some("a video's sign-off")
                } else {
                    None
                };
                if why.is_none() && log_sent && !noise(&t) {
                    idn_open_protocol::log::log(&format!(
                        "captions: kept (no speech {no_speech:.2}, sure {sure:.2}): {:?}",
                        t.trim()
                    ));
                }
                match why {
                    Some(why) if !noise(&t) => {
                        idn_open_protocol::log::log(&format!(
                            "captions: left out ({why}: no speech {no_speech:.2}, sure {sure:.2}): {:?}",
                            t.trim()
                        ));
                        None
                    }
                    Some(_) => None,
                    None => Some(t),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let took = started.elapsed().as_secs_f64();
        let heard = len as f64 / rate as f64;
        // How much of a second's work a second of sound costs, smoothed.
        self.load = 0.8 * self.load + 0.2 * (took / heard.max(1.0));
        let at_ms = idn_common::epoch::millis() as i64;
        // Its loudest tenth of a second: faint noise only, and a stock phrase from it, is
        // left out.
        let loudest = self
            .audio
            .chunks(rate / 10)
            .map(Self::rms)
            .fold(0.0f32, f32::max);
        let made_up = loudest < 0.05 && MADE_UP.contains(&text.to_lowercase().trim());
        let at_end = tail_quiet || len >= (self.cfg.longest.as_secs_f64() * rate as f64) as usize;
        let repeat = !text.is_empty()
            && self
                .previous_at
                .is_some_and(|t| t.elapsed() < REPEAT_WITHIN)
            && plain(&text) == plain(&self.previous);
        if repeat && at_end {
            idn_open_protocol::log::log(&format!(
                "captions: left out (the last line again): {:?}",
                text.trim()
            ));
        }
        let text = if noise(&text) || made_up || repeat {
            String::new()
        } else {
            text
        };
        let done = at_end && !text.is_empty();
        let mut out = Vec::new();
        if done {
            out.push(CaptionLine {
                line: self.line,
                text: text.clone(),
                is_final: true,
                lang: self.cfg.lang.clone(),
                at_ms,
                ..Default::default()
            });
            self.previous = text;
            self.previous_at = Some(Instant::now());
            self.line += 1;
            self.draft.clear();
            // What was heard is said. A line cut at its longest (mid-speech) keeps its last
            // fifth of a second, which may hold the next word; one ended at a pause, nothing.
            let keep = if tail_quiet { 0 } else { rate / 5 };
            self.audio.drain(..len.saturating_sub(keep));
            self.kept = self.audio.len();
        } else if text != self.draft && !text.is_empty() {
            self.draft = text.clone();
            self.draft_at = self.heard;
            out.push(CaptionLine {
                line: self.line,
                text,
                is_final: false,
                lang: self.cfg.lang.clone(),
                at_ms,
                ..Default::default()
            });
        } else if text.is_empty() && at_end {
            // Only noise said, at a pause or at the line's longest: start afresh (the next
            // draft takes this line's place). At the longest, the last fifth of a second
            // is kept, as for a line said (it was never drained).
            let keep = if tail_quiet { 0 } else { rate / 5 };
            self.audio.drain(..len.saturating_sub(keep));
            self.kept = self.audio.len();
            self.draft.clear();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each language gets models that hear it: English the `.en` ones,
    /// others the multilingual ones; the languages listed are whisper's codes, once each.
    #[test]
    fn models_for_each_language() {
        assert!(model_speaks("small.en", "en") && model_speaks("small.en", " EN "));
        assert!(!model_speaks("small.en", "es") && model_speaks("small", "es"));
        assert!(model_speaks("large-v3-turbo", "ko"));
        for lang in ["en", "es", "ko"] {
            for gpu in [
                None,
                Some(Gpu {
                    index: 0,
                    name: "card".into(),
                    discrete: true,
                    memory_mb: 8000,
                }),
            ] {
                let c = auto_candidates(gpu.as_ref(), lang);
                assert!(!c.is_empty());
                assert!(c.iter().all(|m| model_speaks(m, lang)), "{lang}: {c:?}");
                assert!(c.iter().all(|m| MODEL_LIST.iter().any(|(n, _, _)| n == m)));
            }
        }
        let mut codes: Vec<&str> = LANGUAGES.iter().map(|l| l.0).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), LANGUAGES.len());
        assert!(codes.iter().all(|c| c.len() == 2));
    }

    /// whisper runs in this process only when normal priority is asked for, no worker
    /// program is found, or none found starts and says IDN's hello (an integrator's bug run
    /// H05-1); by default a worker at low priority is wanted.
    #[test]
    fn whisper_runs_here_when_no_worker_starts() {
        // (Hosts keep the engine on their own threads, as with whisper in-process.)
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Engine>();
        let cfg = EngineConfig::default();
        assert_eq!((cfg.priority, cfg.worker.as_ref()), (Priority::Low, None));
        let model = Path::new("/nowhere/ggml-base.en.bin");
        let mut tried = 0;
        let mut local = |_: &Path, _: &EngineConfig| -> Result<Local, String> {
            tried += 1;
            Err("loaded here".into())
        };
        let mut programs = vec![
            None,
            Some(Err("no worker program".to_owned())),
            Some(Ok(vec![PathBuf::from(
                "/nowhere/idn-captions-worker-missing",
            )])),
        ];
        // (One that ends at once without IDN's hello: GNU libidn's `idn` would be like it.)
        #[cfg(unix)]
        programs.extend(
            ["/bin/true", "/usr/bin/true"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
                .map(|p| Some(Ok(vec![p]))),
        );
        let n = programs.len();
        for program in programs {
            let e = open_whisper_with(model, &cfg, program, &mut local)
                .err()
                .unwrap();
            assert_eq!(e, "loaded here");
        }
        assert_eq!(tried, n);
    }

    /// Every offered model has a pin; a model on disk is taken only when it is the pinned
    /// file, checked once (bug round E, E4-11).
    #[test]
    fn models_on_disk_are_taken_only_when_they_match_their_pin() {
        for (name, _, _) in MODEL_LIST {
            let p = model_pin(name).unwrap_or_else(|| panic!("{name}: no pin"));
            assert!(p.size > 1_000_000);
        }
        assert!(MODELS.contains("/resolve/") && !MODELS.contains("/main/"));
        let dir = idn_common::testing::TempDir::new("captions-pin");
        let path = dir.join("ggml-test.bin");
        let body = vec![7u8; 4096];
        std::fs::write(&path, &body).unwrap();
        let pin = idn_open_protocol::web::httpc::Expected {
            size: body.len() as u64,
            sha256: idn_open_protocol::hash::sha256(&body),
        };
        // The right file: taken, and marked.
        assert!(already_checked(&path, pin, &AtomicBool::new(false)).unwrap());
        assert!(marker(&path).exists());
        assert!(already_checked(&path, pin, &AtomicBool::new(false)).unwrap());
        // Changed in place (same size; its time moves on): hashed again and refused.
        let mut changed = body.clone();
        changed[100] = 8;
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, &changed).unwrap();
        assert!(!already_checked(&path, pin, &AtomicBool::new(false)).unwrap());
        // Another size: refused without hashing.
        std::fs::write(&path, b"short").unwrap();
        assert!(!already_checked(&path, pin, &AtomicBool::new(false)).unwrap());
    }

    #[test]
    fn music_is_told_from_speech_by_its_pauses() {
        let rate = RATE as usize;
        // Syllables: 150 ms of a voice-like tone, 120 ms of near silence, for 4 s.
        let speech: Vec<f32> = (0..rate * 4)
            .map(|i| {
                let on = (i % (rate * 27 / 100)) < rate * 15 / 100;
                let v = (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / RATE as f32).sin();
                if on { 0.3 * v } else { 0.002 * v }
            })
            .collect();
        // A held chord with a 4 Hz tremolo.
        let music: Vec<f32> = (0..rate * 4)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let w = |f: f32| (t * 2.0 * std::f32::consts::PI * f).sin();
                0.2 * (1.0 + 0.3 * w(4.0)) * (w(262.0) + w(330.0) + w(392.0)) / 3.0
            })
            .collect();
        let mut m = Music::default();
        assert!(!m.hear(&speech, 0.01), "speech");
        assert!(m.hear(&music, 0.01), "music");
        assert!(m.hear(&vec![0.0; rate * 4], 0.01), "quiet changes nothing");
        assert!(!m.hear(&speech, 0.01), "speech again");
    }

    #[test]
    fn sound_is_made_16k_and_noise_left_out() {
        let s: Vec<f32> = (0..4800).map(|i| (i % 3) as f32).collect();
        let r = to_16k(&s, 48000);
        assert_eq!(r.len(), 1600);
        assert!((r[0] - 1.0).abs() < 1e-6);
        assert_eq!(to_16k(&s[..10], 16000).len(), 10);
        for n in ["[BLANK_AUDIO]", "(music)", " ", "♪ ♪", "."] {
            assert!(noise(n), "{n}");
        }
        assert!(!noise("Welcome, everyone."));
        for t in [
            "Thanks for watching!",
            "Please subscribe and like! Thanks for watching!",
            " Subscribe and like! Thanks for watching!",
            "Thank you for watching.",
        ] {
            assert!(sign_off(t), "{t}");
        }
        for t in [
            "Thank you.",
            "Thank you all for watching over the children today.",
            "You can subscribe to the newsletter at the door.",
        ] {
            assert!(!sign_off(t), "{t}");
        }
        assert_eq!(
            plain(" ...And then, you can't GO!"),
            "and then you can't go"
        );
        assert_eq!(model_file("small.en").as_deref(), Some("ggml-small.en.bin"));
        assert!(model_file("gpt").is_none());
    }
}
