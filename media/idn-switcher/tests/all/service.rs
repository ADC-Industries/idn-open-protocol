//! The switcher as a service embedded in a program (IDN Media as a server and libraries):
//! started in its own folder, driven by commands, its state and HTTP API answering, its
//! setup kept in its folder.

use idn_open_protocol::web::Request;
use idn_switcher::SwitcherConfig;
use idn_switcher::service::{Service, ServiceConfig};
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn a_switcher_service_embedded_in_this_process() {
    let dir = idn_common::testing::TempDir::new("idn-switcher-service");
    let svc = Service::start(ServiceConfig {
        switcher: SwitcherConfig {
            name: "ServiceTest".into(),
            width: 320,
            height: 180,
            fps: (25, 1),
            mdns: Some(false),
            gpu: false,
            ..Default::default()
        },
        dir: Some(dir.to_path_buf()),
        inputs: Vec::new(),
        node: None,
        rate: Some(48_000),
        machine_rate: None,
    })
    .expect("it starts");
    assert_eq!(svc.dir(), dir.path());
    // A command, as POST /api/v1/add_input.
    svc.command("add_input", &json!({"colour": "#204080", "label": "Blue"}))
        .expect("an input added");
    let st = svc.state();
    let inputs = st["inputs"].as_array().expect("inputs");
    assert!(inputs.iter().any(|i| i["label"] == "Blue"), "{inputs:?}");
    // The same through the HTTP API.
    let r = svc.handle(Request {
        method: "GET".into(),
        path: "/api/v1/state".into(),
        query: String::new(),
        headers: Default::default(),
        body: Vec::new(),
        peer: [127, 0, 0, 1].into(),
        tls: false,
    });
    assert_eq!(r.status, "200 OK");
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["version"], st["version"]);
    // Its setup kept in its folder.
    assert!(idn_common::testing::wait_until(
        Duration::from_secs(5),
        || { std::fs::read_to_string(dir.join("sources.json")).is_ok_and(|t| t.contains("Blue")) }
    ));
    // The push channel: the state at once, a command's reply, the new state, meters.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler: idn_open_protocol::web::Handler = {
        let svc = svc.clone();
        std::sync::Arc::new(move |r| svc.handle(r))
    };
    idn_open_protocol::web::serve(listener, None, handler, stop.clone()).unwrap();
    let (mut ws, _) =
        tungstenite::connect(format!("ws://127.0.0.1:{port}/api/v1/events")).expect("it upgrades");
    let mut next = |want: &str| -> Value {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(std::time::Instant::now() < end, "no {want} message");
            let m = ws.read().expect("a message");
            if let tungstenite::Message::Text(t) = m {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == want {
                    return v;
                }
            }
        }
    };
    let first = next("state");
    assert!(first["state"]["inputs"].is_array());
    drop(next);
    ws.send(tungstenite::Message::text(
        json!({"id": 7, "cmd": "add_input", "body": {"colour": "#00ff00", "label": "Green"}})
            .to_string(),
    ))
    .unwrap();
    let mut read = |want: &str| -> Value {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(std::time::Instant::now() < end, "no {want} message");
            if let tungstenite::Message::Text(t) = ws.read().expect("a message") {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == want {
                    return v;
                }
            }
        }
    };
    let reply = read("reply");
    assert_eq!(
        (reply["id"].as_u64(), reply["ok"].as_bool()),
        (Some(7), Some(true))
    );
    let st = read("state");
    assert!(
        st["state"]["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["label"] == "Green"),
        "{st}"
    );
    drop(read);
    ws.send(tungstenite::Message::text(
        json!({"subscribe": ["meters"]}).to_string(),
    ))
    .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(5);
    let mut got_meters = false;
    while std::time::Instant::now() < end && !got_meters {
        if let tungstenite::Message::Text(t) = ws.read().unwrap() {
            got_meters = t.as_str().contains("\"type\":\"meters\"");
        }
    }
    assert!(got_meters, "meters once subscribed");
    let _ = ws.close(None);
    tokens_and_the_audit(&svc);
    what_the_panel_did(&svc);
    setups_keep_the_program(&svc, &dir);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    svc.close(Duration::from_secs(1));
    drop(svc);
    // Started again in its folder: an input's sound offset kept by its label.
    let svc = Service::start(ServiceConfig {
        switcher: SwitcherConfig {
            name: "ServiceTest2".into(),
            width: 320,
            height: 180,
            fps: (25, 1),
            mdns: Some(false),
            gpu: false,
            ..Default::default()
        },
        dir: Some(dir.to_path_buf()),
        inputs: Vec::new(),
        node: None,
        rate: Some(48_000),
        machine_rate: None,
    })
    .expect("it starts again");
    let offset = |label: &str| {
        svc.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["label"] == label)
            .and_then(|i| i["audio_offset_ms"].as_u64())
    };
    assert!(
        idn_common::testing::wait_until(Duration::from_secs(5), || offset("Blue") == Some(200)),
        "{:?}",
        offset("Blue")
    );
    assert_eq!(offset("Green"), Some(0));
    svc.close(Duration::from_secs(1));
    drop(svc);
}

