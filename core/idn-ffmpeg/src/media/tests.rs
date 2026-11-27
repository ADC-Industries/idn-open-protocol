use super::*;
use std::path::PathBuf;

fn dir(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("idn-media-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A file made with the `ffmpeg` command (skipped where there is none, or it lacks the
/// encoder): its path.
fn make(path: &Path, args: &[&str]) -> bool {
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(path)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("no ffmpeg (or encoder) here: skipped");
    }
    ok
}

/// 2 s at 25 fps, picture N's luma N × 4 (lossless H.264 with B-frames, a keyframe every
/// 10 pictures).
fn numbered(d: &Path) -> Option<PathBuf> {
    let p = d.join("numbered.mp4");
    make(
        &p,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x36:r=25:d=2,geq=lum='N*4':cb=128:cr=128",
            "-c:v",
            "libx264",
            "-qp",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "10",
            "-bf",
            "2",
        ],
    )
    .then_some(p)
}

/// The number a picture of [`numbered`] shows (its first luma sample / 4).
fn number(f: &VideoFrame) -> i64 {
    match f.format {
        // (The 10-bit file's filter makes N × 4 at 10 bits: P010 holds it in the top 10 of
        // 16 bits, planar 16-bit scaled to the full 16.)
        PixelFormat::P010 => {
            let v = u16::from_le_bytes([f.planes[0].data[0], f.planes[0].data[1]]);
            (f64::from(v >> 6) / 4.0).round() as i64
        }
        PixelFormat::Yuv420p16 => {
            let v = u16::from_le_bytes([f.planes[0].data[0], f.planes[0].data[1]]);
            (f64::from(v) / 64.0 / 4.0).round() as i64
        }
        _ => (f64::from(f.planes[0].data[0]) / 4.0).round() as i64,
    }
}

#[test]
fn the_timeline_of_a_span_played_three_times() {
    let ms = Duration::from_millis;
    let s = Span::new(ms(400), Some(ms(1200)), Some(ms(2000)), Some(3));
    assert_eq!(s.total(), Some(ms(2400)));
    assert_eq!(s.locate(ms(0)), (0, ms(400)));
    assert_eq!(s.locate(ms(900)), (1, ms(500)));
    // Past the end: the last play's end.
    assert_eq!(s.locate(ms(9000)), (2, ms(1200)));
    assert_eq!(s.time_of(1, ms(500)), ms(900));
    // Forever: no total.
    assert_eq!(Span::new(ms(0), None, Some(ms(2000)), None).total(), None);
}

