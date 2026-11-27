//! A source that ignores size requests (an NDI sender, scripted here) is decoded at half
//! size when the input's `max_size` is half the picture or less, and at full size
//! otherwise; with two pictures decoding at once and one at a time.

use idn_open_protocol::color::{AlphaMode, ColorConfig, rgba_to_planes};
use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::media::VideoHeader;
use idn_open_protocol::shq::Fourcc;
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// Sends `n` 640×360 SHQ2 pictures on the control connection, as an NDI sender would.
fn ndi_like_sender(listener: TcpListener, n: usize) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut rx = s.try_clone().unwrap();
        let limits = Limits::default();
        for _ in 0..5 {
            read_frame(&mut rx, &limits).unwrap().unwrap();
        }
        let ver = r#"<ndi_version text="3" video="7" audio="5" sdk="x" platform="LINUX"/>"#;
        s.write_all(&Frame::text(kind::CONTROL, 0, ver).encode())
            .unwrap();
        s.write_all(&Frame::text(kind::CONTROL, 0, r#"<ndi_identify name="T (S)"/>"#).encode())
            .unwrap();
        let (w, h) = (640, 360);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 90, 255])
            .collect();
        let planes = rgba_to_planes(
            &rgba,
            w,
            h,
            w * 4,
            Fourcc::Shq2,
            AlphaMode::Opaque,
            &ColorConfig::default(),
        );
        let payload = encode(
            &EncodeInput {
                fourcc: Fourcc::Shq2,
                width: w,
                height: h,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions {
                quality: 90,
                ..Default::default()
            },
        );
        for i in 0..n {
            let vh = VideoHeader {
                fourcc: *b"SHQ2",
                width: w as u32,
                height: h as u32,
                frame_rate_n: 30,
                frame_rate_d: 1,
                aspect_ratio: 16.0 / 9.0,
                field_id: 0,
                stream: 1,
                timecode: i as i64,
                timestamp: i as i64,
                metadata: None,
            };
            let v = Frame {
                kind: kind::VIDEO,
                scrambled: true,
                version: 0,
                ext: vh.encode(),
                payload: payload.clone(),
            };
            if s.write_all(&v.encode()).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let _ = read_frame(&mut rx, &limits);
    })
}

/// The sizes of the pictures an input with this `max_size` gets.
fn sizes(max_size: Option<(u32, u32)>, parallel: bool) -> Vec<(usize, usize)> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let sender = ndi_like_sender(listener, 10);
    let input = Input::open(InputConfig {
        transports: Vec::new(),
        max_size,
        parallel_decode: Some(parallel),
        rgba: Some(AlphaMode::Opaque),
        reconnect: false,
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while got.len() < 10 && Instant::now() < end {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50)) {
            let rgba = v.rgba.expect("rgba asked for");
            assert_eq!(rgba.len(), v.picture.width * v.picture.height * 4);
            got.push((v.picture.width, v.picture.height));
        }
    }
    drop(input);
    let _ = sender.join();
    got
}

#[test]
fn half_size_when_the_source_does_not_scale() {
    for parallel in [true, false] {
        let half = sizes(Some((320, 180)), parallel);
        assert!(
            half.len() >= 8 && half.iter().all(|&s| s == (320, 180)),
            "{half:?}"
        );
        let full = sizes(Some((400, 300)), parallel);
        assert!(
            full.len() >= 8 && full.iter().all(|&s| s == (640, 360)),
            "{full:?}"
        );
        let none = sizes(None, parallel);
        assert!(
            none.len() >= 8 && none.iter().all(|&s| s == (640, 360)),
            "{none:?}"
        );
    }
}
