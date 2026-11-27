//! The bridge (§O.5) both ways, as the Node runs it: an IDN source through `idn-omt send`
//! to an OMT receiver, and an OMT source through `idn-omt recv` to an IDN receiver.

use crate::common;
use common::idn;

use idn_omt::vmx::planes_to_uyvy;
use idn_omt::{Event, Receiver, Sender, SenderConfig};
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};

struct Kill(std::process::Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn an_idn_source_goes_out_as_omt() {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&p, OutputConfig::new("Bridge test")).unwrap();
    let mut prog = Kill(
        idn("omt")
            .args([
                "send",
                &format!("127.0.0.1:{}", out.port()),
                "--name",
                "Bridged out",
            ])
            .args(["--no-node", "--seconds", "20"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // Its OMT port, from what it says.
    let err = BufReader::new(prog.0.stderr.take().unwrap());
    let mut lines = err.lines().map_while(Result::ok);
    let port: u16 = lines
        .find_map(|l| {
            l.split("(port ")
                .nth(1)
                .and_then(|p| p.trim_end_matches(')').parse().ok())
        })
        .expect("the OMT port");
    std::thread::spawn(move || for _ in lines {});
    let rx = Receiver::start(&format!("127.0.0.1:{port}"), true).unwrap();
    let rgba: std::sync::Arc<[u8]> = vec![200u8; 640 * 360 * 4].into();
    let end = Instant::now() + Duration::from_secs(15);
    let (mut v, mut a) = (0, 0);
    while Instant::now() < end && (v < 5 || a < 3) {
        let _ = out.send(VideoFrame::rgba(640, 360, rgba.clone(), AlphaMode::Opaque));
        let _ = out.send_audio(
            AudioBlock {
                sample_rate: 48000,
                channels: vec![vec![0.5; 1600]; 2],
            },
            None,
        );
        while let Some(e) = rx.recv(Duration::from_millis(30)) {
            match e {
                Event::Video(p) => {
                    assert_eq!((p.width, p.height), (640, 360));
                    v += 1;
                }
                Event::Audio { channels, .. } => {
                    assert!((channels[0][5] - 0.5).abs() < 1e-6);
                    a += 1;
                }
                _ => {}
            }
        }
    }
    assert!(v >= 5 && a >= 3, "video {v}, audio {a}");
    // The OMT receiver's tally reaches the IDN source.
    rx.set_tally(true, false);
    let end = Instant::now() + Duration::from_secs(5);
    while out.tally() != (true, false) && Instant::now() < end {
        let _ = out.send(VideoFrame::rgba(640, 360, rgba.clone(), AlphaMode::Opaque));
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(out.tally(), (true, false));
}

#[test]
fn an_omt_source_comes_in_as_idn() {
    let tx = Sender::start(SenderConfig {
        announce: false,
        ..SenderConfig::new("Bridge in")
    })
    .unwrap();
    let name = format!("Bridged in {}", std::process::id());
    let _prog = Kill(
        idn("omt")
            .args(["recv", &format!("127.0.0.1:{}", tx.port()), "--name", &name])
            .args(["--no-node", "--seconds", "20"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let host = idn_open_protocol::discovery::hostname();
    let input = Input::open(InputConfig::new(&format!("{host} ({name})"))).unwrap();
    let (w, h) = (640usize, 360usize);
    let y: Vec<u8> = (0..w * h).map(|i| 16 + ((i % w) * 200 / w) as u8).collect();
    let (cb, cr) = (vec![128u8; w * h / 2], vec![128u8; w * h / 2]);
    let end = Instant::now() + Duration::from_secs(15);
    let (mut v, mut a) = (0, 0);
    let mut k = 0;
    while Instant::now() < end && (v < 5 || a < 3) {
        let mut uyvy = planes_to_uyvy(&y, &cb, &cr, None, w, h);
        tx.send_video(&mut uyvy, w, h, (30, 1), false, false, k * 333_333)
            .unwrap();
        tx.send_audio(&[vec![0.25; 1600], vec![0.25; 1600]], 48000, k * 333_333);
        k += 1;
        while let Some(e) = input.recv(Duration::from_millis(30)) {
            match e {
                InputEvent::Video(p) => {
                    assert_eq!((p.picture.width, p.picture.height), (w, h));
                    let row = &p.picture.planes[0].data;
                    assert!(
                        (row[w - 1] as i32 - y[w - 1] as i32).abs() <= 16,
                        "{} vs {}",
                        row[w - 1],
                        y[w - 1]
                    );
                    v += 1;
                }
                InputEvent::Audio(au) => {
                    assert!((au.channels[0][3] - 0.25).abs() < 1e-3);
                    a += 1;
                }
                _ => {}
            }
        }
    }
    assert!(v >= 5 && a >= 3, "video {v}, audio {a}");
    // The IDN receiver's tally reaches the OMT source.
    input.set_tally(true, true);
    idn_common::testing::wait_until(Duration::from_secs(5), || tx.tally() == (true, true));
    assert_eq!(tx.tally(), (true, true));
}
