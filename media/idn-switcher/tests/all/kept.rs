//! What the switcher keeps between runs (spec/extensions/idn-switcher.md §S.5, §S.11,
//! §S.36): the global layers, the multiview and the inputs kept active.

use idn_switcher::SwitcherConfig;
use idn_switcher::service::{Service, ServiceConfig};
use serde_json::{Value, json};
use std::time::Duration;

fn start(dir: &std::path::Path, name: &str) -> std::sync::Arc<Service> {
    Service::start(ServiceConfig {
        switcher: SwitcherConfig {
            name: name.into(),
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
    .expect("it starts")
}

#[test]
fn layers_multiview_and_kept_inputs_are_kept_between_runs() {
    let dir = idn_common::testing::TempDir::new("idn-switcher-kept");
    let svc = start(&dir, "KeptTest");
    let by_label = |st: &Value, l: &str| -> Value {
        st["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["label"] == l)
            .cloned()
            .unwrap_or_else(|| panic!("no input {l}: {st}"))
    };
    for (label, colour) in [("Cam", "#808080"), ("Slate", "#202020")] {
        svc.command("add_input", &json!({"colour": colour, "label": label}))
            .unwrap();
    }
    let cam = by_label(&svc.state(), "Cam")["id"].clone();
    let made = svc
        .command("add_input", &json!({"scene": true, "label": "Cam (scene)"}))
        .unwrap();
    svc.command(
        "scene_items",
        &json!({"id": made["id"], "items": [{"source": cam}]}),
    )
    .unwrap();
    let mix = svc
        .command(
            "add_mix",
            &json!({"name": "Vertical", "width": 180, "height": 320}),
        )
        .unwrap()["id"]
        .clone();
    let st = svc.state();
    let scene = by_label(&st, "Cam (scene)");
    // The API takes no raw input on a bus.
    assert!(svc.command("program", &json!({"id": "Cam"})).is_err());
    assert!(
        svc.command("mix_program", &json!({"mix": mix, "id": "Cam"}))
            .is_err()
    );

    // Global layers, the multiview's layout and output, an input kept active: kept.
    let g = svc
        .command(
            "global_layer",
            &json!({"layer": {"source": cam, "rect": [0.7, 0.0, 0.3, 0.3], "name": "Corner"},
                "add_to_scenes": true}),
        )
        .unwrap()["id"]
        .clone();
    svc.command("keep_active", &json!({"id": "Slate", "on": true}))
        .unwrap();
    svc.command(
        "multiview_layout",
        &json!({"tiles": [
            {"kind": "program", "rect": [0.0, 0.0, 0.5, 0.5]},
            {"kind": "scene", "id": scene["id"], "rect": [0.5, 0.0, 0.5, 0.5], "label": false},
            {"kind": "mix", "id": mix, "rect": [0.0, 0.5, 0.25, 0.25]},
        ]}),
    )
    .unwrap();
    svc.command("multiview", &json!({"output": true})).unwrap();
    assert!(
        svc.command(
            "multiview_layout",
            &json!({"tiles": [{"kind": "input", "rect": [0, 0, 1, 1]}]})
        )
        .is_err()
    );
    assert!(idn_common::testing::wait_until(
        Duration::from_secs(5),
        || std::fs::read_to_string(dir.join("keep-active.json")).is_ok_and(|t| t.contains("Slate"))
    ));
    svc.close(Duration::from_secs(1));
    drop(svc);

    let svc = start(&dir, "KeptTest2");
    let st = svc.state();
    let cam = by_label(&st, "Cam")["id"].clone();
    let scene = by_label(&st, "Cam (scene)");
    assert_eq!(st["global_layers"][0]["id"], g);
    assert_eq!(st["global_layers"][0]["source"], cam);
    assert_eq!(st["global_layers"][0]["name"], "Corner");
    let items = &scene["source"]["scene"]["items"];
    assert_eq!(items[1]["global"], g, "{items}");
    let mv = &st["multiview"];
    assert_eq!(
        (&mv["custom"], &mv["output"]),
        (&json!(true), &json!(true)),
        "{mv}"
    );
    assert_eq!(mv["tiles"].as_array().unwrap().len(), 3);
    assert_eq!(mv["tiles"][1]["id"], scene["id"]);
    assert_eq!(mv["tiles"][1]["label"], false);
    assert_eq!(mv["tiles"][2]["id"], st["mixes"][0]["id"]);
    assert!(
        idn_common::testing::wait_until(Duration::from_secs(5), || {
            by_label(&svc.state(), "Slate")["keep_active"] == true
        }),
        "kept active"
    );
    svc.close(Duration::from_secs(1));
    drop(svc);
}
