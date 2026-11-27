//! Decodes an H.264/HEVC Annex B file (with access unit delimiters) through the adapter
//! as an input would, and reports pictures per second and the time from an access unit
//! in to its picture out, for the GPU and software decoders.
//! Run: cargo run --release -p idn-ffmpeg --example decode_bench -- FILE [--hevc]
//!      [--gpu | --software] [--fps N (feed at the stream's rate; default: as fast as possible)]
//! Default decoder: automatic (software, the GPU if software falls behind; fed as fast as
//! possible it always is). `IDN_GPU_DOWNLOADS=1` downloads in series, for comparison.
use idn_open_protocol::codec::{Codec, DecoderFactory};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    idn_ffmpeg::keep_frame_memory();
    let args: Vec<String> = std::env::args().collect();
    let file = args
        .get(1)
        .expect("usage: decode_bench FILE [--hevc] [--software]");
    let hevc = args.iter().any(|a| a == "--hevc");
    let mut ff = idn_ffmpeg::Ffmpeg::probe();
    if args.iter().any(|a| a == "--software") {
        ff.set_decoder(idn_ffmpeg::DecoderChoice::Software);
    } else if args.iter().any(|a| a == "--gpu") {
        ff.set_decoder(idn_ffmpeg::DecoderChoice::Gpu);
    }
    let fps: Option<f64> = args
        .iter()
        .position(|a| a == "--fps")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    let aus = idn_open_protocol::hx::access_units(&std::fs::read(file)?, hevc);
    let mut dec = ff.open(if hevc { Codec::Hevc } else { Codec::H264 })?;
    let mut sent: std::collections::HashMap<i64, Instant> = Default::default();
    let (mut out, mut lat) = (0usize, Vec::new());
    let take = |p: Option<idn_open_protocol::codec::Decoded>,
                sent: &std::collections::HashMap<i64, Instant>,
                out: &mut usize,
                lat: &mut Vec<f64>| {
        if let Some(p) = p {
            *out += 1;
            if let Some(t) = p.pts.and_then(|pts| sent.get(&pts)) {
                lat.push(t.elapsed().as_secs_f64() * 1e3);
            }
        }
    };
    let start = Instant::now();
    for (i, au) in aus.iter().enumerate() {
        if let Some(f) = fps {
            let due = start + Duration::from_secs_f64(i as f64 / f);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        let data = idn_open_protocol::hx::with_parameter_sets(&au.data, &au.params, hevc);
        sent.insert(i as i64, Instant::now());
        let p = dec.decode(&data, au.keyframe, i as i64)?;
        take(p, &sent, &mut out, &mut lat);
    }
    // What is still downloading.
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        match dec.poll()? {
            Some(p) => take(Some(p), &sent, &mut out, &mut lat),
            None => std::thread::sleep(Duration::from_millis(2)),
        }
        if out == aus.len() {
            break;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    lat.sort_by(f64::total_cmp);
    let pct = |q: f64| {
        lat.get(((lat.len() as f64 - 1.0) * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    println!(
        "{} access units, {} pictures in {:.2} s = {:.1} pictures/s; in→out median {:.1} / p99 {:.1} ms",
        aus.len(),
        out,
        secs,
        out as f64 / secs,
        pct(0.5),
        pct(0.99)
    );
    Ok(())
}
