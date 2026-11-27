//! Gateways out (R.12) on this machine's GPU (skipped without one): recordings in MP4
//! and MKV, live streams to RTMP and SRT servers (FFmpeg as the server), reconnection,
//! and shared encodes. Uses the `ffmpeg` and `ffprobe` command-line tools.

use crate::common;
use common::free_port;

use idn_common::testing::TempDir;
use idn_ffmpeg::Ffmpeg;
use idn_ffmpeg::gateway::{AddOptions, Container, Destination, Gateway, State, StreamSettings};
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::codec::EncoderFactory;
use idn_open_protocol::color::AlphaMode;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A test's own folder (tests run at once), removed when dropped.
fn dir(test: &str) -> TempDir {
    TempDir::new(&format!("gateway-{test}"))
}

/// An output fed 1280×720 at 30 fps with a 440 Hz tone, while `run` lasts.
struct Rig {
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    _p: Publisher,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
    }
}

fn rig() -> Rig {
    rig_with(2)
}

/// The same with `channels` channels of the tone.
fn rig_with(channels: usize) -> Rig {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (30, 1),
                ..OutputConfig::new("Program")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let (w, h) = (1280usize, 720usize);
        let start = Instant::now();
        let mut n = 0u32;
        let mut phase = 0f64;
        while !s.load(Ordering::Relaxed) {
            let mut px = vec![0u8; w * h * 4];
            let bar = (n as usize * 10) % w;
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let x = i % w;
                let v = if (bar..bar + 40).contains(&x) {
                    230
                } else {
                    (40 + x * 150 / w) as u8
                };
                *p = [v, 128, 255 - v, 255];
            }
            let mut f = VideoFrame::rgba(w, h, px.into(), AlphaMode::Opaque);
            let tone: Vec<f32> = (0..1600)
                .map(|i| {
                    (0.3 * (phase + i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 48000.0).sin())
                        as f32
                })
                .collect();
            phase = (phase + 1600.0 * 2.0 * std::f64::consts::PI * 440.0 / 48000.0)
                % (2.0 * std::f64::consts::PI);
            f.audio = Some(AudioBlock {
                sample_rate: 48_000,
                channels: vec![tone; channels],
            });
            let _ = o.send(f);
            n += 1;
            let due = start + Duration::from_secs_f64(n as f64 / 30.0);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    });
    Rig {
        out,
        stop,
        feeder: Some(feeder),
        _p: p,
    }
}

/// (codec, width, height) per stream, and the duration in seconds.
fn probe(path: &Path) -> (Vec<(String, u32, u32)>, f64) {
    let o = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,width,height:format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut streams = Vec::new();
    let mut dur = 0.0;
    for line in text.lines() {
        let f: Vec<&str> = line.split(',').collect();
        match f.as_slice() {
            [c, w, h] => streams.push((
                c.to_string(),
                w.parse().unwrap_or(0),
                h.parse().unwrap_or(0),
            )),
            [c] if c.parse::<f64>().is_ok() => dur = c.parse().unwrap(),
            [c] => streams.push((c.to_string(), 0, 0)),
            [c, ..] => streams.push((c.to_string(), 0, 0)),
            _ => {}
        }
    }
    (streams, dur)
}

fn has_gpu(ff: &Ffmpeg) -> bool {
    if EncoderFactory::codecs(ff).is_empty() {
        println!("no hardware encoder here: skipped");
        return false;
    }
    true
}

