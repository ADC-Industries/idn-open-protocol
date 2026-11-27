//! The typed commands' tests: every name the dispatcher takes is typed, and each round-trips.

use super::*;
use std::collections::BTreeSet;

/// The names the dispatcher matches: its arms at the match level, and `cmd == "…"`.
fn dispatcher_names() -> BTreeSet<String> {
    let src = include_str!("../commands.rs");
    let mut out = BTreeSet::new();
    for line in src.lines() {
        if let Some(rest) = line.strip_prefix("            ")
            && rest.starts_with('"')
            && rest.contains("=>")
        {
            let head = rest.split("=>").next().unwrap_or("");
            let head = head.split(" if ").next().unwrap_or("");
            for part in head.split('|') {
                let p = part.trim();
                assert!(
                    p.len() > 2 && p.starts_with('"') && p.ends_with('"'),
                    "an arm not understood: {line}"
                );
                out.insert(p.trim_matches('"').to_owned());
            }
        }
        let mut s = line;
        while let Some(i) = s.find("cmd == \"") {
            let r = &s[i + 8..];
            let end = r.find('"').unwrap_or(r.len());
            out.insert(r[..end].to_owned());
            s = &r[end..];
        }
    }
    out
}

/// The variants' names, from the schema.
fn schema_names() -> BTreeSet<String> {
    let s = schema();
    let list = s["oneOf"]
        .as_array()
        .or(s["anyOf"].as_array())
        .expect("oneOf");
    list.iter()
        .map(|v| {
            let c = &v["properties"]["cmd"];
            c["const"]
                .as_str()
                .or(c["enum"][0].as_str())
                .expect("a cmd name")
                .to_owned()
        })
        .collect()
}

/// One body for every command (a smallest one, or a fuller one).
fn samples() -> Vec<(&'static str, Value)> {
    vec![
        (
            "mqtt_set",
            json!({"url": "mqtt://sm:1883", "triggers": [{"topic": "a/#", "command": "cut", "body": {}}]}),
        ),
        ("starter", json!({"name": "Sunday"})),
        ("undo", json!({})),
        ("redo", json!({})),
        (
            "add_input",
            json!({"template": "count", "text": "Back in", "label": "Countdown"}),
        ),
        ("list", json!({"id": 3, "action": "goto", "n": 2})),
        (
            "list_items",
            json!({"id": "List", "items": ["a.mp4", {"path": "b.png", "seconds": 3.0}], "from": "c.mp4|d.mp4"}),
        ),
        (
            "media",
            json!({"id": 4, "action": "seek", "position": 12.5}),
        ),
        ("filters", json!({"id": 1, "filters": [{"kind": "crop"}]})),
        (
            "title",
            json!({"id": "Lower third", "layers": [{"kind": "text", "text": "Hi"}], "fields": {"name": "A"}}),
        ),
        ("title_fields", json!({"id": 5, "fields": {"score": 3}})),
        (
            "camera",
            json!({"id": 2, "control": {"visca": "udp", "address": "10.0.0.9"}}),
        ),
        (
            "ptz",
            json!({"id": 2, "move": "recall", "preset": 3, "speed": 0.5}),
        ),
        (
            "virtual_view",
            json!({"id": 6, "view": {"x": 0.25, "zoom": 2.0}, "seconds": 1.5}),
        ),
        ("title_row", json!({"id": 5, "action": "read"})),
        (
            "title_field_add",
            json!({"id": 5, "name": "home", "by": -1.0}),
        ),
        ("title_template", json!({"name": "LT"})),
        (
            "timer",
            json!({"name": "Sermon", "action": "set", "mode": "down", "seconds": "5:00"}),
        ),
        ("slideshow", json!({"id": 7, "action": "next"})),
        ("slide", json!({"action": "goto", "n": 3})),
        (
            "scene_item_sound",
            json!({"id": 8, "layer": 3, "volume_db": -6.0, "muted": false}),
        ),
        ("scene_items", json!({"id": 8, "items": []})),
        (
            "scene_layer_visible",
            json!({"scene": "Pulpit", "layer": 2, "on": false}),
        ),
        (
            "global_layer",
            json!({"layer": {"source": 4, "rect": [0.85, 0.05, 0.1, 0.1], "name": "Logo"}, "add_to_scenes": true}),
        ),
        ("global_layer_remove", json!({"id": 1})),
        ("keep_active", json!({"id": "Camera 1", "on": true})),
        ("format", json!({"format": "1080p60"})),
        (
            "destination_save",
            json!({"name": "YouTube", "body": {"url": "rtmp://a.example/live/KEY"}}),
        ),
        ("destination_remove", json!({"id": "d1"})),
        ("go_live", json!({"ids": ["d1", "d2"]})),
        (
            "scene_snapshot",
            json!({"id": "Wide", "desk": "BOOTH (Mixer)", "snapshot": "Sermon"}),
        ),
        (
            "multiview_layout",
            json!({"tiles": [{"kind": "program", "rect": [0.0, 0.0, 1.0, 0.5]}, {"kind": "scene", "id": 9, "rect": [0.0, 0.5, 0.25, 0.25], "label": false, "tally": true}]}),
        ),
        ("multiview", json!({"output": true})),
        ("remove_input", json!({"id": 9})),
        ("rename_input", json!({"id": "Cam 1", "label": "Camera 1"})),
        ("audio_offset", json!({"id": 1, "ms": 40})),
        ("preview", json!({"id": "Camera 2"})),
        ("program", json!({"id": 1})),
        ("cut", json!({})),
        ("auto", json!({})),
        ("tbar", json!({"position": 0.5})),
        (
            "transition",
            json!({"kind": "luma", "pattern": 3, "soft": 0.1, "invert": true, "duration": 2.0}),
        ),
        (
            "stinger",
            json!({"n": 1, "input": 4, "duration": 1.2, "cut": 0.6}),
        ),
        ("ftb", json!({})),
        (
            "audio",
            json!({"source": {"card": null, "first": 3}, "gain_db": -3.0}),
        ),
        ("monitor", json!({"solo": [1, 2], "card": null})),
        ("monitor_solo", json!({"input": 2, "on": true})),
        ("screen", json!({"screen": "1", "output": "program"})),
        ("caption", json!({"text": "Hello"})),
        (
            "captions",
            json!({"on": true, "clear_after": 4.0, "from": ""}),
        ),
        ("setup_save", json!({"name": "Sunday"})),
        ("setup_load", json!({"name": "Sunday"})),
        ("setup_delete", json!({"name": "Sunday"})),
        ("cue_go", json!({"if_next": 3})),
        ("cue_back", json!({})),
        ("cue_standby", json!({"n": 2})),
        ("cue_fire", json!({"n": 2})),
        (
            "show_set",
            json!({"show": {"name": "S", "cues": []}, "if_version": 12}),
        ),
        (
            "cue_add",
            json!({"cue": {"name": "Welcome", "kind": "header"}, "at": 1}),
        ),
        (
            "cue_update",
            json!({"n": 2, "cue": {"notes": "x"}, "if_version": 5}),
        ),
        ("cue_delete", json!({"n": 2})),
        ("cue_move", json!({"n": 2, "to": 1})),
        ("show_new", json!({"name": "New"})),
        ("show_load", json!({"name": "Old"})),
        ("show_save", json!({"name": "Old"})),
        ("show_delete", json!({"name": "Old"})),
        ("pco_login", json!({"app_id": "a", "secret": "b"})),
        ("pco_logout", json!({})),
        ("pco_plans", json!({})),
        ("pco_import", json!({"service_type": "1", "plan": "2"})),
        (
            "stream_add",
            json!({"url": "rtmp://live/app/key", "backup": "rtmp://b/app/key", "adaptive": true, "floor_mbps": 2.5, "name": "YouTube"}),
        ),
        ("iso_add", json!({"input": 1, "split_s": 600})),
        (
            "add_mix",
            json!({"format": "1080x1920", "name": "Vertical"}),
        ),
        ("remove_mix", json!({"mix": 1})),
        ("mix_program", json!({"mix": 1, "id": "Camera 1"})),
        ("mix_preview", json!({"mix": 1, "id": 2})),
        ("mix_cut", json!({"mix": 1})),
        ("mix_auto", json!({"mix": 1})),
        (
            "mix_transition",
            json!({"mix": 1, "kind": "mix", "duration": 0.5}),
        ),
        ("stream_pause", json!({"id": 7, "paused": false})),
        ("stream_remove", json!({"id": 7})),
    ]
}

