//! An input on an output of the same process (`InputConfig::local`) takes its pictures and
//! sound as they are, and hears when it closes; without `local`, the same address is
//! received over the network as before.

use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig,
};
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn feed(out: Arc<Output>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (w, h) = (640usize, 360usize);
        let plane = |w: usize, v: u8| Plane {
            width: w,
            height: h,
            data: vec![v; w * h],
        };
        while !stop.load(Ordering::Relaxed) {
            let _ = out.send_planes(PlanarFrame {
                layout: Fourcc::Shq2,
                width: w,
                height: h,
                planes: Arc::new(vec![plane(w, 77), plane(w / 2, 100), plane(w / 2, 150)]),
                timestamp: None,
                timecode: None,
                audio: Some(AudioBlock {
                    sample_rate: 48000,
                    channels: vec![vec![0.25; 1600]; 2],
                }),
                coded: None,
            });
            std::thread::sleep(Duration::from_millis(33));
        }
    })
}

#[test]
fn an_output_of_this_process_is_taken_as_it_is() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Local")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(out.clone(), stop.clone());
    let addr = format!("127.0.0.1:{}", out.port());
    let input = Input::open(InputConfig {
        local: true,
        ..InputConfig::new(&addr)
    })
    .unwrap();
    let (mut connected, mut video, mut audio) = (false, None, 0);
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && (video.is_none() || audio < 3) {
        match input.recv(Duration::from_millis(50)) {
            Some(InputEvent::State(ConnectionState::Connected(a))) => {
                assert_eq!(a.to_string(), addr);
                connected = true;
            }
            Some(InputEvent::Video(v)) => video = Some(v),
            Some(InputEvent::Audio(a)) => {
                assert_eq!((a.header.sample_rate, a.channels.len()), (48000, 2));
                assert_eq!(a.channels[0][0], 0.25);
                assert_eq!(a.via, "local");
                audio += 1;
            }
            _ => {}
        }
    }
    assert!(connected);
    let v = video.expect("a picture");
    assert_eq!(v.via, "local");
    assert_eq!((v.picture.width, v.picture.height), (640, 360));
    assert_eq!(v.header.fourcc, *b"SHQ2");
    // The very samples sent, not a decode of them.
    assert!(v.picture.planes[0].data.iter().all(|&y| y == 77));
    assert!(v.picture.planes[2].data.iter().all(|&c| c == 150));
    assert!(input.stats().video >= 1 && input.stats().audio >= 3);
    // Nobody connected over the network.
    assert_eq!(out.receivers(), 0);

    // The output closing is heard.
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    drop(out);
    let end = Instant::now() + Duration::from_secs(3);
    let mut lost = false;
    while Instant::now() < end && !lost {
        lost = matches!(
            input.recv(Duration::from_millis(50)),
            Some(InputEvent::State(ConnectionState::Lost(_)))
        );
    }
    assert!(lost, "the output's closing wasn't heard");
}

#[test]
fn without_local_the_network_is_used() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Remote")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(out.clone(), stop.clone());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let mut via = None;
    while Instant::now() < end && via.is_none() {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50)) {
            via = Some(v.via);
        }
    }
    assert!(via.is_some_and(|v| v != "local"), "{via:?}");
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}
