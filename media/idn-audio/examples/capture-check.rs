//! Lab tool: captures from a device through cpal alone (no resampling, no network) and
//! reports what arrived, to tell a device or cpal problem from an idn-audio one.
//!
//! `cargo run --release -p idn-audio --example capture-check -- [DEVICE] [SECONDS] [BUFFER_FRAMES]`
//!
//! Writes `capture-check.f32` (interleaved f32 at the device's rate and channels) in the
//! current directory and prints each callback's frame count when it changes, the total,
//! and the stream errors (xruns) cpal reported.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use idn_common::sync::LockOk as _;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn main() {
    let mut args = std::env::args().skip(1);
    let name = args.next().filter(|a| a != "-");
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);
    let fixed: Option<u32> = args.next().and_then(|a| a.parse().ok());
    let host = cpal::default_host();
    let device = match &name {
        None => host.default_input_device().expect("no default input"),
        Some(n) => host
            .devices()
            .unwrap()
            .filter(|d| d.default_input_config().is_ok())
            .find(|d| {
                let desc = d
                    .description()
                    .map(|d| d.name().to_owned())
                    .unwrap_or_default();
                let id = d.id().map(|i| i.to_string()).unwrap_or_default();
                desc.contains(n.as_str()) || id.contains(n.as_str())
            })
            .expect("no such input device"),
    };
    let cfg = device.default_input_config().unwrap();
    println!(
        "device {:?}: {} ch, {} Hz, {:?}, buffer {:?}",
        device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_default(),
        cfg.channels(),
        cfg.sample_rate(),
        cfg.sample_format(),
        cfg.buffer_size()
    );
    let config = cpal::StreamConfig {
        channels: cfg.channels(),
        sample_rate: cfg.sample_rate(),
        buffer_size: fixed.map_or(cpal::BufferSize::Default, cpal::BufferSize::Fixed),
    };
    let file = Arc::new(Mutex::new(std::io::BufWriter::new(
        std::fs::File::create("capture-check.f32").unwrap(),
    )));
    let frames = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let (f, fr, ch) = (file.clone(), frames.clone(), cfg.channels() as usize);
    let last = Arc::new(AtomicU64::new(0));
    let on_data = move |data: &[f32]| {
        let n = (data.len() / ch) as u64;
        if last.swap(n, Ordering::Relaxed) != n {
            println!("callback of {n} frames");
        }
        fr.fetch_add(n, Ordering::Relaxed);
        let bytes: Vec<u8> = data.iter().flat_map(|s| s.to_le_bytes()).collect();
        f.lock_ok().write_all(&bytes).unwrap();
    };
    let on_err = {
        let errors = errors.clone();
        move |e: cpal::Error| {
            errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("stream error: {e}");
        }
    };
    // The device's own sample format, converted to f32 for the file.
    let on_data = Arc::new(Mutex::new(on_data));
    macro_rules! build {
        ($t:ty) => {{
            let on_data = on_data.clone();
            device.build_input_stream::<$t, _, _>(
                config,
                move |data, _| {
                    use cpal::Sample;
                    let f: Vec<f32> = data.iter().map(|s| f32::from_sample(*s)).collect();
                    (on_data.lock_ok())(&f)
                },
                on_err,
                None,
            )
        }};
    }
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::U16 => build!(u16),
        _ => build!(f32),
    }
    .expect("build stream");
    stream.play().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(secs));
    drop(stream);
    file.lock_ok().flush().unwrap();
    println!(
        "{} frames in {secs} s ({:.1}/s), {} stream errors; capture-check.f32 written",
        frames.load(Ordering::Relaxed),
        frames.load(Ordering::Relaxed) as f64 / secs as f64,
        errors.load(Ordering::Relaxed)
    );
}
