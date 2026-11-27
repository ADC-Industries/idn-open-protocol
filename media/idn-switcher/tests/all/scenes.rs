//! Scenes as OBS has them (spec/extensions/idn-switcher.md §S.3, §S.5, §S.7, §S.12): only
//! scenes on the buses, layers with stable ids shown and hidden with a fade, global layers,
//! no fixed input limit, inputs received only while in use, the adjustable multiview.

use idn_common::sync::LockOk as _;
use idn_common::testing::wait_for;
use idn_open_protocol::api::{Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::Fourcc;
use idn_switcher::multiview::{TileKind, TileSpec};
use idn_switcher::scene::{Item, ItemSource};
use idn_switcher::{Frame, Source, Switcher, SwitcherConfig, Ycc};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const W: usize = 640;
const H: usize = 360;

fn start(name: &str, idle_after: Duration) -> Switcher {
    Switcher::start(SwitcherConfig {
        name: name.into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        idle_after,
        ..Default::default()
    })
    .unwrap()
}

fn layer(id: u32) -> Item {
    Item {
        source: ItemSource::Input(id),
        ..Default::default()
    }
}

/// The program's luma at (x, y), as sent.
fn watch(sw: &Switcher, x: usize, y: usize) -> (Arc<Mutex<u8>>, idn_open_protocol::api::Tap) {
    let seen = Arc::new(Mutex::new(0u8));
    let g = seen.clone();
    let tap = sw.program_output().tap(move |f| {
        if let Some(p) = f.planes.first()
            && !p.data.is_empty()
        {
            *g.lock_ok() = p.data[y * p.width + x];
        }
    });
    (seen, tap)
}

fn input(sw: &Switcher, id: u32) -> Value {
    sw.state()["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .cloned()
        .unwrap()
}

/// Only scenes go on program, preview and the mixes; a layer keeps its id as the scene's
/// list is sent back; showing one fades it in over its fade time, not as an undo step (the
/// engine has none) and with the state moving along; at most 16 layers.
#[test]
fn scenes_layers_and_their_fades() {
    let sw = start("LayerTest", Duration::from_secs(10));
    let black = sw.add_input("Black", Source::Colour(Ycc::BLACK)).unwrap();
    let white = sw.add_input("White", Source::Colour(Ycc::WHITE)).unwrap();
    assert!(sw.program(black).is_err(), "an input isn't a scene");
    assert!(sw.state()["program"].is_null(), "inputs fill no bus");
    let sc = sw.add_input("Scene", Source::Scene).unwrap();
    assert_eq!(sw.state()["program"], sc, "the first scene fills program");
    // White over black, hidden, fading over 0.6 s.
    sw.scene_items(
        sc,
        vec![
            layer(black),
            Item {
                name: "Card".into(),
                volunteer: true,
                visible: false,
                fade: 0.6,
                ..layer(white)
            },
        ],
    )
    .unwrap();
    let ids: Vec<u32> = sw.scene(sc).unwrap().iter().map(|l| l.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] != 0 && ids[0] != ids[1], "{ids:?}");
    // Sent back reordered with a new one on top: the ids stay, the new one gets its own.
    let mut back = sw.scene(sc).unwrap();
    back.reverse();
    back.insert(
        0,
        Item {
            visible: false,
            ..layer(black)
        },
    );
    sw.scene_items(sc, back).unwrap();
    let now: Vec<u32> = sw.scene(sc).unwrap().iter().map(|l| l.id).collect();
    assert_eq!(&now[1..], [ids[1], ids[0]]);
    assert!(!ids.contains(&now[0]));
    // Back to two, in their places.
    let mut two = sw.scene(sc).unwrap();
    two.remove(0);
    two.reverse();
    sw.scene_items(sc, two).unwrap();
    let card = ids[1];
    let st = sw.state();
    let l = &input(&sw, sc)["source"]["scene"]["items"][1];
    assert_eq!(
        (&l["id"], &l["name"], &l["volunteer"], &l["visible"]),
        (
            &Value::from(card),
            &Value::from("Card"),
            &Value::from(true),
            &Value::from(false)
        ),
        "{st}"
    );
    // Shown with its fade: through the middle, then white.
    let (seen, _tap) = watch(&sw, W / 2, H / 2);
    wait_for("black", Duration::from_secs(3), || *seen.lock_ok() == 16);
    let v = sw.version();
    sw.scene_layer_visible(sc, card, true).unwrap();
    let mut mids = 0;
    wait_for("white after the fade", Duration::from_secs(3), || {
        let y = *seen.lock_ok();
        if (40..210).contains(&y) {
            mids += 1;
        }
        y == 235
    });
    assert!(mids > 0, "a fade, not a cut");
    assert!(sw.version() > v + 3, "the state moved along with the fade");
    assert_eq!(
        input(&sw, sc)["source"]["scene"]["items"][1]["visible"],
        true
    );
    // Hidden again (a fade of 0: at once).
    let mut l = sw.scene(sc).unwrap();
    l[1].fade = 0.0;
    sw.scene_items(sc, l).unwrap();
    sw.scene_layer_visible(sc, card, false).unwrap();
    wait_for("black at once", Duration::from_secs(1), || {
        *seen.lock_ok() == 16
    });
    assert!(sw.scene_layer_visible(sc, 999, true).is_err());
    assert!(
        sw.scene_layer_visible(white, 1, true).is_err(),
        "not a scene"
    );
    // At most 16 layers.
    assert!(sw.scene_items(sc, vec![layer(black); 16]).is_ok());
    assert!(
        sw.scene_items(sc, vec![layer(black); 17])
            .unwrap_err()
            .contains("16")
    );
}

/// Global layers (§S.5): in every scene made after, as an ordinary layer of it; a change
/// of the definition reaches each copy but its place and visibility; removed from all.
#[test]
fn global_layers_in_every_new_scene() {
    let sw = start("GlobalTest", Duration::from_secs(10));
    let cam = sw
        .add_input("Cam", Source::Colour(Ycc(80, 128, 128)))
        .unwrap();
    let logo = sw.add_input("Logo", Source::Colour(Ycc::WHITE)).unwrap();
    let before = sw.add_input("Before", Source::Scene).unwrap();
    let g = sw
        .set_global_layer(
            Item {
                name: "Logo".into(),
                rect: (0.8, 0.0, 0.2, 0.2),
                ..layer(logo)
            },
            false,
        )
        .unwrap();
    assert!(
        sw.scene(before).unwrap().is_empty(),
        "not in a scene made before"
    );
    let a = sw.add_input("A", Source::Scene).unwrap();
    let b = sw.add_input("B", Source::Scene).unwrap();
    for s in [a, b] {
        let l = sw.scene(s).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].global, l[0].name.as_str()), (Some(g), "Logo"));
    }
    // In A: the camera beneath, the logo hidden; in B: the logo moved under the camera.
    let mut la = sw.scene(a).unwrap();
    la[0].visible = false;
    la.insert(0, layer(cam));
    sw.scene_items(a, la).unwrap();
    let mut lb = sw.scene(b).unwrap();
    lb.push(layer(cam));
    sw.scene_items(b, lb).unwrap();
    // The definition changed: both copies take it, each keeping its place and visibility.
    sw.set_global_layer(
        Item {
            id: g,
            name: "Logo 2".into(),
            rect: (0.0, 0.0, 0.25, 0.25),
            opacity: 0.5,
            ..layer(logo)
        },
        false,
    )
    .unwrap();
    let la = sw.scene(a).unwrap();
    assert_eq!(la[1].global, Some(g));
    assert_eq!(
        (
            la[1].name.as_str(),
            la[1].rect,
            la[1].opacity,
            la[1].visible
        ),
        ("Logo 2", (0.0, 0.0, 0.25, 0.25), 0.5, false)
    );
    let lb = sw.scene(b).unwrap();
    assert_eq!(
        (lb[0].global, lb[0].visible),
        (Some(g), true),
        "below the camera"
    );
    // A copy sent with other fields keeps the definition's.
    let mut lb2 = lb.clone();
    lb2[0].opacity = 1.0;
    sw.scene_items(b, lb2).unwrap();
    assert_eq!(sw.scene(b).unwrap()[0].opacity, 0.5);
    // On top of every scene there is, when asked.
    let g2 = sw
        .set_global_layer(
            Item {
                name: "Bug".into(),
                ..layer(logo)
            },
            true,
        )
        .unwrap();
    for s in [before, a, b] {
        assert_eq!(sw.scene(s).unwrap().last().unwrap().global, Some(g2));
    }
    assert_eq!(sw.state()["global_layers"].as_array().unwrap().len(), 2);
    // A scene isn't a global layer's source; a missing one is refused in a scene.
    let nested = sw.set_global_layer(layer(a), false);
    assert!(nested.is_err());
    // Removed: from every scene.
    sw.remove_global_layer(g).unwrap();
    assert!(
        sw.scene(a)
            .unwrap()
            .iter()
            .chain(sw.scene(b).unwrap().iter())
            .all(|l| l.global != Some(g))
    );
    let mut bad = sw.scene(a).unwrap();
    bad.push(Item {
        global: Some(g),
        ..Item::default()
    });
    assert!(sw.scene_items(a, bad).is_err(), "no such global layer");
}

