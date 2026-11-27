//! Files as sources (R.12) on this machine's GPU (skipped without one): an H.264 file
//! recorded by the gateway plays with pass-through to an IDN input that takes H.264 and
//! as SpeedHQ to the others; an MPEG-2 file is decoded; pause, seek and loop.

use crate::common;
use common::free_port;

use idn_common::testing::TempDir;
use idn_ffmpeg::Ffmpeg;
use idn_ffmpeg::gateway::{Container, Destination, Gateway, State};
use idn_ffmpeg::player::{FilePlayer, PlayerConfig, PlayerState};
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::codec::{Codec, CodecInput, CodecUse, EncoderFactory};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::LinkConfig;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn publisher() -> Publisher {
    Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap()
}

/// A 4 s 1280×720 H.264/AAC MP4 made with this machine's GPU through the gateway.
fn record_h264(ff: &Arc<Ffmpeg>, path: &Path) {
    let p = publisher();
    let out = Output::new(
        &p,
        OutputConfig {
            port: free_port(),
            frame_rate: (30, 1),
            ..OutputConfig::new("Fixture")
        },
    )
    .unwrap();
    let gw = Arc::new(Gateway::new(ff.clone()));
    let id = gw
        .add(Destination::File(path.to_owned(), Container::Mp4))
        .unwrap();
    let _tap = gw.attach(&out);
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < Duration::from_millis(4300) {
        let (w, h) = (1280usize, 720usize);
        let mut px = vec![0u8; w * h * 4];
        let bar = (n as usize * 10) % w;
        for (i, q) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = i % w;
            let v = if (bar..bar + 40).contains(&x) {
                230
            } else {
                (40 + x * 150 / w) as u8
            };
            *q = [v, 100, 255 - v, 255];
        }
        let mut f = VideoFrame::rgba(w, h, px.into(), AlphaMode::Opaque);
        f.audio = Some(AudioBlock {
            sample_rate: 48_000,
            channels: vec![vec![0.1; 1600], vec![0.1; 1600]],
        });
        let _ = out.send(f);
        n += 1;
        std::thread::sleep(
            (start + Duration::from_secs_f64(n as f64 / 30.0))
                .saturating_duration_since(Instant::now()),
        );
    }
    while gw.status()[0].state != State::Live {
        std::thread::sleep(Duration::from_millis(50));
    }
    gw.remove(id);
    std::thread::sleep(Duration::from_millis(500));
}

/// A test's own folder (tests run at once), removed when dropped.
fn dir(test: &str) -> TempDir {
    TempDir::new(&format!("player-{test}"))
}

