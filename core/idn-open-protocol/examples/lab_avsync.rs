//! Lab script for lip sync: 1080p30 with 48 kHz stereo; three white flashes 1.5 s apart,
//! each with a 1 kHz beep (100 ms, −12 dBFS) starting at the same media moment.
//! Run: AV_MODE=<mode> lab_avsync [--name NAME]
//!
//! Media timeline: frame n is at n/30 s, audio sample s at s/48000 s; flashes on frames
//! 30, 75 and 120 (1.0, 2.5, 4.0 s), each beep starting at sample 1600·n.
//!
//! Modes (expected beep timecode − flash timecode at the receiver):
//! - `attached` (default): each frame carries its own 1600 samples. Expected 0.
//! - `audio-none`: video without audio; audio with `send_audio(block, None)` in 10 ms
//!   blocks, both paced in real time from one start instant. Expected 0 within about
//!   ±10 ms (both on the output's clock).
//! - `audio-tc`: as `audio-none`, but video sets `VideoFrame::timecode` and audio passes
//!   `Some(tc)`, both media time. Expected exactly 0.
//! - `replace`: attached audio and media-time video timecodes, but frames go in bursts of
//!   three (every 100 ms), so the middle one of each burst is normally replaced before
//!   sending and its audio is carried into the next. The beep starts at a middle frame
//!   (31, 76, 121) while the flash is on the frame after it (32, 77, 122): expected
//!   −33.3 ms (the beep really is a frame earlier). Before the carried-audio fix this
//!   read 0. Audio timecodes should also be contiguous: each block's timecode plus its
//!   length equals the next block's, to within 1 (100 ns, from integer rounding). The number of frames replaced is printed at the end.

use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use std::sync::Arc;
use std::time::{Duration, Instant};

const RATE: i64 = 48_000;
const PER_FRAME: i64 = 1600; // samples per frame at 30 fps
const FRAMES: i64 = 165; // 5.5 s

/// The samples `from..from + n` of the media timeline (stereo).
fn samples(from: i64, n: i64, beep_frames: &[i64]) -> Vec<Vec<f32>> {
    let ch: Vec<f32> = (from..from + n)
        .map(|s| {
            let on = beep_frames
                .iter()
                .any(|&f| (f * PER_FRAME..f * PER_FRAME + RATE / 10).contains(&s));
            if on {
                // 1 kHz, −12 dBFS, phase 0 at the beep's first sample.
                let t = (s % PER_FRAME) as f64 / RATE as f64;
                (0.25 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as f32
            } else {
                0.0
            }
        })
        .collect();
    vec![ch.clone(), ch]
}

fn media_tc_frame(n: i64) -> i64 {
    n * 10_000_000 / 30
}

fn media_tc_sample(s: i64) -> i64 {
    s * 10_000_000 / RATE
}

fn main() {
    let mode = std::env::var("AV_MODE").unwrap_or_else(|_| "attached".into());
    let mut name = "AV Sync".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next().expect("--name NAME"),
            _ => {
                eprintln!(
                    "usage: AV_MODE=attached|audio-none|audio-tc|replace lab_avsync [--name NAME]"
                );
                std::process::exit(2);
            }
        }
    }
    let (flash, beep): (Vec<i64>, Vec<i64>) = match mode.as_str() {
        "attached" | "audio-none" | "audio-tc" => (vec![30, 75, 120], vec![30, 75, 120]),
        "replace" => (vec![32, 77, 122], vec![31, 76, 121]),
        m => {
            eprintln!("unknown AV_MODE {m}");
            std::process::exit(2);
        }
    };
    let (w, h) = (1920, 1080);
    let black: Arc<[u8]> = [16u8, 16, 16, 255].repeat(w * h).into();
    let white: Arc<[u8]> = [255u8; 4].repeat(w * h).into();
    let mut cfg = OutputConfig::new(&name);
    cfg.frame_rate = (30, 1);
    let publisher = Publisher::new(PublisherConfig::default()).expect("publisher");
    let out = Arc::new(Output::new(&publisher, cfg).expect("output"));
    println!("{} on TCP port {}, mode {mode}", out.name(), out.port());
    println!("flash frames {flash:?}, beeps start at frames {beep:?} (media time n/30 s)");
    // Wait for a receiver, then a little more so it is fully set up.
    while out.receivers() == 0 {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_secs(2));
    println!("receiver connected; starting");

    let picture = |n: i64| {
        if flash.contains(&n) {
            white.clone()
        } else {
            black.clone()
        }
    };
    let start = Instant::now() + Duration::from_millis(100);
    let at = |secs: f64| start + Duration::from_secs_f64(secs);
    let sleep_until = |t: Instant| {
        if let Some(d) = t.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    };
    let separate_audio = mode == "audio-none" || mode == "audio-tc";
    let audio_thread = separate_audio.then(|| {
        let (out, beep, own_tc) = (out.clone(), beep.clone(), mode == "audio-tc");
        std::thread::spawn(move || {
            let block = RATE / 100;
            let mut s = 0;
            while s < FRAMES * PER_FRAME {
                // Each block goes out when its first sample is due.
                if let Some(d) = (start + Duration::from_secs_f64(s as f64 / RATE as f64))
                    .checked_duration_since(Instant::now())
                {
                    std::thread::sleep(d);
                }
                let b = AudioBlock {
                    sample_rate: RATE as u32,
                    channels: samples(s, block, &beep),
                };
                out.send_audio(b, own_tc.then(|| media_tc_sample(s)))
                    .expect("audio");
                s += block;
            }
        })
    });
    let frame = |n: i64| {
        let mut f = VideoFrame::rgba(w, h, picture(n), AlphaMode::Opaque);
        if !separate_audio {
            f.audio = Some(AudioBlock {
                sample_rate: RATE as u32,
                channels: samples(n * PER_FRAME, PER_FRAME, &beep),
            });
        }
        if mode == "audio-tc" || mode == "replace" {
            f.timecode = Some(media_tc_frame(n));
        }
        f
    };
    if mode == "replace" {
        // Bursts of three back to back, every three frame periods.
        for n0 in (0..FRAMES).step_by(3) {
            sleep_until(at(n0 as f64 / 30.0));
            for n in n0..(n0 + 3).min(FRAMES) {
                out.send(frame(n)).expect("video");
            }
        }
    } else {
        for n in 0..FRAMES {
            sleep_until(at(n as f64 / 30.0));
            out.send(frame(n)).expect("video");
        }
    }
    if let Some(t) = audio_thread {
        t.join().unwrap();
    }
    std::thread::sleep(Duration::from_millis(500));
    let st = out.stats();
    println!(
        "done: frames submitted {}, replaced {}",
        st.submitted, st.replaced
    );
}
