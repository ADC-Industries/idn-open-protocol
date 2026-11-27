//! What goes out: ISO taps and recordings, the program recording's sound tracks, the
//! program output's on-air description, and HDR programs.

use super::*;

/// ISO tap: an input's picture every program frame, full size, at the program's rate,
/// stamped with rising program timestamps (so an ISO file lines up with the program).
#[test]
fn iso_tap_gets_every_program_frame() {
    let sw = Switcher::start(SwitcherConfig {
        name: "IsoTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let cam = sw.add_input("Cam", Source::Feed).unwrap();
    // Fed once: still handed over every frame (a constant frame rate file).
    sw.feed(cam, Arc::new(Frame::solid(W, H, Ycc::WHITE)))
        .unwrap();
    let got = Arc::new(Mutex::new(Vec::<(usize, usize, i64, (u32, u32))>::new()));
    let g = got.clone();
    let tap = sw
        .tap_input(cam, move |f| {
            g.lock()
                .unwrap()
                .push((f.width, f.height, f.timestamp, f.frame_rate))
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    drop(tap);
    let got = got.lock_ok().clone();
    assert!(
        got.len() >= 10,
        "about 18 frames in 0.6 s, got {}",
        got.len()
    );
    assert!(
        got.iter()
            .all(|&(w, h, _, fr)| (w, h) == (W, H) && fr == (30, 1))
    );
    assert!(got.windows(2).all(|p| p[1].2 > p[0].2), "timestamps rise");
    assert!(sw.tap_input(999, |_| {}).is_err());
}

/// ISO end to end on this machine's GPU (skipped without one): an input tapped into its own
/// gateway is recorded on its own file, at its own size, as long as it was recorded.
#[cfg(feature = "ffmpeg")]
#[test]
fn iso_recording_writes_the_input_on_its_own_file() {
    use idn_ffmpeg::Ffmpeg;
    use idn_ffmpeg::gateway::{Container, Destination, Gateway, State};
    use idn_open_protocol::codec::EncoderFactory;
    let ff = Arc::new(Ffmpeg::probe());
    if EncoderFactory::codecs(&*ff).is_empty() {
        println!("no hardware encoder here: skipped");
        return;
    }
    let sw = Switcher::start(SwitcherConfig {
        name: "IsoRecTest".into(),
        width: 1280,
        height: 720,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    // A camera smaller than the program: its ISO file keeps the camera's own size.
    let cam = sw.add_input("Cam", Source::Feed).unwrap();
    sw.feed(
        cam,
        Arc::new(Frame::solid(W, H, Ycc::from_rgb(30, 120, 200))),
    )
    .unwrap();
    let gw = Arc::new(Gateway::new(ff));
    let d = idn_common::testing::TempDir::new("idn-iso");
    let path = d.join("cam.mp4");
    let id = gw
        .add(Destination::File(path.clone(), Container::Mp4))
        .unwrap();
    let g = gw.clone();
    let tap = sw.tap_input(cam, move |f| g.send(f)).unwrap();
    wait_until(Duration::from_secs(10), || {
        !gw.status()
            .iter()
            .any(|s| s.id == id && s.state != State::Live)
    });
    std::thread::sleep(Duration::from_secs(3));
    drop(tap);
    gw.remove(id);
    std::thread::sleep(Duration::from_millis(500));
    let o = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height:format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    println!("{}: {text}", path.display());
    assert!(text.contains(&format!("h264,{W},{H}")), "{text}");
    let dur: f64 = text
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .next()
        .unwrap_or(0.0);
    assert!((2.0..4.5).contains(&dur), "about 3 s: {dur}");
}

/// The program recorded with extra sound tracks on this machine's GPU (skipped without
/// one): its pictures and the stream's sound, plus a track per chosen input — three tracks.
#[cfg(feature = "ffmpeg")]
#[test]
fn program_recording_with_sound_tracks() {
    use idn_ffmpeg::Ffmpeg;
    use idn_ffmpeg::gateway::{Container, Destination, Gateway, State, StreamSettings};
    use idn_open_protocol::codec::EncoderFactory;
    let ff = Arc::new(Ffmpeg::probe());
    if EncoderFactory::codecs(&*ff).is_empty() {
        println!("no hardware encoder here: skipped");
        return;
    }
    let sw = Switcher::start(SwitcherConfig {
        name: "TracksTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    sw.add_input("Prog", Source::Colour(Ycc::from_rgb(30, 120, 200)))
        .unwrap();
    let a = sw.add_input("Mic A", Source::Feed).unwrap();
    let b = sw.add_input("Mic B", Source::Feed).unwrap();
    let gw = Arc::new(Gateway::new(ff));
    let d = idn_common::testing::TempDir::new("idn-tracks");
    let path = d.join("program.mkv");
    let id = gw
        .add_with(
            Destination::File(path.clone(), Container::Mkv),
            StreamSettings {
                audio_tracks: 3,
                ..Default::default()
            },
            true,
        )
        .unwrap();
    let g = gw.clone();
    let sound = sw.tap_sound_tracks(&[a, b], move |f| g.send(f)).unwrap();
    let g = gw.clone();
    let pictures = sw.program_output().tap(move |f| {
        if !f.planes.is_empty() {
            let mut p = f.clone();
            p.audio.clear();
            g.send(&p);
        }
    });
    wait_until(Duration::from_secs(10), || {
        !gw.status()
            .iter()
            .any(|s| s.id == id && s.state != State::Live)
    });
    std::thread::sleep(Duration::from_secs(3));
    drop(pictures);
    drop(sound);
    gw.remove(id);
    std::thread::sleep(Duration::from_millis(500));
    let o = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    println!("{}: {}", path.display(), text.replace('\n', " "));
    assert!(text.lines().any(|l| l.trim() == "h264"), "{text}");
    assert_eq!(
        text.lines().filter(|l| l.trim() == "aac").count(),
        3,
        "{text}"
    );
    assert!(
        sw.tap_sound_tracks(&[1, 2, 3, 4, 5, 6, 7, 8], |_| {})
            .is_err()
    );
}

/// The tally message (§S.23): on the program output, for a mixer's audio follows video —
/// the program's scene and what it shows (a global layer shown in it too) counted as on
/// air; every input's label; sent again on a change.
#[test]
fn the_program_output_says_what_is_on_air() {
    let sw = Switcher::start(SwitcherConfig {
        name: "TallyTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let a = sw.add_input("Cam 1", Source::Colour(Ycc::WHITE)).unwrap();
    let b = sw.add_input("Cam 2", Source::Colour(Ycc::BLACK)).unwrap();
    let logo = sw
        .add_input("Logo & co", Source::Colour(Ycc(100, 128, 128)))
        .unwrap();
    let sc = sw.add_input("Two shot", Source::Scene).unwrap();
    sw.scene_items(
        sc,
        vec![Item {
            source: ItemSource::Input(b),
            ..Default::default()
        }],
    )
    .unwrap();
    let full = |id| Item {
        source: ItemSource::Input(id),
        ..Default::default()
    };
    // The logo a global layer: in the scenes made from now on, shown.
    sw.set_global_layer(
        Item {
            rect: (0.8, 0.0, 0.2, 0.2),
            ..full(logo)
        },
        false,
    )
    .unwrap();
    let main = sw.add_input("Main", Source::Scene).unwrap();
    let mut layers = sw.scene(main).unwrap();
    assert_eq!(layers.len(), 1, "the global layer's copy");
    layers.insert(0, full(a));
    sw.scene_items(main, layers).unwrap();
    sw.program(main).unwrap();
    let rx = Input::open(InputConfig {
        receive_video: false,
        audio_channels: Some(Vec::new()),
        link: Some(idn_open_protocol::link::LinkConfig::ephemeral()),
        ..InputConfig::new(&format!("127.0.0.1:{}", sw.program_output().port()))
    })
    .unwrap();
    let next = |want: &str, not: &str| -> String {
        let end = Instant::now() + Duration::from_secs(6);
        while Instant::now() < end {
            if let Some(InputEvent::Metadata { xml, .. }) = rx.recv(Duration::from_millis(100))
                && xml.contains(want)
                && (not.is_empty()
                    || idn_open_protocol::control::parse_element(&xml)
                        .and_then(|e| e.attr("on_air").map(|a| !a.contains(not)))
                        .unwrap_or(false))
            {
                return xml;
            }
        }
        panic!("no tally with {want}");
    };
    let x = next("idn_tally", "");
    let e = idn_open_protocol::control::parse_element(&x).unwrap();
    assert_eq!(e.attr("program"), Some("Main"));
    assert_eq!(e.attr("on_air"), Some("Cam 1\nLogo & co\nMain"));
    assert_eq!(
        e.attr("inputs"),
        Some("Cam 1\nCam 2\nLogo & co\nTwo shot\nMain")
    );
    // Another scene on program (made before the logo: without it): it and what it shows.
    sw.program(sc).unwrap();
    let x = next("program=\"Two shot\"", "Logo");
    let e = idn_open_protocol::control::parse_element(&x).unwrap();
    assert_eq!(e.attr("on_air"), Some("Cam 2\nTwo shot"));
}

/// An HDR program (R.24 stage 1): sent with its colour described (BT.2100 PQ, BT.2020,
/// 10 bits), and SDR white where BT.2408 puts it (58 % PQ), at high precision; HLG's at
/// 75 %.
#[test]
fn hdr_program_is_described_and_white_sits_at_bt2408() {
    hdr_program(false);
}

/// The same composed on the GPU (read back for the conversion; on the CPU without one).
#[test]
fn hdr_program_on_the_gpu() {
    hdr_program(true);
}

fn hdr_program(gpu: bool) {
    use idn_open_protocol::color::Transfer;
    for (transfer, name, want) in [
        (Transfer::Pq, "bt_2100_pq", 0.58),
        (Transfer::Hlg, "bt_2100_hlg", 0.75),
    ] {
        let sw = Switcher::start(SwitcherConfig {
            name: format!("HdrTest {name} {gpu}"),
            width: W,
            height: H,
            fps: (30, 1),
            mdns: Some(false),
            gpu,
            hdr: transfer,
            ..Default::default()
        })
        .unwrap();
        let white = sw.add_input("White", Source::Colour(Ycc::WHITE)).unwrap();
        scene_of(&sw, white);
        let input = Input::open(InputConfig {
            precise: true,
            ..InputConfig::new(&format!("127.0.0.1:{}", sw.program_output().port()))
        })
        .unwrap();
        // 16-bit limited: 64–940 (10-bit) × 64.
        let signal_of = |v: &idn_open_protocol::input::VideoIn| {
            let y = &v.picture.precise.as_ref().unwrap()[0];
            (y.data[y.data.len() / 2] as f64 / 64.0 - 64.0) / 876.0
        };
        // (The first pictures may be black: the program before the input is on it.)
        let end = Instant::now() + Duration::from_secs(8);
        let mut got = None;
        while Instant::now() < end && !got.as_ref().is_some_and(|v| signal_of(v) > 0.1) {
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50))
                && v.color.transfer == name
                && v.picture.precise.is_some()
            {
                got = Some(v);
            }
        }
        let v = got.unwrap_or_else(|| panic!("{name}: an HDR program picture, described"));
        assert_eq!(
            (
                v.color.primaries.as_str(),
                v.color.matrix.as_str(),
                v.color.bits
            ),
            ("bt_2020", "bt_2020", 10)
        );
        let signal = signal_of(&v);
        assert!(
            (signal - want).abs() < 0.015,
            "{name}: white at {signal:.3}, want {want}"
        );
        // Composited on the GPU: made HDR there too (not the CPU's fallback).
        let st = sw.state();
        eprintln!(
            "{name}: compositor {}, made HDR on the {}",
            st["compositor"], st["hdr_path"]
        );
        if gpu
            && st["compositor"]
                .as_str()
                .is_some_and(|c| c.starts_with("GPU"))
        {
            assert_eq!(st["hdr_path"], "gpu", "{name}: {}", st["compositor"]);
        }
        // A colour, against the exact conversion of its 8-bit Y'CbCr (within an 8-bit
        // step: 256 in 16 bits).
        let red = Ycc::from_rgb(200, 30, 30);
        let id = sw.add_input("Red", Source::Colour(red)).unwrap();
        sw.program(scene_of(&sw, id)).unwrap();
        let want =
            idn_open_protocol::color::to_hdr::to_hdr_exact(red.0, red.1, red.2, transfer, 203.0);
        let end = Instant::now() + Duration::from_secs(5);
        let mut seen = None;
        while Instant::now() < end {
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50))
                && let Some(p) = v.picture.precise.as_ref()
            {
                let mid = |k: usize| p[k].data[p[k].data.len() / 2] as i32;
                let got = [mid(0), mid(1), mid(2)];
                if (got[0] - want[0] as i32).abs() <= 256 {
                    seen = Some(got);
                    break;
                }
                seen = Some(got);
            }
        }
        let got = seen.unwrap_or_else(|| panic!("{name}: red never came"));
        for k in 0..3 {
            assert!(
                (got[k] - want[k] as i32).abs() <= 256,
                "{name} red {k}: {got:?} vs {want:?}"
            );
        }
    }
}

/// R.24 stage 2: an HDR camera (PQ, 16-bit) on an HDR PQ program keeps its highlights: a
/// level well above SDR white (80 % PQ, over 1000 nits) comes out as it went in, not
/// capped where an 8-bit picture would be (on the GPU compositor; the CPU's makes the
/// 8-bit picture HDR, so it caps).
#[test]
fn an_hdr_camera_keeps_its_highlights() {
    use idn_open_protocol::color::Transfer;
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let cam = Output::new(
        &publisher,
        OutputConfig {
            color_info: Some(idn_open_protocol::color::to_hdr::hdr_color_info(
                Transfer::Pq,
            )),
            ..OutputConfig::new("HDR camera")
        },
    )
    .unwrap();
    let cam_addr = format!("127.0.0.1:{}", cam.port());
    let (w, h) = (W, H);
    // 80 % PQ (10-bit 64 + 0.8 × 876), neutral chroma.
    let level = ((64.0 + 0.8 * 876.0) as u16) << 6;
    let mut d = vec![level; w * h];
    d.extend(std::iter::repeat_n(128u16 << 8, w * h));
    let d: Arc<[u16]> = d.into();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (stop, d) = (stop.clone(), d.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = cam.send(VideoFrame::p216(w, h, d.clone()));
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let sw = Switcher::start(SwitcherConfig {
        name: "HdrCam".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: true,
        hdr: Transfer::Pq,
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let on_gpu = sw.state()["compositor"]
        .as_str()
        .is_some_and(|c| c.starts_with("GPU"));
    let id = sw.add_input("Cam", Source::Network(cam_addr)).unwrap();
    sw.program(scene_of(&sw, id)).unwrap();
    let input = Input::open(InputConfig {
        precise: true,
        ..InputConfig::new(&format!("127.0.0.1:{}", sw.program_output().port()))
    })
    .unwrap();
    let signal = |p: &[idn_open_protocol::shq::Plane16]| {
        (p[0].data[p[0].data.len() / 2] as f64 / 64.0 - 64.0) / 876.0
    };
    let end = Instant::now() + Duration::from_secs(10);
    let mut best = 0.0f64;
    while Instant::now() < end && best < 0.79 {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50))
            && let Some(p) = v.picture.precise.as_ref()
        {
            best = best.max(signal(p));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    eprintln!(
        "compositor {}: the camera's 80 % PQ came out at {best:.3}",
        sw.state()["compositor"]
    );
    if on_gpu {
        assert!((best - 0.8).abs() < 0.01, "kept: {best:.3}");
    } else {
        assert!(best > 0.5, "something came: {best:.3}");
    }
}