fn input(port: u16, codec: Option<Arc<Ffmpeg>>) -> Arc<Input> {
    Arc::new(
        Input::open(InputConfig {
            // Pass-through and decoding are what is tested: over the connection (§E.26's
            // ring otherwise).
            machine: false,
            link: Some(LinkConfig::ephemeral()),
            codec: codec.map(|ff| CodecInput::new(ff, CodecUse::Prefer)),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap(),
    )
}

/// Waits until every input shows a picture (connecting takes longer on a busy machine,
/// and the counts below start after it).
fn wait_for_pictures(inputs: &[Arc<Input>]) {
    for i in inputs {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if let Some(InputEvent::Video(_)) = i.recv(Duration::from_millis(50)) {
                break;
            }
        }
    }
}

/// Pictures by FourCC over `secs`, with each input's video bit rate.
fn watch(inputs: &[Arc<Input>], secs: f64) -> Vec<(BTreeMap<String, usize>, f64)> {
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = inputs
        .iter()
        .map(|i| {
            let (i, stop) = (i.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut by = BTreeMap::new();
                let b0 = i.stats().video_bytes;
                let t0 = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(InputEvent::Video(v)) = i.recv(Duration::from_millis(20)) {
                        *by.entry(String::from_utf8_lossy(&v.header.fourcc).into_owned())
                            .or_insert(0) += 1;
                    }
                }
                let mbps =
                    (i.stats().video_bytes - b0) as f64 * 8.0 / t0.elapsed().as_secs_f64() / 1e6;
                (by, mbps)
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    threads.into_iter().map(|t| t.join().unwrap()).collect()
}

#[test]
fn an_h264_file_passes_through_and_mpeg2_is_decoded() {
    let ff = Arc::new(Ffmpeg::probe());
    if EncoderFactory::codecs(&*ff).is_empty() {
        println!("no hardware encoder here: skipped");
        return;
    }
    let d = dir("an_h264_file_passes_thro");
    let h264 = d.join("welcome.mp4");
    record_h264(&ff, &h264);
    let p = publisher();
    let player = FilePlayer::open(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("Welcome video")
        },
        &h264,
        ff.clone(),
        PlayerConfig {
            looped: true,
            ..Default::default()
        },
    )
    .unwrap();
    let st = player.status();
    println!("{st:?}");
    assert_eq!(st.pass_through, Ok(Codec::H264));
    assert_eq!(st.video.as_deref(), Some("h264 1280x720"));
    let port = player.output().port();
    let inputs = [input(port, Some(ff.clone())), input(port, None)];
    wait_for_pictures(&inputs);
    // 6 s of a 4 s file: it loops.
    let got = watch(&inputs, 6.0);
    println!("{got:?}");
    let (h, s) = (&got[0], &got[1]);
    assert!(h.0.get("H264").copied().unwrap_or(0) > 120, "{h:?}");
    assert!(s.0.get("SHQ2").copied().unwrap_or(0) > 120, "{s:?}");
    // The file's AAC went as it is to the input taking compressed video (decoded back
    // to sound there), float audio to the other (§E.19.2): a fraction of the bytes.
    let (ha, sa) = (inputs[0].stats(), inputs[1].stats());
    println!(
        "audio: AAC input {} blocks {} bytes, float input {} blocks {} bytes",
        ha.audio, ha.audio_bytes, sa.audio, sa.audio_bytes
    );
    assert!(ha.audio > 100 && ha.audio_errors == 0, "{ha:?}");
    assert!(
        ha.audio_bytes * 5 < sa.audio_bytes,
        "{} vs {}",
        ha.audio_bytes,
        sa.audio_bytes
    );
    // The file's own pictures; encodes only until the file's next keyframe after the
    // input joined (keyframes 2 s apart: at most about 60 pictures).
    let os = player.output().stats();
    println!("passed {}, encoded {}", os.codec_passed, os.codec_encoded);
    assert!(os.codec_passed > 80 && os.codec_encoded <= 66, "{os:?}");
    assert_eq!(inputs[0].stats().video_errors, 0);
    // Pause, seek, resume.
    player.pause();
    std::thread::sleep(Duration::from_millis(300));
    let paused = watch(&inputs[1..], 1.0);
    assert!(paused[0].0.values().sum::<usize>() <= 3, "{paused:?}");
    assert_eq!(player.status().state, PlayerState::Paused);
    player.seek(Duration::from_secs(2));
    player.resume();
    std::thread::sleep(Duration::from_millis(400));
    let pos = player.status().position;
    assert!(
        pos >= Duration::from_secs(2) && pos < Duration::from_millis(3200),
        "{pos:?}"
    );
    drop(inputs);
    drop(player);

    // MPEG-2 (FFmpeg's own encoder): decoded; SpeedHQ, and H.264 from the GPU.
    let mpeg2 = d.join("old.mkv");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=25",
            "-t",
            "3",
            "-c:v",
            "mpeg2video",
            "-q:v",
            "4",
        ])
        .arg(&mpeg2)
        .status()
        .unwrap();
    assert!(ok.success());
    let player = FilePlayer::open(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("Old video")
        },
        &mpeg2,
        ff.clone(),
        PlayerConfig::default(),
    )
    .unwrap();
    let st = player.status();
    println!("{st:?}");
    assert!(st.pass_through.is_err());
    let port = player.output().port();
    let inputs = [input(port, Some(ff.clone())), input(port, None)];
    let got = watch(&inputs, 2.5);
    println!("{got:?}");
    assert!(got[0].0.get("H264").copied().unwrap_or(0) > 40, "{got:?}");
    let os = player.output().stats();
    assert!(os.codec_passed == 0 && os.codec_encoded > 40, "{os:?}");
    assert!(got[1].0.get("SHQ2").copied().unwrap_or(0) > 45, "{got:?}");
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(player.status().state, PlayerState::Ended);
}

