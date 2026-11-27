//! The output API as an application uses it: several outputs in one process, fed RGBA.

use crate::common;
use common::idn;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn solid(w: usize, h: usize, px: [u8; 4]) -> Arc<[u8]> {
    px.iter()
        .copied()
        .cycle()
        .take(w * h * 4)
        .collect::<Vec<u8>>()
        .into()
}

fn recv(port: u16, out: &std::path::Path) -> std::process::Child {
    idn("recv")
        .args([
            &format!("127.0.0.1:{port}"),
            "--seconds",
            "3",
            "--max-frames",
            "3",
            "--no-reconnect",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn three_outputs_in_one_process() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let mut cfg = OutputConfig::new("Program");
    cfg.frame_rate = (25, 1);
    let program = Output::new(&publisher, cfg.clone()).unwrap();
    let lower = Output::new(
        &publisher,
        OutputConfig {
            name: "Lower third".into(),
            alpha: true,
            ..cfg.clone()
        },
    )
    .unwrap();
    let slide = Output::new(
        &publisher,
        OutputConfig {
            name: "Slide".into(),
            ..cfg
        },
    )
    .unwrap();
    assert!(program.name().ends_with("(Program)"));

    let dir = idn_common::testing::TempDir::new("idn-api");
    let (dp, dl, ds) = (dir.join("p"), dir.join("l"), dir.join("s"));
    let (mut rp, mut rl, mut rs) = (
        recv(program.port(), &dp),
        recv(lower.port(), &dl),
        recv(slide.port(), &ds),
    );

    let (w, h) = (320, 180);
    // Premultiplied half-transparent red: (128, 0, 0, 128) is straight red at A 128.
    let lt = solid(w, h, [128, 0, 0, 128]);
    let still = solid(w, h, [0, 0, 255, 255]);
    let start = Instant::now();
    let mut n = 0u32;
    // Sending until the receivers have had their three seconds (on a busy machine one
    // connects late: a fixed 2.8 s gave it 48 pictures, under the 50 asked), at most 8 s.
    while start.elapsed() < Duration::from_secs(8)
        && [&mut rp, &mut rl, &mut rs]
            .iter_mut()
            .any(|c| c.try_wait().ok().flatten().is_none())
    {
        let moving = solid(w, h, [(n * 5 % 256) as u8, 255, 0, 255]);
        program
            .send(VideoFrame::rgba(w, h, moving, AlphaMode::Opaque))
            .unwrap();
        lower
            .send(VideoFrame::rgba(w, h, lt.clone(), AlphaMode::Premultiplied))
            .unwrap();
        // A new buffer each time with the same content, as a host re-rendering a slide would.
        slide
            .send(VideoFrame::rgba(
                w,
                h,
                still.to_vec().into(),
                AlphaMode::Opaque,
            ))
            .unwrap();
        n += 1;
        std::thread::sleep(Duration::from_millis(40));
    }
    let outs: Vec<String> = [rp, rl, rs]
        .into_iter()
        .map(|c| String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout).into_owned())
        .collect();
    for o in &outs {
        let decoded: usize = o
            .lines()
            .find(|l| l.contains("video messages decoded"))
            .and_then(|l| l.split(':').nth(1)?.split_whitespace().next()?.parse().ok())
            .unwrap_or(0);
        // (25 a second sent for 3 s; on a busy machine, the full run side by side, the
        // sending thread fell behind, 45 in the 3 s once.)
        assert!(decoded >= 35, "{o}");
    }
    assert!(outs[1].contains("SHQ7 320x180"), "{}", outs[1]);
    assert!(outs[0].contains("SHQ2 320x180") && outs[2].contains("SHQ2 320x180"));
    // Lower third: straight red on the wire (BT.709 limited Y 63, Cb 102, Cr 240), alpha 128.
    let y4m = std::fs::read(dl.join("video-0.y4m")).unwrap();
    let f = y4m.windows(6).position(|x| x == b"FRAME\n").unwrap() + 6;
    let (y, cb, cr) = (y4m[f], y4m[f + w * h], y4m[f + w * h + w / 2 * h]);
    assert!(
        (y as i32 - 63).abs() <= 1 && (cb as i32 - 102).abs() <= 1 && (cr as i32 - 240).abs() <= 1,
        "{y} {cb} {cr}"
    );
    let alpha = std::fs::read(dl.join("alpha-0.y4m")).unwrap();
    let fa = alpha.windows(6).position(|x| x == b"FRAME\n").unwrap() + 6;
    assert_eq!(alpha[fa], 128);
    // The slide's repeats were detected; the moving program never repeats.
    assert!(
        slide.stats().repeated as u32 >= n - 2,
        "{:?}",
        slide.stats()
    );
    assert_eq!(program.stats().repeated, 0);
}

#[test]
fn send_never_blocks() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Burst")).unwrap();
    let f = VideoFrame::rgba(
        1920,
        1080,
        solid(1920, 1080, [1, 2, 3, 255]),
        AlphaMode::Opaque,
    );
    let t = Instant::now();
    for _ in 0..10_000 {
        out.send(f.clone()).unwrap();
    }
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    // Frames are replaced in the mailbox, never queued: each one was either replaced or
    // taken by the worker (a repeat of the first, so cheap that it takes many).
    std::thread::sleep(Duration::from_millis(100));
    let s = out.stats();
    assert_eq!(s.submitted, 10_000);
    assert_eq!(s.replaced + s.repeated + 1, s.submitted, "{s:?}");
    assert!(s.replaced > 5_000, "{s:?}");
    // Inconsistent buffers are rejected as values, not panics.
    let bad = VideoFrame::rgba(1920, 1080, solid(10, 10, [0; 4]), AlphaMode::Opaque);
    assert!(out.send(bad).is_err());
}

