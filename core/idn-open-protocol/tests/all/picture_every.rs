//! A preview asks for one picture in N (`adapt/1`'s `x_idn_every`, §E.16.2): the source
//! sends it about that many, the others not made for it at all.

use idn_open_protocol::api::{Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[test]
fn a_preview_gets_one_picture_in_n() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Screen")).unwrap());
    let (stop, sent) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
    );
    let feeder = {
        let (out, stop, sent) = (out.clone(), stop.clone(), sent.clone());
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
                    planes: Arc::new(vec![plane(w, 120), plane(w / 2, 128), plane(w / 2, 128)]),
                    timestamp: None,
                    timecode: None,
                    audio: None,
                    coded: None,
                });
                sent.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let input = Input::open(InputConfig {
        max_size: Some((160, 90)),
        picture_every: Some(10),
        ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
    })
    .unwrap();
    // The first picture, then a count over the next 3 s.
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if let Some(InputEvent::Video(_)) = input.recv(Duration::from_millis(50)) {
            break;
        }
    }
    let from = sent.load(Ordering::Relaxed);
    let (mut got, end) = (0u64, Instant::now() + Duration::from_secs(3));
    while Instant::now() < end {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50)) {
            assert!(v.picture.width <= 160, "{}", v.picture.width);
            got += 1;
        }
    }
    let offered = sent.load(Ordering::Relaxed) - from;
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    // About one in ten (the clock's sleeps make `offered` a little uneven).
    let want = offered / 10;
    assert!(
        got >= want.saturating_sub(3) && got <= want + 3,
        "{got} pictures of {offered} offered"
    );
}