/// Pictures for times on the caller's clock: the right one at each, across plays of an
/// in/out span (one timeline), an exact seek back that waits for its picture, the last
/// picture past the end; probe and a thumbnail of the same file.
#[test]
fn pictures_for_the_callers_timeline() {
    let d = dir("timeline");
    let Some(path) = numbered(&d) else { return };
    let ff = crate::Ffmpeg::probe();
    let info = probe(&path, &ff).unwrap();
    let v = info.video.as_ref().unwrap();
    assert_eq!((v.codec.as_str(), v.width, v.height), ("h264", 64, 36));
    assert_eq!((v.bit_depth, v.frame_rate.round() as i64), (8, 25));
    assert!(info.duration.unwrap() >= Duration::from_millis(1900));
    let ms = Duration::from_millis;
    let r = VideoReader::open(
        &path,
        &ff,
        VideoReaderConfig {
            in_point: ms(400),
            out_point: Some(ms(1200)),
            loops: Some(3),
            format: Some(PixelFormat::I420),
            decoder: Some(crate::DecoderChoice::Software),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.length(), Some(ms(2400)));
    let at = |t: u64| {
        let f = r
            .frame_at_wait(ms(t), Duration::from_secs(10))
            .unwrap_or_else(|| panic!("a picture at {t} ms ({:?})", r.error()));
        (number(&f), f.play)
    };
    // 0 ms: the in point, picture 10 (0.4 s × 25).
    assert_eq!(at(0), (10, 0));
    assert_eq!(at(100), (12, 0));
    // The second play: 900 ms is 500 ms into the file, picture 12 again.
    assert_eq!(at(900), (12, 1));
    // The third.
    assert_eq!(at(1700), (12, 2));
    // Back to the first play: a seek, waited for.
    assert_eq!(at(200), (15, 0));
    // Past the end: the last picture of the last play (1.16 s, picture 29).
    assert_eq!(at(5000), (29, 2));
    // (Ended once it has read past that picture: a moment later.)
    let t0 = std::time::Instant::now();
    while !r.ended() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(r.ended());
    // A still at 1 s: picture 25, RGBA at most 32 wide.
    let t = thumbnail(&path, ms(1000), 32).unwrap();
    assert_eq!((t.format, t.width, t.height), (PixelFormat::Rgba, 32, 18));
    // Its time: the picture's own, in the file (picture 10 at 0.4 s).
    assert_eq!((t.time, t.file_time), (ms(1000), ms(1000)));
    let t = thumbnail(&path, ms(400), 32).unwrap();
    assert_eq!((t.time, t.file_time), (ms(400), ms(400)));
    // Software, as chosen, and said.
    assert_eq!(
        r.decoding(),
        Decoding {
            gpu: None,
            why_software: Some("software chosen".into())
        }
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// The machine's own choice of decoder (the GPU where it decodes H.264): the same
/// pictures, downloaded.
#[test]
fn pictures_from_the_machines_decoder() {
    let d = dir("gpu");
    let p = d.join("lossy.mp4");
    if !make(
        &p,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x180:r=25:d=2,geq=lum='N*4':cb=128:cr=128",
            "-c:v",
            "libx264",
            "-crf",
            "10",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "10",
        ],
    ) {
        return;
    }
    let ff = crate::Ffmpeg::probe();
    let r = VideoReader::open(&p, &ff, VideoReaderConfig::default()).unwrap();
    for (t, n) in [(0u64, 0i64), (400, 10), (1500, 37), (120, 3)] {
        let f = r
            .frame_at_wait(Duration::from_millis(t), Duration::from_secs(10))
            .unwrap_or_else(|| panic!("a picture at {t} ms ({:?})", r.error()));
        assert_eq!((f.format, f.width), (PixelFormat::Nv12, 320));
        assert!((number(&f) - n).abs() <= 1, "{t} ms: {}", number(&f));
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A 10-bit HEVC file: probed as 10 bits, its pictures as P010 (the value in the top 10
/// of 16 bits), and as planar 16-bit.
#[test]
fn ten_bit_pictures_as_sixteen_bit_samples() {
    let d = dir("tenbit");
    let p = d.join("ten.mkv");
    if !make(
        &p,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x36:r=25:d=1,geq=lum='N*4':cb=128:cr=128,format=yuv420p10le",
            "-c:v",
            "libx265",
            "-x265-params",
            "lossless=1:log-level=none",
            "-pix_fmt",
            "yuv420p10le",
        ],
    ) {
        return;
    }
    let ff = crate::Ffmpeg::probe();
    let v = probe(&p, &ff).unwrap().video.unwrap();
    assert_eq!((v.codec.as_str(), v.bit_depth), ("hevc", 10));
    let r = VideoReader::open(
        &p,
        &ff,
        VideoReaderConfig {
            decoder: Some(crate::DecoderChoice::Software),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.format(), PixelFormat::P010);
    let f = r
        .frame_at_wait(Duration::from_millis(200), Duration::from_secs(10))
        .unwrap();
    assert_eq!((f.bit_depth, f.planes.len()), (10, 2));
    assert_eq!(f.planes[0].stride, 2 * 64);
    let y = u16::from_le_bytes([f.planes[0].data[0], f.planes[0].data[1]]);
    assert_eq!(y & 0x3f, 0, "P010: the low 6 bits clear");
    assert_eq!(number(&f), 5);
    let r16 = VideoReader::open(
        &p,
        &ff,
        VideoReaderConfig {
            format: Some(PixelFormat::Yuv420p16),
            decoder: Some(crate::DecoderChoice::Software),
            ..Default::default()
        },
    )
    .unwrap();
    let f = r16
        .frame_at_wait(Duration::from_millis(200), Duration::from_secs(10))
        .unwrap();
    assert_eq!(f.planes.len(), 3);
    assert_eq!(number(&f), 5);
    let _ = std::fs::remove_dir_all(&d);
}

/// A file's sound as float at a sample position: each sample its time (the file says so),
/// over an in point and two plays, after a seek, at another channel count.
#[test]
fn sound_at_a_sample_position() {
    let d = dir("sound");
    let p = d.join("ramp.wav");
    if !make(
        &p,
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc='t':s=48000:d=1",
            "-c:a",
            "pcm_f32le",
        ],
    ) {
        return;
    }
    let mut r = AudioReader::open(
        &p,
        AudioReaderConfig {
            sample_rate: 48_000,
            channels: 2,
            in_point: Duration::from_millis(500),
            loops: Some(2),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.info().channels, 1);
    assert_eq!(r.length(), Some(48_000));
    let mut buf = vec![0f32; 2 * 480];
    // Sample 0: the in point (0.5 s), both channels.
    assert_eq!(r.read(0, &mut buf).unwrap(), 480);
    assert!(
        (buf[0] - 0.5).abs() < 1e-3 && (buf[1] - 0.5).abs() < 1e-3,
        "{}",
        buf[0]
    );
    // Read on: the next 10 ms.
    r.read(480, &mut buf).unwrap();
    assert!((buf[0] - 0.51).abs() < 1e-3, "{}", buf[0]);
    // 0.75 s on the timeline: the second play, 0.25 s past the in point.
    r.read(36_000, &mut buf).unwrap();
    assert!((buf[0] - 0.75).abs() < 1e-3, "{}", buf[0]);
    // The timeline's end: what is left, then silence.
    assert_eq!(r.read(47_760, &mut buf).unwrap(), 240);
    assert_eq!(buf[2 * 240], 0.0);
    let _ = std::fs::remove_dir_all(&d);
}

/// The file's samples as FFmpeg's own command decodes them (mono float at `rate`): the
/// reference for positions.
fn decoded_by_ffmpeg(path: &Path, rate: u32) -> Vec<f32> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-ac", "1", "-ar", &rate.to_string(), "-f", "f32le", "-"])
        .output()
        .unwrap();
    out.stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// Reads `r` to its end in `block`-frame reads, never seeking: every sample.
fn read_all(r: &mut AudioReader, channels: usize, block: usize) -> Vec<f32> {
    let mut all = Vec::new();
    let mut buf = vec![0f32; block * channels];
    loop {
        let n = r.read(r.position(), &mut buf).unwrap();
        all.extend_from_slice(&buf[..n * channels]);
        if n < block {
            return all;
        }
    }
}

/// A sound shorter than its play (an out point past its end): silence to the out point,
/// each play starting at a multiple of the play's length, read straight through or after a
/// seek alike; every sample up to `length()`.
#[test]
fn plays_keep_their_length_when_the_sound_ends_early() {
    let d = dir("short");
    let p = d.join("ramp4.wav");
    if !make(
        &p,
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc='t/8':s=48000:d=4",
            "-c:a",
            "pcm_f32le",
        ],
    ) {
        return;
    }
    let cfg = AudioReaderConfig {
        sample_rate: 48_000,
        channels: 1,
        out_point: Some(Duration::from_millis(5000)),
        loops: Some(3),
        ..Default::default()
    };
    let mut r = AudioReader::open(&p, cfg.clone()).unwrap();
    assert_eq!(r.length(), Some(720_000));
    let all = read_all(&mut r, 1, 1024);
    assert_eq!(all.len(), 720_000);
    for play in 0..3 {
        let o = play * 240_000;
        // The sound (t / 8), then a second of silence.
        assert!(all[o].abs() < 1e-4, "play {play} starts at {}", all[o]);
        assert!(
            (all[o + 48_000] - 0.125).abs() < 1e-4,
            "{}",
            all[o + 48_000]
        );
        assert!(
            (all[o + 191_999] - 0.5).abs() < 1e-3,
            "{}",
            all[o + 191_999]
        );
        assert!(all[o + 192_000..o + 240_000].iter().all(|s| *s == 0.0));
    }
    // After a seek: the same samples.
    let mut r = AudioReader::open(&p, cfg).unwrap();
    let mut buf = vec![0f32; 4800];
    r.read(240_000 + 48_000, &mut buf).unwrap();
    assert_eq!(buf[..], all[288_000..292_800]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Every sample up to `length()` (the resampler's last ones too), and MP3 placed as FFmpeg
/// places it (its encoder delay skipped), read through or after a seek, as WAV.
#[test]
fn sound_positions_are_sample_accurate() {
    let d = dir("accurate");
    let chirp = "aevalsrc='0.5*sin(2*PI*(200+400*t)*t)':s=44100:d=3";
    let wav = d.join("chirp.wav");
    let mp3 = d.join("chirp.mp3");
    if !make(&wav, &["-f", "lavfi", "-i", chirp]) {
        return;
    }
    let have_mp3 = make(
        &mp3,
        &[
            "-f",
            "lavfi",
            "-i",
            chirp,
            "-c:a",
            "libmp3lame",
            "-b:a",
            "256k",
        ],
    );
    let aac = d.join("chirp.m4a");
    let have_aac = make(
        &aac,
        &["-f", "lavfi", "-i", chirp, "-c:a", "aac", "-b:a", "256k"],
    );
    let files = [
        Some(&wav),
        have_mp3.then_some(&mp3),
        have_aac.then_some(&aac),
    ];
    for p in files.into_iter().flatten() {
        let reference = decoded_by_ffmpeg(p, 48_000);
        let near = |got: &[f32], from: usize, what: &str| {
            let worst = got
                .iter()
                .zip(&reference[from..])
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(
                worst < 0.02,
                "{}: {what}: off by up to {worst}",
                p.display()
            );
        };
        // All of it, straight through.
        let mut r = AudioReader::open(
            p,
            AudioReaderConfig {
                channels: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.length(), Some(144_000), "{}", p.display());
        let all = read_all(&mut r, 1, 1024);
        assert_eq!(all.len(), 144_000, "{}", p.display());
        near(&all[..4800], 0, "the start");
        near(&all[100_000..104_800], 100_000, "2.08 s on");
        // A seek to 1.5 s: no silence before it, the right samples.
        let mut buf = vec![0f32; 4800];
        assert_eq!(r.read(72_000, &mut buf).unwrap(), 4800);
        near(&buf, 72_000, "a seek to 1.5 s");
        // An in point at 0.7 s.
        let mut r = AudioReader::open(
            p,
            AudioReaderConfig {
                channels: 1,
                in_point: Duration::from_millis(700),
                ..Default::default()
            },
        )
        .unwrap();
        r.read(0, &mut buf).unwrap();
        near(&buf, 33_600, "the in point 0.7 s");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// What this machine decodes and with which FFmpeg.
#[test]
fn this_machines_capabilities() {
    let c = capabilities(&crate::Ffmpeg::probe());
    assert!(c.software_codecs.iter().any(|n| n == "h264"), "{c:?}");
    assert!(
        c.libraries
            .iter()
            .any(|(l, v)| l == "avcodec" && v.split('.').count() == 3)
    );
    assert!(!c.ffmpeg.is_empty());
    // Hardware codecs only with a hardware decoder.
    assert!(c.hardware.is_some() || c.hardware_codecs.is_empty());
    eprintln!("{c:?}");
}

/// A variable-rate MPEG-4 Part 2 file with 44.1 kHz sound: probed as variable, made a
/// constant-rate H.264 MP4 with AAC at 48 kHz, as long as it was.
#[test]
fn a_variable_rate_file_made_easy_to_play() {
    let d = dir("convert-vfr");
    let src = d.join("vfr.mkv");
    // 4 s at 30 fps with the first second's pictures thinned to one in three: gaps.
    if !make(
        &src,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=320x240:r=30:d=4",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=4",
            "-vf", "select='gt(n\\,29)+not(mod(n\\,3))'",
            "-fps_mode", "vfr",
            "-c:v", "mpeg4", "-q:v", "4",
            "-c:a", "pcm_s16le",
        ],
    ) {
        return;
    }
    let ff = crate::Ffmpeg::probe();
    let p = probe(&src, &ff).unwrap();
    let v = p.video.as_ref().unwrap();
    println!("source: avg {} base {} vfr {} bit rate {:?}", v.avg_frame_rate, v.base_frame_rate, v.variable_frame_rate, p.bit_rate);
    assert_eq!(v.codec, "mpeg4");
    assert!(p.bit_rate.is_some_and(|b| b > 0));
    let out = d.join("ready.mp4");
    let mut ticks = 0;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let cfg = ConvertConfig {
        frame_rate: Some((30, 1)),
        gpu: false,
        ..Default::default()
    };
    let r = convert(&src, &out, &cfg, &ff, |_| ticks += 1, &cancel).unwrap();
    println!("{r:?}");
    assert!(ticks >= 1);
    assert_eq!((r.width, r.height, r.frame_rate), (320, 240, (30, 1)));
    assert!(r.audio && r.repeated > 0, "{r:?}");
    assert!((r.frames as i64 - 120).abs() <= 1, "{} frames", r.frames);
    let o = probe(&out, &ff).unwrap();
    let (ov, oa) = (o.video.unwrap(), o.audio.unwrap());
    assert_eq!((ov.codec.as_str(), ov.pixel_format.as_str()), ("h264", "yuv420p"));
    assert!((ov.avg_frame_rate - 30.0).abs() < 0.05 && !ov.variable_frame_rate, "{ov:?}");
    assert_eq!((oa.codec.as_str(), oa.sample_rate, oa.channels), ("aac", 48_000, 2));
    let len = o.duration.unwrap().as_secs_f64();
    assert!((len - 4.0).abs() < 0.1, "{len} s");
    assert!(!d.join("ready.mp4.part").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// Interlaced H.264 4:2:2: deinterlaced, made 4:2:0, fitted inside a smaller size; a
/// cancelled conversion leaves nothing behind.
#[test]
fn an_interlaced_file_deinterlaced_and_a_cancel() {
    let d = dir("convert-interlaced");
    let src = d.join("interlaced.mkv");
    if !make(
        &src,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=720x576:r=25:d=2",
            "-c:v", "libx264", "-pix_fmt", "yuv422p", "-flags", "+ildct+ilme",
            "-x264-params", "tff=1", "-field_order", "tt",
        ],
    ) {
        return;
    }
    let ff = crate::Ffmpeg::probe();
    let v = probe(&src, &ff).unwrap().video.unwrap();
    println!("source: {v:?}");
    assert_eq!(v.pixel_format, "yuv422p");
    let out = d.join("ready.mp4");
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let cfg = ConvertConfig {
        max_size: Some((640, 360)),
        ..Default::default()
    };
    let r = convert(&src, &out, &cfg, &ff, |_| {}, &cancel).unwrap();
    println!("{r:?}");
    assert_eq!((r.width, r.height, r.frame_rate), (450, 360, (25, 1)));
    assert!(r.deinterlaced >= 49 && !r.audio, "{r:?}");
    let o = probe(&out, &ff).unwrap().video.unwrap();
    assert_eq!((o.pixel_format.as_str(), o.width, o.height), ("yuv420p", 450, 360));
    assert!(!o.interlaced, "{o:?}");
    // Cancelled: an error, no file, no part.
    let gone = d.join("gone.mp4");
    let stop = std::sync::atomic::AtomicBool::new(true);
    let e = convert(&src, &gone, &cfg, &ff, |_| {}, &stop).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Interrupted);
    assert!(!gone.exists() && !d.join("gone.mp4.part").exists());
    let _ = std::fs::remove_dir_all(&d);
}