#[test]
fn format_changes_reach_connected_receivers() {
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let mut cfg = OutputConfig::new("Changing");
    cfg.frame_rate = (25, 1);
    let out = Arc::new(Output::new(&publisher, cfg).unwrap());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The first format's frames the receiver has had: the change waits for a few (on a
    // busy machine it connected after 40 frames had gone, and saw none before).
    let had = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let feeder = {
        let (out, stop, had) = (out.clone(), stop.clone(), had.clone());
        std::thread::spawn(move || {
            let (mut n, mut changed) = (0usize, false);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // After the receiver has seen the first format for a while: alpha on,
                // 50 fps, and a new size, all while it stays connected.
                if !changed
                    && n >= 40
                    && (had.load(std::sync::atomic::Ordering::Relaxed) >= 5 || n >= 400)
                {
                    changed = true;
                    out.set_alpha(true);
                    out.set_frame_rate((50, 1));
                }
                let (w, h) = if changed { (640, 360) } else { (320, 180) };
                let px = [(n % 200) as u8, 64, 32, 128];
                let _ = out.send(VideoFrame::rgba(w, h, solid(w, h, px), AlphaMode::Straight));
                n += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let (mut before, mut after) = (0, 0);
    let mut last_tc = i64::MIN;
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && after < 10 {
        let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(200)) else {
            continue;
        };
        let hd = &v.header;
        assert!(hd.timecode > last_tc, "timecodes keep increasing");
        last_tc = hd.timecode;
        match (&hd.fourcc, hd.width) {
            (b"SHQ2", 320) => {
                assert_eq!((hd.frame_rate_n, hd.frame_rate_d, after), (25, 1, 0));
                before += 1;
                had.store(before, std::sync::atomic::Ordering::Relaxed);
            }
            (b"SHQ7", 640) => {
                assert_eq!((hd.frame_rate_n, hd.frame_rate_d, hd.height), (50, 1, 360));
                assert_eq!(v.picture.planes.len(), 4, "alpha plane");
                after += 1;
            }
            // The frame being converted when the format changed may mix old and new.
            _ => {}
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    feeder.join().unwrap();
    assert!(
        before > 3 && after >= 10,
        "{before} frames before, {after} after the change"
    );
    assert_eq!(
        input.stats().reconnects,
        0,
        "no reconnect for a format change"
    );
}

#[test]
fn audio_on_its_own_reaches_receivers_with_continuous_timecodes() {
    use idn_open_protocol::api::{AudioBlock, SendError};
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Audio only")).unwrap());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    let block = |k: usize| AudioBlock {
        sample_rate: 48_000,
        channels: vec![vec![0.25; 480], vec![-0.25 + k as f32 * 1e-3; 480]],
    };
    assert!(matches!(
        out.send_audio(
            AudioBlock {
                sample_rate: 48_000,
                channels: vec![vec![0.0; 4], vec![0.0; 5]]
            },
            None
        ),
        Err(SendError::BadAudio(_))
    ));
    let feeder = {
        let out = out.clone();
        std::thread::spawn(move || {
            // 10 ms blocks, as an audio callback would deliver them, and no video at all.
            for k in 0..400 {
                out.send_audio(block(k), None).unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let mut got: Vec<(i64, usize)> = Vec::new();
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && got.len() < 50 {
        match input.recv(Duration::from_millis(200)) {
            Some(InputEvent::Audio(a)) => {
                assert_eq!((a.header.sample_rate, a.channels.len()), (48_000, 2));
                assert!((a.channels[0][0] - 0.25).abs() < 1e-3);
                got.push((a.header.timecode, a.channels[0].len()));
            }
            Some(InputEvent::Video(_)) => panic!("no video was sent"),
            _ => {}
        }
    }
    feeder.join().unwrap();
    assert!(got.len() >= 50, "{} audio blocks", got.len());
    // Consecutive blocks are 480 samples = 100 000 × 100 ns apart. (The feeding thread held
    // up on a busy machine, the full run side by side, sends late: the next block's time
    // follows the clock, a jump forward (51 ms once). A couple of those; never back.)
    let mut jumps = 0;
    for w in got.windows(2) {
        let step = w[1].0 - w[0].0;
        assert!(step >= 100_000, "{got:?}");
        jumps += (step != 100_000) as usize;
        assert_eq!(w[0].1, 480);
    }
    assert!(jumps <= 2, "{jumps} jumps: {got:?}");
}

#[test]
fn metadata_goes_both_ways() {
    use idn_open_protocol::api::SendError;
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Meta")).unwrap());
    assert!(matches!(
        out.send_metadata("not xml"),
        Err(SendError::BadMetadata(_))
    ));
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while out.receivers() == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    input.send_metadata(r#"<app_hello from="receiver"/>"#);
    let mut got_out = false;
    let mut got_in = false;
    let mut k = 0;
    while Instant::now() < end && !(got_out && got_in) {
        // Repeat until seen: the receiver may still be completing its handshake.
        out.send_metadata(&format!(r#"<app_slide n="{k}">Amazing grace</app_slide>"#))
            .unwrap();
        k += 1;
        if let Some(InputEvent::Metadata { kind: 3, xml }) = input.recv(Duration::from_millis(100))
        {
            got_in |= xml.starts_with("<app_slide") && xml.contains("Amazing grace");
        }
        got_out |= out
            .recv_metadata()
            .iter()
            .any(|(_, m)| m == r#"<app_hello from="receiver"/>"#);
    }
    assert!(got_in, "the receiver got the output's metadata");
    assert!(got_out, "the output got the receiver's metadata");
}

#[test]
fn video_and_audio_timecodes_share_a_clock_across_pauses() {
    use idn_open_protocol::api::AudioBlock;
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let mut cfg = OutputConfig::new("Clock");
    cfg.frame_rate = (25, 1);
    let out = Arc::new(Output::new(&publisher, cfg).unwrap());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    // Audio starts first; video starts later and pauses twice (as while a host loads a
    // new scene). Neither may shift video timecodes against audio.
    let audio = {
        let out = out.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            for k in 0..800u64 {
                let b = AudioBlock {
                    sample_rate: 48_000,
                    channels: vec![vec![0.1; 480]],
                };
                out.send_audio(b, None).unwrap();
                let due = start + Duration::from_millis(10 * (k + 1));
                if let Some(d) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        })
    };
    let video = {
        let out = out.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            // Paced by deadlines, as a real frame clock is: sleeping 40 ms after each send
            // made the period 40 ms plus the send, and the timecodes rightly followed that.
            let mut next = Instant::now();
            for n in 0..100usize {
                if n == 30 || n == 60 {
                    std::thread::sleep(Duration::from_millis(400));
                    next = Instant::now();
                }
                let px = [(n * 2) as u8, 0, 0, 255];
                let _ = out.send(VideoFrame::rgba(
                    64,
                    36,
                    solid(64, 36, px),
                    AlphaMode::Opaque,
                ));
                next += Duration::from_millis(40);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        })
    };
    // For each video frame, the latest audio timecode received before it.
    let mut audio_tc: Option<i64> = None;
    let mut offsets = Vec::new();
    let mut video_tcs = Vec::new();
    let end = Instant::now() + Duration::from_secs(9);
    while Instant::now() < end && !video.is_finished() {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Audio(a)) => audio_tc = Some(a.header.timecode),
            Some(InputEvent::Video(v)) => {
                video_tcs.push(v.header.timecode);
                if let Some(a) = audio_tc {
                    offsets.push((v.header.timecode - a) / 10_000); // ms
                }
            }
            _ => {}
        }
    }
    video.join().unwrap();
    audio.join().unwrap();
    // (25 a second for the run; on a busy machine, the full run side by side, the sending
    // thread fell behind, 43 frames once: enough to see the clocks agree.)
    assert!(offsets.len() > 30, "{} video frames", offsets.len());
    // (The first few frames left out: on a busy machine the first audio block can trail
    // the first picture while the connection settles; it is the steady state that shares
    // the clock.)
    let steady = &offsets[5..];
    let (lo, hi) = (steady.iter().min().unwrap(), steady.iter().max().unwrap());
    assert!(
        *lo > -100 && *hi < 100,
        "video − audio timecode stays near 0 ms: {lo}..{hi} {offsets:?}"
    );
    // Steady stretches are evenly spaced at the frame duration (40 ms).
    let steps = video_tcs
        .windows(2)
        .filter(|w| w[1] - w[0] == 400_000)
        .count();
    assert!(steps > video_tcs.len() * 3 / 4, "{video_tcs:?}");
}

#[test]
fn attached_audio_starts_at_its_frames_timecode_and_own_timecodes_pass_through() {
    use idn_open_protocol::api::AudioBlock;
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let mut cfg = OutputConfig::new("AV");
    cfg.frame_rate = (30, 1);
    let out = Arc::new(Output::new(&publisher, cfg).unwrap());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    // The connection up first (pictures without sound until the receiver has one, then a
    // moment's quiet): on a busy machine, the full run side by side, it joined so late
    // that half the tagged frames had gone (21 of the 40 needed).
    let primed = Instant::now() + Duration::from_secs(20);
    let mut up = false;
    while !up && Instant::now() < primed {
        let _ = out.send(VideoFrame::rgba(
            64,
            36,
            solid(64, 36, [200, 0, 0, 255]),
            AlphaMode::Opaque,
        ));
        up = matches!(
            input.recv(Duration::from_millis(100)),
            Some(InputEvent::Video(_))
        );
    }
    assert!(up, "the receiver connected");
    while input.recv(Duration::from_millis(300)).is_some() {}
    let feeder = {
        let out = out.clone();
        std::thread::spawn(move || {
            for n in 0..90i64 {
                let mut f = VideoFrame::rgba(
                    64,
                    36,
                    solid(64, 36, [n as u8, 0, 0, 255]),
                    AlphaMode::Opaque,
                );
                f.audio = Some(AudioBlock {
                    sample_rate: 48_000,
                    channels: vec![vec![n as f32 / 100.0; 1600]],
                });
                // The second half on the caller's own timeline.
                if n >= 45 {
                    f.timecode = Some(1_000_000_000 + n * 333_333);
                }
                let _ = out.send(f);
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let mut video = std::collections::BTreeMap::new(); // arrival order -> timecode
    let mut audio = std::collections::BTreeMap::new(); // sample level -> timecode
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && !feeder.is_finished() || audio.len() < 40 && Instant::now() < end
    {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Video(v)) => {
                video.insert(video.len(), v.header.timecode);
            }
            Some(InputEvent::Audio(a)) => {
                let tag = (a.channels[0][0] * 100.0).round() as i64;
                audio.insert(tag, (a.header.timecode, a.channels[0].len()));
            }
            _ => {}
        }
    }
    feeder.join().unwrap();
    let vtcs: std::collections::BTreeSet<i64> = video.values().copied().collect();
    let (mut matched, mut no_frame) = (0, Vec::new());
    for (tag, (tc, len)) in &audio {
        // 1600 per frame; a multiple when the output fell behind and a replaced frame's
        // audio went out ahead of the next frame's (keeping its own, earlier, time).
        assert!(*len > 0 && len % 1600 == 0, "audio {tag}: {len} samples");
        // Each frame's own audio (the last 1600 samples) starts exactly at a video
        // frame's timecode.
        // (Its frame may have been dropped by the receiver when the machine was busy, the
        // full run side by side: a few such are allowed; the alignment of the rest is the
        // test.)
        let own = tc + (*len as i64 - 1600) * 10_000_000 / 48_000;
        // (The first frames' too: on a busy machine the receiver joins a few frames in.)
        if !vtcs.contains(&own) && *tag >= 10 {
            no_frame.push(format!(
                "audio {tag} at {tc} ({len} samples): no frame at {own}"
            ));
        }
        if *tag >= 45 {
            assert_eq!(*tc, 1_000_000_000 + tag * 333_333, "caller's timecode kept");
        }
        matched += 1;
    }
    assert!(matched >= 40, "{matched} audio blocks");
    assert!(no_frame.len() <= 3, "{no_frame:?}");
}