fn wait_for(gw: &Gateway, id: u64, want: fn(&State) -> bool, secs: u64) -> State {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = gw.status().into_iter().find(|s| s.id == id).unwrap().state;
        if want(&s) || Instant::now() > end {
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An FFmpeg server receiving `url` (listening) and copying it into `out`.
/// An ffmpeg server, stopped when dropped (a test that fails before `stop_server` left
/// its listener running).
struct Server(Option<Child>);

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn server(input: &[&str], out: &Path) -> Server {
    Server(Some(
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(input)
            .args(["-c", "copy"])
            .arg(out)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    ))
}

fn stop_server(mut s: Server) {
    let Some(mut c) = s.0.take() else { return };
    // q on stdin isn't available; SIGINT lets it finish the file.
    unsafe {
        libc_kill(c.id() as i32, 2);
    }
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if c.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = c.kill();
    let _ = c.wait();
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[test]
fn recordings_in_mp4_and_mkv_with_one_shared_encode() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let r = rig();
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("recordings_in_mp4_and_mkv_with_one_shared_encode");
    let (mp4, mkv, small) = (
        d.join("service.mp4"),
        d.join("service.mkv"),
        d.join("small.mp4"),
    );
    let a = gw
        .add(Destination::File(mp4.clone(), Container::Mp4))
        .unwrap();
    let b = gw
        .add(Destination::File(mkv.clone(), Container::Mkv))
        .unwrap();
    // Another size: its own encode.
    let c = gw
        .add_with(
            Destination::File(small.clone(), Container::Mp4),
            StreamSettings {
                size: Some((640, 360)),
                video_mbps: 2.0,
                ..Default::default()
            },
            false,
        )
        .unwrap();
    let tap = gw.attach(&r.out);
    assert_eq!(wait_for(&gw, a, |s| *s == State::Live, 10), State::Live);
    assert_eq!(gw.encodes(), 2);
    std::thread::sleep(Duration::from_secs(4));
    for id in [a, b, c] {
        gw.remove(id);
    }
    drop(tap);
    std::thread::sleep(Duration::from_millis(500));
    for (path, w, h) in [(&mp4, 1280, 720), (&mkv, 1280, 720), (&small, 640, 360)] {
        let (streams, dur) = probe(path);
        println!("{}: {streams:?}, {dur:.2} s", path.display());
        assert!(streams.contains(&("h264".into(), w, h)), "{streams:?}");
        assert!(streams.iter().any(|s| s.0 == "aac"), "{streams:?}");
        assert!((3.0..6.0).contains(&dur), "{dur} s");
    }
}

#[test]
fn live_streams_to_rtmp_and_srt_and_reconnects() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let r = rig();
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("live_streams_to_rtmp_and_srt_and_reconnects");
    let (rtmp_port, srt_port) = (free_port(), free_port());
    let rtmp_url = format!("rtmp://127.0.0.1:{rtmp_port}/live/streamkey123");
    // The RTMP server isn't up yet: the destination retries.
    let a = gw.add(Destination::Rtmp(rtmp_url.clone())).unwrap();
    let _tap = gw.attach(&r.out);
    let s = wait_for(&gw, a, |s| matches!(s, State::Retrying(_)), 10);
    assert!(matches!(s, State::Retrying(_)), "{s:?}");
    assert!(
        !gw.status()[0].target.contains("streamkey123"),
        "the key shows"
    );
    let (flv, ts) = (d.join("rtmp.flv"), d.join("srt.ts"));
    let rtmp = server(&["-listen", "1", "-i", &rtmp_url], &flv);
    let srt = server(
        &["-i", &format!("srt://127.0.0.1:{srt_port}?mode=listener")],
        &ts,
    );
    std::thread::sleep(Duration::from_millis(500));
    let b = gw
        .add(Destination::Srt(format!("srt://127.0.0.1:{srt_port}")))
        .unwrap();
    assert_eq!(wait_for(&gw, a, |s| *s == State::Live, 40), State::Live);
    assert_eq!(wait_for(&gw, b, |s| *s == State::Live, 10), State::Live);
    std::thread::sleep(Duration::from_secs(4));
    let st = gw.status();
    println!("{st:?}");
    assert!(st.iter().all(|s| s.bytes > 500_000), "{st:?}");
    assert!(st[0].reconnects >= 1);
    gw.remove(a);
    gw.remove(b);
    std::thread::sleep(Duration::from_millis(500));
    stop_server(rtmp);
    stop_server(srt);
    for (path, f) in [(&flv, "flv"), (&ts, "ts")] {
        let (streams, dur) = probe(path);
        println!("{f}: {streams:?}, {dur:.2} s");
        assert!(
            streams.contains(&("h264".into(), 1280, 720)),
            "{f}: {streams:?}"
        );
        assert!(streams.iter().any(|s| s.0 == "aac"), "{f}: {streams:?}");
    }
}

/// Beep − flash in a recording, ms, per flash: pictures by their times and luma (each
/// as stored: no frames repeated to a constant rate), sound end to end from its first
/// frame's time, as players play it.
fn beep_minus_flash(path: &Path) -> Vec<f64> {
    let run = |args: &[&str]| {
        Command::new(args[0])
            .args(&args[1..])
            .arg(path)
            .output()
            .unwrap()
            .stdout
    };
    let p = path.to_str().unwrap();
    let vt: Vec<f64> = String::from_utf8_lossy(&run(&[
        "ffprobe",
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "frame=pts_time",
        "-of",
        "csv=p=0",
    ]))
    .lines()
    .filter_map(|l| l.trim_matches(',').parse().ok())
    .collect();
    let luma = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            p,
            "-fps_mode",
            "passthrough",
            "-vf",
            "scale=16:9,format=gray",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .unwrap()
        .stdout;
    let flashes: Vec<f64> = (1..vt.len().min(luma.len() / 144))
        .filter(|&i| {
            let m = |k: usize| {
                luma[k * 144..(k + 1) * 144]
                    .iter()
                    .map(|&b| b as u32)
                    .sum::<u32>()
                    / 144
            };
            m(i) > 128 && m(i - 1) < 64
        })
        .map(|i| vt[i])
        .collect();
    let at: Vec<(f64, usize)> = String::from_utf8_lossy(&run(&[
        "ffprobe",
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "frame=pts_time,nb_samples",
        "-of",
        "csv=p=0",
    ]))
    .lines()
    .filter_map(|l| {
        let mut f = l.split(',');
        Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
    })
    .collect();
    let pcm = Command::new("ffmpeg")
        .args(["-v", "error", "-i", p, "-ac", "1", "-f", "f32le", "-"])
        .output()
        .unwrap()
        .stdout;
    let s: Vec<f32> = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let start = at.first().map_or(0.0, |a| a.0);
    let (mut beeps, mut quiet) = (Vec::new(), 0usize);
    for (i, x) in s.iter().enumerate() {
        if x.abs() > 0.1 {
            if quiet > 4800 {
                beeps.push(start + i as f64 / 48_000.0);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    beeps
        .iter()
        .filter_map(|b| {
            flashes
                .iter()
                .map(|f| b - f)
                .min_by(|x, y| x.abs().total_cmp(&y.abs()))
        })
        .filter(|d| d.abs() < 0.3)
        .map(|d| d * 1000.0)
        .collect()
}

#[test]
fn a_recording_keeps_sound_with_its_pictures() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (30, 1),
                ..OutputConfig::new("Sync")
            },
        )
        .unwrap(),
    );
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("a_recording_keeps_sound_with_its_pictures");
    let mp4 = d.join("sync.mp4");
    let id = gw
        .add(Destination::File(mp4.clone(), Container::Mp4))
        .unwrap();
    let tap = gw.attach(&out);
    // As the switcher sends: each picture with its tick's timestamp, and the sound on
    // its own, stamped 37 ms earlier (the time its first sample was taken); a white
    // picture and a beep at the same moment, once a second.
    let (w, h) = (640usize, 360usize);
    let t0 = idn_open_protocol::api::timestamp_now();
    let start = Instant::now();
    let lag = 370_000i64;
    for n in 0..150i64 {
        let tick = t0 + n * 10_000_000 / 30;
        let from = tick - lag;
        let block: Vec<f32> = (0..1600i64)
            .map(|i| {
                let t = from + i * 10_000_000 / 48_000 - t0; // since the start, 100 ns
                let into = (t - 5_000_000).rem_euclid(10_000_000); // flashes at 0.5 s, 1.5 s, …
                if t >= 0 && into < 1_000_000 {
                    (i as f32 * 0.13).sin() * 0.5
                } else {
                    0.0
                }
            })
            .collect();
        let _ = out.send_audio_at(
            AudioBlock {
                sample_rate: 48_000,
                channels: vec![block.clone(), block],
            },
            None,
            from,
        );
        let y = if (n * 10_000_000 / 30 - 5_000_000).rem_euclid(10_000_000) < 10_000_000 / 30 {
            235
        } else {
            16
        };
        let plane = |pw, ph, v| idn_open_protocol::shq::Plane {
            width: pw,
            height: ph,
            data: vec![v; pw * ph],
        };
        let _ = out.send_planes(idn_open_protocol::api::PlanarFrame {
            layout: idn_open_protocol::shq::Fourcc::Shq2,
            width: w,
            height: h,
            planes: Arc::new(vec![
                plane(w, h, y),
                plane(w / 2, h, 128),
                plane(w / 2, h, 128),
            ]),
            timestamp: Some(tick),
            timecode: None,
            audio: None,
            coded: None,
        });
        let due = start + Duration::from_secs_f64((n + 1) as f64 / 30.0);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
    gw.remove(id);
    drop(tap);
    gw.close(Duration::from_secs(3));
    let d_ms = beep_minus_flash(&mp4);
    println!("beep - flash (ms): {d_ms:.1?}");
    assert!(d_ms.len() >= 3, "{d_ms:?}");
    assert!(d_ms.iter().all(|d| d.abs() < 3.0), "{d_ms:?}");
}

/// A recording split every 3 s: parts name.mp4, name-2.mp4, … each a whole file, back to
/// back (together about as long as the recording ran).
#[test]
fn a_recording_splits_into_parts() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let r = rig();
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("a_recording_splits_into_parts");
    let base = d.join("service.mp4");
    let id = gw
        .add(Destination::File(base.clone(), Container::Mp4))
        .unwrap();
    gw.set_split(id, 3, 0);
    let tap = gw.attach(&r.out);
    assert_eq!(wait_for(&gw, id, |s| *s == State::Live, 10), State::Live);
    std::thread::sleep(Duration::from_secs(9));
    gw.remove(id);
    drop(tap);
    std::thread::sleep(Duration::from_millis(500));
    let mut parts = vec![base.clone()];
    for k in 2.. {
        let p = d.join(format!("service-{k}.mp4"));
        if !p.exists() {
            break;
        }
        parts.push(p);
    }
    let mut total = 0.0;
    for p in &parts {
        let (streams, dur) = probe(p);
        println!("{}: {streams:?}, {dur:.2} s", p.display());
        assert!(streams.iter().any(|s| s.0 == "h264"), "{streams:?}");
        assert!(streams.iter().any(|s| s.0 == "aac"), "{streams:?}");
        total += dur;
    }
    assert!(parts.len() >= 2, "split into parts: {}", parts.len());
    for p in &parts[..parts.len() - 1] {
        let dur = probe(p).1;
        assert!(
            (2.5..6.5).contains(&dur),
            "a full part runs past 3 s to a keyframe: {dur}"
        );
    }
    assert!(
        total > 7.0,
        "back to back, little lost between parts: {total:.2} s"
    );
}

