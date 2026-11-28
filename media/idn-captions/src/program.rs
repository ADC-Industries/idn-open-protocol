//! `idn-captions`: live captions as a program (spec/extensions/idn-captions.md, G2): an
//! IDN sound source (a mixer's sermon mix) heard on this machine and sent as caption lines
//! on the text-only source "HOST (Captions)". The IDN Node runs it (`captions.*`); `idn
//! captions` is the same.
//!
//!   idn-captions SOURCE [--name NAME] [--model auto|NAME] [--words TEXT] [--lang en]
//!                [--channels 1,2] [--models-dir DIR] [--gpu auto|any|N|NAME]
//!                [--cpu] [--keep-music] [--no-node]
//!   idn-captions --list-gpus

use crate::{
    Engine, EngineConfig, GpuDevice, MODEL_LIST, auto_choose_stop, choose_gpu, ensure_model_stop,
    gpus_here, model_speaks,
};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::text::TextSource;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: idn-captions SOURCE [--name NAME] [--model auto|NAME] [--words TEXT] [--lang en]
                    [--channels 1,2] [--models-dir DIR] [--gpu auto|any|N|NAME]
                    [--cpu] [--keep-music] [--no-node]
       idn-captions --list-gpus

  Live captions on this machine: the IDN source SOURCE's sound (its first two channels
  mixed, or --channels, or SOURCE|ch=N for N and N+1) turned into text by whisper.cpp
  and sent as caption lines on the text-only source NAME (default \"Captions\"): a
  draft about every second, final at a pause. --gpu: auto (the default: a graphics card
  of its own, Vulkan, else the CPU; never a GPU built into the processor, which renders
  the screens), any (or integrated: any GPU, a built-in one too; a card still comes
  first), N (the GPU of index N, as --list-gpus shows it; a number of 100 or more no
  GPU has as its index is looked for as a whole number in the names, so 3060 finds an
  RTX 3060) or NAME (the first whose name contains it); index:N and name:TEXT say which
  is meant. One not found is logged and auto used. --cpu (or --gpu cpu): the CPU
  whatever --gpu says. --list-gpus: the GPUs whisper can use here (each one's index,
  name, kind and memory, best first, and the one auto takes), then ends. While music
  plays (told by its few pauses) lines are left out, whisper making things up over it;
  --keep-music captions sung lines too.
  --model: auto (the largest that keeps up, measured on this machine the first time),
  tiny.en, base.en, small.en, medium.en (English only), tiny, base, small, medium or
  large-v3-turbo (every language), downloaded the first time from
  whisper.cpp's models (Hugging Face) into --models-dir (default: models in the state
  directory). --words: names and words to spell right, with commas between. Nothing
  leaves the machine but the text.";

fn fail(m: &str) -> ! {
    eprintln!("idn-captions: {m}");
    std::process::exit(1)
}

/// `r`'s value; else, once a stop was asked (the error [`crate::STOPPED`], or any error
/// after it: a worker ended by the same Ctrl+C), "stopped" said and the program ended;
/// else the error said and the program ended with 1.
fn or_end<T>(r: Result<T, String>, stop: &AtomicBool) -> T {
    match r {
        Ok(v) => v,
        Err(_) if stop.load(Ordering::SeqCst) => {
            eprintln!("idn-captions: stopped");
            std::process::exit(0)
        }
        Err(e) => fail(&e),
    }
}

/// `--list-gpus`: the GPUs whisper can use where it would run with `cfg` (asked of the
/// worker when one starts, so this process opens no GPU driver), best first, each with
/// the index `--gpu N` takes; the one `--gpu auto` takes marked.
fn print_gpus(cfg: &EngineConfig) {
    let list = gpus_here(cfg).unwrap_or_else(|e| fail(&e));
    if list.is_empty() {
        println!("No GPU whisper can use here: captions run on the CPU.");
        return;
    }
    let (auto, _) = choose_gpu(&list, true, &GpuDevice::Auto);
    println!("GPUs whisper can use here, best first (--gpu takes the index or part of the name):");
    for g in &list {
        println!(
            "  {:>2}  {}  ({}, {} MB){}",
            g.index,
            g.name,
            if g.discrete {
                "a card of its own"
            } else {
                "built into the processor"
            },
            g.memory_mb,
            if auto.as_ref() == Some(g) {
                "  <- auto"
            } else {
                ""
            }
        );
    }
    if auto.is_none() {
        println!("auto: the CPU (no card of its own; --gpu any takes the built-in GPU).");
    }
}

