//! The switcher end to end (spec/extensions/idn-switcher.md §S.10, test 2): IDN outputs
//! of different colours as inputs, the switcher's program received by an IDN input.

use idn_common::sync::LockOk as _;
use idn_common::testing::{wait_for, wait_until};
use idn_open_protocol::api::{
    Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::Fourcc;
use idn_switcher::scene::{Item, ItemSource};
use idn_switcher::{
    Frame, Kind, Source, Switcher, SwitcherConfig, Trigger, TriggerAction, TriggerEvent, Ycc,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod mixes;
mod outputs;
mod program;
mod sound;
mod titles;

const W: usize = 640;
const H: usize = 360;

/// An output sending one colour (or, with `lower`, white over the bottom half with alpha
/// and transparent above) 30 times a second until dropped.
struct Colour {
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Colour {
    fn start(publisher: &Publisher, name: &str, c: Ycc, lower: bool) -> Colour {
        let out = Arc::new(
            Output::new(
                publisher,
                OutputConfig {
                    alpha: lower,
                    ..OutputConfig::new(name)
                },
            )
            .unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (out, stop) = (out.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if lower {
                        let mut rgba = vec![0u8; W * H * 4];
                        for px in rgba[W * H * 2..].chunks_mut(4) {
                            px.copy_from_slice(&[255, 255, 255, 255]);
                        }
                        let _ = out.send(VideoFrame::rgba(W, H, rgba.into(), AlphaMode::Straight));
                    } else {
                        let f = Frame::solid(W, H, c);
                        let _ = out.send_planes(PlanarFrame {
                            layout: Fourcc::Shq2,
                            width: W,
                            height: H,
                            planes: Arc::new(f.into_planes()),
                            timestamp: None,
                            timecode: None,
                            audio: None,
                            coded: None,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(33));
                }
            })
        };
        Colour {
            out,
            stop,
            thread: Some(thread),
        }
    }

    fn address(&self) -> String {
        format!("127.0.0.1:{}", self.out.port())
    }
}

impl Drop for Colour {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The program as received: Y at the top centre and the bottom centre of the newest picture.
struct Watch {
    last: Arc<Mutex<Option<(u8, u8)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    fn start(address: &str) -> Watch {
        let input = Input::open(InputConfig::new(address)).unwrap();
        let last = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (last, stop) = (last.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(100)) {
                        let y = &v.picture.planes[0];
                        let at = |row: usize| y.data[row * y.width + y.width / 2];
                        *last.lock_ok() = Some((at(H / 4), at(3 * H / 4)));
                    }
                }
            })
        };
        Watch {
            last,
            stop,
            thread: Some(thread),
        }
    }

    /// Waits for (top, bottom) to satisfy `pred`; panics with the last seen after `secs`.
    fn until(&self, secs: u64, what: &str, pred: impl Fn(u8, u8) -> bool) {
        let held = |l: Option<(u8, u8)>| l.is_some_and(|(t, b)| pred(t, b));
        if !wait_until(Duration::from_secs(secs), || held(*self.last.lock_ok())) {
            let l = *self.last.lock_ok();
            panic!("timed out waiting for {what}: last {l:?}");
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn near(v: u8, want: u8) -> bool {
    (v as i32 - want as i32).abs() <= 3
}

/// Input `id` in a scene of its own, over the whole picture (only scenes go on the buses,
/// §S.3): the scene.
fn scene_of(sw: &Switcher, id: u32) -> u32 {
    let s = sw
        .add_input(&format!("{id} (scene)"), Source::Scene)
        .unwrap();
    let mut layers = sw.scene(s).unwrap();
    layers.insert(
        0,
        Item {
            source: ItemSource::Input(id),
            ..Default::default()
        },
    );
    sw.scene_items(s, layers).unwrap();
    s
}