/// A recording paused for 3 s in the middle is one file, as long as it was recording —
/// the pause isn't in it, and nothing is lost either side.
#[test]
fn a_paused_recording_is_one_file_without_the_pause() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let r = rig();
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("a_paused_recording_is_one_file_without_the_pause");
    let path = d.join("service.mp4");
    let id = gw
        .add(Destination::File(path.clone(), Container::Mp4))
        .unwrap();
    let tap = gw.attach(&r.out);
    assert_eq!(wait_for(&gw, id, |s| *s == State::Live, 10), State::Live);
    std::thread::sleep(Duration::from_secs(3));
    gw.set_paused(id, true);
    assert!(gw.status().iter().any(|s| s.id == id && s.paused));
    std::thread::sleep(Duration::from_secs(3));
    gw.set_paused(id, false);
    std::thread::sleep(Duration::from_secs(3));
    gw.remove(id);
    drop(tap);
    std::thread::sleep(Duration::from_millis(500));
    let (streams, dur) = probe(&path);
    println!("{}: {streams:?}, {dur:.2} s", path.display());
    assert!(streams.iter().any(|s| s.0 == "h264") && streams.iter().any(|s| s.0 == "aac"));
    assert!((4.5..7.5).contains(&dur), "about 6 s, not 9: {dur:.2} s");
    assert!(!d.join("service-2.mp4").exists(), "one file");
}