/// A 64×36 BMP of one colour (any picture format takes the same one-picture path).
fn write_bmp(path: &Path, rgb: [u8; 3]) {
    let (w, h) = (64u32, 36u32);
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(b"BM");
    for v in [size, 0, 54, 40] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&w.to_le_bytes());
    b.extend_from_slice(&h.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    for v in [0u32, row * h, 2835, 2835, 0, 0] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for _ in 0..h {
        for _ in 0..w {
            b.extend_from_slice(&[rgb[2], rgb[1], rgb[0]]);
        }
        b.resize(b.len() + (row - w * 3) as usize, 0);
    }
    std::fs::write(path, b).unwrap();
}

/// A still played looped keeps sending its picture, so an input that connects later
/// gets it too (a one-picture file used to end at once: its picture only comes out of
/// the decoder at the end, and the loop's length was taken before it did).
#[test]
fn a_looped_still_keeps_sending() {
    let ff = Arc::new(Ffmpeg::probe());
    let path_dir = dir("a_looped_still_keeps_sen");
    let path = path_dir.join("still.bmp");
    write_bmp(&path, [200, 30, 30]);
    let p = publisher();
    let player = FilePlayer::open(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("Still")
        },
        &path,
        ff,
        PlayerConfig {
            looped: true,
            ..Default::default()
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let late = input(player.output().port(), None);
    wait_for_pictures(std::slice::from_ref(&late));
    let seen = watch(&[late], 1.0);
    let n: usize = seen[0].0.values().sum();
    assert!(n >= 10, "{n} pictures in 1 s");
    assert_eq!(player.status().state, PlayerState::Playing);
}

/// A looped AAC file keeps its sound with its pictures across the seam: a flash and a
/// beep at every whole second (one at the start), measured by timestamps over two
/// seams. (A seek to the start skipped the AAC priming packet: the decoder started cold
/// and the sound after each seam was 10 ms late, the last frame 10 ms too long.)
#[test]
fn a_looped_file_keeps_sound_with_pictures_across_the_seam() {
    let path_dir = dir("a_looped_file_keeps_soun");
    let path = path_dir.join("flashbeep.mp4");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("color=black:s=320x180:r=30:d=3,drawbox=c=white:t=fill:enable='lt(mod(t,1),0.03)'")
        .args(["-f", "lavfi", "-i"])
        .arg("aevalsrc='if(lt(mod(t,1),0.1),0.5*sin(2*PI*1000*t),0)':s=48000:d=3")
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("no ffmpeg with libx264 here: skipped");
        return;
    }
    let p = publisher();
    let player = FilePlayer::open(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("Flashbeep")
        },
        &path,
        Arc::new(Ffmpeg::probe()),
        PlayerConfig {
            looped: true,
            ..Default::default()
        },
    )
    .unwrap();
    let rx = input(player.output().port(), None);
    let (mut flashes, mut beeps, mut bright, mut quiet) = (Vec::new(), Vec::new(), true, 0usize);
    // (By timecode too: the file's own time, exact; the timestamps are when each was handed
    // over, which a loaded machine moves by a few ms: bug run M7-T.)
    let (mut flashes_tc, mut beeps_tc) = (Vec::new(), Vec::new());
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end {
        match rx.recv(Duration::from_millis(100)) {
            Some(InputEvent::Video(v)) => {
                let y = &v.picture.planes[0].data;
                let lit = y.iter().map(|&b| b as u64).sum::<u64>() / y.len().max(1) as u64 > 128;
                if lit && !bright {
                    flashes.push(v.header.timestamp);
                    flashes_tc.push(v.header.timecode);
                }
                bright = lit;
            }
            Some(InputEvent::Audio(a)) => {
                for (i, &x) in a.channels[0].iter().enumerate() {
                    if x.abs() > 0.1 {
                        if quiet > 4800 {
                            beeps.push(a.header.timestamp + i as i64 * 10_000_000 / 48_000);
                            beeps_tc.push(a.header.timecode + i as i64 * 10_000_000 / 48_000);
                        }
                        quiet = 0;
                    } else {
                        quiet += 1;
                    }
                }
            }
            _ => {}
        }
    }
    let gaps = |beeps: &[i64], flashes: &[i64]| -> Vec<f64> {
        beeps
            .iter()
            .filter_map(|&b| {
                flashes
                    .iter()
                    .map(|&f| b - f)
                    .min_by_key(|x| x.abs())
                    .filter(|x| x.abs() < 3_000_000)
            })
            .map(|x| x as f64 / 10_000.0)
            .collect()
    };
    let d = gaps(&beeps, &flashes);
    let dt = gaps(&beeps_tc, &flashes_tc);
    println!("beep - flash (ms): by timestamp {d:.2?}, by timecode {dt:.2?}");
    assert!(d.len() >= 6, "{d:?}");
    // The media's own time: exact (the seam's AAC priming slip this guards is 10 ms).
    assert!(dt.len() >= 6 && dt.iter().all(|x| x.abs() < 1.0), "{dt:?}");
    // As handed over: within 2 ms typically, 10 ms at worst on a loaded machine.
    let mut sorted = d.iter().map(|x| x.abs()).collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    assert!(
        sorted[sorted.len() / 2] < 2.0 && sorted[sorted.len() - 1] < 10.0,
        "{d:?}"
    );
}