#[test]
fn every_dispatched_command_has_a_variant() {
    let dispatched = dispatcher_names();
    let names: BTreeSet<String> = Command::NAMES.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(Command::NAMES.len(), names.len(), "a name twice in NAMES");
    assert_eq!(
        dispatched, names,
        "the dispatcher's names and Command::NAMES"
    );
    assert_eq!(
        schema_names(),
        names,
        "the schema's variants and Command::NAMES"
    );
}

#[test]
fn every_variant_round_trips() {
    let samples = samples();
    let covered: BTreeSet<&str> = samples.iter().map(|(n, _)| *n).collect();
    let names: BTreeSet<&str> = Command::NAMES.iter().copied().collect();
    assert_eq!(covered, names, "a sample per command");
    for (name, body) in samples {
        let c = Command::parse(name, &body).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.name(), name);
        assert_eq!(c.body(), body, "{name}'s body");
        // And again from what it gives, as a JSON client sends it.
        let again: Command =
            serde_json::from_value(json!({"cmd": c.name(), "body": c.body()})).expect(name);
        assert_eq!(again, c, "{name}");
    }
}

#[test]
fn typed_values_give_the_dispatchers_json() {
    let c = Command::Program {
        id: "Camera 1".into(),
    };
    assert_eq!((c.name(), c.body()), ("program", json!({"id": "Camera 1"})));
    assert_eq!(Command::Cut {}.body(), json!({}));
    let c = Command::Monitor {
        preview: None,
        solo: None,
        volume_db: None,
        mute: None,
        card: Some(None),
    };
    assert_eq!(c.body(), json!({"card": null}));
    let c = Command::Audio {
        source: Some(AudioSourceSpec::Named("program".into())),
        gain_db: None,
        delay_ms: None,
        take_feed: None,
        denoise: None,
    };
    assert_eq!(c.body(), json!({"source": "program"}));
    // An input's own sound and a card are told apart by their keys.
    assert_eq!(
        Command::parse("audio", &json!({"source": {"input": 3}})).unwrap(),
        Command::Audio {
            source: Some(AudioSourceSpec::Input { input: 3 }),
            gain_db: None,
            delay_ms: None,
            take_feed: None,
            denoise: None,
        }
    );
    assert!(Command::parse("no_such", &json!({})).is_err());
    assert!(Command::parse("program", &json!({})).is_err());
}

#[test]
fn schema_lists_the_commands() {
    let s = schema();
    assert!(s.is_object());
    let text = s.to_string();
    for n in ["cut", "add_input", "stream_add", "cue_fire", "mqtt_set"] {
        assert!(text.contains(&format!("\"{n}\"")), "{n} in the schema");
    }
    assert!(text.contains("Swaps preview and program at once."));
}