/// Four channels recorded as two stereo tracks: the file has both (a stream would carry
/// the first only).
#[test]
fn a_recording_writes_two_sound_tracks() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let r = rig_with(4);
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("a_recording_writes_two_sound_tracks");
    let (mp4, mkv) = (d.join("stems.mp4"), d.join("stems.mkv"));
    let two = StreamSettings {
        audio_tracks: 2,
        ..Default::default()
    };
    let a = gw
        .add_with(
            Destination::File(mp4.clone(), Container::Mp4),
            two.clone(),
            false,
        )
        .unwrap();
    let b = gw
        .add_with(Destination::File(mkv.clone(), Container::Mkv), two, false)
        .unwrap();
    let tap = gw.attach(&r.out);
    assert_eq!(wait_for(&gw, a, |s| *s == State::Live, 10), State::Live);
    std::thread::sleep(Duration::from_secs(3));
    gw.remove(a);
    gw.remove(b);
    drop(tap);
    std::thread::sleep(Duration::from_millis(500));
    for path in [&mp4, &mkv] {
        let (streams, dur) = probe(path);
        println!("{}: {streams:?}, {dur:.2} s", path.display());
        assert!(streams.iter().any(|s| s.0 == "h264"), "{streams:?}");
        assert_eq!(
            streams.iter().filter(|s| s.0 == "aac").count(),
            2,
            "{streams:?}"
        );
        assert!((2.0..4.5).contains(&dur), "{dur} s");
    }
}

