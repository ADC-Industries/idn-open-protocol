//! The typed commands' tests: names, bodies and parsing match the dispatcher, the schema.

use super::*;
use std::collections::BTreeSet;

/// The names the dispatcher matches: its arms at the match's own level.
fn dispatched() -> BTreeSet<String> {
    let src = include_str!("../commands.rs");
    let start = src.find("match cmd {").expect("the dispatcher");
    let end = start
        + src[start..]
            .find("other => Err(format!(\"no command")
            .expect("its end");
    // The arms sit one level in from `match cmd {` (its line's indent + 4).
    let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent = start - line_start + 4;
    let mut names = BTreeSet::new();
    for line in src[start..end].lines() {
        let t = line.trim_start();
        if line.len() - t.len() != indent || !t.starts_with('"') {
            continue;
        }
        let head = t.split("=>").next().unwrap_or("");
        let head = head.split(" if ").next().unwrap_or("");
        for alt in head.split('|') {
            let alt = alt.trim().trim_matches('"');
            assert!(!alt.is_empty(), "{line}");
            names.insert(alt.to_owned());
        }
    }
    names
}

/// The variants' names, from the schema (each variant's `cmd` constant).
fn variants() -> BTreeSet<String> {
    fn walk(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(o) => {
                if let Some(c) = o.get("properties").and_then(|p| p.get("cmd")) {
                    if let Some(s) = c["const"].as_str() {
                        out.insert(s.to_owned());
                    }
                    for s in c["enum"].as_array().into_iter().flatten() {
                        out.insert(s.as_str().unwrap_or("").to_owned());
                    }
                }
                o.values().for_each(|x| walk(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(&schema(), &mut out);
    out
}

#[test]
fn typed_commands_are_the_dispatchers() {
    let d = dispatched();
    assert!(d.len() >= 30, "{d:?}");
    assert_eq!(d, variants());
}

/// One of every variant.
fn samples() -> Vec<Command> {
    let name = || "A".to_owned();
    let p = || one("name", json!("X"));
    vec![
        Command::Starter { name: name() },
        Command::PluginsScan {},
        Command::Route {
            receiver: name(),
            changes: vec![RouteChange {
                ch: 3,
                tx: None,
                tx_ch: None,
            }],
        },
        Command::ReceiverLatency {
            receiver: name(),
            ms: None,
        },
        Command::Lock { on: None },
        Command::Group {
            group: 1,
            patch: p(),
        },
        Command::Matrix {
            matrix: 2,
            patch: p(),
        },
        Command::Fx { fx: 3, patch: p() },
        Command::MuteGroup {
            group: 1,
            name: None,
            on: Some(true),
            color: Some("#ff8800".into()),
        },
        Command::Talkback {
            channel: Some(8),
            on: None,
            level: Some(-6.0),
            aux: Some(vec![Some(true), None]),
            main: None,
            dim_main: Some(-10.0),
            to_iem: Some(true),
        },
        Command::Button {
            button: 1,
            label: Some("Mute 1".into()),
            action: Some("mute:1".into()),
        },
        Command::Press { button: 2 },
        Command::ClearSolo {},
        Command::LoudnessReset {},
        Command::RecordStart {
            channels: None,
            name: None,
        },
        Command::RecordStop {},
        Command::Soundcheck { name: None },
        Command::SoundcheckTransport(Transport::Play),
        Command::SoundcheckTransport(Transport::Loop { on: Some(false) }),
        Command::SoundcheckLive {
            n: 1,
            live: Some(false),
        },
        Command::Output {
            out: 1,
            source: Some("bus:1".into()),
            delay: Some(10.0),
        },
        Command::Desk {
            solo_afl: Some(true),
            auto_park: Some(false),
            automix: None,
            duckers: Some(vec![json!({"on": true}), Value::Null]),
            afv: Some(AfvPatch {
                on: Some(true),
                fade: Some(500.0),
                ..Default::default()
            }),
            block: None,
            rate: Some(Rate::Text(String::new())),
            output_delay: None,
            x32: Some(X32Patch {
                address: Some("auto".into()),
            }),
        },
        Command::Preamp {
            n: 1,
            gain_db: Some(24.0),
            phantom: None,
            invert: Some(false),
        },
        Command::PresetSave { name: name(), n: 1 },
        Command::PresetLoad { name: name(), n: 2 },
        Command::PresetDelete { name: name() },
        Command::ChannelCount { n: 16 },
        Command::Restart {},
        Command::LatencyTest {
            output: 1,
            input: 1,
        },
        Command::ChannelReset { n: 3 },
        Command::CopyChannel { from: 1, n: 2 },
        Command::Channel { n: 1, patch: p() },
        Command::Channels {
            n: vec![1, 3],
            patch: one("mute", json!(true)),
        },
        Command::Bus {
            bus: BusRef::Named(BusName::Headphones),
            patch: p(),
        },
        Command::Dca {
            dca: 1,
            name: None,
            fader: Some(-6.0),
            mute: None,
            color: None,
        },
        Command::SnapshotSave {
            name: name(),
            include_iem: Some(true),
        },
        Command::IemMixAdd {
            name: name(),
            id: None,
        },
        Command::IemMix {
            id: "drums".into(),
            name: None,
            master_db: Some(-3.0),
            channels: Some(
                [
                    (
                        "3".to_owned(),
                        Some(IemSendPatch {
                            level_db: Some(-6.0),
                            ..Default::default()
                        }),
                    ),
                    ("4".to_owned(), None),
                ]
                .into(),
            ),
        },
        Command::IemMixDelete { id: "drums".into() },
        Command::Send {
            n: 2,
            bus: SendBus::Aux(5),
            level: Some(-10.0),
            on: Some(true),
            pan: None,
            pre: None,
        },
        Command::Send {
            n: 1,
            bus: SendBus::Named(SendBusName::Stream),
            level: Some(-3.0),
            on: None,
            pan: None,
            pre: None,
        },
        Command::EndpointAdd {
            name: "PA".into(),
            outputs: vec![1, 2],
        },
        Command::Endpoint {
            id: "e1".into(),
            name: Some("Main PA".into()),
            outputs: None,
        },
        Command::EndpointRemove { id: "e1".into() },
        Command::SnapshotRecall {
            name: name(),
            scope: Some(SnapshotScope {
                channels: Some(ScopeChannels::All(false)),
                ..Default::default()
            }),
            fade_s: Some(2.0),
        },
        Command::SnapshotOrder {
            names: vec![name()],
        },
        Command::SnapshotDelete { name: name() },
        Command::SwitcherInputs { on: None },
        Command::SwitcherInput {
            name: name(),
            on: Some(false),
        },
        Command::SwitcherFeed {
            name: name(),
            bus: Some("main".into()),
        },
        Command::Insert {
            strip: Strip::Channel(1),
            op: InsertOp::Add {
                key: "clap:x#y".into(),
                name: None,
                at: Some(0),
            },
        },
        Command::Insert {
            strip: Strip::Group(2),
            op: InsertOp::Bypass { index: 0, on: true },
        },
        Command::Insert {
            strip: Strip::Fx(1),
            op: InsertOp::Remove { index: 1 },
        },
        Command::PluginSet {
            strip: Strip::Matrix(1),
            index: 0,
            param: 1,
            value: 1.0,
        },
        Command::PluginWindow {
            strip: Strip::Bus(BusRef::Aux(3)),
            index: 0,
            open: None,
            parent: Some("x11:42".into()),
        },
    ]
}

/// Every variant back from `{"cmd": name, "body": body()}` (a JSON client's form, `{}`
/// bodies included), and the samples cover every variant.
#[test]
/// One command path (refactor 4): every typed command reaches a handler of the
/// desk's (none falls to "no command"), and a name the types don't know is refused
/// before it runs.
fn every_typed_command_has_a_handler() {
    // (`record_start` records: into a folder of the test's, not the working one.)
    let dir = idn_common::testing::TempDir::new("mixer-typed-rec");
    let c = crate::Console::start(crate::ConsoleConfig {
        name: "Typed test".into(),
        channels: 8,
        mdns: Some(false),
        recordings: Some(dir.path().to_owned()),
        ..Default::default()
    })
    .unwrap();
    for cmd in samples() {
        // (Restart would end the test's process under a supervisor; it isn't one.)
        if let Err(e) = c.command(cmd.name(), &cmd.body()) {
            assert!(!e.starts_with("no command"), "{}: {e}", cmd.name());
        }
    }
    assert_eq!(
        c.command("frobnicate", &json!({})).unwrap_err(),
        "no such command \"frobnicate\""
    );
    assert!(
        c.command("channel", &json!({"n": "one"}))
            .unwrap_err()
            .starts_with("channel:")
    );
    drop(c);
}

#[test]
fn typed_every_variant_round_trips() {
    let mut names = BTreeSet::new();
    for c in samples() {
        names.insert(c.name().to_owned());
        let wire = json!({"cmd": c.name(), "body": c.body()});
        let back: Command =
            serde_json::from_value(wire.clone()).unwrap_or_else(|e| panic!("{wire}: {e}"));
        assert_eq!(back, c, "{wire}");
    }
    assert_eq!(names, variants());
}

#[test]
fn typed_round_trips() {
    let cases = [
        (
            Command::Channel {
                n: 1,
                patch: json!({"fader": -10.0, "mute": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            json!({"n": 1, "fader": -10.0, "mute": true}),
        ),
        (
            Command::Bus {
                bus: BusRef::Named(BusName::Main),
                patch: one("fader", json!(-3.0)),
            },
            json!({"bus": "main", "fader": -3.0}),
        ),
        (
            Command::Bus {
                bus: BusRef::Aux(2),
                patch: one("mute", json!(true)),
            },
            json!({"bus": 2, "mute": true}),
        ),
        (Command::ClearSolo {}, json!({})),
        (Command::Lock { on: Some(false) }, json!({"on": false})),
        (
            Command::RecordStart {
                channels: Some(vec![1, 2]),
                name: Some("Sunday".into()),
            },
            json!({"channels": [1, 2], "name": "Sunday"}),
        ),
        (
            Command::SoundcheckTransport(Transport::Locate {
                seconds: Some(12.5),
            }),
            json!({"action": "locate", "seconds": 12.5}),
        ),
        (
            Command::SnapshotRecall {
                name: "Sermon".into(),
                scope: Some(SnapshotScope {
                    channels: Some(ScopeChannels::List(vec![1, 3])),
                    buses: Some(false),
                    outputs: None,
                    parts: None,
                }),
                fade_s: None,
            },
            json!({"name": "Sermon", "scope": {"channels": [1, 3], "buses": false}}),
        ),
        (
            Command::Insert {
                strip: Strip::Bus(BusRef::Named(BusName::Main)),
                op: InsertOp::Move { index: 0, to: 2 },
            },
            json!({"bus": "main", "op": "move", "index": 0, "to": 2}),
        ),
        (
            Command::PluginSet {
                strip: Strip::Channel(4),
                index: 1,
                param: 7,
                value: 0.5,
            },
            json!({"channel": 4, "index": 1, "param": 7, "value": 0.5}),
        ),
        (
            Command::Desk {
                solo_afl: None,
                auto_park: None,
                automix: Some(true),
                duckers: None,
                afv: None,
                block: Some(64),
                rate: Some(Rate::Hz(96000)),
                output_delay: Some(vec![None, Some(5.0)]),
                x32: None,
            },
            json!({"automix": true, "block": 64, "rate": 96000, "output_delay": [null, 5.0]}),
        ),
        (
            Command::Route {
                receiver: "Mixer".into(),
                changes: vec![RouteChange {
                    ch: 0,
                    tx: Some("B (Mic)".into()),
                    tx_ch: Some(1),
                }],
            },
            json!({"receiver": "Mixer", "changes": [{"ch": 0, "tx": "B (Mic)", "tx_ch": 1}]}),
        ),
    ];
    for (c, body) in cases {
        assert_eq!(c.body(), body, "{c:?}");
        let back: Command = serde_json::from_value(json!({"cmd": c.name(), "body": body})).unwrap();
        assert_eq!(back, c);
        // name() agrees with serde's tag.
        assert_eq!(serde_json::to_value(&c).unwrap()["cmd"], c.name());
    }
}

#[test]
fn typed_commands_run_on_a_console() {
    let c = Console::start(crate::engine::ConsoleConfig {
        name: "Typed test".into(),
        channels: 8,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    c.run(Command::Channel {
        n: 2,
        patch: one("name", json!("Pastor")),
    })
    .unwrap();
    c.set_fader(2, -10.0).unwrap();
    c.set_mute(3, true).unwrap();
    c.set_bus_fader(BusRef::Named(BusName::Main), -3.0).unwrap();
    let s = c.state();
    let ch = &s["settings"]["channels"];
    assert_eq!(ch[1]["name"], "Pastor");
    assert_eq!(ch[1]["fader"], -10.0);
    assert_eq!(ch[2]["mute"], true);
    assert_eq!(s["settings"]["main"]["fader"], -3.0);
    assert!(
        c.run(Command::Channel {
            n: 99,
            patch: Map::new()
        })
        .is_err()
    );
    assert!(schema()["$schema"].is_string());
}
