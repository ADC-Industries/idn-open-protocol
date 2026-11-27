//! The program: cut, auto, layers and fade to black; feed and delay inputs; triggers;
//! scenes on program; stingers and luma wipes.

use super::*;

#[test]
fn preview_cut_auto_layer_and_ftb() {
    run(true);
}

/// The same on the CPU compositor (the GPU's fallback).
#[test]
fn the_same_on_the_cpu() {
    run(false);
}

fn run(gpu: bool) {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let (red, blue) = (Ycc::from_rgb(255, 0, 0), Ycc::from_rgb(0, 0, 255));
    let a = Colour::start(&publisher, "Red", red, false);
    let b = Colour::start(&publisher, "Blue", blue, false);
    let lower = Colour::start(&publisher, "Lower third", Ycc::WHITE, true);
    let sw = Switcher::start(SwitcherConfig {
        width: W,
        height: H,
        mdns: Some(false),
        gpu,
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    eprintln!("compositor: {}", sw.state()["compositor"]);
    if !gpu {
        assert_eq!(sw.state()["compositor"], "CPU");
    }
    let ia = sw.add_input("Red", Source::Network(a.address())).unwrap();
    let ib = sw.add_input("Blue", Source::Network(b.address())).unwrap();
    let il = sw
        .add_input("Lower third", Source::Network(lower.address()))
        .unwrap();
    // Inputs fill no bus; the first two scenes fill program and preview.
    assert!(sw.state()["program"].is_null());
    let (sa, sb) = (scene_of(&sw, ia), scene_of(&sw, ib));
    let st = sw.state();
    assert_eq!(
        (st["program"].as_u64(), st["preview"].as_u64()),
        (Some(sa as u64), Some(sb as u64))
    );
    // Only scenes go on the buses (§S.3).
    assert!(sw.program(ia).unwrap_err().contains("scene"));
    assert!(sw.preview(il).is_err());
    let watch = Watch::start(&format!("127.0.0.1:{}", sw.program_output().port()));
    watch.until(10, "red on program", |t, b| {
        near(t, red.0) && near(b, red.0)
    });

    // Cut: blue at once; the old program goes to preview.
    sw.cut().unwrap();
    watch.until(3, "blue after the cut", |t, _| near(t, blue.0));
    assert_eq!(sw.state()["preview"].as_u64(), Some(sa as u64));

    // Auto with a 1 s mix: through the middle, then red.
    sw.transition(Kind::Mix, 1.0).unwrap();
    sw.auto().unwrap();
    let mid = (red.0 as u16 + blue.0 as u16) / 2;
    watch.until(3, "the mix's middle", |t, _| {
        (t as i32 - mid as i32).abs() <= 8
    });
    wait_for("the mix to finish", Duration::from_secs(2), || {
        !sw.state()["transition"]["running"].as_bool().unwrap()
    });
    watch.until(3, "red after the mix", |t, _| near(t, red.0));
    assert_eq!(sw.state()["program"].as_u64(), Some(sa as u64));

    // The lower third as a global layer (§S.5), in every scene, hidden: shown in the scene
    // on program, keyed over it — white below, red above.
    let g = sw
        .set_global_layer(
            Item {
                name: "Lower third".into(),
                source: ItemSource::Input(il),
                visible: false,
                fade: 0.0,
                ..Default::default()
            },
            true,
        )
        .unwrap();
    let layer = sw
        .scene(sa)
        .unwrap()
        .iter()
        .find(|l| l.global == Some(g))
        .expect("the copy in the scene")
        .id;
    sw.scene_layer_visible(sa, layer, true).unwrap();
    watch.until(5, "the lower third", |t, b| near(t, red.0) && near(b, 235));
    sw.scene_layer_visible(sa, layer, false).unwrap();
    watch.until(3, "the lower third off", |_, b| near(b, red.0));

    // Fade to black and back (1 s each).
    sw.ftb().unwrap();
    watch.until(3, "black", |t, b| near(t, 16) && near(b, 16));
    sw.ftb().unwrap();
    watch.until(3, "back from black", |t, _| near(t, red.0));

    // Straight to program: the preview input taken there swaps with the program.
    let (pg, pv) = (
        sw.state()["program"].as_u64(),
        sw.state()["preview"].as_u64(),
    );
    sw.program(pv.unwrap() as u32).unwrap();
    assert_eq!(
        (
            sw.state()["program"].as_u64(),
            sw.state()["preview"].as_u64()
        ),
        (pv, pg)
    );
    sw.program(pg.unwrap() as u32).unwrap();
    // The state's version moves with every change; waiting for a change returns at once
    // when there is one.
    let v = sw.version();
    sw.preview(sb).unwrap();
    assert!(sw.wait_change(v, Duration::from_secs(1)) > v);
    // Removing the program scene's input leaves black.
    sw.remove_input(ia).unwrap();
    watch.until(3, "black without a program", |t, _| near(t, 16));
}

/// A live-feed input shows pictures the host pushes in-process, with no network source.
#[test]
fn live_feed_input_takes_host_pictures() {
    let sw = Switcher::start(SwitcherConfig {
        name: "FeedTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let find = |st: &serde_json::Value, id: u32| {
        st["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"].as_u64() == Some(id as u64))
            .unwrap()
            .clone()
    };

    let key = sw.add_input("Key", Source::Feed).unwrap();
    // Blank and unconnected until fed; its source shows as a feed.
    let before = find(&sw.state(), key);
    assert_eq!(before["connected"].as_bool(), Some(false));
    assert_eq!(before["source"], serde_json::json!({ "feed": true }));

    // Feeding a non-feed input is refused.
    let grey = sw
        .add_input("Grey", Source::Colour(Ycc::from_rgb(80, 80, 80)))
        .unwrap();
    assert!(
        sw.feed(grey, Arc::new(Frame::solid(W, H, Ycc::WHITE)))
            .is_err()
    );

    // A pushed picture connects the input and shows at the fed size.
    sw.feed(key, Arc::new(Frame::solid(W, H, Ycc::WHITE)))
        .unwrap();
    let after = find(&sw.state(), key);
    assert_eq!(after["connected"].as_bool(), Some(true));
    assert_eq!(after["width"].as_u64(), Some(W as u64));

    // It goes on program in a scene like any input; its tally says so.
    assert!(sw.program(key).is_err(), "not a scene");
    sw.program(scene_of(&sw, key)).unwrap();
    assert!(find(&sw.state(), key)["program"].as_bool().unwrap());
}

/// A delay input shows another input's pictures late.
#[test]
fn delay_input_shows_the_source_late() {
    let sw = Switcher::start(SwitcherConfig {
        name: "DelayTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let find = |st: &serde_json::Value, id: u32| {
        st["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"].as_u64() == Some(id as u64))
            .unwrap()
            .clone()
    };

    // A delay of an input that doesn't exist is refused.
    assert!(
        sw.add_input(
            "Late",
            Source::Delay {
                input: 999,
                by_ms: 100
            }
        )
        .is_err()
    );

    let live = sw.add_input("Live", Source::Feed).unwrap();
    let late = sw
        .add_input(
            "Late",
            Source::Delay {
                input: live,
                by_ms: 100,
            },
        )
        .unwrap();
    // Nothing shown on the delay input yet.
    assert_eq!(find(&sw.state(), late)["connected"].as_bool(), Some(false));

    // Feed the source; within the delay it connects and shows a picture at the fed size.
    wait_until(Duration::from_secs(3), || {
        sw.feed(live, Arc::new(Frame::solid(W, H, Ycc::WHITE)))
            .unwrap();
        find(&sw.state(), late)["connected"].as_bool() == Some(true)
    });
    let st = find(&sw.state(), late);
    assert_eq!(st["connected"].as_bool(), Some(true));
    assert_eq!(
        st["source"],
        serde_json::json!({ "delay": { "input": live, "ms": 100 } })
    );
    assert_eq!(st["width"].as_u64(), Some(W as u64));
}

/// A trigger runs its action when its event fires (an input going on air).
#[test]
fn triggers_fire_on_air() {
    let sw = Switcher::start(SwitcherConfig {
        name: "TrigTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let a = sw
        .add_input("A", Source::Colour(Ycc::from_rgb(200, 0, 0)))
        .unwrap();
    let _a = scene_of(&sw, a); // program
    let b = sw
        .add_input("B", Source::Colour(Ycc::from_rgb(0, 200, 0)))
        .unwrap();
    let b = scene_of(&sw, b); // preview
    sw.set_triggers(vec![Trigger {
        on: TriggerEvent::OnAir(b),
        action: TriggerAction::Ftb { on: true },
    }]);
    assert_eq!(sw.state()["ftb"]["on"].as_bool(), Some(false));
    assert_eq!(sw.state()["triggers"].as_array().unwrap().len(), 1);

    // Put B on program: OnAir(b) fires the trigger, which turns fade-to-black on.
    sw.program(b).unwrap();
    wait_until(Duration::from_secs(2), || {
        sw.state()["ftb"]["on"].as_bool() == Some(true)
    });
    assert_eq!(
        sw.state()["ftb"]["on"].as_bool(),
        Some(true),
        "the on-air trigger should have turned FTB on"
    );
}

/// A scene on program (§S.12): its items in the composed program; a scene inside a scene;
/// a loop refused; the items in the state.
#[test]
fn a_scene_on_program() {
    scene_on_program(false);
}

/// The same through the GPU compositor (the CPU's without a GPU).
#[test]
fn a_scene_on_program_on_the_gpu() {
    scene_on_program(true);
}

fn scene_on_program(gpu: bool) {
    use idn_switcher::scene::{Item, ItemSource};
    let sw = Switcher::start(SwitcherConfig {
        name: "SceneTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu,
        ..Default::default()
    })
    .unwrap();
    let white = sw.add_input("White", Source::Colour(Ycc::WHITE)).unwrap();
    let grey = sw
        .add_input("Grey", Source::Colour(Ycc(100, 128, 128)))
        .unwrap();
    let s = sw.add_input("Scene", Source::Scene).unwrap();
    let place = |src, rect| Item {
        source: ItemSource::Input(src),
        rect,
        ..Default::default()
    };
    // Grey behind, white in the top-left quarter.
    sw.scene_items(
        s,
        vec![
            place(grey, (0.0, 0.0, 1.0, 1.0)),
            place(white, (0.0, 0.0, 0.5, 0.5)),
        ],
    )
    .unwrap();
    assert_eq!(
        sw.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == s)
            .unwrap()["source"]["scene"]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Program: the scene; read the composed program's luma.
    let seen = Arc::new(Mutex::new((0u8, 0u8)));
    let g = seen.clone();
    let _tap = sw.program_output().tap(move |f| {
        if let Some(y) = f.planes.first() {
            *g.lock_ok() = (
                y.data[(H / 8) * W + W / 8],
                y.data[(H * 7 / 8) * W + W * 7 / 8],
            );
        }
    });
    sw.program(s).unwrap();
    wait_until(Duration::from_secs(3), || *seen.lock_ok() == (235, 100));
    assert_eq!(
        *seen.lock_ok(),
        (235, 100),
        "white top left, grey bottom right"
    );
    // A scene inside a scene, half size top left: the inner scene's white is now a sixteenth.
    let outer = sw.add_input("Outer", Source::Scene).unwrap();
    sw.scene_items(outer, vec![place(s, (0.0, 0.0, 0.5, 0.5))])
        .unwrap();
    sw.program(outer).unwrap();
    wait_until(Duration::from_secs(3), || seen.lock_ok().0 == 235);
    assert_eq!(
        seen.lock_ok().0,
        235,
        "the inner scene's white, at (W/8, H/8)"
    );
    // The scene can't take in the one that holds it.
    assert!(
        sw.scene_items(s, vec![place(outer, (0.0, 0.0, 1.0, 1.0))])
            .is_err()
    );
    assert!(sw.scene_items(white, vec![]).is_err(), "not a scene");
    assert!(
        sw.scene_items(s, vec![place(999, (0.0, 0.0, 1.0, 1.0))])
            .is_err()
    );
}

/// A stinger (§S.14): its clip over the bus while it runs, program and preview swapped at
/// its cut point (still running), not swapped again at the end; the hook told to start the
/// clip. And a luma wipe by a pattern, held half way on the T-bar.
#[test]
fn stingers_and_luma_wipes() {
    use idn_switcher::Stinger;
    let sw = Switcher::start(SwitcherConfig {
        name: "StingerTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let a = sw
        .add_input("A", Source::Colour(Ycc(60, 128, 128)))
        .unwrap();
    let a = scene_of(&sw, a); // program
    let b = sw
        .add_input("B", Source::Colour(Ycc(120, 128, 128)))
        .unwrap();
    let b = scene_of(&sw, b); // preview
    let clip = sw
        .add_input("Clip", Source::Colour(Ycc(200, 128, 128)))
        .unwrap();
    let luma = Arc::new(Mutex::new((0u8, 0u8)));
    let g = luma.clone();
    let _tap = sw.program_output().tap(move |f| {
        if let Some(y) = f.planes.first() {
            *g.lock_ok() = (y.data[(H / 2) * W + W / 8], y.data[(H / 2) * W + W * 7 / 8]);
        }
    });
    let started = Arc::new(Mutex::new(None));
    let s2 = started.clone();
    sw.on_stinger(move |i| *s2.lock_ok() = Some(i));
    assert!(sw.stinger(9, None).is_err());
    assert!(
        sw.stinger(
            1,
            Some(Stinger {
                input: clip,
                duration: 1.0,
                cut: 2.0
            })
        )
        .is_err(),
        "cut past the end"
    );
    sw.stinger(
        1,
        Some(Stinger {
            input: clip,
            duration: 1.0,
            cut: 0.5,
        }),
    )
    .unwrap();
    sw.transition(Kind::Stinger(1), 1.0).unwrap();
    sw.auto().unwrap();
    assert_eq!(*started.lock_ok(), Some(clip), "the hook started the clip");
    // While it runs: the clip over everything (opaque here).
    let wait = |want: (u8, u8), what: &str| {
        wait_until(Duration::from_secs(3), || *luma.lock_ok() == want);
        assert_eq!(*luma.lock_ok(), want, "{what}");
    };
    wait((200, 200), "the clip over the bus");
    // Past the cut point, still running: B is program beneath the clip.
    wait_until(Duration::from_secs(3), || {
        sw.state()["program"].as_u64() == Some(b as u64)
    });
    let st = sw.state();
    assert_eq!(st["program"].as_u64(), Some(b as u64), "swapped at the cut");
    assert_eq!(st["transition"]["running"], true, "the clip still plays");
    // The end: B stays program (no second swap), A on preview, the clip gone.
    wait((120, 120), "B after the stinger");
    let st = sw.state();
    assert_eq!(
        (st["program"].as_u64(), st["preview"].as_u64()),
        (Some(b as u64), Some(a as u64))
    );
    assert_eq!(st["transition"]["running"], false);

    // A luma wipe by a left-to-right ramp: half way, the new picture (A) on the left.
    let ramp = sw.add_input("Ramp", Source::Feed).unwrap();
    let mut r = Frame::solid(W, H, Ycc::BLACK);
    for row in 0..H {
        for x in 0..W {
            r.y[row * W + x] = (16 + x * 219 / (W - 1)) as u8;
        }
    }
    sw.feed(ramp, Arc::new(r)).unwrap();
    assert!(
        sw.transition(
            Kind::Luma {
                pattern: 999,
                soft: 0.0,
                invert: false
            },
            1.0
        )
        .is_err()
    );
    sw.transition(
        Kind::Luma {
            pattern: ramp,
            soft: 0.0,
            invert: false,
        },
        1.0,
    )
    .unwrap();
    assert_eq!(
        sw.state()["transition"]["luma"]["pattern"].as_u64(),
        Some(ramp as u64)
    );
    sw.tbar(0.5).unwrap();
    wait((60, 120), "left: A coming in; right: still B");
}

/// A stinger's swap at its cut point doesn't carry over to the next transition when an
/// input was taken straight to program meanwhile (the next AUTO played out and snapped
/// back).
#[test]
fn a_take_after_a_stinger_leaves_the_next_transition_whole() {
    use idn_switcher::Stinger;
    let sw = Switcher::start(SwitcherConfig {
        name: "StingerTake".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let a = sw
        .add_input("A", Source::Colour(Ycc(60, 128, 128)))
        .unwrap();
    let a = scene_of(&sw, a);
    let b = sw
        .add_input("B", Source::Colour(Ycc(120, 128, 128)))
        .unwrap();
    let b = scene_of(&sw, b);
    let clip = sw
        .add_input("Clip", Source::Colour(Ycc(200, 128, 128)))
        .unwrap();
    sw.stinger(
        1,
        Some(Stinger {
            input: clip,
            duration: 2.0,
            cut: 0.3,
        }),
    )
    .unwrap();
    sw.transition(Kind::Stinger(1), 2.0).unwrap();
    sw.auto().unwrap();
    let program = |sw: &Switcher| sw.state()["program"].as_u64();
    wait_until(Duration::from_secs(3), || program(&sw) == Some(b as u64));
    assert_eq!(program(&sw), Some(b as u64), "swapped at the cut");
    // Taken straight to program while the clip still plays.
    sw.program(a).unwrap();
    sw.transition(Kind::Mix, 0.2).unwrap();
    sw.auto().unwrap();
    wait_until(Duration::from_secs(3), || {
        sw.state()["transition"]["running"] != true
    });
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        program(&sw),
        Some(b as u64),
        "the mix took B to program and stayed"
    );
}
