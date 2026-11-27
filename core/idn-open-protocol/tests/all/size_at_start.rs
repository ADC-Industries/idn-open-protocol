//! A size asked for while the session starts (`adapt/1`, §E.16.2) reaches the source: a
//! switcher adds an input small and, the same moment, puts it on preview (full size).
//! It used to be dropped, leaving the input small for good.

use idn_open_protocol::api::{Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
fn a_size_changed_at_the_start_is_sent() {
    // Encrypted, as on a site: the session takes longer to start, so the window is wider.
    let secure = || Some(idn_open_protocol::secure::SecureConfig::new("St Mark").unwrap());
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: secure(),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Big")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            let (w, h) = (1280usize, 720usize);
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
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    // At moments across the start (the switcher's first tally comes with its next frame,
    // up to a frame period after adding the input).
    for round in 0..20 {
        let input = Input::open(InputConfig {
            max_size: Some((320, 180)),
            secure: secure(),
            ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(round * 3));
        input.set_max_size(None);
        let end = Instant::now() + Duration::from_secs(5);
        let mut last = (0, 0);
        while Instant::now() < end && last != (1280, 720) {
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50)) {
                last = (v.picture.width, v.picture.height);
            }
        }
        assert_eq!(last, (1280, 720), "round {round}: still small");
    }
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}