/// A global logo shown in the program's scene, and fade to black still takes all of it.
#[test]
fn a_global_layer_on_program_and_fade_to_black() {
    let sw = start("GlobalFtb", Duration::from_secs(10));
    let cam = sw
        .add_input("Cam", Source::Colour(Ycc(80, 128, 128)))
        .unwrap();
    let logo = sw.add_input("Logo", Source::Colour(Ycc::WHITE)).unwrap();
    sw.set_global_layer(
        Item {
            rect: (0.0, 0.0, 0.5, 0.5),
            ..layer(logo)
        },
        false,
    )
    .unwrap();
    let sc = sw.add_input("Main", Source::Scene).unwrap();
    let mut l = sw.scene(sc).unwrap();
    l.insert(0, layer(cam));
    sw.scene_items(sc, l).unwrap();
    let (logo_px, _t1) = watch(&sw, W / 4, H / 4);
    let (cam_px, _t2) = watch(&sw, W * 3 / 4, H * 3 / 4);
    wait_for("the logo over the camera", Duration::from_secs(3), || {
        *logo_px.lock_ok() == 235 && *cam_px.lock_ok() == 80
    });
    sw.ftb().unwrap();
    wait_for("all black", Duration::from_secs(3), || {
        *logo_px.lock_ok() == 16 && *cam_px.lock_ok() == 16
    });
}