/// A slow upload: a proxy passing `kbps` from the stream to `to` (the server's answers
/// unhindered), with a small receive buffer, as a congested link's queue. Returns its port.
fn throttled_proxy(to: u16, kbps: u64) -> u16 {
    use std::io::{Read, Write};
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    // SOL_SOCKET, SO_RCVBUF (Linux): taken over by the accepted connection. (Elsewhere the
    // system's buffer: the proxy's pacing alone.)
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let size: i32 = 64 << 10;
        unsafe {
            setsockopt(l.as_raw_fd(), 1, 8, (&size as *const i32).cast(), 4);
        }
    }
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut a, _)) = l.accept() else { return };
        // The test server may still be starting (a busy machine): a few seconds of tries.
        let mut tries = 0;
        let mut b = loop {
            match std::net::TcpStream::connect(("127.0.0.1", to)) {
                Ok(b) => break b,
                Err(_) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => panic!("the test server: {e}"),
            }
        };
        let (mut a2, mut b2) = (a.try_clone().unwrap(), b.try_clone().unwrap());
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut b2, &mut a2);
        });
        let start = Instant::now();
        let (mut sent, mut buf) = (0u64, vec![0u8; 4096]);
        loop {
            let due = start + Duration::from_secs_f64(sent as f64 * 8.0 / (kbps as f64 * 1000.0));
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            match a.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if b.write_all(&buf[..n]).is_err() {
                        return;
                    }
                    sent += n as u64;
                }
            }
        }
    });
    port
}

#[cfg(unix)]
unsafe extern "C" {
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const u8, len: u32) -> i32;
}

