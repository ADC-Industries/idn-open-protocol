//! NDI|HX end-to-end (spec chapter 7): real H.264 and AAC produced by ffmpeg are sent as
//! compressed packets to idn-recv, and its output must decode with ffmpeg. Skipped when
//! ffmpeg is not installed.

use crate::common;
use common::idn;

use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
use idn_open_protocol::hx::{CompressedPacket, FLAG_KEYFRAME, FOURCC_AAC, FOURCC_H264};
use idn_open_protocol::media::{AudioHeader, VideoHeader};
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use std::process::Command;

/// A test that can't run without ffmpeg or one of its encoders. With
/// `IDN_REQUIRE_FFMPEG=1` (CI, the lab) that is a failure; otherwise the skip is reported,
/// so that a pass isn't mistaken for a test that ran.
fn skip(what: &str) {
    let msg = format!("SKIPPED: {what} unavailable (set IDN_REQUIRE_FFMPEG=1 to fail instead)");
    if idn_common::switches::on("IDN_REQUIRE_FFMPEG") {
        panic!("{msg}");
    }
    eprintln!("{msg}");
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn hx_h264_and_aac_decode_after_receipt() {
    let dir = idn_common::testing::TempDir::new("idn-hx");
    let h264 = dir.join("src.h264");
    let aac = dir.join("src.aac");
    if !ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=160x120:rate=25",
        "-frames:v",
        "30",
        "-c:v",
        "libx264",
        "-bf",
        "0",
        "-g",
        "10",
        "-pix_fmt",
        "yuv420p",
        "-x264-params",
        "aud=1",
        h264.to_str().unwrap(),
    ]) || !ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=1000:sample_rate=48000",
        "-ac",
        "2",
        "-t",
        "1",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        aac.to_str().unwrap(),
    ]) {
        skip("ffmpeg/libx264");
        return;
    }
    let aus: Vec<(bool, Vec<u8>, Vec<u8>)> =
        idn_open_protocol::hx::access_units(&std::fs::read(&h264).unwrap(), false)
            .into_iter()
            .map(|a| (a.keyframe, a.params, a.data))
            .collect();
    assert_eq!(aus.len(), 30);
    let aac_bytes = std::fs::read(&aac).unwrap();
    let (asc, aframes) = idn_open_protocol::hx::adts_frames(&aac_bytes).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = dir.join("out");
    let child = idn("recv")
        .args([
            &format!("127.0.0.1:{port}"),
            "--seconds",
            "3",
            "--transport",
            "inline",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut s, _) = listener.accept().unwrap();
    let mut rx = s.try_clone().unwrap();
    for _ in 0..5 {
        read_frame(&mut rx, &Limits::default()).unwrap().unwrap();
    }
    let vh = VideoHeader {
        fourcc: FOURCC_H264,
        width: 160,
        height: 120,
        frame_rate_n: 25,
        frame_rate_d: 1,
        aspect_ratio: 4.0 / 3.0,
        field_id: 0,
        stream: 1,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    for (i, (key, ps, au)) in aus.iter().enumerate() {
        let pkt = CompressedPacket {
            fourcc: FOURCC_H264,
            pts: i as i64 * 400_000,
            dts: i as i64 * 400_000,
            counter: i as u64,
            flags: if *key { FLAG_KEYFRAME } else { 0 },
            data: au,
            extra: if *key { ps } else { &[] },
        };
        s.write_all(
            &Frame {
                kind: kind::VIDEO,
                scrambled: true,
                version: 0,
                ext: vh.encode(),
                payload: pkt.encode(),
            }
            .encode(),
        )
        .unwrap();
    }
    let ah = AudioHeader {
        fourcc: FOURCC_AAC,
        samples: 1024,
        channels: 2,
        sample_rate: 48000,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    for (i, f) in aframes.iter().enumerate() {
        let pkt = CompressedPacket {
            fourcc: FOURCC_AAC,
            pts: i as i64,
            dts: i as i64,
            counter: i as u64,
            flags: 1,
            data: f,
            extra: &asc,
        };
        s.write_all(
            &Frame {
                kind: kind::AUDIO,
                scrambled: true,
                version: 1,
                ext: ah.encode(),
                payload: pkt.encode(),
            }
            .encode(),
        )
        .unwrap();
    }
    let child = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&child.stdout);
    println!("{stdout}");
    assert!(child.status.success());
    assert!(stdout.contains("video messages decoded: 30"), "{stdout}");
    assert!(stdout.contains("HX keyframes: 3"), "{stdout}");

    let count = |path: &Path, stream: &str| -> u64 {
        let o = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                stream,
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "csv=p=0",
                path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    };
    assert_eq!(count(&out.join("video.h264"), "v:0"), 30);
    assert_eq!(count(&out.join("audio.aac"), "a:0"), aframes.len() as u64);
    // The received elementary stream decodes to the same pictures as the source.
    let md5 = |p: &Path| {
        let o = Command::new("ffmpeg")
            .args(["-v", "error", "-i", p.to_str().unwrap(), "-f", "md5", "-"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    assert_eq!(md5(&out.join("video.h264")), md5(&h264));
}

/// idn-send --hx → idn-recv for H.264 and HEVC with AAC; the receiver's output decodes.
#[test]
fn hx_sender_to_receiver() {
    let dir = idn_common::testing::TempDir::new("idn-hxs");
    let aac = dir.join("a.aac");
    if !ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-ac",
        "2",
        "-t",
        "2",
        "-c:a",
        "aac",
        aac.to_str().unwrap(),
    ]) {
        skip("ffmpeg");
        return;
    }
    for (codec, ext, port) in [("libx264", "h264", 16201u16), ("libx265", "hevc", 16202)] {
        // A thousand up in a release build: the debug and release runs can run at once.
        let port = if cfg!(debug_assertions) {
            port
        } else {
            port + 1000
        };
        let src = dir.join(format!("v.{ext}"));
        let params = if codec == "libx264" {
            vec!["-x264-params", "aud=1"]
        } else {
            vec!["-x265-params", "aud=1:log-level=error"]
        };
        let mut a = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=25",
            "-frames:v",
            "50",
            "-c:v",
            codec,
            "-bf",
            "0",
            "-g",
            "25",
            "-pix_fmt",
            "yuv420p",
        ];
        a.extend(params);
        a.push(src.to_str().unwrap());
        if !ffmpeg(&a) {
            skip(codec);
            continue;
        }
        let mut snd = idn("send")
            .args([
                "--name",
                "HX",
                "--size",
                "320x180",
                "--fps",
                "25",
                "--seconds",
                "8",
                "--port",
                &port.to_string(),
                "--no-mdns",
                "--hx",
                src.to_str().unwrap(),
                "--aac",
                aac.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let out = dir.join(format!("out-{ext}"));
        let o = idn("recv")
            .args([
                &format!("127.0.0.1:{port}"),
                // 5 s for the 60 frames asked of it: right after a build, under load, the
                // sender's encoder can start a second or so late (36 frames came in 3 s).
                "--seconds",
                "5",
                "--out",
                out.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let _ = snd.kill();
        let _ = snd.wait();
        let stdout = String::from_utf8_lossy(&o.stdout);
        println!("{stdout}");
        assert!(
            stdout.contains(&format!("{} 320x180 HX", ext.to_uppercase())),
            "{stdout}"
        );
        assert!(stdout.contains("q_change_slow"), "{stdout}");
        let vfile = out.join(format!("video.{ext}"));
        let n = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "csv=p=0",
                vfile.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let frames: u64 = String::from_utf8_lossy(&n.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        assert!(frames >= 60, "{ext}: decoded {frames} frames");
        let dec = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                vfile.to_str().unwrap(),
                "-f",
                "null",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            dec.stderr.is_empty(),
            "{ext} decode errors: {}",
            String::from_utf8_lossy(&dec.stderr)
        );
        assert!(out.join("audio.aac").exists());
    }
}

/// §7.1.4: after a counter gap the receiver discards non-keyframes until a keyframe and
/// sends a kind-3 keyframe request for each discarded frame.
#[test]
fn hx_counter_gap_discards_and_requests_keyframe() {
    use idn_open_protocol::control::parse_element;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = idn_common::testing::TempDir::new("idn-hxgap");
    let child = idn("recv")
        .args([
            &format!("127.0.0.1:{port}"),
            "--seconds",
            "2",
            "--transport",
            "inline",
            "--no-reconnect",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut s, _) = listener.accept().unwrap();
    let mut rx = s.try_clone().unwrap();
    for _ in 0..5 {
        read_frame(&mut rx, &Limits::default()).unwrap().unwrap();
    }
    let vh = VideoHeader {
        fourcc: FOURCC_H264,
        width: 16,
        height: 16,
        frame_rate_n: 25,
        frame_rate_d: 1,
        aspect_ratio: 1.0,
        field_id: 0,
        stream: 1,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    // counters: 0K 1 2 [3 lost] 4 5 6K 7  → kept: 0 1 2 6 7; discarded: 4 5.
    for (c, key) in [
        (0u64, true),
        (1, false),
        (2, false),
        (4, false),
        (5, false),
        (6, true),
        (7, false),
    ] {
        let data = [0u8, 0, 0, 1, 0x09, c as u8];
        let pkt = CompressedPacket {
            fourcc: FOURCC_H264,
            pts: 0,
            dts: 0,
            counter: 1000 + c,
            flags: if key { FLAG_KEYFRAME } else { 0 },
            data: &data,
            extra: if key { &[0, 0, 0, 1, 0x67] } else { &[] },
        };
        s.write_all(
            &Frame {
                kind: kind::VIDEO,
                scrambled: true,
                version: 0,
                ext: vh.encode(),
                payload: pkt.encode(),
            }
            .encode(),
        )
        .unwrap();
    }
    rx.set_read_timeout(Some(Duration::from_millis(800)))
        .unwrap();
    let mut requests = 0;
    while let Ok(Some(f)) = read_frame(&mut rx, &Limits::default()) {
        let e = parse_element(f.text_payload().unwrap()).unwrap();
        if e.name == "ndi_video" && e.attr("request_keyframe") == Some("true") {
            assert_eq!(f.kind, kind::METADATA);
            requests += 1;
        }
    }
    let o = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    println!("{stdout}");
    assert_eq!(requests, 2);
    assert!(
        stdout.contains("frames discarded waiting for a keyframe: 2"),
        "{stdout}"
    );
    // The written elementary stream holds exactly the kept frames' AUD payload bytes 0,1,2,6,7.
    let es = std::fs::read(out.join("video.h264")).unwrap();
    let kept: Vec<u8> = es
        .windows(5)
        .filter(|w| w[..4] == [0, 0, 1, 0x09])
        .map(|w| w[4])
        .collect();
    assert_eq!(kept, vec![0, 1, 2, 6, 7]);
}

/// 07-hx.md §7.1.1: idn-send's HX keyframes carry the parameter sets in-band (AUD, then
/// SPS/PPS or VPS/SPS/PPS, then slices) AND as extra data, read straight off the wire as
/// an NDI receiver sees them. NDI Studio Monitor shows black video otherwise.
#[test]
fn hx_keyframes_carry_parameter_sets_in_band() {
    let dir = idn_common::testing::TempDir::new("idn-hxib");
    for (codec, ext, port, hevc) in [
        ("libx264", "h264", 16211u16, false),
        ("libx265", "hevc", 16212, true),
    ] {
        // A thousand up in a release build: the debug and release runs can run at once.
        let port = if cfg!(debug_assertions) {
            port
        } else {
            port + 1000
        };
        let src = dir.join(format!("v.{ext}"));
        let p = if hevc {
            vec!["-x265-params", "aud=1:log-level=error"]
        } else {
            vec!["-x264-params", "aud=1"]
        };
        let mut a = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=25",
            "-frames:v",
            "30",
            "-c:v",
            codec,
            "-bf",
            "0",
            "-g",
            "10",
            "-pix_fmt",
            "yuv420p",
        ];
        a.extend(p);
        a.push(src.to_str().unwrap());
        if !ffmpeg(&a) {
            skip(codec);
            continue;
        }
        let mut args = vec![
            "--name",
            "HXIB",
            "--size",
            "320x180",
            "--fps",
            "25",
            "--seconds",
            "5",
            "--port",
        ];
        let port_s = port.to_string();
        args.push(&port_s);
        args.extend(["--no-mdns", "--hx", src.to_str().unwrap()]);
        if hevc {
            args.push("--hevc");
        }
        let mut snd = idn("send")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let mut s = common::connect_when_up(port);
        for x in [
            r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.0" platform="LINUX"/>"#,
            r#"<ndi_identify name="TEST (HXIB)"/>"#,
            r#"<ndi_video quality="high"/>"#,
            r#"<ndi_enabled_streams video="true" audio="false" text="true"/>"#,
        ] {
            s.write_all(&Frame::text(kind::CONTROL, 0, x).encode())
                .unwrap();
        }
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut keyframes = 0;
        while keyframes < 2 {
            let f = read_frame(&mut s, &Limits::default()).unwrap().unwrap();
            if f.kind != kind::VIDEO {
                continue;
            }
            let pkt = CompressedPacket::parse(&f.payload).unwrap();
            if !pkt.keyframe() {
                continue;
            }
            keyframes += 1;
            let types: Vec<u8> = idn_open_protocol::hx::nal_units(pkt.data)
                .iter()
                .map(|n| {
                    let h = n[n.iter().position(|&b| b == 1).unwrap() + 1];
                    if hevc { (h >> 1) & 0x3f } else { h & 0x1f }
                })
                .collect();
            let want: &[u8] = if hevc { &[35, 32, 33, 34] } else { &[9, 7, 8] };
            assert_eq!(
                &types[..want.len()],
                want,
                "{codec}: keyframe NAL order {types:?}"
            );
            assert!(
                !pkt.extra.is_empty(),
                "{codec}: parameter sets also as extra data"
            );
            assert!(
                pkt.data.windows(pkt.extra.len()).any(|w| w == pkt.extra),
                "{codec}: extra repeats the in-band sets"
            );
        }
        let _ = snd.kill();
        let _ = snd.wait();
    }
}

/// §7.1.2: with a sender that has no `x_idn_color` (every NDI|HX sender), the range and
/// matrix the decoder reports set what the application sees and the RGBA conversion;
/// untagged is limited range, BT.709 from 720 lines up and BT.601 below.
#[test]
fn hx_colour_comes_from_the_bitstream_tags() {
    use idn_open_protocol::codec::{
        Codec, CodecInput, CodecUse, Decoded, DecoderFactory, VideoDecoder,
    };
    use idn_open_protocol::color::AlphaMode;
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    use idn_open_protocol::shq::Plane;
    use std::sync::Arc;
    use std::time::Instant;

    /// A stand-in decoder: the access unit's last 5 bytes are width, height (u16 BE) and
    /// the Y value of a flat grey picture; `full`/`matrix` are what its "VUI" says.
    struct TagDec(Option<bool>, Option<&'static str>);
    impl DecoderFactory for TagDec {
        fn codecs(&self) -> Vec<Codec> {
            vec![Codec::H264]
        }
        fn open(&self, _codec: Codec) -> std::io::Result<Box<dyn VideoDecoder>> {
            Ok(Box::new(TagDec(self.0, self.1)))
        }
    }
    impl VideoDecoder for TagDec {
        fn decode(&mut self, d: &[u8], _key: bool, _pts: i64) -> std::io::Result<Option<Decoded>> {
            let t = &d[d.len() - 5..];
            let (w, h) = (
                u16::from_be_bytes([t[0], t[1]]) as usize,
                u16::from_be_bytes([t[2], t[3]]) as usize,
            );
            let flat = |pw: usize, ph: usize, v: u8| Plane {
                width: pw,
                height: ph,
                data: vec![v; pw * ph],
            };
            Ok(Some(Decoded {
                width: w,
                height: h,
                planes: vec![
                    flat(w, h, t[4]),
                    flat(w.div_ceil(2), h.div_ceil(2), 128),
                    flat(w.div_ceil(2), h.div_ceil(2), 128),
                ],
                full_range: self.0,
                matrix: self.1,
                pts: None,
            }))
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // (decoder tags, picture height) → (range, matrix, R of the Y 16 grey).
    type Tags = (Option<bool>, Option<&'static str>);
    let cases: [(Tags, u16, (&str, &str, u8)); 4] = [
        ((None, None), 180, ("limited", "bt_601", 0)),
        ((None, None), 720, ("limited", "bt_709", 0)),
        ((Some(true), None), 180, ("full", "bt_601", 16)),
        ((Some(false), Some("bt_601")), 720, ("limited", "bt_601", 0)),
    ];
    for (tags, height, want) in cases {
        let input = Input::open(InputConfig {
            codec: Some(CodecInput::new(
                Arc::new(TagDec(tags.0, tags.1)),
                CodecUse::Prefer,
            )),
            rgba: Some(AlphaMode::Opaque),
            reconnect: false,
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        let (mut s, _) = listener.accept().unwrap();
        // The receiver's greeting; whatever it sends later stays in the socket buffer.
        let mut rx = s.try_clone().unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        while let Ok(Some(_)) = read_frame(&mut rx, &Limits::default()) {}
        let vh = VideoHeader {
            fourcc: FOURCC_H264,
            width: 320,
            height: height as u32,
            frame_rate_n: 25,
            frame_rate_d: 1,
            aspect_ratio: 16.0 / 9.0,
            field_id: 0,
            stream: 1,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        let mut data = vec![0u8, 0, 0, 1, 0x09];
        data.extend_from_slice(&320u16.to_be_bytes());
        data.extend_from_slice(&height.to_be_bytes());
        data.push(16); // limited-range black, full-range RGB 16
        for c in 0..3u64 {
            let pkt = CompressedPacket {
                fourcc: FOURCC_H264,
                pts: c as i64,
                dts: c as i64,
                counter: c,
                flags: FLAG_KEYFRAME,
                data: &data,
                extra: &[],
            };
            s.write_all(
                &Frame {
                    kind: kind::VIDEO,
                    scrambled: true,
                    version: 0,
                    ext: vh.encode(),
                    payload: pkt.encode(),
                }
                .encode(),
            )
            .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let got = loop {
            assert!(
                Instant::now() < deadline,
                "no picture for {tags:?} at {height} lines"
            );
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50)) {
                let rgba = v.rgba.expect("RGBA was asked for");
                break (v.color.range.clone(), v.color.matrix.clone(), rgba[0]);
            }
        };
        assert_eq!(
            (got.0.as_str(), got.1.as_str()),
            (want.0, want.1),
            "{tags:?} at {height} lines"
        );
        assert!(
            got.2.abs_diff(want.2) <= 1,
            "{tags:?} at {height} lines: R {} wanted {}",
            got.2,
            want.2
        );
        drop(input);
    }
}
