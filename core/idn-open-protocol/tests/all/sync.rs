//! Frame-accurate presentation (`sync/1`, spec/extensions/idn-extensions.md §E.14):
//! displays of a sync group show each frame at the same moment.

use crate::common;
use common::free_port;
use idn_common::sync::LockOk;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::LinkConfig;
use idn_open_protocol::sync::{SyncConfig, SyncDelay};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wall_100ns() -> i64 {
    idn_common::epoch::now().as_nanos() as i64 / 100
}

/// Outputs named `names` of one machine, fed together at 25 fps: each tick's frames
/// carry the same timestamp, shifted by `skew` (100 ns units; negative = in the past).
struct Rig {
    ports: Vec<u16>,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    _outs: Vec<Arc<Output>>,
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

fn rig(names: &[&str], skew: Arc<AtomicI64>) -> Rig {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let outs: Vec<Arc<Output>> = names
        .iter()
        .map(|n| {
            Arc::new(
                Output::new(
                    &p,
                    OutputConfig {
                        port: free_port(),
                        frame_rate: (25, 1),
                        ..OutputConfig::new(n)
                    },
                )
                .unwrap(),
            )
        })
        .collect();
    let ports = outs.iter().map(|o| o.port()).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let (o2, s2) = (outs.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let start = Instant::now();
        let mut n = 0u32;
        while !s2.load(Ordering::Relaxed) {
            let ts = wall_100ns() + skew.load(Ordering::Relaxed);
            for o in &o2 {
                let mut f =
                    VideoFrame::rgba(64, 36, vec![n as u8; 64 * 36 * 4].into(), AlphaMode::Opaque);
                f.timestamp = Some(ts);
                let _ = o.send(f);
            }
            n += 1;
            let due = start + Duration::from_millis(40) * n;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    });
    Rig {
        ports,
        stop,
        feeder: Some(feeder),
        _outs: outs,
        _p: p,
    }
}

fn synced(port: u16, group: &str, delay: SyncDelay) -> Input {
    Input::open(InputConfig {
        sync: Some(SyncConfig::new(group, delay)),
        link: Some(LinkConfig::ephemeral()),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap()
}

/// For each input, when (on this machine's clock) each frame timestamp was handed over.
type Seen = Arc<Mutex<BTreeMap<i64, Instant>>>;

fn watch(input: Arc<Input>, stop: Arc<AtomicBool>) -> (Seen, std::thread::JoinHandle<()>) {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let t = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(20)) {
                s.lock_ok().insert(v.header.timestamp, Instant::now());
            }
        }
    });
    (seen, t)
}

#[test]
fn displays_of_a_group_show_each_frame_together() {
    let skew = Arc::new(AtomicI64::new(0));
    let r = rig(&["Stage left", "Stage right"], skew);
    // Three displays of one group: two of the left output, one of the right.
    let inputs: Vec<Arc<Input>> = [r.ports[0], r.ports[0], r.ports[1]]
        .iter()
        .map(|&p| Arc::new(synced(p, "stage", SyncDelay::Millis(100))))
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let watched: Vec<_> = inputs
        .iter()
        .map(|i| watch(i.clone(), stop.clone()))
        .collect();
    std::thread::sleep(Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    let seen: Vec<BTreeMap<i64, Instant>> = watched
        .into_iter()
        .map(|(s, t)| {
            t.join().unwrap();
            s.lock_ok().clone()
        })
        .collect();
    // Timestamps every display showed, after the first second (offsets measured).
    let common: Vec<i64> = seen[0]
        .keys()
        .copied()
        .filter(|ts| seen.iter().all(|s| s.contains_key(ts)))
        .skip(25)
        .collect();
    assert!(common.len() > 50, "{} frames in common", common.len());
    let mut apart: Vec<Duration> = common
        .iter()
        .map(|ts| {
            let times: Vec<Instant> = seen.iter().map(|s| s[ts]).collect();
            *times.iter().max().unwrap() - *times.iter().min().unwrap()
        })
        .collect();
    apart.sort();
    // Nearly every frame within 5 ms; one of this test's three watching threads woken
    // late on a loaded machine (a full run: 5.6 ms once in several runs, 2026-11-16)
    // isn't the displays apart, so the worst only has to stay well inside a frame.
    let (most, worst) = (apart[apart.len() * 95 / 100], apart[apart.len() - 1]);
    assert!(
        most < Duration::from_millis(5),
        "displays {most:?} apart (95 in 100)"
    );
    assert!(
        worst < Duration::from_millis(20),
        "displays {worst:?} apart"
    );
    for i in &inputs {
        let st = i.stats().sync.unwrap();
        assert_eq!(
            (st.mode, st.group.as_str(), st.delay_ms),
            ("fixed", "stage", 100.0)
        );
        assert!(st.offset_us.is_some_and(|o| o.abs() < 5_000), "{st:?}");
    }
}

#[test]
fn groups_keep_their_own_delays() {
    let skew = Arc::new(AtomicI64::new(0));
    let r = rig(&["Stage", "Screens"], skew);
    let stage: Vec<Input> = (0..2)
        .map(|_| synced(r.ports[0], "stage", SyncDelay::Auto))
        .collect();
    let screens: Vec<Input> = (0..2)
        .map(|_| synced(r.ports[1], "screens", SyncDelay::Frames(3)))
        .collect();
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        for i in stage.iter().chain(&screens) {
            while i.try_recv().is_some() {}
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let delays =
        |v: &[Input]| -> Vec<f64> { v.iter().map(|i| i.stats().sync.unwrap().delay_ms).collect() };
    let (st, sc) = (delays(&stage), delays(&screens));
    // Screens: 3 frames at 25 fps. Stage: the delay its displays need on this machine
    // (about a frame), the same on both, agreed through the sender.
    assert_eq!(sc, [120.0, 120.0]);
    assert_eq!(st[0], st[1], "{st:?}");
    assert!(st[0] > 0.0 && st[0] < 100.0, "{st:?}");
    assert_eq!(stage[0].stats().sync.unwrap().mode, "auto");
}

#[test]
fn late_frames_are_dropped_and_counted() {
    // Frames stamped 1 s in the past cannot be shown 100 ms after their timestamp.
    let skew = Arc::new(AtomicI64::new(-10_000_000));
    let r = rig(&["Late"], skew.clone());
    let input = synced(r.ports[0], "late", SyncDelay::Millis(100));
    let mut shown = 0;
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        if let Some(InputEvent::Video(_)) = input.recv(Duration::from_millis(50)) {
            shown += 1;
        }
    }
    let st = input.stats().sync.unwrap();
    // Until the clock offset is measured (the first ping) frames wait from arrival.
    assert!(shown < 15, "{shown} late frames shown");
    assert!(st.late > 50, "{st:?}");
    // On time again: shown.
    skew.store(0, Ordering::Relaxed);
    let mut shown = 0;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        if let Some(InputEvent::Video(_)) = input.recv(Duration::from_millis(50)) {
            shown += 1;
        }
    }
    assert!(shown > 40, "{shown}");
}