/// Inputs' sound offsets kept by label, and a setup with the program's transition, the
/// stingers, the global layers and the offsets (§S.28); a setup without them leaves them; a
/// damaged one is refused and set aside.
fn setups_keep_the_program(svc: &std::sync::Arc<Service>, dir: &std::path::Path) {
    let input = |label: &str| -> Value {
        svc.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("no input {label}"))
    };
    let id = |label: &str| input(label)["id"].as_u64().unwrap();
    let offset = |label: &str| input(label)["audio_offset_ms"].as_u64();
    let cmd = |c: &str, v: Value| {
        svc.command(c, &v).unwrap_or_else(|e| panic!("{c}: {e}"));
    };
    let offsets_file = || -> Value {
        std::fs::read(dir.join("audio-offsets.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null)
    };
    // Kept by label as it changes.
    cmd("audio_offset", json!({"id": id("Blue"), "ms": 120}));
    cmd("audio_offset", json!({"id": id("Green"), "ms": 80}));
    assert!(
        idn_common::testing::wait_until(Duration::from_secs(5), || {
            let f = offsets_file();
            f["Blue"] == 120 && f["Green"] == 80
        }),
        "{}",
        offsets_file()
    );
    // Removed and brought back by undo: with its offset, at once.
    cmd("remove_input", json!({"id": id("Green")}));
    cmd("undo", json!({}));
    assert_eq!(offset("Green"), Some(80));
    // The program as a setup keeps it.
    cmd(
        "transition",
        json!({"kind": "luma", "pattern": id("Green"), "soft": 0.1, "invert": true, "duration": 2.0}),
    );
    cmd(
        "stinger",
        json!({"n": 2, "input": id("Blue"), "duration": 2.0, "cut": 0.8}),
    );
    cmd(
        "global_layer",
        json!({"layer": {"source": id("Green"), "rect": [0.1, 0.1, 0.4, 0.4], "border": 6,
            "name": "Green bug", "fade": 0.0}}),
    );
    cmd("add_input", json!({"scene": true, "label": "Main"}));
    let before = svc.state();
    assert_eq!(
        input("Main")["source"]["scene"]["items"][0]["name"],
        "Green bug"
    );
    cmd("setup_save", json!({"name": "Live"}));
    let saved: Value =
        serde_json::from_slice(&std::fs::read(dir.join("setups/Live.json")).unwrap()).unwrap();
    assert_eq!(saved["transition"]["luma"]["pattern"], "Green", "{saved}");
    assert_eq!(saved["stingers"][1]["input"], "Blue");
    assert_eq!(
        saved["files"]["global-layers.json"][0]["source"]["input"],
        "Green"
    );
    assert!(saved["files"]["multiview.json"].is_object());
    assert_eq!(saved["files"]["audio-offsets.json"]["Blue"], 120);
    // Everything changed, then the setup loaded: as it was saved, in the state at once.
    cmd("transition", json!({"kind": "mix", "duration": 1.0}));
    cmd("stinger", json!({"n": 2, "input": null}));
    let g = before["global_layers"][0]["id"].clone();
    cmd("global_layer_remove", json!({"id": g}));
    cmd("audio_offset", json!({"id": id("Blue"), "ms": 0}));
    cmd("audio_offset", json!({"id": id("Green"), "ms": 300}));
    cmd("setup_load", json!({"name": "Live"}));
    let st = svc.state();
    let t = &st["transition"];
    assert_eq!(
        (t["kind"].as_str(), t["duration"].as_f64()),
        (Some("luma"), Some(2.0)),
        "{t}"
    );
    assert_eq!(t["luma"]["pattern"].as_u64(), Some(id("Green")));
    assert_eq!(t["luma"]["invert"], true);
    assert!((t["luma"]["soft"].as_f64().unwrap() - 0.1).abs() < 1e-6);
    assert_eq!(st["stingers"][1]["input"].as_u64(), Some(id("Blue")));
    assert!((st["stingers"][1]["cut"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    let (gl, was) = (&st["global_layers"][0], &before["global_layers"][0]);
    assert_eq!(gl["source"].as_u64(), Some(id("Green")));
    assert_eq!(
        (&gl["id"], &gl["rect"], &gl["border"]),
        (&was["id"], &was["rect"], &was["border"]),
        "{gl}"
    );
    assert_eq!(
        input("Main")["source"]["scene"]["items"][0]["global"],
        was["id"],
        "the scene's copy back"
    );
    assert_eq!((offset("Blue"), offset("Green")), (Some(120), Some(80)));
    assert!(st["undo"]["back"].is_null(), "undo starts again");
    // A setup without offsets, a transition or stingers: those left as they are.
    let mut old = saved.clone();
    for k in ["transition", "stingers"] {
        old.as_object_mut().unwrap().remove(k);
    }
    old["files"]
        .as_object_mut()
        .unwrap()
        .remove("audio-offsets.json");
    std::fs::write(dir.join("setups/Old.json"), old.to_string()).unwrap();
    cmd(
        "transition",
        json!({"kind": "wipe-up", "soft": 0.15, "duration": 0.5}),
    );
    cmd("audio_offset", json!({"id": id("Blue"), "ms": 200}));
    cmd("audio_offset", json!({"id": id("Green"), "ms": 0}));
    let before = svc.state();
    cmd("setup_load", json!({"name": "Old"}));
    let st = svc.state();
    assert_eq!(st["transition"]["kind"], "wipe-up");
    assert!((st["transition"]["soft"].as_f64().unwrap() - 0.15).abs() < 1e-6);
    assert_eq!(st["stingers"], before["stingers"]);
    assert_eq!((offset("Blue"), offset("Green")), (Some(200), Some(0)));
    // (Kept so for the restart.)
    assert!(
        idn_common::testing::wait_until(Duration::from_secs(5), || {
            let f = offsets_file();
            f["Blue"] == 200 && f.get("Green").is_none()
        }),
        "{}",
        offsets_file()
    );
    // A damaged setup: refused, saying so, and set aside (not read as "none" and saved
    // over), the setup as it was left alone.
    std::fs::write(
        dir.join("setups/Broken.json"),
        "{\"name\": \"Broken\", \"fil",
    )
    .unwrap();
    let e = svc
        .command("setup_load", &json!({"name": "Broken"}))
        .unwrap_err();
    assert!(e.contains("set aside"), "{e}");
    assert!(!dir.join("setups/Broken.json").exists());
    assert!(
        std::fs::read_dir(dir.join("setups"))
            .unwrap()
            .flatten()
            .any(|f| f
                .file_name()
                .to_string_lossy()
                .starts_with("Broken.json.bad-"))
    );
    assert_eq!(svc.state()["transition"]["kind"], "wipe-up");
    let e = svc
        .command("setup_load", &json!({"name": "Nowhere"}))
        .unwrap_err();
    assert!(e.contains("no setup"), "{e}");
}

/// What the deleted panel did on its own, now the API's (one Service a process: run from
/// the test above).
fn what_the_panel_did(svc: &std::sync::Arc<Service>) {
    let get = |path: &str| -> Value {
        let r = svc.handle(Request {
            method: "GET".into(),
            path: path.into(),
            query: String::new(),
            headers: Default::default(),
            body: Vec::new(),
            peer: [127, 0, 0, 1].into(),
            tls: false,
        });
        assert_eq!(r.status, "200 OK", "{path}");
        serde_json::from_slice(&r.body).unwrap()
    };
    let input = |label: &str| -> Value {
        svc.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("no input {label}"))
    };
    // The title styles built in, listed and used by name.
    let t = get("/api/v1/title_templates");
    assert_eq!(t["lower"]["built_in"], true, "{t}");
    assert_eq!(t.as_object().unwrap().len(), 8);
    svc.command("add_input", &json!({"template": "count"}))
        .unwrap();
    let count = input("Countdown");
    assert_eq!(count["summary"], "Starting in {timer:countdown}");
    let timers = svc.state()["timers"].clone();
    let cd = timers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "countdown");
    assert_eq!(
        cd.map(|t| t["mode"]["down"].clone()),
        Some(json!(300.0)),
        "{timers}"
    );
    svc.command(
        "add_input",
        &json!({"template": "two", "text": "Ann Smith"}),
    )
    .unwrap();
    assert_eq!(input("Ann Smith")["summary"], "Ann Smith");
    assert!(
        svc.command("add_input", &json!({"template": "lower"}))
            .is_err()
    );
    // A field's name with a brace is refused.
    let ann = input("Ann Smith")["id"].as_u64().unwrap();
    assert!(
        svc.command("title_fields", &json!({"id": ann, "fields": {"a{b": "x"}}))
            .is_err()
    );
    // A timer as "m:ss".
    svc.command(
        "timer",
        &json!({"name": "talk", "action": "set", "mode": "down", "seconds": "2:30"}),
    )
    .unwrap();
    let timers = svc.state()["timers"].clone();
    let talk = timers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "talk");
    assert_eq!(talk.map(|t| t["mode"]["down"].clone()), Some(json!(150.0)));
    // The filters' schema.
    let f = get("/api/v1/filters_schema");
    assert_eq!(f["filters"].as_array().unwrap().len(), 8, "{f}");
    // A global layer (§S.5): in a scene made after it; undone and redone. A layer shown
    // or hidden is live, not an undo step; only a scene goes on a bus.
    let blue = input("Blue")["id"].as_u64().unwrap();
    let g = svc
        .command(
            "global_layer",
            &json!({"layer": {"source": blue, "rect": [0.65, 0.05, 0.3, 0.3], "border": 4,
                "name": "PiP", "visible": false}}),
        )
        .unwrap()["id"]
        .clone();
    svc.command("add_input", &json!({"scene": true, "label": "Wide"}))
        .unwrap();
    let copy = input("Wide")["source"]["scene"]["items"][0].clone();
    assert_eq!((&copy["global"], &copy["visible"]), (&g, &json!(false)));
    let back = svc.state()["undo"]["back"].clone();
    svc.command(
        "scene_layer_visible",
        &json!({"scene": "Wide", "layer": copy["id"], "on": true}),
    )
    .unwrap();
    assert_eq!(
        input("Wide")["source"]["scene"]["items"][0]["visible"],
        true
    );
    assert_eq!(svc.state()["undo"]["back"], back, "not an undo step");
    assert!(
        svc.command("program", &json!({"id": "Blue"}))
            .unwrap_err()
            .contains("scene")
    );
    svc.command("program", &json!({"id": "Wide"})).unwrap();
    svc.command("undo", &json!({})).unwrap(); // the scene
    svc.command("undo", &json!({})).unwrap(); // the global layer
    assert!(svc.state()["global_layers"].as_array().unwrap().is_empty());
    svc.command("redo", &json!({})).unwrap();
    assert_eq!(svc.state()["global_layers"][0]["id"], g);
    svc.command("global_layer_remove", &json!({"id": g}))
        .unwrap();
    // A virtual shot's crop, the engine's.
    svc.command(
        "add_input",
        &json!({"virtual": {"input": blue, "view": {"x": 0.5, "y": 0.5, "zoom": 2.0}}, "label": "Shot"}),
    )
    .unwrap();
    assert_eq!(
        input("Shot")["source"]["virtual"]["crop"],
        json!([0.25, 0.25, 0.5, 0.5])
    );
    // `slide`: the slideshow (none on program or preview: the first); "|" makes files.
    svc.command(
        "add_input",
        &json!({"slideshow": {"from": "a.png | b.png"}, "label": "Slides"}),
    )
    .unwrap();
    let slides = input("Slides");
    assert_eq!(
        slides["source"]["slideshow"]["pictures"],
        json!(["a.png", "b.png"])
    );
    let r = svc.command("slide", &json!({"action": "pause"})).unwrap();
    assert_eq!(
        (r["id"].clone(), r["kind"].as_str()),
        (slides["id"].clone(), Some("slideshow"))
    );
    // Cues edited one by one, with the show's version.
    svc.command("show_new", &json!({"name": "Test"})).unwrap();
    let v0 = svc.state()["show"]["version"].as_u64().unwrap();
    let r = svc
        .command(
            "cue_add",
            &json!({"cue": {"name": "Start"}, "if_version": v0}),
        )
        .unwrap();
    assert_eq!(r["n"], 1);
    assert!(
        svc.command("cue_add", &json!({"if_version": v0})).is_err(),
        "an old version"
    );
    svc.command("cue_add", &json!({})).unwrap();
    svc.command(
        "cue_add",
        &json!({"cue": {"name": "Part 1", "kind": "header"}, "at": 1}),
    )
    .unwrap();
    let names = || -> Vec<String> {
        svc.state()["show"]["show"]["cues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(), ["Part 1", "Start", "Cue 2"]);
    assert!(
        svc.command("cue_fire", &json!({"n": 1})).is_err(),
        "a header"
    );
    assert!(
        svc.command("cue_standby", &json!({"n": 1})).is_err(),
        "a header"
    );
    svc.command(
        "cue_update",
        &json!({"n": 3, "cue": {"name": "End", "notes": "bye"}}),
    )
    .unwrap();
    svc.command("cue_move", &json!({"n": 3, "to": 2})).unwrap();
    assert_eq!(names(), ["Part 1", "End", "Start"]);
    assert_eq!(svc.state()["show"]["show"]["cues"][1]["notes"], "bye");
    svc.command("cue_delete", &json!({"n": 2})).unwrap();
    assert_eq!(names(), ["Part 1", "Start"]);
    // The whole show, only over the version it was read from.
    let st = svc.state();
    let (v, show) = (
        st["show"]["version"].as_u64().unwrap(),
        st["show"]["show"].clone(),
    );
    assert!(
        svc.command("show_set", &json!({"show": show, "if_version": v + 1}))
            .is_err()
    );
    let r = svc
        .command("show_set", &json!({"show": show, "if_version": v}))
        .unwrap();
    assert_eq!(r["version"].as_u64(), Some(v));
    cue_edits_undone(svc, &names);
    // A change refused over HTTP doesn't use up its sequence number.
    let post = |cmd: &str, body: Value| -> (&str, Value) {
        let headers = [
            ("x-idn-switcher", "1"),
            ("x-idn-client", "test"),
            ("x-idn-control", "show"),
            ("x-idn-seq", "5"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let r = svc.handle(Request {
            method: "POST".into(),
            path: format!("/api/v1/{cmd}"),
            query: String::new(),
            headers,
            body: body.to_string().into_bytes(),
            peer: [127, 0, 0, 1].into(),
            tls: false,
        });
        (r.status, serde_json::from_slice(&r.body).unwrap())
    };
    assert_eq!(post("cue_standby", json!({"n": 99})).0, "400 Bad Request");
    let (status, out) = post("cue_standby", json!({"n": 1}));
    assert_eq!(
        (status, out["skipped"].as_bool()),
        ("200 OK", None),
        "{out}"
    );
    assert_eq!(
        post("cue_standby", json!({"n": 1})).1["skipped"],
        true,
        "used now"
    );
}

/// Cue edits undone as cue edits (bug run O): the places follow both ways; a cue's
/// errors follow it; the current cue removed at the end leaves no next.
fn cue_edits_undone(svc: &std::sync::Arc<Service>, names: &dyn Fn() -> Vec<String>) {
    let places = || {
        let s = svc.state();
        (s["show"]["current"].as_u64(), s["show"]["next"].as_u64())
    };
    let undo = |cmd: &str| {
        svc.command(cmd, &json!({}))
            .unwrap_or_else(|e| panic!("{cmd}: {e}"));
    };
    let show = json!({"name": "Places", "cues": [
        {"name": "A"},
        {"name": "B", "actions": [{"do": "switcher", "command": "no_such"}]},
        {"name": "C"},
    ]});
    svc.command("show_set", &json!({"show": show})).unwrap();
    svc.command("cue_fire", &json!({"n": 1})).unwrap();
    assert_eq!(places(), (Some(1), Some(2)));
    svc.command("cue_add", &json!({"cue": {"name": "X"}, "at": 2}))
        .unwrap();
    assert_eq!(places(), (Some(1), Some(3)));
    undo("undo");
    assert_eq!(names(), ["A", "B", "C"]);
    assert_eq!(places(), (Some(1), Some(2)), "B still next");
    undo("redo");
    assert_eq!(names(), ["A", "X", "B", "C"]);
    assert_eq!(places(), (Some(1), Some(3)));
    undo("undo");
    // Deleted and moved, put back.
    svc.command("cue_delete", &json!({"n": 2})).unwrap();
    assert_eq!(
        (names(), places()),
        (vec!["A".to_owned(), "C".into()], (Some(1), Some(2)))
    );
    undo("undo");
    assert_eq!(names(), ["A", "B", "C"]);
    svc.command("cue_standby", &json!({"n": 2})).unwrap();
    svc.command("cue_move", &json!({"n": 3, "to": 1})).unwrap();
    assert_eq!(
        (names(), places()),
        (
            vec!["C".to_owned(), "A".into(), "B".into()],
            (Some(2), Some(3))
        )
    );
    undo("undo");
    assert_eq!(
        (names(), places()),
        (
            vec!["A".to_owned(), "B".into(), "C".into()],
            (Some(1), Some(2))
        )
    );
    // One cue changed twice in a row: one step back to before both.
    svc.command("cue_update", &json!({"n": 3, "cue": {"name": "C1"}}))
        .unwrap();
    svc.command("cue_update", &json!({"n": 3, "cue": {"notes": "x"}}))
        .unwrap();
    undo("undo");
    let c = svc.state()["show"]["show"]["cues"][2].clone();
    assert_eq!(
        (c["name"].as_str(), c["notes"].as_str()),
        (Some("C"), Some("")),
        "{c}"
    );
    // A cue's error follows it, and goes with it.
    let r = svc.command("cue_fire", &json!({"n": 2})).unwrap();
    assert_eq!(r["errors"].as_array().map(Vec::len), Some(1), "{r}");
    let error = || {
        svc.state()["show"]["error"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    assert!(error().starts_with("2. B:"), "{}", error());
    svc.command("cue_move", &json!({"n": 2, "to": 1})).unwrap();
    assert!(error().starts_with("1. B:"), "{}", error());
    svc.command("cue_delete", &json!({"n": 1})).unwrap();
    assert_eq!(error(), "");
    // The last cue fired, then removed: no next (not cue 1).
    svc.command("cue_fire", &json!({"n": 2})).unwrap();
    assert_eq!(places(), (Some(2), None));
    svc.command("cue_delete", &json!({"n": 2})).unwrap();
    assert_eq!(places(), (None, None));
}

/// One HTTP request to 127.0.0.1:`port`: the status line's code and the body.
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let code = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map_or("", |x| x.1);
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The network's front with site tokens (idn-node.md §N.31.5): a read-only token reads and
/// is refused changes over HTTP and the push channel; a control token changes; tokens
/// for another engine, expired or forged are refused; the same machine stays open; the
/// audit says who did what from where (§S.35).
fn tokens_and_the_audit(svc: &std::sync::Arc<Service>) {
    use idn_open_protocol::trust::pki::KeyFile;
    use idn_open_protocol::trust::token::{self, Claims, Scope};
    use idn_open_protocol::web::{self, PasswordGate, SiteKeys, TokenCheck};
    use std::sync::Arc;
    let key = KeyFile::generate().unwrap();
    let site = key.fingerprint()[..16].to_owned();
    let (public, s2) = (key.public, site.clone());
    let check = TokenCheck::new("switcher", move || {
        Some(SiteKeys {
            ca_public: public,
            site: s2.clone(),
            machine: "this-machine".into(),
            options: Default::default(),
        })
    });
    let gate = Arc::new(PasswordGate::tokens_only(
        check,
        "idn_switcher",
        "X-IDN-Switcher",
    ));
    const LOGIN: web::ui::Login = web::ui::Login {
        title: "IDN Switcher",
        hint: None,
        label: "Password",
        header: "X-IDN-Switcher",
        who: "switcher",
        console: true,
        user: false,
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler: web::Handler = {
        let (svc, gate) = (svc.clone(), gate.clone());
        Arc::new(move |r| {
            web::lan::front(Some(&gate), &LOGIN, r, &|_| {}, |r, who| {
                svc.handle_as(r, who)
            })
        })
    };
    web::serve(listener, None, handler, stop.clone()).unwrap();
    let make = |sub: &str, engine: &str, scope: Scope, iat: u64, k: &KeyFile| {
        let c = Claims::new(&site, sub, sub, engine, "*", scope, iat, 600).unwrap();
        token::sign(&c, k).unwrap()
    };
    let now = idn_common::epoch::secs();
    let read = make("alice", "switcher", Scope::Read, now, &key);
    let control = make("bob", "switcher", Scope::Control, now, &key);
    let bearer = |t: &str| format!("Bearer {t}");
    let change = r##"{"colour": "#ff0000", "label": "Red"}"##;
    let labels = || -> Vec<String> {
        svc.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap_or("").to_owned())
            .collect()
    };
    // Read-only: state yes, changes no.
    let (code, _) = http(
        port,
        "GET",
        "/api/v1/state",
        &[("Authorization", &bearer(&read))],
        "",
    );
    assert_eq!(code, 200);
    let (code, v) = http(
        port,
        "POST",
        "/api/v1/add_input",
        &[("Authorization", &bearer(&read)), ("X-IDN-Switcher", "1")],
        change,
    );
    assert_eq!((code, v["error"].as_str()), (403, Some("read-only")));
    assert!(!labels().contains(&"Red".to_owned()));
    // Control: the change made.
    let (code, v) = http(
        port,
        "POST",
        "/api/v1/add_input",
        &[
            ("Authorization", &bearer(&control)),
            ("X-IDN-Switcher", "1"),
            ("X-IDN-Client", "Streamer"),
        ],
        change,
    );
    assert_eq!(code, 200, "{v}");
    assert!(labels().contains(&"Red".to_owned()));
    // Refused: no token, another engine's, expired, forged.
    let other = KeyFile::generate().unwrap();
    for (t, want) in [
        (None, 403),
        (Some(make("bob", "desk", Scope::Control, now, &key)), 401),
        (
            Some(make("bob", "switcher", Scope::Control, now - 7200, &key)),
            401,
        ),
        (
            Some(make("bob", "switcher", Scope::Control, now, &other)),
            401,
        ),
        (Some("idn1.not.a-token".to_owned()), 401),
    ] {
        let h = t.as_deref().map(bearer);
        let headers: Vec<(&str, &str)> = h.iter().map(|b| ("Authorization", b.as_str())).collect();
        assert_eq!(
            http(port, "GET", "/api/v1/state", &headers, "").0,
            want,
            "{t:?}"
        );
    }
    // The same machine: open, audited as `local`.
    let r = svc.handle(Request {
        method: "POST".into(),
        path: "/api/v1/rename_input".into(),
        query: String::new(),
        headers: [("x-idn-switcher".to_owned(), "1".to_owned())].into(),
        body: json!({"input": "Red", "label": "Crimson"})
            .to_string()
            .into_bytes(),
        peer: [127, 0, 0, 1].into(),
        tls: false,
    });
    assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
    // The push channel: a read-only token in the address; its commands refused, and a
    // `denied` event on its own channel (idn-node.md §N.41).
    let ws_read = |url: String| -> (Value, Value) {
        let (mut ws, _) = tungstenite::connect(url).expect("it upgrades");
        let mut got_state = false;
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !got_state {
            assert!(std::time::Instant::now() < end, "no state");
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                got_state = t.as_str().contains("\"type\":\"state\"");
            }
        }
        ws.send(tungstenite::Message::text(
            json!({"id": 1, "cmd": "add_input", "body": {"colour": "#000000", "label": "Black"}})
                .to_string(),
        ))
        .unwrap();
        let mut reply = Value::Null;
        loop {
            assert!(std::time::Instant::now() < end, "no reply and denied event");
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "reply" {
                    reply = v;
                } else if v["type"] == "event" && v["event"] == "denied" {
                    assert!(!reply.is_null(), "the reply first");
                    return (reply, v);
                }
            }
        }
    };
    let (r, denied) = ws_read(format!("ws://127.0.0.1:{port}/api/v1/events?token={read}"));
    assert_eq!(
        (r["ok"].as_bool(), r["error"].as_str()),
        (Some(false), Some("read-only"))
    );
    assert_eq!(
        (denied["cmd"].as_str(), denied["error"].as_str()),
        (Some("add_input"), Some("read-only"))
    );
    // No token in the upgrade: nothing until the first message gives one; then control.
    let (mut ws, _) =
        tungstenite::connect(format!("ws://127.0.0.1:{port}/api/v1/events")).expect("it upgrades");
    let timeout = |ws: &mut tungstenite::WebSocket<
        tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    >,
                   d| {
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
            s.set_read_timeout(Some(d)).unwrap();
        }
    };
    timeout(&mut ws, Duration::from_millis(500));
    assert!(
        !matches!(ws.read(), Ok(tungstenite::Message::Text(_))),
        "nothing before a token"
    );
    timeout(&mut ws, Duration::from_secs(5));
    let mut send = |v: Value, want: &str| -> Value {
        ws.send(tungstenite::Message::text(v.to_string())).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(std::time::Instant::now() < end, "no {want}");
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == want {
                    return v;
                }
            }
        }
    };
    let t = send(json!({"token": control}), "token");
    assert_eq!(
        (t["ok"].as_bool(), t["user"].as_str()),
        (Some(true), Some("bob"))
    );
    assert_eq!(
        send(json!({"subscribe": ["audit"]}), "subscribe")["ok"],
        true
    );
    let r = send(
        json!({"id": 2, "cmd": "rename_input", "body": {"input": "Crimson", "label": "Scarlet"}}),
        "reply",
    );
    assert_eq!(r["ok"], true, "{r}");
    let pushed = send(json!({"subscribe": ["meters"]}), "audit");
    assert!(
        pushed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["user"] == "bob" && e["action"] == "rename_input"),
        "{pushed}"
    );
    let _ = ws.close(None);
    // The audit: who, from where, what, refused or done.
    let (code, log) = http(
        port,
        "GET",
        "/api/v1/audit?n=50",
        &[("Authorization", &bearer(&read))],
        "",
    );
    assert_eq!(code, 200);
    let lines = log.as_array().unwrap();
    let find = |user: &str, ok: bool| {
        lines
            .iter()
            .find(|l| l["user"] == user && l["ok"] == ok)
            .unwrap_or_else(|| panic!("no line for {user} ok={ok}: {log}"))
    };
    let refused = find("alice", false);
    assert_eq!(
        (refused["error"].as_str(), refused["peer"].as_str()),
        (Some("read-only"), Some("127.0.0.1"))
    );
    let done = find("bob", true);
    assert!(
        lines
            .iter()
            .any(|l| l["user"] == "bob" && l["client"] == "Streamer")
    );
    assert!(done["at"].as_u64().is_some());
    let local = find("local", true);
    assert_eq!(local["action"], "rename_input");
    assert!(!serde_json::to_string(&log).unwrap().contains("idn1."));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // (Left as the rest of the test had it.)
    svc.command("remove_input", &json!({"id": "Scarlet"}))
        .expect("the input taken out again");
}

/// The declared state (refactor 4): every key the switcher's state gives is in its
/// schema.
#[test]
fn the_state_keys_are_declared() {
    let dir = idn_common::testing::TempDir::new("idn-switcher-keys");
    let svc = Service::start(ServiceConfig {
        switcher: SwitcherConfig {
            name: "Keys".into(),
            width: 320,
            height: 180,
            fps: (25, 1),
            mdns: Some(false),
            gpu: false,
            ..Default::default()
        },
        dir: Some(dir.to_path_buf()),
        inputs: Vec::new(),
        node: None,
        rate: Some(48_000),
        machine_rate: None,
    })
    .unwrap();
    // A colour, a scene with a layer and a global layer, a mix: their parts typed too.
    let a = svc
        .command("add_input", &json!({"colour": "#ff0000", "label": "Red"}))
        .unwrap();
    let sc = svc
        .command("add_input", &json!({"scene": true, "label": "S"}))
        .unwrap();
    svc.command(
        "scene_items",
        &json!({"id": sc["id"], "items": [{"source": a["id"], "radius": 0.1, "key": "none"}]}),
    )
    .unwrap();
    svc.command(
        "global_layer",
        &json!({"layer": {"source": {"colour": "#00ff00"}, "rect": [0, 0, 0.1, 0.1]},
                "add_to_scenes": true}),
    )
    .unwrap();
    svc.command("add_mix", &json!({"name": "Vertical", "format": "180x320"}))
        .unwrap();
    let st = svc.state();
    // Typed below the top level too: read into the types and back, nothing left out.
    assert_eq!(
        idn_switcher::service::state_schema::undeclared(&st),
        Ok(Vec::new())
    );
    let schema = idn_switcher::service::state_schema::state_schema();
    let declared = schema["properties"].as_object().unwrap();
    let undeclared: Vec<&String> = st
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| !declared.contains_key(*k))
        .collect();
    assert!(undeclared.is_empty(), "not in the schema: {undeclared:?}");
    // The format from the API (§S.38): checked, kept for the next start (not run by the
    // IDN Node here: no restart), shown as `format_next`.
    assert!(svc.command("format", &json!({"format": "999p7"})).is_err());
    let r = svc
        .command("format", &json!({"format": "1080p60"}))
        .unwrap();
    assert_eq!(r["restart_needed"], true);
    assert_eq!(svc.state()["format_next"], "1080p60");
    assert_eq!(
        idn_switcher::service::saved_format(&dir).map(|f| (f.0, f.1, f.2)),
        Some((1920, 1080, (60, 1)))
    );
    // The format it runs: no change, nothing next.
    let r = svc
        .command("format", &json!({"format": "320x180@25"}))
        .unwrap();
    assert_eq!(r["restart"], false);
    assert!(svc.state()["format_next"].is_null());
}
