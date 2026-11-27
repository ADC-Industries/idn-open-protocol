use super::*;

/// Manual parking (the default): channels are placed once, then stay; automatic parking
/// hands them back to routing.
#[test]
fn channels_park_by_hand_unless_set_to_automatic() {
    let c = Console::start(ConsoleConfig {
        name: "Park test".into(),
        channels: 4,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let shown = |c: &Console| {
        c.settings()
            .channels
            .iter()
            .map(|ch| ch.shown)
            .collect::<Vec<_>>()
    };
    let wait = |c: &Console, want: Vec<Option<bool>>| {
        let end = Instant::now() + Duration::from_secs(3);
        while shown(c) != want && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(shown(c), want);
    };
    // No Node: every input counts as routed, so all four go on the desk.
    wait(&c, vec![Some(true); 4]);
    c.channel(2, &json!({"shown": false})).unwrap();
    c.desk(&json!({"auto_park": true})).unwrap();
    assert_eq!(shown(&c), vec![None; 4], "automatic: routing decides");
    c.desk(&json!({"auto_park": false})).unwrap();
    wait(&c, vec![Some(true); 4]);
}

/// In-process audio feed: desk inputs are 1-based and bounds-checked, and the same name
/// reuses its lane.
#[test]
fn feed_puts_audio_on_the_desk_inputs() {
    let c = Console::start(ConsoleConfig {
        name: "Feed test".into(),
        channels: 4,
        inputs: 8,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let block = vec![vec![0.2f32; 96], vec![0.2f32; 96]];
    // Desk inputs are 1-based.
    assert!(c.feed("prog", 0, 48_000, &block).is_err());
    // Beyond the desk's inputs is refused (8 inputs; from 8, two channels reach 9).
    assert!(c.feed("prog", 8, 48_000, &block).is_err());
    // A valid feed is accepted, and the same name reuses its lane.
    c.feed("prog", 1, 48_000, &block).unwrap();
    c.feed("prog", 1, 48_000, &block).unwrap();
    c.feed_stop("prog");
}

/// A recalled snapshot leaves parking as the desk has it, and doesn't count it as a
/// change.
#[test]
fn a_recalled_snapshot_keeps_parking() {
    let c = Console::start(ConsoleConfig {
        name: "Snapshot park test".into(),
        channels: 4,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    c.channel(2, &json!({"shown": false})).unwrap();
    c.snapshot_save("Manual").unwrap();
    c.desk(&json!({"auto_park": true})).unwrap();
    c.snapshot_recall("Manual").unwrap();
    let s = c.settings();
    assert!(s.auto_park);
    assert!(s.channels.iter().all(|ch| ch.shown.is_none()));
    assert_eq!(
        c.state()["snapshot"],
        json!({"name": "Manual", "changed": false})
    );
    c.channel(1, &json!({"fader": -5.0})).unwrap();
    assert_eq!(c.state()["snapshot"]["changed"], json!(true));
}

/// Audio follows video (§M.12): the mixer hears a switcher's tally messages (a stand-in
/// here) over a metadata-only connection; a channel following the input on air stays
/// up, the other goes off; cut, and they swap; the switcher gone, both come up.
#[test]
fn audio_follows_the_switchers_tally() {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(idn_open_protocol::link::LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let sw = Arc::new(Output::new(&p, OutputConfig::new("Fake switcher")).unwrap());
    let on = Arc::new(Mutex::new("Cam 1".to_owned()));
    let stop = Arc::new(AtomicBool::new(false));
    let sender = {
        let (sw, on, stop) = (sw.clone(), on.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let now = on.lock_ok().clone();
                let _ = sw.send_metadata(&format!(
                        "<idn_tally program=\"{now}\" preview=\"\" on_air=\"{now}\" inputs=\"Cam 1&#10;Cam 2\"/>"
                    ));
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let c = Console::start(ConsoleConfig {
        name: "AFV test".into(),
        channels: 4,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    c.channel(1, &json!({"follows": "Cam 1"})).unwrap();
    c.channel(2, &json!({"follows": "Cam 2"})).unwrap();
    c.desk(
        &json!({"afv": {"on": true, "switcher": format!("127.0.0.1:{}", sw.port()), "fade": 50}}),
    )
    .unwrap();
    let auto = |c: &Console| {
        let m = c.meters();
        (
            m["auto"][0].as_f64().unwrap(),
            m["auto"][1].as_f64().unwrap(),
        )
    };
    let wait = |c: &Console, want: (f64, f64), what: &str| {
        let end = Instant::now() + Duration::from_secs(10);
        while auto(c) != want && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(auto(c), want, "{what}");
    };
    wait(&c, (0.0, -144.0), "Cam 1 on air");
    let st = c.state();
    assert_eq!(st["afv"]["connected"], true);
    assert_eq!(st["afv"]["inputs"], json!(["Cam 1", "Cam 2"]));
    *on.lock_ok() = "Cam 2".into();
    wait(&c, (-144.0, 0.0), "cut to Cam 2");
    // The switcher goes quiet: after 6 s both come back up.
    stop.store(true, Ordering::Relaxed);
    sender.join().unwrap();
    wait(&c, (0.0, 0.0), "switcher gone");
    assert_eq!(c.state()["afv"]["connected"], false);
    assert_eq!(
        parse_tally("<idn_tally program=\"A &amp; B\" on_air=\"A &amp; B&#10;C\" inputs=\"\"/>"),
        Some(("A & B".into(), vec!["A & B".into(), "C".into()], vec![]))
    );
    assert!(parse_tally("<ntk_ptz_zoom zoom=\"1\"/>").is_none());
}

/// Multitrack recording and virtual soundcheck (§M.13): two channels recorded from
/// their inputs; the inputs gone, the recording played in their place shows on the
/// channels again; a channel kept live stays silent; back to live.
#[test]
fn record_then_soundcheck() {
    let root = idn_common::testing::TempDir::new("mixer-rec");
    let c = Arc::new(
        Console::start(ConsoleConfig {
            name: "Record test".into(),
            channels: 4,
            inputs: 4,
            mdns: Some(false),
            recordings: Some(root.path().to_owned()),
            ..Default::default()
        })
        .unwrap(),
    );
    c.channel(1, &json!({"name": "Pastor"})).unwrap();
    let tone: Vec<f32> = (0..480).map(|k| 0.3 * (k as f32 * 0.13).sin()).collect();
    let feeding = Arc::new(AtomicBool::new(true));
    let feeder = {
        let (con, on) = (c.clone(), feeding.clone());
        std::thread::spawn(move || {
            while on.load(Ordering::Relaxed) {
                let _ = con.feed("test", 1, 48_000, &[tone.clone(), tone.clone()]);
                std::thread::sleep(Duration::from_millis(10));
            }
            con.feed_stop("test");
        })
    };
    let peak_in = |c: &Console, ch: usize| c.meters()["ch"][ch][0].as_f64().unwrap();
    let wait = |c: &Console, what: &str, f: &dyn Fn(&Console) -> bool| {
        let end = Instant::now() + Duration::from_secs(5);
        while !f(c) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(f(c), "{what}: {}", c.meters()["ch"]);
    };
    wait(&c, "the feed on channels 1 and 2", &|c| {
        peak_in(c, 0) > -20.0 && peak_in(c, 1) > -20.0
    });
    let folder = c.record_start(Some(vec![1, 2]), Some("Test take")).unwrap();
    assert_eq!(folder, "Test take");
    assert!(c.record_start(None, None).is_err(), "one at a time");
    std::thread::sleep(Duration::from_millis(800));
    let st = c.record_stop().unwrap();
    assert!(st["seconds"].as_f64().unwrap() > 0.6, "{st}");
    assert!(root.join("Test take").join("01 Pastor.wav").exists());
    assert_eq!(c.recordings()[0]["name"], "Test take");
    feeding.store(false, Ordering::Relaxed);
    feeder.join().unwrap();
    wait(&c, "the inputs quiet", &|c| peak_in(c, 0) < -60.0);
    // The recording in their place.
    c.soundcheck(Some("Test take")).unwrap();
    c.soundcheck_transport(crate::record::Transport::Loop(true))
        .unwrap();
    c.soundcheck_transport(crate::record::Transport::Play)
        .unwrap();
    wait(&c, "played back", &|c| {
        peak_in(c, 0) > -20.0 && peak_in(c, 1) > -20.0
    });
    assert_eq!(c.state()["soundcheck"]["playing"], true);
    // Channel 2 kept live: its (quiet) input again.
    c.soundcheck_live(2, true).unwrap();
    wait(&c, "channel 2 live", &|c| {
        peak_in(c, 1) < -60.0 && peak_in(c, 0) > -20.0
    });
    assert_eq!(c.state()["soundcheck"]["live"][1], true);
    c.soundcheck(None).unwrap();
    wait(&c, "back to live", &|c| peak_in(c, 0) < -60.0);
    assert!(c.soundcheck(Some("../etc")).is_err());
}

/// Recall safes and scope (§M.15): a safe channel and a safe bus keep their settings
/// through a recall; a scoped recall touches only what it names.
#[test]
fn recall_safes_and_scope() {
    let c = Console::start(ConsoleConfig {
        name: "Safe test".into(),
        channels: 4,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    c.channel(1, &json!({"fader": -10.0})).unwrap();
    c.channel(2, &json!({"fader": -10.0})).unwrap();
    c.bus("main", &json!({"fader": -5.0})).unwrap();
    c.snapshot_save("Quiet").unwrap();
    c.channel(1, &json!({"fader": 0.0, "safe": true})).unwrap();
    c.channel(2, &json!({"fader": 0.0})).unwrap();
    c.bus("main", &json!({"fader": 0.0, "safe": true})).unwrap();
    c.snapshot_recall("Quiet").unwrap();
    let s = c.settings();
    assert_eq!(s.channels[0].fader, 0.0, "safe: kept");
    assert!(s.channels[0].safe, "the safe stays the desk's");
    assert_eq!(s.channels[1].fader, -10.0, "recalled");
    assert_eq!(s.main.fader, 0.0, "a safe bus: kept");
    // Scoped: channel 3 only.
    c.channel(1, &json!({"safe": false})).unwrap();
    c.channel(3, &json!({"fader": -20.0})).unwrap();
    c.channel(2, &json!({"fader": 3.0})).unwrap();
    c.snapshot_recall_scoped(
        "Quiet",
        &SnapshotRecallScope {
            channels: Some(vec![3]),
            buses: false,
            outputs: false,
            ..Default::default()
        },
    )
    .unwrap();
    let s = c.settings();
    assert_eq!(
        (
            s.channels[2].fader,
            s.channels[1].fader,
            s.channels[0].fader
        ),
        (0.0, 3.0, 0.0)
    );
}

#[test]
fn merge_patches_settings() {
    let c = Channel::default();
    let p = patched(
        &c,
        &json!({"fader": -6.0, "eq": [null, {"gain": 3.0}], "comp": {"on": true}}),
    )
    .unwrap();
    assert_eq!(p.fader, -6.0);
    assert_eq!(
        (p.eq[0].gain, p.eq[1].gain, p.eq[1].freq),
        (0.0, 3.0, c.eq[1].freq)
    );
    assert!(p.comp.on);
    assert_eq!(p.comp.ratio, c.comp.ratio, "the rest stays");
    assert!(patched(&c, &json!({"fader": "loud"})).is_err());
}

/// The block size: asked for at the start, chosen in Setup for the
/// next start (saved with the desk), and refused when not 32, 64, 128 or 256.
#[test]
fn the_block_size_is_kept_with_the_desk() {
    let dir = idn_common::testing::TempDir::new("mixer-block");
    let cfg = |block: Option<usize>| ConsoleConfig {
        name: "Block test".into(),
        channels: 4,
        mdns: Some(false),
        state_dir: Some(dir.path().to_owned()),
        block,
        ..Default::default()
    };
    let c = Console::start(cfg(Some(64))).unwrap();
    assert_eq!(c.state()["block"], 64);
    assert_eq!(c.settings().block, 64);
    // Setup's choice: the settings now, the desk from its next start.
    c.desk(&json!({"block": 256})).unwrap();
    assert_eq!(
        (c.state()["block"].as_u64(), c.settings().block),
        (Some(64), 256)
    );
    assert!(c.desk(&json!({"block": 100})).is_err());
    assert!(c.desk(&json!({"block": 96})).is_err());
    drop(c);
    let c = Console::start(cfg(None)).unwrap();
    assert_eq!(c.state()["block"], 256, "the saved choice");
    drop(c);
}

fn small(name: &str) -> Console {
    Console::start(ConsoleConfig {
        name: name.into(),
        channels: 8,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap()
}

/// `hpf_on` / `lpf_on`: a filter switched on at its last frequency (80 Hz / 12 kHz before
/// any), off keeping it; `hpf` itself still means what it did (0: off).
#[test]
fn filters_switch_at_their_last_frequency() {
    let c = small("Filter test");
    let ch = |c: &Console| c.settings().channels[0].clone();
    assert_eq!((ch(&c).hpf, ch(&c).hpf_last), (0.0, 80.0));
    c.channel(1, &json!({"hpf_on": true, "lpf_on": true}))
        .unwrap();
    assert_eq!((ch(&c).hpf, ch(&c).lpf), (80.0, 12_000.0));
    c.channel(1, &json!({"hpf": 120.0})).unwrap();
    c.channel(1, &json!({"hpf_on": false})).unwrap();
    assert_eq!((ch(&c).hpf, ch(&c).hpf_last), (0.0, 120.0));
    // On again: where it was; already on, it stays where it is.
    c.channel(1, &json!({"hpf_on": true})).unwrap();
    assert_eq!(ch(&c).hpf, 120.0);
    c.channel(1, &json!({"hpf_on": true, "hpf": 200.0}))
        .unwrap();
    assert_eq!(ch(&c).hpf, 200.0);
    c.channel(1, &json!({"hpf": 0.0})).unwrap();
    assert_eq!((ch(&c).hpf, ch(&c).hpf_last), (0.0, 200.0));
    assert!(c.channel(1, &json!({"hpf_on": "yes"})).is_err());
    // Off with a frequency: the one it comes back on at (bug run O L3).
    c.channel(1, &json!({"hpf_on": false, "hpf": 120.0}))
        .unwrap();
    assert_eq!((ch(&c).hpf, ch(&c).hpf_last), (0.0, 120.0));
    // On with 0: at its last; already on, it stays on where it is.
    c.channel(1, &json!({"hpf_on": true, "hpf": 0.0})).unwrap();
    assert_eq!(ch(&c).hpf, 120.0);
    c.channel(1, &json!({"hpf_on": true, "hpf": 0.0})).unwrap();
    assert_eq!(ch(&c).hpf, 120.0);
    // Not a number: refused, nothing changed.
    assert!(
        c.channel(1, &json!({"hpf_on": true, "hpf": "low"}))
            .is_err()
    );
    assert!(c.channel(1, &json!({"lpf_on": false, "lpf": "x"})).is_err());
    assert_eq!(ch(&c).hpf, 120.0);
    c.channel(1, &json!({"lpf_on": false, "lpf": 9000.0}))
        .unwrap();
    c.channel(1, &json!({"lpf_on": true})).unwrap();
    assert_eq!(ch(&c).lpf, 9000.0);
    // The state shows the frequency kept.
    assert_eq!(c.state()["settings"]["channels"][0]["hpf_last"], 120.0);
}

/// A linked pair's second channel: its name and inputs are its own; the rest of a patch
/// to it goes to the pair's first, whose settings the DSP plays the pair with (writes
/// to the second were ignored by the sound).
#[test]
fn a_linked_pairs_second_channel_patches_its_first() {
    let c = small("Pair test");
    c.channel(3, &json!({"link": true})).unwrap();
    c.channel(4, &json!({"name": "Keys R", "source": 6, "fader": -10.0, "eq": [{"gain": 3.0}], "hpf_on": true}))
        .unwrap();
    let s = c.settings();
    let (first, second) = (&s.channels[2], &s.channels[3]);
    assert_eq!((second.name.as_str(), second.source), ("Keys R", Some(6)));
    assert_eq!((first.name.as_str(), first.source), ("Ch 3", None));
    assert_eq!(
        (first.fader, first.eq[0].gain, first.hpf),
        (-10.0, 3.0, 80.0)
    );
    // The second is given the pair's settings (what every reader of it sees).
    assert_eq!(
        (second.fader, second.eq[0].gain, second.hpf, second.link),
        (-10.0, 3.0, 80.0, true)
    );
    // An input patched to the second puts the pair on the desk.
    assert_eq!((first.shown, second.shown), (Some(true), Some(true)));
    // The pair's mute from a user key on either side turns over, and back.
    c.button(1, &json!({"action": "mute:4"})).unwrap();
    c.press(1).unwrap();
    assert!(c.settings().channels[2].mute);
    c.press(1).unwrap();
    assert!(!c.settings().channels[2].mute);
    // Unlinked from the second: the pair's link is its first's; the second keeps the
    // pair's levels and processing (not what it had before the link), its name and input.
    c.channel(4, &json!({"link": false})).unwrap();
    let s = c.settings();
    assert!(!s.channels[2].link && !s.channels[3].link);
    assert_eq!(
        (
            s.channels[3].fader,
            s.channels[3].eq[0].gain,
            s.channels[3].hpf
        ),
        (-10.0, 3.0, 80.0)
    );
    assert_eq!(
        (s.channels[3].name.as_str(), s.channels[3].source),
        ("Keys R", Some(6))
    );
    // Unlinked, each side is its own.
    c.channel(4, &json!({"fader": -20.0})).unwrap();
    let s = c.settings();
    assert_eq!((s.channels[2].fader, s.channels[3].fader), (-10.0, -20.0));
    // Only an odd-numbered channel starts a pair (bug run O M6).
    let e = c.channel(4, &json!({"link": true})).unwrap_err();
    assert!(e.contains("odd"), "{e}");
    assert!(!c.settings().channels[3].link);
}

/// What reads a linked pair's second reads the pair (bug run O M1–M4): a surface's
/// mute toggles and lights, recall safe set from either side, presets and copies, a
/// scoped recall naming the second.
#[test]
fn a_linked_pairs_second_reads_as_the_pair() {
    use crate::surface::{Action, Mcu};
    let c = small("Pair readers test");
    c.channel(1, &json!({"link": true, "mute": true, "fader": -6.0}))
        .unwrap();
    // The surface: strip 2's mute light on, and its button unmutes the pair.
    let mut m = Mcu::default();
    let out = m.outgoing(&c.settings());
    assert!(
        out.contains(&vec![0x90, 0x11, 0x7F]),
        "strip 2's mute light"
    );
    let a = m.incoming(&[0x90, 0x11, 0x7F], &c.settings());
    assert_eq!(a, Some(Action::Channel(2, json!({"mute": false}))));
    c.channel(2, &json!({"mute": false})).unwrap();
    assert!(!c.settings().channels[0].mute);
    // Recall safe from the second: both sides safe; a recall leaves the pair.
    c.snapshot_save("Before").unwrap();
    c.channel(2, &json!({"safe": true, "fader": -20.0}))
        .unwrap();
    let s = c.settings();
    assert!(s.channels[0].safe && s.channels[1].safe);
    c.snapshot_recall("Before").unwrap();
    let s = c.settings();
    assert_eq!((s.channels[0].fader, s.channels[1].fader), (-20.0, -20.0));
    assert_eq!(c.state()["snapshot"]["changed"], json!(true));
    // Not safe (from the first), a recall scoped to the second recalls the pair.
    c.channel(1, &json!({"safe": false})).unwrap();
    c.snapshot_recall_scoped(
        "Before",
        &SnapshotRecallScope {
            channels: Some(vec![2]),
            buses: false,
            outputs: false,
            ..Default::default()
        },
    )
    .unwrap();
    let s = c.settings();
    assert_eq!((s.channels[0].fader, s.channels[1].fader), (-6.0, -6.0));
    // A preset loaded on, and a copy to, the second go on the pair.
    c.channel(5, &json!({"hpf": 150.0, "comp": {"on": true}}))
        .unwrap();
    c.preset_save("Bright", 5).unwrap();
    c.preset_load("Bright", 2).unwrap();
    let s = c.settings();
    assert_eq!((s.channels[0].hpf, s.channels[1].hpf), (150.0, 150.0));
    c.channel(6, &json!({"delay": 12.0})).unwrap();
    c.copy_channel(6, 2).unwrap();
    let s = c.settings();
    assert_eq!((s.channels[0].delay, s.channels[1].delay), (12.0, 12.0));
    // A DCA joined from the second: both sides members.
    c.channel(2, &json!({"dca": [true]})).unwrap();
    assert_eq!(
        c.state()["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["kind"] == "dca" && x["index"] == 1)
            .unwrap()["members"],
        json!([1, 2])
    );
}

/// A snapshot saved before the filters' last frequencies (hpf_last, lpf_last) were kept
/// isn't "changed" right after its recall (bug run O M5): snapshots are made what the
/// desk's settings are made on loading.
#[test]
fn an_older_snapshot_is_unchanged_after_its_recall() {
    let dir = idn_common::testing::TempDir::new("mixer-oldsnapshot");
    let mut old = serde_json::to_value(Settings::new(4)).unwrap();
    old["channels"][0]["hpf"] = json!(120.0);
    old["channels"][1]["lpf"] = json!(9000.0);
    for ch in old["channels"].as_array_mut().unwrap() {
        let o = ch.as_object_mut().unwrap();
        o.remove("hpf_last");
        o.remove("lpf_last");
    }
    std::fs::write(
        dir.join("snapshots.json"),
        json!({ "Old": old }).to_string(),
    )
    .unwrap();
    let c = Console::start(ConsoleConfig {
        name: "Old snapshot test".into(),
        channels: 4,
        mdns: Some(false),
        state_dir: Some(dir.path().to_owned()),
        ..Default::default()
    })
    .unwrap();
    c.snapshot_recall("Old").unwrap();
    assert_eq!(c.settings().channels[0].hpf_last, 120.0);
    assert_eq!(
        c.state()["snapshot"],
        json!({"name": "Old", "changed": false})
    );
    drop(c);
}

/// The desk's saved states are snapshots (§M.4): the scene command names are refused,
/// no aliases.
#[test]
fn scene_commands_are_refused() {
    let c = small("Scene names test");
    for cmd in ["scene_save", "scene_recall", "scene_delete"] {
        assert!(c.command(cmd, &json!({"name": "Sermon"})).is_err(), "{cmd}");
    }
}

/// A damaged saved file of the desk's (its channel count for the next start, the
/// snapshots' order) is set aside as `NAME.bad-SECS-PID-N`, not read as none and saved over.
#[test]
fn a_damaged_state_file_is_set_aside() {
    let dir = idn_common::testing::TempDir::new("mixer-damaged");
    std::fs::write(dir.join(SHAPE_FILE), "{\"channels\": 2").unwrap();
    std::fs::write(dir.join(SNAPSHOT_ORDER_FILE), "[\"A\"").unwrap();
    assert_eq!(super::saved_channel_count(&dir), None);
    let c = Console::start(ConsoleConfig {
        name: "Damaged state test".into(),
        channels: 4,
        mdns: Some(false),
        state_dir: Some(dir.path().to_owned()),
        ..Default::default()
    })
    .unwrap();
    drop(c);
    let aside = |file: &str| {
        std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{file}.bad-"))
        })
    };
    for file in [SHAPE_FILE, SNAPSHOT_ORDER_FILE] {
        assert!(aside(file), "{file} set aside");
    }
}

/// `channels`: one patch on several channels, all or nothing (a panel's "clear page").
#[test]
fn channels_patches_several_at_once() {
    let c = small("Bulk test");
    let clear = json!({"main": false, "sends": [{"on": true}, {"on": true}], "groups": [true]});
    c.command("channels", &json!({"n": [1, 2, 5], "main": false, "sends": [{"on": true}, {"on": true}], "groups": [true]}))
        .unwrap();
    let s = c.settings();
    for i in [0, 1, 4] {
        let ch = &s.channels[i];
        assert!(
            !ch.main && ch.sends[0].on && ch.sends[1].on && ch.groups[0],
            "{i}"
        );
    }
    assert!(s.channels[2].main && !s.channels[2].sends[0].on);
    // One channel out of range: nothing changes.
    let mut p = clear.clone();
    p["n"] = json!([3, 99]);
    assert!(c.command("channels", &p).is_err());
    assert!(c.settings().channels[2].main);
    assert!(
        c.command("channels", &json!({"n": [], "mute": true}))
            .is_err()
    );
    assert!(
        c.command("channels", &json!({"n": 3, "mute": true}))
            .is_err()
    );
    // Typed.
    c.run(crate::Command::Channels {
        n: vec![7, 8],
        patch: json!({"mute": true}).as_object().unwrap().clone(),
    })
    .unwrap();
    let s = c.settings();
    assert!(s.channels[6].mute && s.channels[7].mute && !s.channels[5].mute);
}

/// Named output endpoints: each its own IDN output, in the state with
/// its source name; renamed, given other outputs, removed; bad ones refused.
#[test]
fn output_endpoints_are_their_own_outputs() {
    let c = Console::start(ConsoleConfig {
        name: "Endpoint test".into(),
        channels: 4,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let id = c
        .endpoint_add(&json!({"name": "PA", "outputs": [1, 2]}))
        .unwrap();
    assert_eq!(id, "e1");
    let st = c.state();
    let e = &st["endpoints"][0];
    assert_eq!(
        (e["id"].as_str(), e["name"].as_str()),
        (Some("e1"), Some("PA"))
    );
    assert!(
        e["source"].as_str().unwrap().contains("Endpoint test PA"),
        "{e}"
    );
    assert_eq!(e["channels"].as_array().unwrap().len(), 2);
    assert_eq!(st["settings"]["endpoints"][0]["outputs"], json!([1, 2]));
    // Refused: the same name, no outputs, an output past the desk's.
    assert!(
        c.endpoint_add(&json!({"name": "PA", "outputs": [3]}))
            .is_err()
    );
    assert!(
        c.endpoint_add(&json!({"name": "Lobby", "outputs": []}))
            .is_err()
    );
    assert!(
        c.endpoint_add(&json!({"name": "Lobby", "outputs": [999]}))
            .is_err()
    );
    // Renamed and given other outputs: its output made again under the new name.
    c.endpoint(&json!({"id": "e1", "name": "Main PA", "outputs": [3, 4, 5]}))
        .unwrap();
    let e = c.state()["endpoints"][0].clone();
    assert!(e["source"].as_str().unwrap().contains("Main PA"), "{e}");
    assert_eq!(e["channels"].as_array().unwrap().len(), 3);
    // The typed command answers the id as the API does.
    let who = idn_open_protocol::web::Who::host("test");
    let a = c
        .command_answer_as(
            &who,
            "endpoint_add",
            &json!({"name": "Lobby", "outputs": [6]}),
        )
        .unwrap();
    assert_eq!(a["id"], "e2");
    c.endpoint_remove(&json!({"id": "e1"})).unwrap();
    let st = c.state();
    assert_eq!(st["endpoints"].as_array().unwrap().len(), 1);
    assert!(c.endpoint_remove(&json!({"id": "e1"})).is_err());
}

/// The clock chosen by hand: kept, shown, refused when unknown; the
/// desk follows it when it is there (here its own clock: no card, no site).
#[test]
fn the_clock_can_be_chosen_by_hand() {
    let c = Console::start(ConsoleConfig {
        name: "Clock test".into(),
        channels: 2,
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(c.state()["clock"]["chosen"], "auto");
    c.desk(&json!({"clock": "own"})).unwrap();
    assert_eq!(c.state()["clock"]["chosen"], "own");
    assert!(c.desk(&json!({"clock": "atomic"})).is_err());
    assert_eq!(c.settings().clock, "own");
}

/// `channel_reset` (§M.28): the channel as on a fresh desk, its name, colour and input kept.
#[test]
fn a_channel_resets_keeping_what_it_is() {
    let c = small("reset");
    c.command(
        "channel",
        &serde_json::json!({"n": 3, "name": "Kick", "color": "#ff0000", "source": 5,
            "fader": -10.0, "gate": {"on": true, "mode": "ducker", "key": 1},
            "comp": {"on": true, "mix": 40.0}}),
    )
    .unwrap();
    c.command("channel_reset", &serde_json::json!({"n": 3}))
        .unwrap();
    let s = c.settings();
    let ch = &s.channels[2];
    assert_eq!(
        (ch.name.as_str(), ch.color.as_str(), ch.source),
        ("Kick", "#ff0000", Some(5))
    );
    assert!(!ch.gate.on && ch.gate.key == 0 && !ch.comp.on && ch.comp.mix == 100.0);
    assert_eq!(ch.fader, s.channels[3].fader, "its fader as a fresh one's");
    // DCA and mute-group colours: kept when #rrggbb, dropped otherwise.
    c.command("dca", &serde_json::json!({"dca": 2, "color": "#00ff00"}))
        .unwrap();
    c.command(
        "mute_group",
        &serde_json::json!({"group": 1, "color": "green"}),
    )
    .unwrap();
    let s = c.settings();
    assert_eq!(s.dca[1].color, "#00ff00");
    assert_eq!(s.mute_groups[0].color, "");
}

/// Recall parts, fade and order (§M.28): only the levels taken, the rest as the desk
/// has it; a fade out of range refused; the snapshots listed in the order set.
#[test]
fn recall_parts_fade_and_order() {
    let c = small("parts");
    let ch = |p: serde_json::Value| {
        let mut p = p;
        p["n"] = json!(1);
        c.command("channel", &p).unwrap();
    };
    ch(json!({"fader": -10.0, "gate": {"on": true}, "name": "Pastor"}));
    c.command("snapshot_save", &json!({"name": "A"})).unwrap();
    ch(json!({"fader": 0.0, "gate": {"on": false}, "name": "Band"}));
    c.command(
        "snapshot_recall",
        &json!({"name": "A", "scope": {"parts": ["levels"]}, "fade_s": 1.5}),
    )
    .unwrap();
    let s = c.settings();
    assert_eq!(s.channels[0].fader, -10.0, "the level taken");
    assert!(!s.channels[0].gate.on, "the processing left");
    assert_eq!(s.channels[0].name, "Band", "the name left");
    assert!(
        c.command("snapshot_recall", &json!({"name": "A", "fade_s": 20}))
            .is_err()
    );
    assert!(
        c.command(
            "snapshot_recall",
            &json!({"name": "A", "scope": {"parts": ["colour"]}})
        )
        .is_err()
    );
    for n in ["B", "C"] {
        c.command("snapshot_save", &json!({"name": n})).unwrap();
    }
    c.command("snapshot_order", &json!({"names": ["C", "nope", "A"]}))
        .unwrap();
    assert_eq!(c.state()["snapshots"], json!(["C", "A", "B"]));
}

/// `channel_count` and `restart` (§M.28): the count kept for the next start and shown
/// until then; a restart refused for a desk the IDN Node didn't start.
#[test]
fn the_channel_count_is_kept_for_the_next_start() {
    let dir = idn_common::testing::TempDir::new("mixer-count");
    let c = Console::start(ConsoleConfig {
        name: "Count test".into(),
        channels: 8,
        mdns: Some(false),
        state_dir: Some(dir.path().to_owned()),
        ..Default::default()
    })
    .unwrap();
    assert!(c.state()["channels_next"].is_null());
    c.command("channel_count", &json!({"n": 24})).unwrap();
    assert_eq!(c.state()["channels_next"], 24);
    assert!(c.command("channel_count", &json!({"n": 65})).is_err());
    assert_eq!(super::saved_channel_count(&dir), Some(24));
    // Not under the Node's supervisor here.
    let e = c.command("restart", &json!({})).unwrap_err();
    assert!(e.contains("IDN Node"), "{e}");
    assert_eq!(c.state()["restartable"], false);
    drop(c);
}

/// The calculated round trip in the state (§M.28.2): a block, the configured latency,
/// output delays; no measurement yet.
#[test]
fn the_state_adds_up_the_desks_delays() {
    let c = small("latency");
    c.command("desk", &json!({"output_delay": [0.0, 12.5]}))
        .ok();
    let l = c.state()["latency"].clone();
    assert!(l["block_ms"].as_f64().unwrap() > 0.0);
    assert_eq!(l["input_ms"], 10.0);
    assert!(l["measured"].is_null());
    let total = l["total_ms"].as_f64().unwrap();
    let sum: f64 = [
        "input_ms",
        "block_ms",
        "plugins_ms",
        "denoise_ms",
        "output_delay_ms",
    ]
    .iter()
    .map(|k| l[*k].as_f64().unwrap())
    .sum();
    assert!((total - sum).abs() < 0.02, "{l}");
}

/// The typed state (refactor 4): the state a desk gives has exactly the keys its schema
/// declares (none undeclared, none missing).
#[test]
fn the_state_has_its_schemas_keys() {
    let c = small("schema");
    let st = c.state();
    let have: std::collections::BTreeSet<String> =
        st.as_object().unwrap().keys().cloned().collect();
    let schema = super::state::state_schema();
    let declared: std::collections::BTreeSet<String> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(have, declared);
}
