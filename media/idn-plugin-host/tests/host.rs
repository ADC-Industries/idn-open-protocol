//! The plugin host end to end with the test CLAP plugin (`examples/test_gain.rs`): scan,
//! a chain, audio through it, a parameter, state.

use idn_plugin_host::Host;
use idn_plugin_host::protocol::ChainAudio;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The test plugin, built with the tests, copied as a `.clap` file.
fn test_plugin() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"));
    let lib = if cfg!(windows) {
        "test_gain.dll"
    } else if cfg!(target_os = "macos") {
        "libtest_gain.dylib"
    } else {
        "libtest_gain.so"
    };
    let built = exe.parent().unwrap().join("examples").join(lib);
    assert!(
        built.exists(),
        "build the examples first: {}",
        built.display()
    );
    // One folder for the process's tests (a static: never removed while they run).
    static DIR: std::sync::OnceLock<idn_common::testing::TempDir> = std::sync::OnceLock::new();
    let to = DIR
        .get_or_init(|| idn_common::testing::TempDir::new("idn-plugins"))
        .join("test-gain.clap");
    std::fs::copy(&built, &to).unwrap();
    to
}

fn block(host: &Host, seq: u64, chain: u32, value: f32) -> ChainAudio {
    host.submit(
        seq,
        vec![ChainAudio {
            chain,
            channels: 2,
            frames: 96,
            data: vec![value; 192],
        }],
    );
    host.take_wait(seq, Duration::from_secs(5))
        .unwrap_or_else(|| panic!("no result for block {seq}"))
        .remove(0)
}

