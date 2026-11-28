//! `idn-codec convert`: a media file made into an MP4 every player takes
//! (`idn_ffmpeg::media::convert`).

use super::*;
use idn_ffmpeg::media::{ConvertConfig, ConvertProgress, Deinterlace};
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set on Ctrl+C or SIGTERM (Windows: console control events): the conversion stops.
static CANCEL: AtomicBool = AtomicBool::new(false);
/// Set once this program has said how it ended: the signal hook stops waiting.
static FINISHED: AtomicBool = AtomicBool::new(false);

/// `convert IN OUT`: `IN` converted into the MP4 `OUT` (H.264 or HEVC 8-bit 4:2:0 at a
/// constant frame rate, AAC stereo), at a low priority. Progress on stderr once a second,
/// what was made on stdout at the end. Ctrl+C stops it: "cancelled", no output and no
/// `OUT.part`, exit status 130. Exit status 1 when it fails, 2 for wrong arguments.
pub(super) fn convert(mut args: idn_open_protocol::cli::Args) {
    let mut cfg = ConvertConfig::default();
    let (mut input, mut output) = (None::<String>, None::<String>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fps" => {
                let v: String = args.value("--fps");
                cfg.frame_rate = Some(parse_rate(&v).unwrap_or_else(|| {
                    args.fail("--fps N, N/D or a decimal (25, 30000/1001, 29.97)")
                }));
            }
            "--max" => {
                let v: String = args.value("--max");
                cfg.max_size = Some(
                    idn_common::text::parse_size(&v).unwrap_or_else(|| args.fail("--max WxH")),
                );
            }
            "--deinterlace" => {
                let v: String = args.value("--deinterlace");
                cfg.deinterlace = match v.as_str() {
                    "auto" => Deinterlace::Auto,
                    "on" => Deinterlace::On,
                    "off" => Deinterlace::Off,
                    _ => args.fail("--deinterlace auto, on or off"),
                };
            }
            "--cpu" => cfg.gpu = false,
            "--codec" => {
                let v: String = args.value("--codec");
                cfg.codec = match v.as_str() {
                    "h264" => Codec::H264,
                    "hevc" => Codec::Hevc,
                    _ => args.fail("--codec h264 or hevc"),
                };
            }
            "--kbps" => {
                let k: u32 = args.value("--kbps");
                if k == 0 {
                    args.fail("--kbps: more than 0");
                }
                cfg.video_kbps = Some(k);
            }
            "--audio-kbps" => {
                cfg.audio_kbps = args.value("--audio-kbps");
                if !(32..=512).contains(&cfg.audio_kbps) {
                    args.fail("--audio-kbps 32 to 512");
                }
            }
            "--audio-rate" => {
                cfg.audio_rate = args.value("--audio-rate");
                if ![44_100, 48_000, 96_000].contains(&cfg.audio_rate) {
                    args.fail("--audio-rate 44100, 48000 or 96000");
                }
            }
            "--normal-priority" => cfg.low_priority = false,
            s if s.starts_with("--") => args.fail(&format!("unexpected argument: {s}")),
            s if input.is_none() => input = Some(s.to_owned()),
            s if output.is_none() => output = Some(s.to_owned()),
            _ => args.help(),
        }
    }
    let (Some(input), Some(output)) = (input, output) else {
        args.help()
    };
    if !output.to_lowercase().ends_with(".mp4") {
        eprintln!("idn codec convert: {output} is written as MP4 whatever its name");
    }

    // Ctrl+C or SIGTERM: the conversion is cancelled and its partial file removed before
    // the process ends. The shutdown handler (installed in main) runs this hook for at
    // most two seconds, then ends the process; the hook waits for this program to finish
    // within that time.
    let _cancel_on_signal = idn_open_protocol::shutdown::register(|| {
        CANCEL.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_millis(1800);
        while !FINISHED.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let ff = Ffmpeg::probe();
    eprintln!("converting {input} to {output}");
    let start = Instant::now();
    let mut last = None::<Instant>;
    let shown = |p: &ConvertProgress| {
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        last = Some(Instant::now());
        let secs = start.elapsed().as_secs_f64().max(1e-3);
        let pct = p
            .fraction
            .map_or(String::new(), |f| format!("{:.1} %, ", f * 100.0));
        let of = p
            .total
            .map_or(String::new(), |t| format!(" of {}", clock(t)));
        eprintln!(
            "progress: {pct}{}{of}, {} frames, {:.1} fps, {:.2}x real time",
            clock(p.done),
            p.frames,
            p.frames as f64 / secs,
            p.speed
        );
    };
    let r = idn_ffmpeg::media::convert(&input, &output, &cfg, &ff, shown, &CANCEL);
    match r {
        Ok(r) => {
            let fps = r.frame_rate.0 as f64 / r.frame_rate.1.max(1) as f64;
            let took = r.took.as_secs_f64().max(1e-3);
            println!("output: {output}");
            if r.encoder.is_empty() {
                println!("video: none");
            } else {
                println!("encoder: {}", r.encoder);
                println!("codec: {}", r.codec.name());
                println!("size: {}x{}", r.width, r.height);
                println!(
                    "frame rate: {}/{} ({fps:.3} fps)",
                    r.frame_rate.0, r.frame_rate.1
                );
                println!(
                    "frames: {} (repeated {}, dropped {}, deinterlaced {})",
                    r.frames, r.repeated, r.dropped, r.deinterlaced
                );
            }
            println!(
                "audio: {}",
                if r.audio {
                    format!("aac {} Hz stereo", cfg.audio_rate)
                } else {
                    "none".into()
                }
            );
            println!("duration: {:.3} s", r.duration.as_secs_f64());
            println!(
                "took: {took:.2} s ({:.2}x real time)",
                r.duration.as_secs_f64() / took
            );
            for n in &r.notes {
                println!("note: {n}");
            }
            finish(0)
        }
        Err(e) if CANCEL.load(Ordering::SeqCst) || e.kind() == ErrorKind::Interrupted => {
            // The library removes `OUT.part`; again here in case that failed.
            let mut part = std::ffi::OsString::from(&output);
            part.push(".part");
            let _ = std::fs::remove_file(&part);
            eprintln!("cancelled");
            finish(130)
        }
        Err(e) => {
            eprintln!("idn codec convert: {input}: {e}");
            finish(1)
        }
    }
}

/// Ends the process with `code` once its last line is out, the signal hook released.
fn finish(code: i32) -> ! {
    FINISHED.store(true, Ordering::SeqCst);
    std::process::exit(code)
}

/// `N`, `N/D` or a decimal; the NTSC rates (23.976, 29.97, 59.94, …) as N×1000/1001.
fn parse_rate(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let r = if let Some((n, d)) = s.split_once('/') {
        (n.trim().parse().ok()?, d.trim().parse().ok()?)
    } else if let Ok(n) = s.parse::<u32>() {
        (n, 1)
    } else {
        let x: f64 = s.parse().ok()?;
        if !x.is_finite() || x <= 0.0 || x > 1000.0 {
            return None;
        }
        let k = (x * 1.001).round();
        if k >= 1.0 && (k / 1.001 - x).abs() < 0.005 && (k - x).abs() > 0.01 {
            (k as u32 * 1000, 1001)
        } else {
            ((x * 1000.0).round() as u32, 1000)
        }
    };
    (r.0 > 0 && r.1 > 0 && r.0 / r.1 <= 240).then_some(r)
}

/// `H:MM:SS.s` or `M:SS.s`.
fn clock(d: Duration) -> String {
    let s = d.as_secs_f64();
    let (h, m, sec) = ((s / 3600.0) as u64, ((s % 3600.0) / 60.0) as u64, s % 60.0);
    if h > 0 {
        format!("{h}:{m:02}:{sec:04.1}")
    } else {
        format!("{m}:{sec:04.1}")
    }
}