#[test]
fn an_adaptive_stream_follows_a_slow_upload() {
    let ff = Arc::new(Ffmpeg::probe());
    if !has_gpu(&ff) {
        return;
    }
    let r = rig();
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("an_adaptive_stream_follows_a_slow_upload");
    let flv = d.join("slow.flv");
    let port = free_port();
    let server_url = format!("rtmp://127.0.0.1:{port}/live/key");
    let rtmp = server(&["-listen", "1", "-i", &server_url], &flv);
    std::thread::sleep(Duration::from_millis(500));
    let proxy = throttled_proxy(port, THROTTLE_KBPS);
    let id = gw
        .add_opts(
            Destination::Rtmp(format!("rtmp://127.0.0.1:{proxy}/live/key")),
            StreamSettings {
                video_mbps: 6.0,
                ..Default::default()
            },
            AddOptions {
                adaptive_floor_mbps: Some(0.5),
                ..Default::default()
            },
        )
        .unwrap();
    let _tap = gw.attach(&r.out);
    assert_eq!(wait_for(&gw, id, |s| *s == State::Live, 20), State::Live);
    // The stream recorded as its encoder makes it (G4 part 3).
    let mp4 = d.join("as-sent.mp4");
    let rec = gw.record(id, mp4.clone(), Container::Mp4).unwrap();
    assert_eq!(gw.encodes(), 1, "the recording shares the stream's encode");
    let end = Instant::now() + Duration::from_secs(90);
    let st = loop {
        std::thread::sleep(Duration::from_secs(1));
        let st = gw.status().into_iter().find(|s| s.id == id).unwrap();
        println!(
            "{:?}: video {} of {} kbit/s, sent {} kbit/s, waited {} ms, dropped {}",
            st.state, st.video_kbps, st.target_kbps, st.sent_kbps, st.lag_ms, st.dropped
        );
        if (st.video_kbps as u64) < THROTTLE_KBPS || Instant::now() > end {
            break st;
        }
    };
    assert!(st.adaptive);
    assert!((st.video_kbps as u64) < THROTTLE_KBPS, "{st:?}");
    // Some seconds at the new rate, then the stream as the server got it.
    std::thread::sleep(Duration::from_secs(5));
    let st = gw.status().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(st.reconnects, 0, "one connection throughout: {st:?}");
    // Never far behind live: skipped ahead past 3 s.
    assert!(st.lag_ms < 4000, "{st:?}");
    gw.remove(rec);
    gw.remove(id);
    std::thread::sleep(Duration::from_millis(500));
    stop_server(rtmp);
    // The recording has every picture the encoder made, across the stream's skip ahead (a
    // 3 s gap in the stream) and its rate changes, and decodes without an error.
    let gaps = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .args(["packet=dts_time", "-of", "csv=p=0"])
        .arg(&mp4)
        .output()
        .unwrap();
    let t: Vec<f64> = String::from_utf8_lossy(&gaps.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    let widest = t.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
    println!(
        "recording: {} pictures, the widest step {widest:.3} s",
        t.len()
    );
    assert!(t.len() > 300, "{} pictures", t.len());
    assert!(widest < 0.2, "a gap of {widest} s");
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(&mp4)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    let errors: Vec<&str> = std::str::from_utf8(&out.stderr)
        .unwrap()
        .lines()
        // (FFmpeg's "Last message repeated" follows a message, here the one let through.)
        .filter(|l| {
            !l.contains("non monotonically increasing dts") && !l.contains("Last message repeated")
        })
        .collect();
    assert!(errors.is_empty(), "the recording: {errors:?}");
    let (streams, dur) = probe(&flv);
    println!("{streams:?}, {dur:.1} s");
    assert!(streams.contains(&("h264".into(), 1280, 720)), "{streams:?}");
    // Decoded end to end: across the new encoders' parameter sets without an error.
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(&flv)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    // (The null output's notes on its own time base, rounding frame times, aside.)
    let errors: Vec<&str> = std::str::from_utf8(&out.stderr)
        .unwrap()
        .lines()
        .filter(|l| !l.contains("non monotonically increasing dts"))
        .collect();
    assert!(errors.is_empty(), "decoding: {errors:?}");
}

/// The slow upload's rate (kbit/s): well under the stream's 6 Mbit/s.
const THROTTLE_KBPS: u64 = 2000;

/// An HDR program's recording (R.24): its 16-bit pictures encoded in 10 bits (HEVC Main
/// 10), the file tagged BT.2020 with PQ.
#[test]
fn an_hdr_output_records_in_ten_bits() {
    let ff = Arc::new(Ffmpeg::probe());
    if !EncoderFactory::codecs(&*ff).contains(&idn_open_protocol::codec::Codec::Hevc) {
        println!("no hardware HEVC encoder here: skipped");
        return;
    }
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                color_info: Some(idn_open_protocol::color::to_hdr::hdr_color_info(
                    idn_open_protocol::color::Transfer::Pq,
                )),
                ..OutputConfig::new("HDR rec")
            },
        )
        .unwrap(),
    );
    let gw = Arc::new(Gateway::new(ff));
    let d = dir("an_hdr_output_records_in_ten_bits");
    let mkv = d.join("hdr.mkv");
    let id = gw
        .add_with(
            Destination::File(mkv.clone(), Container::Mkv),
            StreamSettings {
                codec: idn_open_protocol::codec::Codec::Hevc,
                ..Default::default()
            },
            false,
        )
        .unwrap();
    let tap = gw.attach(&out);
    let (w, h) = (640usize, 360usize);
    let start = Instant::now();
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(3) {
        // A ramp of PQ levels, neutral chroma.
        let mut f = vec![0u16; 2 * w * h];
        for y in 0..h {
            for x in 0..w {
                f[y * w + x] = (((64 + (x * 876 / w)) as u16) << 6) + (n as u16 & 0x3f);
            }
        }
        f[w * h..].fill(128 << 8);
        let _ = out.send(idn_open_protocol::api::VideoFrame::p216(w, h, f.into()));
        n += 1;
        std::thread::sleep(Duration::from_millis(33));
    }
    let st = wait_for(&gw, id, |s| *s == State::Live, 1);
    gw.remove(id);
    drop(tap);
    std::thread::sleep(Duration::from_millis(500));
    let o = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt,color_transfer,color_primaries,color_space",
            "-of",
            "default=nw=1",
        ])
        .arg(&mkv)
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    println!("{}: {st:?}\n{s}", mkv.display());
    assert!(s.contains("codec_name=hevc"), "{s}");
    assert!(s.contains("pix_fmt=yuv420p10le"), "{s}");
    assert!(s.contains("color_transfer=smpte2084"), "{s}");
    assert!(s.contains("color_primaries=bt2020"), "{s}");
}
