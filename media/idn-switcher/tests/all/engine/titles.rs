//! Titles, slideshows and filters: titles on program, their fields, timers and data
//! rows; slideshows moving on; filters keying an input.

use super::*;

/// A title on program (§S.16): its box and text in the composed program, transparent
/// elsewhere (the black under it shows); the layers in the state.
#[test]
fn a_title_on_program() {
    use idn_switcher::title::{LayerKind, Rgba, Text, Title};
    let sw = Switcher::start(SwitcherConfig {
        name: "TitleTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let t = sw.add_input("Name", Source::Title).unwrap();
    // A white box over the bottom quarter, text in it.
    let title = Title::text(
        Text {
            text: "Hello".into(),
            colour: Rgba::BLACK,
            ..Default::default()
        },
        (0.0, 0.75, 1.0, 0.25),
        Some(Rgba::WHITE),
    );
    assert!(matches!(title.layers[0].kind, LayerKind::Box { .. }));
    sw.set_title(t, title).unwrap();
    let state = sw.state();
    let me = state["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == t)
        .unwrap()
        .clone();
    assert_eq!(me["source"]["title"]["layers"].as_array().unwrap().len(), 2);
    let seen = Arc::new(Mutex::new((0u8, 0u8)));
    let g = seen.clone();
    let _tap = sw.program_output().tap(move |f| {
        if let Some(y) = f.planes.first() {
            *g.lock_ok() = (y.data[(H / 4) * W + W / 2], y.data[(H - 4) * W + W - 4]);
        }
    });
    sw.program(scene_of(&sw, t)).unwrap();
    wait_until(Duration::from_secs(3), || *seen.lock_ok() == (16, 235));
    assert_eq!(
        *seen.lock_ok(),
        (16, 235),
        "black above, the white box below"
    );
    // Not a title: refused.
    let c = sw.add_input("Black", Source::Colour(Ycc::BLACK)).unwrap();
    assert!(sw.set_title(c, Title::default()).is_err());
    assert!(sw.font_families().iter().any(|f| f == "DejaVu Sans"));
}

/// A slideshow (§S.16): pictures from the loader, moved on by hand and by itself, the end
/// of a slideshow that doesn't loop firing its trigger.
#[test]
fn a_slideshow_moves_on() {
    use idn_switcher::{SlideAction, Slideshow};
    let sw = Switcher::start(SwitcherConfig {
        name: "SlideTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    // "Pictures" whose name is their luma; a smaller one, fitted.
    sw.set_picture_loader(Arc::new(|p: &str| {
        let y: u8 = p.parse().map_err(|_| "not a picture".to_string())?;
        Ok(Frame::solid(W / 2, H / 2, Ycc(y, 128, 128)))
    }));
    let show = sw
        .add_input(
            "Slides",
            Source::Slideshow(Slideshow {
                pictures: vec!["50".into(), "100".into(), "150".into()],
                every: 0.4,
                fade: 0.0,
                looped: false,
                shuffle: false,
            }),
        )
        .unwrap();
    let cam = sw
        .add_input("Cam", Source::Colour(Ycc(200, 128, 128)))
        .unwrap();
    // At the slideshow's end, the camera goes on air (in its scene).
    let cam_scene = scene_of(&sw, cam);
    sw.set_triggers(vec![Trigger {
        on: TriggerEvent::SlideshowEnd(show),
        action: TriggerAction::Program(cam_scene),
    }]);
    let seen = Arc::new(Mutex::new(0u8));
    let g = seen.clone();
    let _tap = sw.program_output().tap(move |f| {
        if let Some(y) = f.planes.first() {
            *g.lock_ok() = y.data[(H / 2) * W + W / 2];
        }
    });
    sw.program(scene_of(&sw, show)).unwrap();
    let wait = |want: u8, what: &str| {
        wait_until(Duration::from_secs(4), || *seen.lock_ok() == want);
        assert_eq!(*seen.lock_ok(), want, "{what}");
    };
    sw.slideshow(show, SlideAction::Pause).unwrap();
    wait(50, "the first picture");
    sw.slideshow(show, SlideAction::Next).unwrap();
    wait(100, "the next, by hand");
    sw.slideshow(show, SlideAction::Goto(0)).unwrap();
    wait(50, "back to the first");
    assert!(sw.slideshow(show, SlideAction::Goto(9)).is_err());
    // Playing: through the pictures to the end, where the trigger cuts to the camera.
    sw.slideshow(show, SlideAction::Play).unwrap();
    wait(100, "moved on by itself");
    wait(150, "the last");
    wait(200, "the camera, on the slideshow's end");
    let state = sw.state();
    let me = state["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == show)
        .unwrap()
        .clone();
    assert_eq!(me["source"]["slideshow"]["now"]["ended"], true);
    assert_eq!(me["source"]["slideshow"]["now"]["picture"], 2);
    // A slideshow with no pictures is refused.
    assert!(
        sw.add_input(
            "None",
            Source::Slideshow(Slideshow {
                pictures: vec![],
                every: 1.0,
                fade: 0.0,
                looped: true,
                shuffle: false
            })
        )
        .is_err()
    );
}

/// Filters on an input (§S.17): a green-screen feed keyed over a background on program,
/// the filters in the state; a still corrected at once when its filters change.
#[test]
fn filters_key_an_input_wherever_it_shows() {
    use idn_switcher::filter::items_from_json;
    let sw = Switcher::start(SwitcherConfig {
        name: "FilterTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let bg = sw
        .add_input("Back", Source::Colour(Ycc(60, 128, 128)))
        .unwrap();
    let cam = sw.add_input("Cam", Source::Feed).unwrap();
    // The camera: green, a grey person in the middle.
    let green = Ycc::from_rgb(0, 200, 0);
    let mut f = Frame::solid(W, H, green);
    for y in H / 4..H * 3 / 4 {
        for x in W / 4..W * 3 / 4 {
            f.y[y * W + x] = 180;
        }
        for c in W / 8..W * 3 / 8 {
            f.cb[y * W / 2 + c] = 128;
            f.cr[y * W / 2 + c] = 128;
        }
    }
    sw.set_filters(
        cam,
        items_from_json(&serde_json::json!([{"type": "chroma_key"}])).unwrap(),
    )
    .unwrap();
    sw.feed(cam, Arc::new(f)).unwrap();
    // The keyed camera over the background, in one scene.
    let sc = sw.add_input("Keyed", Source::Scene).unwrap();
    let full = |id| Item {
        source: ItemSource::Input(id),
        ..Default::default()
    };
    sw.scene_items(sc, vec![full(bg), full(cam)]).unwrap();
    sw.program(sc).unwrap();
    let seen = Arc::new(Mutex::new((0u8, 0u8)));
    let g = seen.clone();
    let _tap = sw.program_output().tap(move |f| {
        if let Some(y) = f.planes.first() {
            *g.lock_ok() = (y.data[4 * W + 4], y.data[(H / 2) * W + W / 2]);
        }
    });
    let wait = |want: (u8, u8), what: &str| {
        wait_until(Duration::from_secs(4), || *seen.lock_ok() == want);
        assert_eq!(*seen.lock_ok(), want, "{what}");
    };
    wait(
        (60, 180),
        "the background where the green was, the person over it",
    );
    let state = sw.state();
    let me = state["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == cam)
        .unwrap()
        .clone();
    assert_eq!(me["filters"][0]["type"], "chroma_key");
    // A still's correction shows at once (no next picture comes).
    sw.set_filters(
        bg,
        items_from_json(&serde_json::json!([{"type": "correct", "brightness": 0.2}])).unwrap(),
    )
    .unwrap();
    let want = (16.0 + 219.0 * ((60.0 - 16.0) / 219.0 + 0.2f32)).round() as u8;
    wait((want, 180), "the background brighter");
    // A filter that can't be built leaves them as they were.
    assert!(
        sw.set_filters(
            bg,
            items_from_json(&serde_json::json!([{"type": "lut", "file": "/nope.cube"}])).unwrap()
        )
        .is_err()
    );
    // None: the camera's green is back.
    sw.set_filters(cam, Vec::new()).unwrap();
    sw.feed(cam, Arc::new(Frame::solid(W, H, green))).unwrap();
    wait((green.0, green.0), "unkeyed");
}

/// Titles' fields and timers (§S.19): a score added to, a countdown shown in a title and
/// drawn again as it counts, its end firing a trigger.
#[test]
fn title_fields_and_timers() {
    use idn_switcher::title::Title;
    use idn_switcher::{TimerAction, TimerMode};
    let sw = Switcher::start(SwitcherConfig {
        name: "TimerTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let t = sw.add_input("Board", Source::Title).unwrap();
    let cam = sw
        .add_input("Cam", Source::Colour(Ycc(200, 128, 128)))
        .unwrap();
    let cam = scene_of(&sw, cam);
    sw.set_title(
        t,
        Title::from_json(&serde_json::json!({
            "layers": [{"type": "text", "text": "{home}–{away} · {timer:start}", "rect": [0, 0.8, 1, 0.2]}],
            "fields": {"home": 0, "away": 0},
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sw.add_title_field(t, "home", 1.0).unwrap(), "1");
    assert_eq!(sw.add_title_field(t, "home", 2.0).unwrap(), "3");
    let field = |s: &serde_json::Value, k: &str| {
        s["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == t)
            .unwrap()["source"]["title"]["fields"][k]
            .clone()
    };
    assert_eq!(field(&sw.state(), "home"), "3");
    // A 2 s countdown; at its end the camera goes on air.
    sw.set_triggers(vec![Trigger {
        on: TriggerEvent::TimerEnd("start".into()),
        action: TriggerAction::Program(cam),
    }]);
    sw.timer(
        "start",
        TimerAction::Set(TimerMode::Down(Duration::from_secs(2))),
    )
    .unwrap();
    sw.program(scene_of(&sw, t)).unwrap();
    let pictures = |sw: &Switcher| {
        sw.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == t)
            .unwrap()["width"]
            .as_u64()
    };
    assert_eq!(pictures(&sw), Some(W as u64));
    let timer = |sw: &Switcher| sw.state()["timers"][0].clone();
    assert_eq!(timer(&sw)["value"], "0:02");
    sw.timer("start", TimerAction::Start).unwrap();
    let mut seen_one = false;
    wait_until(Duration::from_secs(5), || {
        seen_one |= timer(&sw)["value"] == "0:01";
        sw.state()["program"] == cam
    });
    assert!(seen_one, "counted through 0:01");
    assert_eq!(sw.state()["program"], cam, "the camera on air at the end");
    assert_eq!(timer(&sw)["ended"], true);
    assert!(sw.timer("bad:name", TimerAction::Start).is_err());
    sw.timer("start", TimerAction::Remove).unwrap();
    assert!(sw.state()["timers"].as_array().unwrap().is_empty());
}

/// A title's data (§S.20): a CSV file's rows fill `{Column}`, moved on by hand and by
/// itself; the file read again when it changes.
#[test]
fn a_title_shows_its_data_a_row_at_a_time() {
    use idn_switcher::SlideAction;
    use idn_switcher::title::Title;
    let dir = idn_common::testing::TempDir::new("idn-data");
    let csv = dir.join("people.csv");
    std::fs::write(&csv, "Name,Role\nSam,Pastor\nAna,Worship\nLee,Youth\n").unwrap();
    let sw = Switcher::start(SwitcherConfig {
        name: "DataTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let t = sw.add_input("People", Source::Title).unwrap();
    sw.set_title(
        t,
        Title::from_json(&serde_json::json!({
            "layers": [{"type": "text", "text": "{Name} – {Role} ({row}/{rows})"}],
            "data": {"from": csv, "every": 5},
        }))
        .unwrap(),
    )
    .unwrap();
    let data = |sw: &Switcher| {
        sw.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == t)
            .unwrap()["data"]
            .clone()
    };
    let wait = |what: &str, f: &dyn Fn(&serde_json::Value) -> bool| {
        wait_until(Duration::from_secs(8), || f(&data(&sw)));
        assert!(f(&data(&sw)), "{what}: {}", data(&sw));
    };
    wait("read", &|d| d["rows"] == 3 && d["row"] == 0);
    assert_eq!(data(&sw)["columns"], serde_json::json!(["Name", "Role"]));
    sw.title_row(t, SlideAction::Next).unwrap();
    assert_eq!(data(&sw)["row"], 1);
    sw.title_row(t, SlideAction::Previous).unwrap();
    sw.title_row(t, SlideAction::Previous).unwrap();
    assert_eq!(data(&sw)["row"], 2, "round to the last");
    assert!(sw.title_row(t, SlideAction::Goto(5)).is_err());
    // The file changes: read again (now, rather than in 5 s).
    std::fs::write(&csv, "Name,Role\nSam,Pastor\n").unwrap();
    sw.title_data_read(t).unwrap();
    wait("read again", &|d| d["rows"] == 1 && d["row"] == 0);
    // Moving on by itself every 0.3 s.
    std::fs::write(&csv, "Name,Role\nA,1\nB,2\n").unwrap();
    sw.set_title(
        t,
        Title::from_json(&serde_json::json!({
            "layers": [{"type": "text", "text": "{Name}"}],
            "data": {"from": csv, "every": 5, "advance": 0.3},
        }))
        .unwrap(),
    )
    .unwrap();
    wait("advanced by itself", &|d| d["rows"] == 2 && d["row"] == 1);
    // A source that can't be read says why.
    sw.set_title(
        t,
        Title::from_json(&serde_json::json!({
            "layers": [{"type": "text", "text": "{Name}"}],
            "data": {"from": dir.join("missing.csv")},
        }))
        .unwrap(),
    )
    .unwrap();
    wait("error", &|d| d["error"].is_string());
}