fn default_models_dir() -> PathBuf {
    idn_open_protocol::link::LinkConfig::default_dir()
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("idn-open-protocol"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("models")
}

pub fn main() {
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    idn_open_protocol::diag::init("idn-captions");
    // Ctrl+C and SIGTERM (Windows: Ctrl+C, Ctrl+Break, the console closed) set `stop`: a
    // download or measuring under way ends within about a second, and the loop below ends,
    // dropping the engine (its worker ended and reaped) and the source. The handler gives
    // its hooks two seconds, then ends the process; this one waits for `finished` within
    // that time.
    #[cfg(unix)]
    idn_open_protocol::shutdown::install_for_command();
    let stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let _stop_on_signal = {
        let (stop, finished) = (stop.clone(), finished.clone());
        idn_open_protocol::shutdown::register(move || {
            stop.store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_millis(1800);
            while !finished.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let (mut source, mut name, mut model) =
        (None::<String>, "Captions".to_owned(), "auto".to_owned());
    let (mut words, mut lang, mut dir, mut use_node, mut gpu) =
        (String::new(), "en".to_owned(), None::<PathBuf>, true, true);
    let mut gpu_device = GpuDevice::Auto;
    let mut channels: Vec<u16> = vec![0, 1];
    let (mut channels_given, mut keep_music, mut list_gpus) = (false, false, false);
    // (Bad or missing values print the usage and stop; `--version` answers.)
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.value::<String>(&a),
            "--model" => model = it.value::<String>(&a),
            "--words" => words = it.value::<String>(&a),
            "--lang" => lang = it.value::<String>(&a),
            "--models-dir" => dir = Some(it.value::<String>(&a).into()),
            "--no-node" => use_node = false,
            "--cpu" => gpu = false,
            "--gpu" => {
                let v = it.value::<String>(&a);
                if v.trim().eq_ignore_ascii_case("cpu") {
                    gpu = false;
                } else {
                    gpu_device = GpuDevice::parse(&v);
                }
            }
            "--keep-music" => keep_music = true,
            "--list-gpus" => list_gpus = true,
            "--channels" => {
                channels_given = true;
                // Each a channel from 1 to 256 (what a sound flow carries): one that isn't
                // a number in that range is an error, not left out.
                channels = it.value::<String>(&a)
                    .split(',')
                    .map(|c| match c.trim().parse::<u16>() {
                        Ok(c @ 1..=256) => c - 1,
                        _ => fail(&format!(
                            "--channels: {:?} isn't a channel from 1 to 256 (numbers with commas between)",
                            c.trim()
                        )),
                    })
                    .collect();
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            s if source.is_none() && !s.starts_with('-') => source = Some(s.to_owned()),
            other => fail(&format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if list_gpus {
        print_gpus(&EngineConfig::default());
        return;
    }
    let mut source = source.unwrap_or_else(|| fail(USAGE));
    // `SOURCE|ch=N` (as the Node's setting takes it): channels N and N+1.
    // N up to 63: the pair's second is N+1 (64 asked for a 65th).
    if let Some((src, n)) = source.rsplit_once("|ch=") {
        if channels_given {
            fail("SOURCE|ch=N and --channels: give one");
        }
        match n.trim().parse::<u16>() {
            Ok(n @ 1..=63) => {
                channels = vec![n - 1, n];
                source = src.trim().to_owned();
            }
            _ => fail("SOURCE|ch=N: N a channel from 1 to 63 (it takes N and N+1)"),
        }
    }
    let cfg = EngineConfig {
        lang: lang.clone(),
        words: words
            .split(',')
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        gpu,
        gpu_device,
        keep_music,
        ..Default::default()
    };
    let dir = dir.unwrap_or_else(default_models_dir);
    if model == "auto" {
        model = or_end(
            auto_choose_stop(
                &dir,
                &cfg,
                &mut |m| eprintln!("idn-captions: {m}"),
                &mut |_, _, _| {},
                &stop,
            ),
            &stop,
        );
    }
    if !MODEL_LIST.iter().any(|(m, _, _)| *m == model) {
        fail(&format!(
            "--model {model:?}: auto or one of {}",
            MODEL_LIST.map(|m| m.0).join(", ")
        ));
    }
    if !model_speaks(&model, &lang) {
        fail(&format!(
            "--model {model}: English only; for --lang {lang} take one without .en (or auto)"
        ));
    }
    let mut tenth = 0;
    let path = or_end(
        ensure_model_stop(
            &dir,
            &model,
            &mut |n, total| {
                if let Some(t) = total.filter(|t| *t > 0)
                    && n * 10 / t != tenth
                {
                    tenth = n * 10 / t;
                    eprintln!(
                        "idn-captions: downloading the {model} model: {}%",
                        tenth * 10
                    );
                }
            },
            &stop,
        ),
        &stop,
    );
    let mut engine = or_end(Engine::new(&path, cfg), &stop);
    if stop.load(Ordering::SeqCst) {
        drop(engine);
        eprintln!("idn-captions: stopped");
        return;
    }
    let node = use_node
        .then(|| idn_open_protocol::node::Node::connect("idn-captions").ok())
        .flatten();
    let publisher = Publisher::new(PublisherConfig {
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| fail(&e.to_string()));
    let out = TextSource::new(&publisher, &name).unwrap_or_else(|e| fail(&e.to_string()));
    let input = Input::open(InputConfig {
        receiver_name: format!("{name} (listening)"),
        secure: node.as_ref().map(|n| n.secure_config()),
        receive_video: false,
        audio_channels: Some(channels.clone()),
        // (Speech recognised: lossless would only double the network's load.)
        lossless_audio: false,
        ..InputConfig::new(&source)
    })
    .unwrap_or_else(|e| fail(&e.to_string()));
    let heard_on: Vec<String> = channels.iter().map(|c| (c + 1).to_string()).collect();
    eprintln!(
        "idn-captions: {source} (channels {}) → {:?} with {model} ({lang}) on {}, {}",
        heard_on.join(", "),
        out.output().name(),
        engine.device,
        if engine.in_worker {
            format!("in a worker at {} priority", engine.priority.name())
        } else {
            "in this process".to_owned()
        }
    );
    // The sound, gathered on its own thread while whisper works: (mono, rate).
    let heard: Arc<Mutex<(Vec<f32>, u32)>> = Arc::default();
    let listening = {
        let heard = heard.clone();
        let asked = channels.len();
        let mut told = false;
        // No sound at all (channels the source hasn't, every one of them: nothing comes,
        // so the count above never runs): said once after 10 s.
        let (started, mut heard_any, mut told_none) = (Instant::now(), false, false);
        let which = channels
            .iter()
            .map(|c| (c + 1).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let stop = stop.clone();
        std::thread::spawn(move || {
            // (Ended with the program's stop: the input dropped, leaving the source.)
            while !stop.load(Ordering::SeqCst) {
                if !heard_any && !told_none && started.elapsed() > Duration::from_secs(10) {
                    told_none = true;
                    eprintln!(
                        "idn-captions: no sound from the source on channel(s) {which} in 10 s (does it have them?)"
                    );
                }
                match input.recv(Duration::from_millis(200)) {
                    Some(InputEvent::Audio(a)) if !a.channels.is_empty() => {
                        heard_any = true;
                        // Channels the source doesn't have come as none (said once).
                        if a.channels.len() < asked && !std::mem::replace(&mut told, true) {
                            eprintln!(
                                "idn-captions: the source sends {} of the {asked} channels asked for",
                                a.channels.len()
                            );
                        }
                        let n = a.channels[0].len();
                        let k = a.channels.len() as f32;
                        let mono: Vec<f32> = (0..n)
                            .map(|i| {
                                a.channels
                                    .iter()
                                    .map(|c| c.get(i).copied().unwrap_or(0.0))
                                    .sum::<f32>()
                                    / k
                            })
                            .collect();
                        let mut h = heard.lock_ok();
                        h.1 = a.header.sample_rate;
                        h.0.extend(mono);
                    }
                    Some(InputEvent::Error(e)) => eprintln!("idn-captions: {e}"),
                    _ => {}
                }
            }
        })
    };
    let mut said_at = Instant::now();
    // The last step's error, said once while it repeats (a worker starting again fails
    // each step the same way until it is up).
    let mut last_error: Option<String> = None;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let got = {
            let mut h = heard.lock_ok();
            (h.1 != 0 && h.0.len() >= h.1 as usize).then(|| (std::mem::take(&mut h.0), h.1))
        };
        let Some((chunk, rate)) = got else {
            // No second of sound yet (or none comes): the worker looked after all the
            // same, so one that ended is started again on time.
            engine.tend();
            continue;
        };
        engine.push(&chunk, rate);
        match engine.step_stop(&stop) {
            Err(_) if stop.load(Ordering::SeqCst) => break,
            Ok(lines) => {
                last_error = None;
                for l in lines {
                    if l.is_final {
                        eprintln!("idn-captions: {}", l.text);
                    }
                    out.send_caption(&l);
                }
            }
            Err(e) => {
                if last_error.as_deref() != Some(e.as_str()) {
                    eprintln!("idn-captions: {e}");
                    last_error = Some(e);
                }
            }
        }
        if said_at.elapsed() >= Duration::from_secs(60) {
            said_at = Instant::now();
            eprintln!(
                "idn-captions: load {:.2} (above 1: this machine can't keep up; a smaller model)",
                engine.load
            );
        }
    }
    // Stopped: the engine dropped first (its worker's input closed: it ends, and is
    // reaped, within a second), then the source (its goodbyes) and the input.
    eprintln!("idn-captions: stopping");
    drop(engine);
    drop(out);
    drop(publisher);
    let _ = listening.join();
    eprintln!("idn-captions: stopped");
    finished.store(true, Ordering::SeqCst);
}
