//! The desk's inserts through the real plugin host and the test CLAP plugin
//! (idn-plugin-host's `examples/test_gain.rs`: a gain, 0.5 by default): built from the
//! settings, a parameter, its state read back, the host killed and started again with
//! that state, bypass, removal.
//!
//! Needs `cargo build -p idn-plugin-host --bins --examples` first (a workspace test run
//! builds them).

use idn_mixer::desk::{BLOCK, Desk, Insert, OUTPUTS, Settings, channel_chain};
use idn_mixer::plugins::{Plugins, store_states};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_owned()
}

fn setup() -> (PathBuf, String) {
    let dir = target_dir();
    let host = dir.join(if cfg!(windows) {
        "idn-plugin-host.exe"
    } else {
        "idn-plugin-host"
    });
    let lib = if cfg!(windows) {
        "test_gain.dll"
    } else if cfg!(target_os = "macos") {
        "libtest_gain.dylib"
    } else {
        "libtest_gain.so"
    };
    let built = dir.join("examples").join(lib);
    assert!(
        host.exists() && built.exists(),
        "build idn-plugin-host's program and examples first ({})",
        dir.display()
    );
    let plugins = std::env::temp_dir().join(format!("idn-mixer-plugins-{}", std::process::id()));
    std::fs::create_dir_all(&plugins).unwrap();
    let clap = plugins.join("test-gain.clap");
    std::fs::copy(&built, &clap).unwrap();
    // A plugin folder, as a site would add one: the desk loads plugins only from those
    // or its last scan. (Every test sets the same value.)
    // SAFETY: called first in each test, before it starts threads, and the tests run one per
    // process (nextest); std's own readers take its environment lock.
    unsafe { std::env::set_var("CLAP_PATH", &plugins) };
    (
        host,
        format!("clap:{}#com.adc-industries.idn.test-gain", clap.display()),
    )
}