/// An IDN output of one colour, 30 pictures a second, until dropped.
struct Colour {
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Colour {
    fn start(publisher: &Publisher, name: &str, c: Ycc) -> Colour {
        let out = Arc::new(Output::new(publisher, OutputConfig::new(name)).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (out, stop) = (out.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
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

/// No fixed input limit (§S.3); a network input not in use doesn't receive, and connects
/// when its scene goes to preview; it disconnects once unused for `idle_after`; kept
/// active, it stays connected; the multiview's tiles count as in use while it is drawn.
#[test]
fn inputs_receive_only_while_in_use() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let sw = start("IdleTest", Duration::from_millis(800));
    // More than the 16 there used to be at most.
    for k in 0..20 {
        let c = sw
            .add_input(
                &format!("Colour {k}"),
                Source::Colour(Ycc(20 + k, 128, 128)),
            )
            .unwrap();
        let s = sw.add_input(&format!("Scene {k}"), Source::Scene).unwrap();
        sw.scene_items(s, vec![layer(c)]).unwrap();
    }
    assert_eq!(sw.state()["inputs"].as_array().unwrap().len(), 40);
    assert_eq!(sw.multiview_layout().len(), 42, "a tile each");
    let red = Colour::start(&publisher, "Red", Ycc::from_rgb(255, 0, 0));
    let blue = Colour::start(&publisher, "Blue", Ycc::from_rgb(0, 0, 255));
    let r = sw.add_input("Red", Source::Network(red.address())).unwrap();
    let b = sw
        .add_input("Blue", Source::Network(blue.address()))
        .unwrap();
    let rs = sw.add_input("Red scene", Source::Scene).unwrap();
    sw.scene_items(rs, vec![layer(r)]).unwrap();
    // Unused: not received.
    std::thread::sleep(Duration::from_millis(600));
    let i = input(&sw, r);
    assert_eq!(
        (&i["active"], &i["connected"]),
        (&Value::from(false), &Value::from(false)),
        "{i}"
    );
    // Its scene on preview: received.
    sw.preview(rs).unwrap();
    wait_for("Red received on preview", Duration::from_secs(5), || {
        let i = input(&sw, r);
        i["active"] == true && i["connected"] == true && i["width"] == W
    });
    assert_eq!(
        input(&sw, r)["preview"],
        true,
        "its tally through the scene"
    );
    // Off preview: disconnected after its idle time, its picture let go.
    let other = sw.state()["inputs"][1]["id"].as_u64().unwrap() as u32;
    sw.preview(other).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(input(&sw, r)["active"], true, "kept a moment");
    wait_for("Red let go", Duration::from_secs(5), || {
        let i = input(&sw, r);
        i["active"] == false && i["width"] == 0
    });
    // Kept active: received though unused.
    assert_eq!(input(&sw, b)["active"], false);
    sw.keep_active(b, true).unwrap();
    wait_for("Blue kept active", Duration::from_secs(5), || {
        input(&sw, b)["connected"] == true
    });
    assert_eq!(input(&sw, b)["keep_active"], true);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(input(&sw, b)["active"], true, "still");
    sw.keep_active(b, false).unwrap();
    wait_for("Blue let go", Duration::from_secs(5), || {
        input(&sw, b)["active"] == false
    });
    // On the multiview while it is drawn (sent here): received, at its tile's size.
    sw.set_multiview_layout(Some(vec![TileSpec {
        kind: TileKind::Input(r),
        rect: (0.0, 0.0, 0.25, 0.25),
        label: true,
        tally: true,
    }]))
    .unwrap();
    sw.multiview_output(true).unwrap();
    wait_for("Red on the multiview", Duration::from_secs(5), || {
        let i = input(&sw, r);
        i["connected"] == true && i["width"].as_u64().is_some_and(|w| w > 0 && w < W as u64)
    });
    sw.multiview_output(false).unwrap();
    wait_for("Red let go again", Duration::from_secs(5), || {
        input(&sw, r)["active"] == false
    });
}

/// The multiview's layout (§S.7): set, in the state, drawn — a scene's tile drawn at the
/// tile's size — and its output switched on and off at run time.
#[test]
fn an_adjustable_multiview() {
    let sw = start("MultiviewTest", Duration::from_secs(10));
    let grey = sw
        .add_input("Grey", Source::Colour(Ycc(100, 128, 128)))
        .unwrap();
    let white = sw.add_input("White", Source::Colour(Ycc::WHITE)).unwrap();
    let sc = sw.add_input("Half", Source::Scene).unwrap();
    // Grey over all of it, white on its left half.
    sw.scene_items(
        sc,
        vec![
            layer(grey),
            Item {
                rect: (0.0, 0.0, 0.5, 1.0),
                ..layer(white)
            },
        ],
    )
    .unwrap();
    // The default: preview and program across the top, then each input and scene.
    let d = sw.state()["multiview"].clone();
    assert_eq!(d["custom"], false);
    assert_eq!(d["tiles"].as_array().unwrap().len(), 5, "{d}");
    assert_eq!(d["tiles"][4]["kind"], "scene");
    assert_eq!(d["output"], false);
    let tiles = vec![
        TileSpec {
            kind: TileKind::Program,
            rect: (0.0, 0.0, 0.5, 0.5),
            label: true,
            tally: true,
        },
        TileSpec {
            kind: TileKind::Scene(sc),
            rect: (0.5, 0.5, 0.5, 0.5),
            label: false,
            tally: false,
        },
    ];
    assert!(
        sw.set_multiview_layout(Some(vec![TileSpec {
            kind: TileKind::Scene(grey),
            ..tiles[1]
        }]))
        .is_err(),
        "not a scene"
    );
    sw.set_multiview_layout(Some(tiles.clone())).unwrap();
    assert_eq!(sw.multiview_layout(), tiles);
    let st = sw.state()["multiview"].clone();
    assert_eq!(st["custom"], true);
    assert_eq!(st["tiles"][1]["kind"], "scene");
    assert_eq!(st["tiles"][1]["label"], false);
    // Its output at run time.
    sw.multiview_output(true).unwrap();
    let mv = sw
        .outputs()
        .into_iter()
        .find(|o| o.0 == "multiview")
        .expect("the multiview's output");
    assert_eq!(sw.state()["multiview"]["output"], true);
    let rx = Input::open(InputConfig::new(&format!("127.0.0.1:{}", mv.2))).unwrap();
    // The scene's tile: white on its left, grey on its right, drawn at the tile's size.
    let ty = 1080 * 3 / 4;
    let (wx, gx) = (1920 / 2 + 1920 / 8, 1920 - 1920 / 8);
    let near = |v: u8, want: u8| (v as i32 - want as i32).abs() <= 3;
    let end = Instant::now() + Duration::from_secs(5);
    let mut got = (0u8, 0u8);
    while Instant::now() < end && !(near(got.0, 235) && near(got.1, 100)) {
        if let Some(InputEvent::Video(v)) = rx.recv(Duration::from_millis(100)) {
            let y = &v.picture.planes[0];
            if y.width == 1920 {
                got = (y.data[ty * 1920 + wx], y.data[ty * 1920 + gx]);
            }
        }
    }
    assert!(
        near(got.0, 235) && near(got.1, 100),
        "the scene's tile: {got:?}"
    );
    sw.multiview_output(false).unwrap();
    assert!(!sw.outputs().iter().any(|o| o.0 == "multiview"));
    assert_eq!(sw.state()["multiview"]["output"], false);
    // The API's picture works either way.
    wait_for("the multiview's JPEG", Duration::from_secs(3), || {
        sw.multiview_jpeg().is_some()
    });
    // Back to the default.
    sw.set_multiview_layout(None).unwrap();
    assert_eq!(sw.state()["multiview"]["custom"], false);
}