#[test]
fn a_clap_chain_processes_and_keeps_its_state() {
    let plugin = test_plugin();
    let host = Host::spawn(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"))).unwrap();
    let t = Duration::from_secs(10);
    let found = json!({"plugins": idn_plugin_host::scan(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host")), &[plugin.to_string_lossy().into_owned()]).unwrap()});
    let mine = found["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "IDN Test Gain")
        .unwrap_or_else(|| panic!("not found: {found}"))
        .clone();
    assert_eq!(mine["format"], "clap");
    // The audio goes through shared memory (§M.6) unless asked not to.
    assert_eq!(
        host.shared_memory(),
        !idn_common::switches::on("IDN_PLUGINS_NO_SHM")
    );
    assert_eq!(mine["vendor"], "ADC Industries");
    let key = mine["key"].as_str().unwrap().to_owned();
    let r = host
        .request(json!({"cmd": "chain", "chain": 7, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": key}]}), t)
        .unwrap();
    assert_eq!(r["latency"], 0);
    assert_eq!(r["plugins"][0]["name"], "IDN Test Gain");
    assert_eq!(r["plugins"][0]["params"][0]["name"], "Gain");
    assert_eq!(r["plugins"][0]["params"][0]["value"], 0.5);
    assert_eq!(r["plugins"][0]["params"][0]["text"], "0.50x");
    // Audio: the default gain, then a parameter change at the next block.
    let out = block(&host, 1, 7, 1.0);
    assert_eq!((out.channels, out.frames), (2, 96));
    assert!(out.data.iter().all(|&v| v == 0.5), "{:?}", &out.data[..4]);
    host.request(
        json!({"cmd": "set", "chain": 7, "index": 0, "param": 1, "value": 2.0}),
        t,
    )
    .unwrap();
    assert!(block(&host, 2, 7, 1.0).data.iter().all(|&v| v == 2.0));
    // Bypassed: the sound passes as it came; back on, at the gain it had.
    host.request(
        json!({"cmd": "bypass", "chain": 7, "index": 0, "on": true}),
        t,
    )
    .unwrap();
    assert!(block(&host, 4, 7, 1.0).data.iter().all(|&v| v == 1.0));
    host.request(
        json!({"cmd": "bypass", "chain": 7, "index": 0, "on": false}),
        t,
    )
    .unwrap();
    host.submit(
        5,
        vec![ChainAudio {
            chain: 7,
            channels: 2,
            frames: 96,
            data: vec![1.0; 192],
        }],
    );
    let r = host.take_wait(5, Duration::from_secs(5)).expect("block 5");
    assert!(r[0].data.iter().all(|&v| v == 2.0));
    // State: saved, then a new chain from it plays at that gain.
    let st = host
        .request(json!({"cmd": "state", "chain": 7, "index": 0}), t)
        .unwrap();
    let state = st["state"].as_str().unwrap().to_owned();
    host.request(json!({"cmd": "chain", "chain": 8, "channels": 1, "rate": 48000.0, "block": 96, "plugins": [{"key": key, "state": state}]}), t)
        .unwrap();
    host.submit(
        3,
        vec![ChainAudio {
            chain: 8,
            channels: 1,
            frames: 96,
            data: vec![0.25; 96],
        }],
    );
    let out = host
        .take_wait(3, Duration::from_secs(5))
        .expect("the mono block back")
        .remove(0);
    assert_eq!(out.channels, 1, "a mono chain through a stereo plugin");
    assert!(out.data.iter().all(|&v| v == 0.5), "{:?}", &out.data[..4]);
    // A state loaded into a running plugin (a scene recall).
    let half = { idn_common::text::base64(0.5f32.to_le_bytes()) };
    host.request(
        json!({"cmd": "load", "chain": 8, "index": 0, "state": half}),
        t,
    )
    .unwrap();
    host.submit(
        6,
        vec![ChainAudio {
            chain: 8,
            channels: 1,
            frames: 96,
            data: vec![1.0; 96],
        }],
    );
    assert!(
        host.take_wait(6, Duration::from_secs(5)).unwrap()[0]
            .data
            .iter()
            .all(|&v| v == 0.5)
    );
    // A plugin that isn't there: an error, and the host carries on.
    let e = host
        .request(
            json!({"cmd": "chain", "chain": 9, "plugins": [{"key": "clap:/nowhere.clap#x"}]}),
            t,
        )
        .unwrap_err();
    assert!(e.contains("nowhere"), "{e}");
    assert!(host.alive());
    host.request(json!({"cmd": "remove", "chain": 7}), t)
        .unwrap();
}

/// Real plugins, when `IDN_REAL_PLUGINS` names a folder with CLAP or VST3 plugins (the lab
/// unpacks LSP Plugins there): the scan finds them, and a few of each format run a block
/// of a tone, report their parameters, and save their state.
#[test]
#[ignore = "needs IDN_REAL_PLUGINS"]
fn real_plugins() {
    let Some(dir) = idn_common::switches::value("IDN_REAL_PLUGINS").map(std::ffi::OsString::from)
    else {
        return;
    };
    let host = Host::spawn(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"))).unwrap();
    let t = Duration::from_secs(60);
    let found = json!({"plugins": idn_plugin_host::scan(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host")), &[dir.to_string_lossy().into_owned()]).unwrap()});
    let list = found["plugins"].as_array().unwrap();
    for format in ["clap", "vst3"] {
        let of: Vec<_> = list
            .iter()
            .filter(|p| p["format"] == format && p["key"].is_string())
            .collect();
        let errors: Vec<_> = list
            .iter()
            .filter(|p| p["format"] == format && p["error"].is_string())
            .collect();
        eprintln!(
            "{format}: {} plugins, {} files not opened {:?}",
            of.len(),
            errors.len(),
            errors.first()
        );
        for (n, p) in of
            .iter()
            .filter(|p| p["name"].as_str().is_some_and(|n| n.contains("Stereo")))
            .take(5)
            .enumerate()
        {
            let chain = 100 + n as u32 + if format == "vst3" { 50 } else { 0 };
            let r = host.request(json!({"cmd": "chain", "chain": chain, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": p["key"]}]}), t);
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  {}: {e}", p["name"]);
                    continue;
                }
            };
            let tone: Vec<f32> = (0..192)
                .map(|i| 0.3 * ((i % 96) as f32 * 0.2).sin())
                .collect();
            let mut peak = 0f32;
            for seq in 0..20u64 {
                host.submit(
                    seq + 1000 * chain as u64,
                    vec![ChainAudio {
                        chain,
                        channels: 2,
                        frames: 96,
                        data: tone.clone(),
                    }],
                );
                let out = host
                    .take_wait(seq + 1000 * chain as u64, Duration::from_secs(5))
                    .unwrap_or_else(|| panic!("{}: no audio back", p["name"]))
                    .remove(0);
                peak = out.data.iter().fold(peak, |m, v| m.max(v.abs()));
            }
            let st = host
                .request(json!({"cmd": "state", "chain": chain, "index": 0}), t)
                .map(|s| s["state"].as_str().unwrap_or("").len());
            eprintln!(
                "  {}: {} params, latency {}, out peak {peak:.3}, state {:?} chars",
                p["name"],
                r["plugins"][0]["params"].as_array().map_or(0, |a| a.len()),
                r["latency"],
                st
            );
            assert!(host.alive());
        }
    }
}

/// Blocks of a tone through `chain`, paced like the desk (one every 2 ms): the longest
/// turnaround, ms, and how many took over 2 ms.
fn turnaround(host: &Host, chain: u32, seq0: u64, n: u64) -> (f64, usize) {
    let tone: Vec<f32> = (0..192)
        .map(|i| 0.3 * ((i % 96) as f32 * 0.2).sin())
        .collect();
    let (mut worst, mut late) = (0f64, 0);
    for seq in seq0..seq0 + n {
        let t = Instant::now();
        host.submit(
            seq,
            vec![ChainAudio {
                chain,
                channels: 2,
                frames: 96,
                data: tone.clone(),
            }],
        );
        let end = t + Duration::from_secs(5);
        while host.take(seq).is_none() {
            assert!(Instant::now() < end, "no audio back");
            std::thread::sleep(Duration::from_micros(100));
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        worst = worst.max(ms);
        late += (ms > 2.0) as usize;
        if let Some(rest) = Duration::from_millis(2).checked_sub(t.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    (worst, late)
}

/// The screen as a PPM file (the window test's look).
#[cfg(target_os = "linux")]
fn screenshot(path: &str) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};
    let (conn, n) = x11rb::connect(None).unwrap();
    let s = &conn.setup().roots[n];
    let (w, h) = (s.width_in_pixels, s.height_in_pixels);
    let img = conn
        .get_image(ImageFormat::Z_PIXMAP, s.root, 0, 0, w, h, !0)
        .unwrap()
        .reply()
        .unwrap();
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in img.data.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::write(path, out).unwrap();
}

/// A mouse drag on the screen (XTEST): from (x, y) up by `dy` pixels.
#[cfg(target_os = "linux")]
fn drag(x: i16, y: i16, dy: i16) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
    use x11rb::protocol::xtest::ConnectionExt;
    let (conn, n) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[n].root;
    let step = |kind: u8, detail: u8, x: i16, y: i16| {
        conn.xtest_fake_input(kind, detail, 0, root, x, y, 0)
            .unwrap();
        conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(15));
    };
    step(MOTION_NOTIFY_EVENT, 0, x, y);
    step(BUTTON_PRESS_EVENT, 1, x, y);
    for k in 1..=10 {
        step(MOTION_NOTIFY_EVENT, 0, x, y - dy * k / 10);
    }
    step(BUTTON_RELEASE_EVENT, 1, x, y - dy);
    use x11rb::protocol::xproto::ConnectionExt as _;
    conn.get_input_focus().unwrap().reply().unwrap();
}

/// The window manager's close button: WM_DELETE_WINDOW to `window`.
#[cfg(target_os = "linux")]
fn close_window(window: u32) {
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};
    let (conn, _) = x11rb::connect(None).unwrap();
    let atom = |n: &[u8]| conn.intern_atom(false, n).unwrap().reply().unwrap().atom;
    let (protocols, delete) = (atom(b"WM_PROTOCOLS"), atom(b"WM_DELETE_WINDOW"));
    let ev = ClientMessageEvent::new(32, window, protocols, [delete, 0, 0, 0, 0]);
    conn.send_event(false, window, EventMask::NO_EVENT, ev)
        .unwrap();
    // A round trip: the server has handled it before this connection goes.
    conn.get_input_focus().unwrap().reply().unwrap();
}

/// Plugins' own windows (idn-mixer.md §M.10) with real plugins on an X display: each
/// format's compressor shows its window, the sound keeps its pace while it is open, and
/// it closes. `IDN_SHOT=PREFIX` saves a screenshot per format.
#[test]
#[ignore = "needs IDN_REAL_PLUGINS and an X display"]
fn real_plugin_windows() {
    let Some(dir) = idn_common::switches::value("IDN_REAL_PLUGINS").map(std::ffi::OsString::from)
    else {
        return;
    };
    assert!(std::env::var_os("DISPLAY").is_some(), "no DISPLAY");
    let host = Host::spawn(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"))).unwrap();
    let events = host.events();
    let t = Duration::from_secs(60);
    let found = json!({"plugins": idn_plugin_host::scan(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host")), &[dir.to_string_lossy().into_owned()]).unwrap()});
    let list = found["plugins"].as_array().unwrap().clone();
    for (k, format) in ["clap", "vst3"].into_iter().enumerate() {
        let p = list
            .iter()
            .find(|p| p["format"] == format && p["name"] == "Compressor Stereo")
            .expect("LSP's Compressor Stereo");
        let chain = 200 + k as u32;
        host.request(json!({"cmd": "chain", "chain": chain, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": p["key"]}]}), t).unwrap();
        let seq0 = 10_000 * (k as u64 + 1);
        let before = turnaround(&host, chain, seq0, 250);
        let r = host.request(json!({"cmd": "editor", "chain": chain, "index": 0, "title": format!("Compressor ({format})")}), t).unwrap();
        assert_eq!(r["open"], true, "{r}");
        let during = turnaround(&host, chain, seq0 + 1000, 1000);
        #[cfg(target_os = "linux")]
        if let Some(prefix) = idn_common::switches::value("IDN_SHOT") {
            screenshot(&format!("{prefix}-{format}.ppm"));
        }
        // A parameter set by the desk while the window is open.
        let params = host
            .request(json!({"cmd": "params", "chain": chain, "index": 0}), t)
            .unwrap();
        let first = &params["params"][0];
        host.request(json!({"cmd": "set", "chain": chain, "index": 0, "param": first["id"], "value": first["max"]}), t).unwrap();
        let after_set = turnaround(&host, chain, seq0 + 3000, 100);
        // A knob turned in the plugin's window (the Ratio knob in LSP's layout), while the
        // sound runs: the desk hears which parameter moved.
        #[cfg(target_os = "linux")]
        {
            let h = std::thread::spawn(|| drag(160, 447, 40));
            let _ = turnaround(&host, chain, seq0 + 4000, 300);
            h.join().unwrap();
            let moved: Vec<_> =
                std::iter::from_fn(|| events.recv_timeout(Duration::from_millis(500)).ok())
                    .filter(|e| e["event"] == "param" && e["chain"] == chain)
                    .collect();
            let names: std::collections::BTreeSet<String> = moved
                .iter()
                .filter_map(|e| {
                    params["params"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|p| p["id"] == e["param"])
                })
                .map(|p| p["name"].as_str().unwrap_or("").to_owned())
                .collect();
            eprintln!(
                "{format}: knob drag: {} events, parameters {names:?}",
                moved.len()
            );
            assert!(!moved.is_empty(), "no parameter events from the window");
        }
        // Closed by the user: the desk hears so.
        #[cfg(target_os = "linux")]
        {
            close_window(r["window"].as_u64().unwrap() as u32);
            let _ = turnaround(&host, chain, seq0 + 4500, 100);
            let closed_ev = std::iter::from_fn(|| events.recv_timeout(Duration::from_secs(2)).ok())
                .find(|e| e["event"] == "editor_closed" && e["chain"] == chain);
            assert!(closed_ev.is_some(), "no editor_closed event");
        }
        host.request(
            json!({"cmd": "editor_close", "chain": chain, "index": 0}),
            t,
        )
        .unwrap();
        let closed = turnaround(&host, chain, seq0 + 5000, 250);
        eprintln!(
            "{format}: window {} ({}); worst block turnaround (over 2 ms): {:.2} ms ({}/250) closed, {:.2} ({}/1000) open, {:.2} ({}/100) after a set, {:.2} ({}/250) after closing",
            r["window"],
            r["platform"],
            before.0,
            before.1,
            during.0,
            during.1,
            after_set.0,
            after_set.1,
            closed.0,
            closed.1
        );
        assert!(
            during.1 <= 10,
            "the window held the sound up: {} of 1000 blocks late",
            during.1
        );
        host.request(json!({"cmd": "remove", "chain": chain}), t)
            .unwrap();
    }
    while let Ok(e) = events.try_recv() {
        eprintln!("event: {e}");
    }
    assert!(host.alive());
}

/// IDN Send and IDN Return (media/idn-audio-plugins, N8) loaded as any host loads them, in both
/// formats, with `IDN_AUDIO_PLUGINS` a folder holding `IDN.clap` and `IDN.vst3` and `IDN_NODE`
/// a running Node's endpoint (the plugin host inherits it): found, their "Number", a send
/// passing its track, state, and a send's sound routed into a return through the Node.
#[test]
fn idn_audio_plugins_plugins() {
    let (Some(dir), Some(endpoint)) = (
        idn_common::switches::value("IDN_AUDIO_PLUGINS").map(std::ffi::OsString::from),
        idn_common::switches::value("IDN_NODE"),
    ) else {
        return;
    };
    let node = idn_open_protocol::node::Node::connect_to(&endpoint, "daw-test").unwrap();
    let host = Host::spawn(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"))).unwrap();
    let t = Duration::from_secs(20);
    let found = json!({"plugins": idn_plugin_host::scan(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host")), &[dir.to_string_lossy().into_owned()]).unwrap()});
    let list = found["plugins"].as_array().unwrap();
    let mut chain = 300;
    for format in ["clap", "vst3"] {
        let key = |name: &str| {
            list.iter()
                .find(|p| p["format"] == format && p["name"] == name)
                .unwrap_or_else(|| panic!("{format} {name} not found: {found}"))["key"]
                .clone()
        };
        let (send, ret) = (chain, chain + 1);
        chain += 2;
        let r = host
            .request(json!({"cmd": "chain", "chain": send, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": key("IDN Send")}]}), t)
            .unwrap();
        assert_eq!(r["plugins"][0]["params"][0]["name"], "Number", "{r}");
        let number = if format == "clap" { 3.0 } else { 2.0 / 63.0 };
        host.request(
            json!({"cmd": "set", "chain": send, "index": 0, "param": 1, "value": number}),
            t,
        )
        .unwrap();
        host.request(json!({"cmd": "chain", "chain": ret, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": key("IDN Return")}]}), t)
            .unwrap();
        // The send's source and the return's receiver appear on the Node.
        let mut tx = None;
        let end = Instant::now() + Duration::from_secs(15);
        let mut seq = 10_000 * chain as u64;
        let mut got = (0.0f32, 0.0f32);
        while Instant::now() < end {
            seq += 1;
            host.submit(
                seq,
                vec![
                    ChainAudio {
                        chain: send,
                        channels: 2,
                        frames: 96,
                        // Planar: the left side, then the right.
                        data: [vec![0.3f32; 96], vec![0.1f32; 96]].concat(),
                    },
                    ChainAudio {
                        chain: ret,
                        channels: 2,
                        frames: 96,
                        data: vec![0.0; 192],
                    },
                ],
            );
            let out = host
                .take_wait(seq, Duration::from_secs(5))
                .expect("blocks back");
            let s = out.iter().find(|c| c.chain == send).unwrap();
            assert!(
                s.channel(0).iter().all(|&v| v == 0.3) && s.channel(1).iter().all(|&v| v == 0.1),
                "{format}: the send passes its track"
            );
            let r = out.iter().find(|c| c.chain == ret).unwrap();
            got = (r.channel(0)[95], r.channel(1)[95]);
            if (got.0 - 0.3).abs() < 0.01 && (got.1 - 0.1).abs() < 0.01 {
                break;
            }
            if tx.is_none() {
                let st = node.status().unwrap();
                let name = st
                    .children_named("audio_tx")
                    .filter_map(|t| t.attr("name"))
                    .find(|n| n.ends_with(" Send 3)"))
                    .map(str::to_owned);
                let rx = st
                    .children_named("audio_rx")
                    .filter_map(|t| t.attr("name"))
                    .find(|n| n.ends_with(" Return 1"))
                    .map(str::to_owned);
                if let (Some(name), Some(rx)) = (name, rx) {
                    eprintln!("{format}: {name} -> {rx}");
                    for ch in 0..2u16 {
                        node.request(
                            idn_open_protocol::xml::Element::new("route")
                                .with_attr("receiver", &rx)
                                .with_attr("ch", &ch.to_string())
                                .with_attr("tx", &name)
                                .with_attr("tx_ch", &ch.to_string()),
                        )
                        .unwrap();
                    }
                    tx = Some(name);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            (got.0 - 0.3).abs() < 0.01 && (got.1 - 0.1).abs() < 0.01,
            "{format}: the return plays {got:?} (routed from {tx:?}); the Node: {}",
            node.status().map(|s| s.to_xml()).unwrap_or_default()
        );
        // The number in the send's state.
        let st = host
            .request(json!({"cmd": "state", "chain": send, "index": 0}), t)
            .unwrap();
        assert!(st["state"].as_str().is_some_and(|s| !s.is_empty()), "{st}");
        for c in [send, ret] {
            let _ = host.request(json!({"cmd": "chain", "chain": c, "channels": 2, "rate": 48000.0, "block": 96, "plugins": []}), t);
        }
        // The next format's plugins take the same names: give the Node a moment.
        std::thread::sleep(Duration::from_secs(1));
    }
    assert!(host.alive());
}

/// A block's way to the host and back (a profile, not a check): 16 stereo chains of the
/// test plugin, 96-frame blocks, the time from sending a block to its results and the
/// whole process's CPU. `IDN_PLUGINS_NO_SHM=1` for the pipe.
#[test]
#[ignore = "a profile"]
fn block_round_trip() {
    let plugin = test_plugin();
    let host = Host::spawn(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"))).unwrap();
    let t = Duration::from_secs(10);
    let found = json!({"plugins": idn_plugin_host::scan(&PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host")), &[plugin.to_string_lossy().into_owned()]).unwrap()});
    let key = found["plugins"][0]["key"].as_str().unwrap().to_owned();
    for c in 0..16u32 {
        host.request(json!({"cmd": "chain", "chain": c, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": key}]}), t)
            .unwrap();
    }
    let block = |seq: u64| {
        (0..16u32)
            .map(|chain| ChainAudio {
                chain,
                channels: 2,
                frames: 96,
                data: vec![seq as f32 * 1e-6; 192],
            })
            .collect::<Vec<_>>()
    };
    let n = 20_000u64;
    let mut times = Vec::with_capacity(n as usize);
    let start = Instant::now();
    for seq in 0..n {
        let t0 = Instant::now();
        host.submit(seq, block(seq));
        let r = host
            .take_wait(seq, Duration::from_secs(5))
            .expect("results");
        assert_eq!(r.len(), 16);
        times.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let all = start.elapsed().as_secs_f64();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "{}: round trip median {:.0} us, p99 {:.0} us; {:.0} blocks/s back to back",
        if host.shared_memory() {
            "shared memory"
        } else {
            "pipe"
        },
        times[times.len() / 2],
        times[times.len() * 99 / 100],
        n as f64 / all
    );
}

/// The host started with the test plugin in a stereo chain (number 1): the host and the
/// plugin's key.
fn host_with_test_chain() -> (Host, String) {
    let program = PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"));
    let plugin = test_plugin();
    let found = idn_plugin_host::scan(&program, &[plugin.to_string_lossy().into_owned()]).unwrap();
    let key = found
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "IDN Test Gain")
        .unwrap_or_else(|| panic!("not found: {found}"))["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let host = Host::spawn(&program).unwrap();
    host.request(
        json!({"cmd": "chain", "chain": 1, "channels": 2, "rate": 48000.0, "block": 96, "plugins": [{"key": key}]}),
        Duration::from_secs(10),
    )
    .unwrap();
    (host, key)
}

/// A plugin that crashes takes only the host down: the desk side sees it at once (no
/// waiting out timeouts: requests and results end straight away), and a new host plays.
#[test]
fn a_crashing_plugin_ends_only_the_host() {
    let (host, _) = host_with_test_chain();
    assert!(block(&host, 1, 1, 1.0).data.iter().all(|&v| v == 0.5));
    host.submit(
        2,
        vec![ChainAudio {
            chain: 1,
            channels: 2,
            frames: 96,
            data: vec![1234.0; 192],
        }],
    );
    idn_common::testing::wait_for("the host to stop", Duration::from_secs(5), || !host.alive());
    let t = Instant::now();
    assert!(host.take_wait(2, Duration::from_secs(5)).is_none());
    assert!(
        host.request(
            json!({"cmd": "state", "chain": 1, "index": 0}),
            Duration::from_secs(5)
        )
        .is_err()
    );
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "waited out a timeout: {:?}",
        t.elapsed()
    );
    let (again, _) = host_with_test_chain();
    assert!(block(&again, 1, 1, 1.0).data.iter().all(|&v| v == 0.5));
}

/// A plugin that hangs in its processing: its chain is left out of the results (the desk
/// plays it dry) while the blocks' results keep coming on time, and the host still
/// answers what isn't audio.
#[test]
fn a_hanging_plugin_leaves_only_its_chain_out() {
    let (host, _) = host_with_test_chain();
    assert!(block(&host, 1, 1, 1.0).data.iter().all(|&v| v == 0.5));
    let chain = |v: f32| ChainAudio {
        chain: 1,
        channels: 2,
        frames: 96,
        data: vec![v; 192],
    };
    host.submit(2, vec![chain(4321.0)]);
    for seq in 2..6u64 {
        if seq > 2 {
            host.submit(seq, vec![chain(1.0)]);
        }
        let t = Instant::now();
        let r = host
            .take_wait(seq, Duration::from_secs(2))
            .unwrap_or_else(|| panic!("block {seq}'s results didn't come"));
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "block {seq}: {:?}",
            t.elapsed()
        );
        assert!(
            r.iter().all(|c| c.chain != 1),
            "block {seq}: the hung chain in its results"
        );
    }
    assert!(host.alive());
    // About another chain: answered (here an error, there is no chain 9) while it hangs.
    let t = Instant::now();
    let r = host.request(
        json!({"cmd": "latency", "chain": 9}),
        Duration::from_secs(3),
    );
    assert!(
        r.as_ref().err().is_none_or(|e| !e.contains("in time")),
        "the host didn't answer while a plugin hangs: {r:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(2));
    // SAFETY: the test's own child process.
    #[cfg(unix)]
    unsafe {
        libc::kill(host.pid() as i32, libc::SIGKILL);
    }
}

/// Files that aren't plugins where plugins are looked for (a text file named .clap, a
/// library with no CLAP entry): the scan goes on, the test plugin is still found.
#[test]
fn a_scan_survives_files_that_are_not_plugins() {
    let program = PathBuf::from(env!("CARGO_BIN_EXE_idn-plugin-host"));
    let dir = idn_common::testing::TempDir::new("idn-plugins-bad");
    let text = dir.join("not-a-plugin.clap");
    std::fs::write(&text, b"this is not a library").unwrap();
    // A real library, but not a plugin: the test binary's own dependency, libc's math.
    let lib = dir.join("a-library.clap");
    let libm = [
        "/lib/x86_64-linux-gnu/libm.so.6",
        "/usr/lib/x86_64-linux-gnu/libm.so.6",
        "/usr/lib/libm.so.6",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).exists());
    if let Some(m) = libm {
        std::fs::copy(m, &lib).unwrap();
    }
    let plugin = test_plugin();
    let extra: Vec<String> = [&text, &lib, &plugin]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let found = idn_plugin_host::scan(&program, &extra).expect("the scan itself works");
    assert!(
        found
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "IDN Test Gain"),
        "{found}"
    );
}