/// Runs the desk on its 2 ms clock until the main bus's left side settles at `want`.
fn settles(desk: &mut Desk, want: f32) -> bool {
    let input = vec![vec![0.5; BLOCK], vec![0.0; BLOCK]];
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    let end = Instant::now() + Duration::from_secs(5);
    let mut good = 0;
    while Instant::now() < end {
        desk.process(&input, &mut out);
        good = if out[0].iter().all(|v| (v - want).abs() < 1e-5) {
            good + 1
        } else {
            0
        };
        if good >= 10 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    eprintln!("main L at {:?}", &out[0][..4]);
    false
}

#[test]
fn inserts_run_in_the_plugin_host_and_survive_it() {
    let (host, key) = setup();
    let plugins = Plugins::new(host);
    let mut s = Settings::new(2);
    s.channels[0].eq_on = false;
    s.channels[0].pan = -1.0;
    s.main.eq_on = false;
    s.channels[0].inserts.push(Insert {
        key,
        name: "IDN Test Gain".into(),
        ..Default::default()
    });
    let mut desk = Desk::new(2);
    desk.set(s.clone(), false);
    let apply = |desk: &mut Desk, s: &Settings| {
        let (io, chains) = plugins.apply(s, false);
        desk.set_plugins(io, chains.clone());
        chains
    };
    assert_eq!(
        apply(&mut desk, &s),
        BTreeMap::from([(channel_chain(0), 0)]),
        "{:?}",
        plugins.errors()
    );
    assert!(settles(&mut desk, 0.25), "the plugin's default gain of 0.5");
    // Its slot's meter: 0.5 in, ×0.5 out: −12 dBFS; read again at once, nothing since.
    let l = plugins.levels(channel_chain(0)).unwrap();
    let db = l["levels"][0].as_f64().unwrap();
    assert!((db + 12.04).abs() < 0.1, "{l}");
    // A parameter, and its state read back into the settings.
    plugins.set(channel_chain(0), 0, 1, 2.0).unwrap();
    assert!(settles(&mut desk, 1.0), "gain 2");
    let states = plugins.refresh(false);
    assert_eq!(states.len(), 1);
    store_states(&mut s, &states);
    assert!(!s.channels[0].inserts[0].state.is_empty());
    desk.set(s.clone(), false);
    // Its own window (§M.10): the test plugin has none, and says so; the sound goes on.
    let e = plugins
        .editor(channel_chain(0), 0, true, None, "Test gain")
        .unwrap_err();
    assert!(e.contains("no window of its own"), "{e}");
    assert!(!plugins.editor_open(channel_chain(0), 0));
    assert!(settles(&mut desk, 1.0), "still gain 2");
    // Turned down in its window and read (not stored yet), then the settings loaded as a
    // snapshot: the snapshot's state (gain 2) goes back in (bug round F, F7-2: the state just
    // read was kept over the recall).
    plugins.set(channel_chain(0), 0, 1, 0.5).unwrap();
    assert!(settles(&mut desk, 0.25), "gain 0.5");
    assert_eq!(plugins.refresh(false).len(), 1);
    let (io, chains) = plugins.apply(&s, true);
    desk.set_plugins(io, chains);
    assert!(settles(&mut desk, 1.0), "the recalled state, gain 2");
    // The host killed: the strip passes on dry, then the host starts again with the
    // plugin as it was.
    let pid = plugins.host_pid().expect("running");
    #[cfg(unix)]
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    #[cfg(windows)]
    std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()
        .unwrap();
    idn_common::testing::wait_for("the host's end noticed", Duration::from_secs(5), || {
        plugins.stopped()
    });
    desk.set_plugins(None, BTreeMap::new());
    assert!(settles(&mut desk, 0.5), "dry while it's down");
    apply(&mut desk, &s);
    assert_ne!(plugins.host_pid(), Some(pid));
    assert!(settles(&mut desk, 1.0), "back, with its state");
    // Bypassed, then removed.
    s.channels[0].inserts[0].bypass = true;
    apply(&mut desk, &s);
    assert!(settles(&mut desk, 0.5), "bypassed");
    s.channels[0].inserts.clear();
    assert!(apply(&mut desk, &s).is_empty());
    assert!(settles(&mut desk, 0.5));
    assert_eq!(desk.plugin_latency(), 0);
}

/// The desk comes back after a restart as it was: its settings and its plugins, with
/// their state (everything but the snapshots was lost).
#[test]
fn the_desk_comes_back_after_a_restart() {
    use idn_mixer::{Console, ConsoleConfig};
    use serde_json::json;
    let (host, key) = setup();
    let dir = idn_common::testing::TempDir::new("mixer-keep");
    let cfg = ConsoleConfig {
        name: "Keep test".into(),
        channels: 4,
        mdns: Some(false),
        state_dir: Some(dir.path().to_owned()),
        plugin_host: Some(host),
        ..Default::default()
    };
    let c = Console::start(cfg.clone()).unwrap();
    c.channel(2, &json!({"fader": -12.0, "name": "Lead vox"}))
        .unwrap();
    c.insert(&json!({"group": 1, "op": "add", "key": key, "name": "IDN Test Gain"}))
        .unwrap();
    let s = c.settings();
    assert_eq!(s.groups[0].inserts.len(), 1);
    drop(c);
    let c = Console::start(cfg).unwrap();
    let s = c.settings();
    assert_eq!(
        (s.channels[1].fader, s.channels[1].name.as_str()),
        (-12.0, "Lead vox")
    );
    assert_eq!(s.groups[0].inserts.len(), 1, "the group's plugin is back");
    assert!(
        c.state()["plugins"]["errors"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{}",
        c.state()["plugins"]
    );
    drop(c);
}

/// A plugin outside the plugin folders and the last scan isn't loaded: an insert's key
/// comes from the panel, and loading a library runs its code.
#[test]
fn a_plugin_outside_the_plugin_folders_is_not_loaded() {
    let (host, key) = setup();
    let elsewhere = idn_common::testing::TempDir::new("mixer-elsewhere");
    let path = key
        .strip_prefix("clap:")
        .and_then(|k| k.rsplit_once('#'))
        .unwrap();
    let moved = elsewhere.join("test-gain.clap");
    std::fs::copy(path.0, &moved).unwrap();
    let plugins = Plugins::new(host);
    let mut s = Settings::new(2);
    s.channels[0].inserts.push(Insert {
        key: format!("clap:{}#{}", moved.display(), path.1),
        name: "IDN Test Gain".into(),
        ..Default::default()
    });
    let (_, chains) = plugins.apply(&s, false);
    assert!(chains.is_empty(), "loaded from outside: {chains:?}");
    let errors = plugins.errors();
    assert!(
        errors.iter().any(|e| e["error"]
            .as_str()
            .is_some_and(|m| m.contains("plugin folders"))),
        "{errors:?}"
    );
}
