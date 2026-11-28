//! Captions a WAV file (16-bit PCM) as the program would hear it live, a second at a
//! time: `caption_file MODEL MODELS_DIR FILE.wav [cpu|auto|any|N|NAME]` (the model
//! downloaded if missing; on the GPU the choice gives, `auto` by default: a card of its
//! own, else the CPU; `any` (or `integrated`) for any GPU, a built-in one too, a card
//! still first; `cpu`, any case, on the CPU).

use idn_captions::{Engine, EngineConfig, GpuDevice, ensure_model};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (model, dir, wav) = (&a[1], std::path::Path::new(&a[2]), &a[3]);
    let t = Instant::now();
    let mut said = 0;
    let path = ensure_model(dir, model, &mut |n, total| {
        if n / 50_000_000 != said {
            said = n / 50_000_000;
            eprintln!("downloading {} of {:?} MB", n >> 20, total.map(|t| t >> 20));
        }
    })
    .unwrap();
    eprintln!(
        "model {} ({} MB) ready in {:.1} s",
        path.display(),
        std::fs::metadata(&path).unwrap().len() >> 20,
        t.elapsed().as_secs_f64()
    );
    let b = std::fs::read(wav).unwrap();
    let rate = u32::from_le_bytes(b[24..28].try_into().unwrap());
    let ch = u16::from_le_bytes(b[22..24].try_into().unwrap()) as usize;
    let data = b.windows(4).position(|w| w == b"data").unwrap() + 8;
    let samples: Vec<f32> = b[data..]
        .chunks_exact(2 * ch)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    let cfg = EngineConfig {
        gpu: a
            .get(4)
            .is_none_or(|x| !x.trim().eq_ignore_ascii_case("cpu")),
        gpu_device: GpuDevice::parse(a.get(4).map_or("auto", String::as_str)),
        ..Default::default()
    };
    let mut e = Engine::new(&path, cfg).unwrap();
    let pass = e.measure().unwrap();
    eprintln!("on {}; a pass: {pass:?}", e.device);
    let second = rate as usize;
    let started = Instant::now();
    // A second of sound at a time, then a second and a half of quiet to end the line.
    let mut all = samples.clone();
    all.extend(std::iter::repeat_n(0.0, second * 3 / 2));
    for (k, chunk) in all.chunks(second).enumerate() {
        e.push(chunk, rate);
        for l in e.step().unwrap() {
            println!(
                "{:>4.1} s  {} line {}: {}",
                (k + 1) as f64,
                if l.is_final { "FINAL" } else { "draft" },
                l.line,
                l.text
            );
        }
    }
    println!(
        "load {:.2} (work per second of sound), {:.1} s in all",
        e.load,
        started.elapsed().as_secs_f64()
    );
}
