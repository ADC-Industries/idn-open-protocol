//! Changed regions only (`regions/1`, spec/extensions/idn-extensions.md §E.15): a mostly
//! static picture costs about its changed area, and receivers see the same pictures.

use crate::common;
use common::free_port;
use idn_common::sync::LockOk;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::LinkConfig;
use idn_open_protocol::shq::Plane;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const W: usize = 640;
const H: usize = 360;

/// A slide: a fixed gradient, with a 48×24 "clock" in the top-right corner that changes
/// every frame.
fn slide(n: u32) -> Vec<u8> {
    let mut px = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) * 4;
            let clock = (W - 64..W - 16).contains(&x) && (16..40).contains(&y);
            let (r, g, b) = if clock {
                let v = ((n * 37 + (x as u32) * 5) % 256) as u8;
                (v, 255 - v, (n % 256) as u8)
            } else {
                ((x * 255 / W) as u8, (y * 255 / H) as u8, 96)
            };
            px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    px
}

/// An output fed `slide`s at 25 fps, frame `n` stamped with timestamp `n + 1`.
struct Rig {
    port: u16,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    _out: Arc<Output>,
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

fn rig(regions: bool) -> Rig {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (25, 1),
                regions,
                ..OutputConfig::new("Lyrics")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let start = Instant::now();
        let mut n = 0u32;
        while !s.load(Ordering::Relaxed) {
            let mut f = VideoFrame::rgba(W, H, slide(n).into(), AlphaMode::Opaque);
            f.timestamp = Some(n as i64 + 1);
            let _ = o.send(f);
            n += 1;
            let due = start + Duration::from_millis(40) * n;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    });
    Rig {
        port: out.port(),
        stop,
        feeder: Some(feeder),
        _out: out,
        _p: p,
    }
}

fn input(port: u16, regions: bool) -> Arc<Input> {
    Arc::new(
        Input::open(InputConfig {
            // Over the connection: the transport is what is tested (§E.26's ring otherwise).
            machine: false,
            regions,
            link: Some(LinkConfig::ephemeral()),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap(),
    )
}

/// Each input's pictures by timestamp.
type Seen = Arc<Mutex<BTreeMap<i64, Vec<Plane>>>>;

fn watch(input: Arc<Input>, stop: Arc<AtomicBool>) -> (Seen, std::thread::JoinHandle<()>) {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let t = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(20)) {
                assert_eq!(&v.header.fourcc, b"SHQ2");
                s.lock_ok().insert(v.header.timestamp, v.picture.planes);
            }
        }
    });
    (seen, t)
}

fn bytes_per_frame(i: &Input) -> f64 {
    let st = i.stats();
    st.video_bytes as f64 / st.video.max(1) as f64
}

#[test]
fn a_static_slide_costs_its_changed_area_and_pictures_stay_equal() {
    let r = rig(true);
    let stop = Arc::new(AtomicBool::new(false));
    let whole = input(r.port, false);
    let (seen_whole, t1) = watch(whole.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(700));
    // Joins mid-stream: a whole picture comes first, no refresh needed (§E.15.5 item 2).
    let regions = input(r.port, true);
    let (seen_regions, t2) = watch(regions.clone(), stop.clone());
    std::thread::sleep(Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    t1.join().unwrap();
    t2.join().unwrap();

    let st = regions.stats();
    assert!(
        st.extensions.iter().any(|e| e == "regions/1"),
        "{:?}",
        st.extensions
    );
    assert!(!whole.stats().extensions.iter().any(|e| e == "regions/1"));
    assert_eq!(st.region_refreshes, 0);
    assert_eq!(st.video_errors, 0);
    // The clock is 48×24 of 640×360: with block rounding, a refresh every 2 s and the
    // headers, still far below a tenth of whole pictures.
    let (b_regions, b_whole) = (bytes_per_frame(&regions), bytes_per_frame(&whole));
    assert!(
        b_regions * 10.0 < b_whole,
        "{b_regions:.0} vs {b_whole:.0} bytes per frame"
    );

    let (a, b) = (seen_regions.lock_ok(), seen_whole.lock_ok());
    let common: Vec<i64> = a.keys().copied().filter(|t| b.contains_key(t)).collect();
    assert!(common.len() > 80, "{} pictures in common", common.len());
    for t in &common {
        assert!(a[t] == b[t], "picture {t} differs");
    }
}

#[test]
fn the_option_on_one_side_only_sends_whole_pictures() {
    for (out_on, in_on) in [(false, true), (true, false)] {
        let r = rig(out_on);
        let i = input(r.port, in_on);
        let end = Instant::now() + Duration::from_secs(2);
        let mut n = 0;
        while Instant::now() < end {
            if let Some(InputEvent::Video(_)) = i.recv(Duration::from_millis(50)) {
                n += 1;
            }
        }
        let st = i.stats();
        assert!(n > 30, "{n} pictures");
        assert!(
            !st.extensions.iter().any(|e| e == "regions/1"),
            "{:?}",
            st.extensions
        );
        // Whole 640×360 gradients: kilobytes each, not a corner's worth.
        assert!(bytes_per_frame(&i) > 5_000.0, "{}", bytes_per_frame(&i));
    }
}