/// A seek the file can't do (far past its end, as an operator's slip) doesn't stop the
/// player: it keeps its place or ends the pass, and a seek after it works.
#[test]
fn a_seek_past_the_end_doesnt_stop_the_player() {
    let ff = Arc::new(Ffmpeg::probe());
    let mp4_dir = dir("a_seek_past_the_end_does");
    let mp4 = mp4_dir.join("seek.mp4");
    record_h264(&ff, &mp4);
    // The same as a raw H.264 stream: no index to seek by.
    let raw_dir = dir("a_seek_past_the_end_does");
    let raw = raw_dir.join("seek.h264");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&mp4)
        .args(["-c:v", "copy", "-an", "-bsf:v", "h264_mp4toannexb"])
        .arg(&raw)
        .status()
        .is_ok_and(|s| s.success());
    for path in [Some(mp4), made.then_some(raw)].into_iter().flatten() {
        let p = publisher();
        let player = FilePlayer::open(
            &p,
            OutputConfig {
                port: free_port(),
                ..OutputConfig::new("Seek")
            },
            &path,
            ff.clone(),
            PlayerConfig::default(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        player.seek(Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(500));
        let st = player.status().state;
        assert!(
            !matches!(st, PlayerState::Failed(_)),
            "{path:?}: a seek past the end: {st:?}"
        );
        player.seek(Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(800));
        let st = player.status();
        assert!(
            !matches!(st.state, PlayerState::Failed(_)),
            "{path:?}: then a seek to 1 s: {st:?}"
        );
    }
}

/// A slider dragged sends many seeks: only the last one waiting is done, so the player
/// is where the slider stopped at once, not after working through each.
#[test]
fn many_seeks_at_once_end_where_the_last_one_says() {
    let ff = Arc::new(Ffmpeg::probe());
    let path_dir = dir("many_seeks_at_once_end_w");
    let path = path_dir.join("seeks.mp4");
    record_h264(&ff, &path);
    let p = publisher();
    let player = FilePlayer::open(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("Seeks")
        },
        &path,
        ff,
        PlayerConfig {
            paused: true,
            ..Default::default()
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    for k in 0..60 {
        player.seek(Duration::from_millis(40 * k));
    }
    player.seek(Duration::from_millis(3000));
    // Where the last one says, soon.
    let mut reached = None;
    while start.elapsed() < Duration::from_secs(10) {
        let st = player.status();
        if st.position >= Duration::from_millis(2900) {
            reached = Some(start.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let took = reached.expect("the last seek's position");
    println!("61 seeks: at the last one's place after {took:?}");
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

/// A list (vMix-style) plays its items one after another on one output: two videos and a
/// picture, in order, the picture for its seconds, ending on its last; then by hand.
#[test]
fn a_list_plays_its_items_in_turn() {
    use idn_ffmpeg::player::{ListConfig, ListItem, ListPlayer};
    let ff = Arc::new(Ffmpeg::probe());
    let d = dir("a_list_plays_its_items_i");
    let clip = |name: &str, colour: &str| {
        let path = d.join(name);
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("color={colour}:s=320x180:r=25:d=1"))
            .args(["-c:v", "mpeg2video", "-q:v", "2"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg made {name}");
        path.to_string_lossy().into_owned()
    };
    let (red, blue) = (clip("red.mpg", "red"), clip("blue.mpg", "blue"));
    let green = d.join("green.bmp");
    write_bmp(&green, [0, 200, 0]);
    let items = vec![
        ListItem {
            path: red,
            seconds: 5.0,
        },
        ListItem {
            path: blue,
            seconds: 5.0,
        },
        ListItem {
            path: green.to_string_lossy().into_owned(),
            seconds: 0.6,
        },
    ];
    let p = publisher();
    let list = ListPlayer::open(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("List")
        },
        items,
        ff,
        ListConfig {
            looped: false,
            ..Default::default()
        },
    )
    .unwrap();
    let rx = input(list.output().port(), None);
    // The colours seen, in order (a run of the same counted once), by the middle's luma
    // and Cb: red, blue, green.
    let name = |y: u8, cb: u8| match (y, cb) {
        (40..=95, 80..=115) => "red",
        (20..=45, 230..=245) => "blue",
        (110..=150, 30..=70) => "green",
        _ => "other",
    };
    let mut seen: Vec<&str> = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && !list.status().ended {
        if let Some(InputEvent::Video(v)) = rx.recv(Duration::from_millis(20)) {
            let (y, cb) = (&v.picture.planes[0], &v.picture.planes[1]);
            let c = name(
                y.data[y.height / 2 * y.width + y.width / 2],
                cb.data[cb.height / 2 * cb.width + cb.width / 2],
            );
            if seen.last() != Some(&c) && c != "other" {
                seen.push(c);
            }
        }
    }
    assert_eq!(seen, ["red", "blue", "green"], "in turn");
    let s = list.status();
    assert!(s.ended && s.index == 2, "{s:?}");
    // By hand: back to the second.
    list.goto(1);
    let end = Instant::now() + Duration::from_secs(5);
    let mut back = false;
    while Instant::now() < end && !back {
        if let Some(InputEvent::Video(v)) = rx.recv(Duration::from_millis(20)) {
            let (y, cb) = (&v.picture.planes[0], &v.picture.planes[1]);
            back = name(
                y.data[y.height / 2 * y.width + y.width / 2],
                cb.data[cb.height / 2 * cb.width + cb.width / 2],
            ) == "blue";
        }
    }
    assert!(back, "blue again after goto(1)");
    assert_eq!(list.status().index, 1);
}
