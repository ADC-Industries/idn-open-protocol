//! Audio routing through the IDN Node (spec/extensions/idn-routing.md §A.9, 2 and 3): a
//! 4-channel transmitter, a 2-channel routable receiver, routes set on the machine; the
//! receiving program gets exactly the routed channels, reports their state, survives a
//! Node restart, and a second program can't take the receiver's name.

use crate::common;
use common::start;
use idn_common::sync::LockOk as _;

use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::routing::{AudioReceiver, Router, RouterConfig, RouterEvent};
use idn_open_protocol::xml::Element;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/iroute-{}-{name}.sock", std::process::id())
}

fn level(c: usize) -> f32 {
    (c as f32 + 1.0) / 10.0
}

/// Waits up to `secs` for the Node's status to satisfy `pred`.
fn status_until(node: &Node, secs: u64, what: &str, pred: impl Fn(&Element) -> bool) -> Element {
    let ok = |s: &Element| pred(s);
    wait_for(what, Duration::from_secs(secs), || {
        node.status().is_ok_and(|s| ok(&s))
    });
    node.status().unwrap()
}

fn rx_routes(s: &Element, name: &str) -> Vec<(String, String, String)> {
    s.children_named("audio_rx")
        .find(|r| r.attr("name") == Some(name))
        .map(|r| {
            r.children_named("route")
                .map(|x| {
                    (
                        x.attr("ch").unwrap_or("").to_owned(),
                        x.attr("tx_ch").unwrap_or("").to_owned(),
                        x.attr("state").unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn routed_channels_reach_the_receiving_program() {
    let (endpoint, state) = (sock("n"), TempDir::new("state"));
    let child = start(&endpoint, &state);

    // The transmitter: a program using the Node, sending four channels.
    let tx_node = Node::connect_to(&endpoint, "transmitter").unwrap();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(tx_node.secure_config()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &publisher,
            OutputConfig {
                audio_labels: vec!["Kick".into(), "Snare".into(), "Vox".into(), "Keys".into()],
                ..OutputConfig::new("Desk")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let channels = (0..4).map(|c| vec![level(c); 480]).collect();
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: 48000,
                        channels,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    // The Node lists it as a transmitter once its sound flows.
    let s = status_until(&tx_node, 5, "the transmitter", |s| {
        s.children_named("audio_tx")
            .any(|t| t.attr("channels") == Some("4"))
    });
    let t = s.children_named("audio_tx").next().unwrap();
    assert_eq!(t.attr("labels"), Some("Kick,Snare,Vox,Keys"));
    // Where its sound comes from, as the program says (§A.16).
    out.set_audio_origin(&idn_open_protocol::node::routing::AudioOrigin {
        kind: "sound_card".into(),
        device: "Focusrite 18i8".into(),
        channels: vec!["In 1".into(), "In 2".into(), "In 3".into(), "In 4".into()],
    });
    status_until(&tx_node, 5, "the transmitter's origin", |s| {
        s.children_named("audio_tx").any(|t| {
            t.attr("origin_kind") == Some("sound_card")
                && t.attr("origin_device") == Some("Focusrite 18i8")
                && t.attr("origin_channels") == Some("In 1,In 2,In 3,In 4")
        })
    });
    // Routes name the transmitter as receivers do: here by address (no mDNS in tests).
    let tx = format!("127.0.0.1:{}", out.port());

    // The receiver: another program, two channels.
    let rx_node = Arc::new(Node::connect_to(&endpoint, "receiver").unwrap());
    // A sound card's outputs (§A.16: its kind reported).
    let rx = Arc::new(
        AudioReceiver::register_as(
            &rx_node,
            "Stage",
            2,
            &[],
            idn_open_protocol::node::routing::ReceiverKind::SoundCard,
        )
        .unwrap(),
    );
    assert!(rx.routes().is_empty());
    let other = Node::connect_to(&endpoint, "intruder").unwrap();
    let e = AudioReceiver::register(&other, "Stage", 8, &[]).unwrap_err();
    assert!(e.to_string().contains("offered by receiver"), "{e}");

    let (btx, brx) = mpsc::channel::<(Vec<(usize, u16)>, Vec<f32>)>();
    // The latency the blocks carry (§A.12): 0 for none.
    let block_latency = Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
    // The path the blocks say (R.30): a transmitter on this machine is local.
    let block_path = Arc::new(Mutex::new(None));
    let router = Router::start(
        rx_node.clone(),
        rx.clone(),
        RouterConfig { lossless: true },
        {
            let (block_latency, block_path) = (block_latency.clone(), block_path.clone());
            move |ev| {
                if let RouterEvent::Block(b) = ev {
                    block_latency.store(b.latency_ms.unwrap_or(0), Ordering::Relaxed);
                    *block_path.lock_ok() = Some(b.path);
                    let levels = b.audio.channels.iter().map(|c| c[0]).collect();
                    let _ = btx.send((b.map.to_vec(), levels));
                }
            }
        },
    )
    .unwrap();

    // Routes from this machine: receiver 1 <- transmitter 3, receiver 2 <- transmitter 4.
    let route = |ch: u16, tx_ch: Option<u16>| {
        let mut e = Element::new("route")
            .with_attr("receiver", "Stage")
            .with_attr("ch", &ch.to_string());
        e = match tx_ch {
            Some(t) => e.with_attr("tx", &tx).with_attr("tx_ch", &t.to_string()),
            None => e.with_attr("tx", ""),
        };
        rx_node.request(e).unwrap();
    };
    route(0, Some(2));
    route(1, Some(3));
    // Exactly the routed channels, each where the routes say.
    let end = Instant::now() + Duration::from_secs(5);
    let (map, levels) = loop {
        let (map, levels) = brx
            .recv_timeout(Duration::from_secs(5))
            .expect("routed sound");
        if map.len() == 2 {
            break (map, levels);
        }
        assert!(Instant::now() < end, "never both routes: {map:?}");
    };
    let mut got = [0.0f32; 2];
    for (i, r) in &map {
        got[*r as usize] = levels[*i];
    }
    assert_eq!(got, [level(2), level(3)]);
    assert_eq!(
        levels.len(),
        2,
        "only the routed channels travel (channels/1)"
    );
    // The state reaches the Node.
    status_until(&rx_node, 5, "the routes ok", |s| {
        rx_routes(s, "Stage")
            == [
                ("0".into(), "2".into(), "ok".into()),
                ("1".into(), "3".into(), "ok".into()),
            ]
    });
    // Each route says how its sound arrives (§A.16): the transport, the receiver its kind.
    let s = status_until(&rx_node, 5, "each route's transport", |s| {
        s.children_named("audio_rx")
            .find(|r| r.attr("name") == Some("Stage"))
            .is_some_and(|r| {
                r.attr("kind") == Some("sound_card")
                    && r.children_named("route")
                        .all(|x| x.attr("via").is_some_and(|v| !v.is_empty()))
            })
    });
    let m = idn_open_protocol::node::routing::matrix_json(&s, "here");
    let r0 = &m["receivers"][0];
    assert_eq!(r0["rx_kind"], "sound_card");
    assert!(
        matches!(r0["routes"][0]["via"].as_str(), Some("machine" | "network")),
        "{r0}"
    );
    // The matrix's meters (§A.15): sound arriving on both routes, and the transmitter's
    // levels once a matrix asks for them (its program reports only then).
    status_until(&rx_node, 5, "a signal on both routes", |s| {
        s.children_named("audio_rx")
            .find(|r| r.attr("name") == Some("Stage"))
            .is_some_and(|r| {
                r.children_named("route")
                    .filter(|x| x.attr("sig") == Some("1"))
                    .count()
                    == 2
            })
    });
    // 0.1, 0.2, 0.3 and 0.4: −20, −14, −10 and −8 dBFS.
    wait_for("the levels", Duration::from_secs(5), || {
        rx_node
            .levels()
            .unwrap()
            .children_named("tx")
            .any(|t| t.attr("db") == Some("-20,-14,-10,-8"))
    });

    // A cleared route: only channel 3 travels now.
    route(1, None);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let (map, levels) = brx.recv_timeout(Duration::from_secs(5)).expect("sound");
        if map == [(0, 0)] && levels == [level(2)] {
            break;
        }
        assert!(Instant::now() < end, "still {map:?} {levels:?}");
    }

    // A latency for the receiver (§A.12): the program and its blocks hear it, the Node
    // reports it; a bad one is refused; it outlives the restart below.
    assert_eq!(rx.latency_ms(), None);
    let latency = |ms: &str| {
        rx_node.request(
            Element::new("receiver_latency")
                .with_attr("receiver", "Stage")
                .with_attr("ms", ms),
        )
    };
    assert_eq!(
        *block_path.lock_ok(),
        Some(idn_common::net::Path::Local),
        "a transmitter on this machine: the local path"
    );
    assert_eq!(latency("2").unwrap().name, "ok");
    assert!(format!("{:?}", latency("0")).contains("1 to 500"));
    wait_for("the latency", Duration::from_secs(5), || {
        rx.latency_ms() == Some(2) && block_latency.load(Ordering::Relaxed) == 2
    });
    status_until(&rx_node, 5, "the latency reported", |s| {
        s.children_named("audio_rx")
            .any(|r| r.attr("name") == Some("Stage") && r.attr("latency_ms") == Some("2"))
    });

    // A sync-group floor (§A.14): to the program with its routes, in the status.
    let floor = |ms: &str| {
        rx_node.request(
            Element::new("receiver_floor")
                .with_attr("receiver", "Stage")
                .with_attr("ms", ms),
        )
    };
    assert_eq!(floor("25").unwrap().name, "ok");
    wait_for("the floor", Duration::from_secs(5), || {
        rx.floor_ms() == Some(25)
    });
    status_until(&rx_node, 5, "the floor reported", |s| {
        s.children_named("audio_rx")
            .any(|r| r.attr("name") == Some("Stage") && r.attr("floor_ms") == Some("25"))
    });
    // An attached mixer with an adapting lane: the floor held, its need told.
    let mixer = idn_open_protocol::audio::Mixer::new(48000, 2, "sync test");
    rx.attach_mixer(&mixer);
    let lane = mixer.lane(&idn_open_protocol::audio::LaneConfig {
        latency_ms: 10,
        adaptive: true,
        ..Default::default()
    });
    let feed_until = Instant::now() + Duration::from_secs(5);
    let mut fed_need = false;
    while Instant::now() < feed_until && !fed_need {
        lane.push(48000, &[vec![0.1; 96], vec![0.1; 96]], &[(0, 0), (1, 1)]);
        let mut buf = vec![0f32; 96 * 2];
        mixer.fill(&mut buf);
        fed_need = mixer.need_ms().is_some();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(fed_need, "the adapting lane says its need");
    status_until(&rx_node, 6, "the need reported", |s| {
        s.children_named("audio_rx")
            .any(|r| r.attr("name") == Some("Stage") && r.attr("need_ms").is_some())
    });
    // (Its target is taken as blocks arrive.)
    for _ in 0..5 {
        lane.push(48000, &[vec![0.1; 96], vec![0.1; 96]], &[(0, 0), (1, 1)]);
        mixer.fill(&mut [0f32; 96 * 2]);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        lane.stats().target_ms >= 25,
        "held at the floor: {}",
        lane.stats().target_ms
    );
    assert_eq!(floor("").unwrap().name, "ok");
    drop(lane);

    // The Node restarts: the routes stay, and the programs register again by themselves.
    drop(child);
    let child = start(&endpoint, &state);
    let watcher = Node::connect_to(&endpoint, "watcher").unwrap();
    status_until(&watcher, 15, "the receiver back with its route", |s| {
        s.children_named("audio_rx").any(|r| {
            r.attr("name") == Some("Stage")
                && r.attr("app") == Some("receiver")
                && r.attr("latency_ms") == Some("2")
        }) && rx_routes(s, "Stage") == [("0".into(), "2".into(), "ok".into())]
            && s.children_named("audio_tx")
                .any(|t| t.attr("channels") == Some("4"))
    });
    assert_eq!(rx.latency_ms(), Some(2), "the latency kept");
    // Cleared: the program's own again.
    assert_eq!(latency("").unwrap().name, "ok");
    wait_for("the latency cleared", Duration::from_secs(5), || {
        rx.latency_ms().is_none()
    });
    while brx.try_recv().is_ok() {}
    let (map, _) = brx
        .recv_timeout(Duration::from_secs(5))
        .expect("sound after the restart");
    assert_eq!(map, [(0, 0)]);

    drop(router);
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    drop(child);
}

/// The OBS plugin's way: routed blocks through a [`Mixer`] lane, lossless,
/// from an 8-channel transmitter's channels 3 and 8 onto a 2-channel consumer.
#[test]
fn routed_high_channels_play_through_a_mixer() {
    use idn_open_protocol::audio::{LaneConfig, Mixer};
    let (endpoint, state) = (sock("m"), TempDir::new("mstate"));
    let _child = start(&endpoint, &state);
    let tx_node = Node::connect_to(&endpoint, "transmitter").unwrap();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(tx_node.secure_config()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Desk8")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut t = 0usize;
            while !stop.load(Ordering::Relaxed) {
                // A tone per channel, louder up the channels.
                let channels = (0..8)
                    .map(|c| {
                        (0..480)
                            .map(|i| 0.05 * (c + 1) as f32 * ((t + i) as f32 * 0.05).sin())
                            .collect()
                    })
                    .collect();
                t += 480;
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: 48000,
                        channels,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let tx = format!("127.0.0.1:{}", out.port());
    let rx_node = Arc::new(Node::connect_to(&endpoint, "OBS-like").unwrap());
    let rx = Arc::new(AudioReceiver::register(&rx_node, "OBS", 2, &[]).unwrap());
    let mixer = Mixer::new(48000, 2, "test");
    let lane = std::sync::Mutex::new(mixer.lane(&LaneConfig {
        latency_ms: 60,
        max_correction_ppm: 1000.0,
        ..Default::default()
    }));
    let _router = Router::start(
        rx_node.clone(),
        rx.clone(),
        RouterConfig { lossless: true },
        move |ev| {
            if let RouterEvent::Block(b) = ev {
                lane.lock()
                    .unwrap()
                    .push(b.audio.header.sample_rate, &b.audio.channels, b.map);
            }
        },
    )
    .unwrap();
    for (ch, tx_ch) in [(0u16, 2u16), (1, 7)] {
        rx_node
            .request(
                Element::new("route")
                    .with_attr("receiver", "OBS")
                    .with_attr("ch", &ch.to_string())
                    .with_attr("tx", &tx)
                    .with_attr("tx_ch", &tx_ch.to_string()),
            )
            .unwrap();
    }
    // Pull like a consumer for 3 s; the RMS of each output channel over the last second.
    let mut sum = [0f64; 2];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let mut buf = vec![0f32; 480 * 2];
        mixer.fill(&mut buf);
        if start.elapsed() > Duration::from_secs(2) {
            for f in buf.chunks(2) {
                sum[0] += (f[0] * f[0]) as f64;
                sum[1] += (f[1] * f[1]) as f64;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    let rms = sum.map(|s| (s / 48_000.0).sqrt());
    // Channel 3's tone (0.15 peak, RMS ~0.106) and channel 8's (0.4, ~0.28).
    assert!(rms[0] > 0.08 && rms[1] > 0.2, "output RMS {rms:?}");
}

/// The switcher's sound from the routing matrix (idn-switcher.md §S.6): it offers itself
/// as a routable receiver; routed, the desk's channels come out with its program.
#[test]
fn the_switcher_plays_routed_sound() {
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    use idn_switcher::{AudioSource, Switcher, SwitcherConfig};
    let (endpoint, state) = (sock("sw"), TempDir::new("swstate"));
    let _child = start(&endpoint, &state);
    let tx_node = Node::connect_to(&endpoint, "desk").unwrap();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(tx_node.secure_config()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Desk")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let channels = (0..4).map(|c| vec![level(c); 480]).collect();
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: 48000,
                        channels,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let sw_node = Arc::new(Node::connect_to(&endpoint, "switcher").unwrap());
    let sw = Switcher::start(SwitcherConfig {
        name: "Stream".into(),
        width: 320,
        height: 180,
        mdns: Some(false),
        secure: Some(sw_node.secure_config()),
        node: Some(sw_node.clone()),
        ..Default::default()
    })
    .unwrap();
    sw.audio(AudioSource::Routed, 0.0, 0).unwrap();
    // The switcher's receiver: left <- desk 3, right <- desk 4.
    let tx = format!("127.0.0.1:{}", out.port());
    for (ch, tx_ch) in [(0, 2), (1, 3)] {
        sw_node
            .request(
                Element::new("route")
                    .with_attr("receiver", "Stream")
                    .with_attr("ch", &ch.to_string())
                    .with_attr("tx", &tx)
                    .with_attr("tx_ch", &tx_ch.to_string()),
            )
            .unwrap();
    }
    // The program's sound, received like any source's.
    let input = Input::open(InputConfig {
        secure: Some(sw_node.secure_config()),
        ..InputConfig::new(&format!("127.0.0.1:{}", sw.program_output().port()))
    })
    .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let mut got = (0.0f32, 0.0f32);
    while Instant::now() < end {
        if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(200))
            && a.channels.len() == 2
        {
            let mean = |c: &[f32]| c.iter().sum::<f32>() / c.len().max(1) as f32;
            got = (mean(&a.channels[0]), mean(&a.channels[1]));
            if (got.0 - level(2)).abs() < 0.01 && (got.1 - level(3)).abs() < 0.01 {
                break;
            }
        }
    }
    assert!(
        (got.0 - level(2)).abs() < 0.01 && (got.1 - level(3)).abs() < 0.01,
        "program sound {got:?}, want {} and {}",
        level(2),
        level(3)
    );
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

/// The mixing desk on the routing matrix (idn-mixer.md §M.7, test 2): a transmitter's
/// channels routed to the desk; its main bus and a pre-fader aux as set.
#[test]
fn the_mixer_mixes_routed_channels() {
    use idn_mixer::{Console, ConsoleConfig};
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let (endpoint, state) = (sock("mx"), TempDir::new("mxstate"));
    let _child = start(&endpoint, &state);
    let tx_node = Node::connect_to(&endpoint, "stage box").unwrap();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(tx_node.secure_config()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Stage")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let channels = (0..4).map(|c| vec![level(c); 96]).collect();
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: 48000,
                        channels,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let node = Arc::new(Node::connect_to(&endpoint, "mixer").unwrap());
    let desk = Console::start(ConsoleConfig {
        name: "FOH".into(),
        channels: 8,
        node: Some(node.clone()),
        secure: Some(node.secure_config()),
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    // Channel 1 hard left at 0 dB, channel 2 hard right at −6 dB and sent to aux 1
    // (pre-fader by default) at 0 dB; both put on the desk (parking is manual).
    desk.channel(1, &serde_json::json!({"pan": -1.0, "shown": true}))
        .unwrap();
    desk.channel(
        2,
        &serde_json::json!({"pan": 1.0, "fader": -6.0, "sends": [{"on": true, "level": 0.0}], "shown": true}),
    )
    .unwrap();
    let tx = format!("127.0.0.1:{}", out.port());
    for (ch, tx_ch) in [(0, 2), (1, 3)] {
        node.request(
            Element::new("route")
                .with_attr("receiver", "FOH")
                .with_attr("ch", &ch.to_string())
                .with_attr("tx", &tx)
                .with_attr("tx_ch", &tx_ch.to_string()),
        )
        .unwrap();
    }
    let input = Input::open(InputConfig {
        secure: Some(node.secure_config()),
        lossless_audio: true,
        ..InputConfig::new(&format!("127.0.0.1:{}", desk.output().port()))
    })
    .unwrap();
    let want = [level(2), level(3) * 0.501_187, level(3)];
    let end = Instant::now() + Duration::from_secs(10);
    let mut got = [0f32; 3];
    while Instant::now() < end {
        if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(200))
            && a.channels.len() >= 3
        {
            let mean = |c: &[f32]| c.iter().sum::<f32>() / c.len().max(1) as f32;
            got = [
                mean(&a.channels[0]),
                mean(&a.channels[1]),
                mean(&a.channels[2]),
            ];
            if got.iter().zip(&want).all(|(g, w)| (g - w).abs() < 0.005) {
                break;
            }
        }
    }
    assert!(
        got.iter().zip(&want).all(|(g, w)| (g - w).abs() < 0.005),
        "main L, main R, aux 1: {got:?}, want {want:?}"
    );
    let st = desk.state();
    assert_eq!(st["outputs"][2], "Aux 1");
    // What the desk is (§A.16): its receiver a desk's, its output a desk's buses.
    status_until(&node, 5, "the desk's kind and origin", |s| {
        s.children_named("audio_rx")
            .any(|r| r.attr("name") == Some("FOH") && r.attr("kind") == Some("desk"))
            && s.children_named("audio_tx").any(|t| {
                t.attr("origin_kind") == Some("desk")
                    && t.attr("origin_device") == Some("FOH")
                    && t.attr("origin_channels")
                        .is_some_and(|c| c.contains("Aux 1"))
            })
    });
    // The stage box stops: a `lost` event for its routes (idn-mixer.md §M.27).
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    let lost = || {
        desk.watch_problems();
        desk.failure_events()
            .since(0)
            .0
            .into_iter()
            .find(|e| e["event"] == "lost")
    };
    wait_for("the lost event", Duration::from_secs(8), || {
        lost().is_some()
    });
    let lost = lost().unwrap();
    assert_eq!(
        (
            lost["source"].as_str(),
            lost["inputs"].clone(),
            lost["cause"].as_str()
        ),
        (
            Some(tx.as_str()),
            serde_json::json!([1, 2]),
            Some("unreachable")
        ),
        "{lost}"
    );
    assert!(
        desk.state()["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["problem"] == "lost")
    );
}

/// The ASIO driver's inputs (idn-node.md §N.14): it offers itself as a routable receiver
/// named like the program; routed channels arrive in the program's input buffers.
#[test]
fn routed_channels_reach_an_asio_program() {
    use idn_asio::{Config, Driver};
    use std::ffi::c_void;
    use std::sync::atomic::AtomicU32;
    let (endpoint, state) = (sock("asio"), TempDir::new("asiostate"));
    let _child = start(&endpoint, &state);
    let tx_node = Node::connect_to(&endpoint, "desk").unwrap();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(tx_node.secure_config()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Desk")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let channels = (0..4).map(|c| vec![level(c); 96]).collect();
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: 48000,
                        channels,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let node = Arc::new(Node::connect_to(&endpoint, "Reaper").unwrap());
    let d = Driver::new(Config {
        name: "Reaper".into(),
        inputs: 2,
        outputs: 0,
        latency_ms: 20,
    });
    d.init(Some(node.clone()), Some(false)).unwrap();
    d.create_buffers(128).unwrap();
    // The stand-in program: each input's mean, as f32 bits.
    struct Program {
        ins: Vec<[*mut f32; 2]>,
        means: [AtomicU32; 2],
    }
    unsafe extern "C" fn switch(ctx: *mut c_void, half: i32, _: u64, _: u64) {
        // SAFETY: `ctx` is the boxed `Program`, which outlives the clock (`Stops`).
        let p = unsafe { &*(ctx as *const Program) };
        for (c, h) in p.ins.iter().enumerate() {
            // SAFETY: the driver's 128-sample input halves, kept until `dispose_buffers`.
            let b = unsafe { std::slice::from_raw_parts(h[half as usize], 128) };
            p.means[c].store((b.iter().sum::<f32>() / 128.0).to_bits(), Ordering::Relaxed);
        }
    }
    let program = Box::new(Program {
        ins: (0..2)
            .map(|c| [d.buffer(true, c, 0), d.buffer(true, c, 1)])
            .collect(),
        means: [AtomicU32::new(0), AtomicU32::new(0)],
    });
    // Stops the clock before `program` goes, also when an assertion fails.
    struct Stops<'a>(&'a Driver);
    impl Drop for Stops<'_> {
        fn drop(&mut self) {
            self.0.dispose_buffers();
        }
    }
    let _stops = Stops(&d);
    d.start(switch, &*program as *const Program as *mut c_void)
        .unwrap();
    // In 1 <- desk 3, in 2 <- desk 4.
    let tx = format!("127.0.0.1:{}", out.port());
    for (ch, tx_ch) in [(0, 2), (1, 3)] {
        node.request(
            Element::new("route")
                .with_attr("receiver", "Reaper")
                .with_attr("ch", &ch.to_string())
                .with_attr("tx", &tx)
                .with_attr("tx_ch", &tx_ch.to_string()),
        )
        .unwrap();
    }
    let mean = |c: usize| f32::from_bits(program.means[c].load(Ordering::Relaxed));
    idn_common::testing::wait_until(Duration::from_secs(10), || {
        (mean(0) - level(2)).abs() < 0.005 && (mean(1) - level(3)).abs() < 0.005
    });
    assert!(
        (mean(0) - level(2)).abs() < 0.005 && (mean(1) - level(3)).abs() < 0.005,
        "inputs {} {}, want {} {}",
        mean(0),
        mean(1),
        level(2),
        level(3)
    );
    d.dispose_buffers();
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

/// The audio plugins, IDN Send and IDN Return (N8, §A.13) on one machine: an IDN Send's track routed into an IDN
/// Return's receiver through the Node, both driven as a host drives them (in place, a
/// block at a time); renumbering a return renames its receiver.
#[test]
fn an_editor_send_reaches_an_editor_return() {
    use idn_audio_plugins::engine::{self, Engine, Kind};
    let (endpoint, state) = (sock("daw"), TempDir::new("dawstate"));
    let _child = start(&endpoint, &state);
    let node = Arc::new(Node::connect_to(&endpoint, "TestDAW").unwrap());
    engine::use_node(node.clone());
    let send = Engine::new(Kind::Send, "TestDAW");
    send.set_slot(4);
    send.activate(48000, 256);
    let ret = Engine::new(Kind::Return, "TestDAW");
    ret.activate(48000, 256);
    let up = idn_common::testing::wait_until(Duration::from_secs(10), || {
        send.port().is_some() && ret.status().ends_with(" up")
    });
    assert!(up, "not up: {} / {}", send.status(), ret.status());
    assert_eq!(ret.name(), "TestDAW Return 1");
    // Return 1 <- Send 4, the sides crossed.
    let tx = format!("127.0.0.1:{}", send.port().unwrap());
    for (ch, tx_ch) in [(0, 1), (1, 0)] {
        node.request(
            Element::new("route")
                .with_attr("receiver", "TestDAW Return 1")
                .with_attr("ch", &ch.to_string())
                .with_attr("tx", &tx)
                .with_attr("tx_ch", &tx_ch.to_string()),
        )
        .unwrap();
    }
    // The host: 256 frames every 5.3 ms, the send's track at 0.2 / 0.4, the return's empty.
    let (mut l, mut r) = (vec![0f32; 256], vec![0f32; 256]);
    let mut got = (0.0, 0.0);
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        let (mut sl, mut sr) = (vec![0.2f32; 256], vec![0.4f32; 256]);
        send.process(&mut [&mut sl[..], &mut sr[..]], 256);
        assert!(sl.iter().all(|&v| v == 0.2), "a send passes its track");
        l.fill(0.0);
        r.fill(0.0);
        ret.process(&mut [&mut l[..], &mut r[..]], 256);
        got = (l[255], r[255]);
        if (got.0 - 0.4f32).abs() < 0.01 && (got.1 - 0.2f32).abs() < 0.01 {
            break;
        }
        std::thread::sleep(Duration::from_micros(5333));
    }
    assert!(
        (got.0 - 0.4f32).abs() < 0.01 && (got.1 - 0.2f32).abs() < 0.01,
        "the return plays {got:?}, want (0.4, 0.2)"
    );
    // A new number: the receiver is offered under its new name.
    ret.set_slot(7);
    status_until(&node, 10, "TestDAW Return 7 offered", |s| {
        s.children_named("audio_rx")
            .any(|r| r.attr("name") == Some("TestDAW Return 7") && r.attr("app") == Some("TestDAW"))
    });
    send.deactivate();
    ret.deactivate();
}

/// A desk's lane (10 ms, 2 ms blocks) on a routed tone: the route taken away and put
/// back, then the sender paused and resumed. Each stop fades out and each start fades in,
/// with no step either way (lab 356-A, 372: the sound stopped dead, and came back with a
/// fade cut short, then a jump to full level).
#[test]
fn a_routed_lane_starts_and_stops_without_a_step() {
    use idn_open_protocol::audio::{LaneConfig, Mixer};
    let (endpoint, state) = (sock("steps"), TempDir::new("stepsstate"));
    let _child = start(&endpoint, &state);
    let tx_node = Node::connect_to(&endpoint, "transmitter").unwrap();
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(tx_node.secure_config()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Tone")).unwrap());
    let (stop, paused) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let feeder = {
        let (out, stop, paused) = (out.clone(), stop.clone(), paused.clone());
        std::thread::spawn(move || {
            // A 1 kHz tone at 0.5 on both channels, 2 ms blocks, kept to real time.
            let t0 = Instant::now();
            let mut sent = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let due = (t0.elapsed().as_secs_f64() * 500.0) as usize;
                while sent < due {
                    let block: Vec<f32> = (0..96)
                        .map(|i| {
                            0.5 * ((sent * 96 + i) as f32 * std::f32::consts::TAU / 48.0).sin()
                        })
                        .collect();
                    sent += 1;
                    if !paused.load(Ordering::Relaxed) {
                        let _ = out.send_audio(
                            AudioBlock {
                                sample_rate: 48000,
                                channels: vec![block.clone(), block],
                            },
                            None,
                        );
                    }
                }
                std::thread::sleep(Duration::from_micros(500));
            }
        })
    };
    let tx = format!("127.0.0.1:{}", out.port());
    let rx_node = Arc::new(Node::connect_to(&endpoint, "Desk-like").unwrap());
    let rx = Arc::new(AudioReceiver::register(&rx_node, "Desk", 2, &[]).unwrap());
    // As the desk takes them: a lane per transmitter, made at its first block and
    // dropped when its routes are all gone, each played by site time where it has it.
    let mixer = Arc::new(Mixer::new(48000, 2, "test"));
    let lanes: std::sync::Mutex<
        std::collections::BTreeMap<String, idn_open_protocol::audio::Lane>,
    > = Default::default();
    let m2 = mixer.clone();
    let _router = Router::start(
        rx_node.clone(),
        rx.clone(),
        RouterConfig { lossless: true },
        move |ev| match ev {
            RouterEvent::Block(b) => {
                let mut l = lanes.lock_ok();
                let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                    m2.lane(&LaneConfig {
                        latency_ms: 10,
                        max_correction_ppm: 1000.0,
                        ..Default::default()
                    })
                });
                let at = idn_open_protocol::audio::site_time(
                    b.audio.header.timestamp,
                    b.audio.header.metadata.as_deref(),
                );
                lane.push_timed(
                    b.audio.header.sample_rate,
                    &b.audio.channels,
                    Some(b.map),
                    at,
                );
            }
            RouterEvent::Gone(tx) => {
                lanes.lock_ok().remove(tx);
            }
        },
    )
    .unwrap();
    let route = |on: bool| {
        for ch in 0..2u16 {
            let mut e = Element::new("route")
                .with_attr("receiver", "Desk")
                .with_attr("ch", &ch.to_string());
            e = if on {
                e.with_attr("tx", &tx).with_attr("tx_ch", &ch.to_string())
            } else {
                e.with_attr("tx", "")
            };
            rx_node.request(e).unwrap();
        }
    };
    // The device: 2 ms blocks in real time; channel 0 kept.
    let mut heard: Vec<f32> = Vec::new();
    let t0 = Instant::now();
    let play = |until_s: f64, heard: &mut Vec<f32>| {
        while t0.elapsed().as_secs_f64() < until_s {
            let due = (t0.elapsed().as_secs_f64() * 500.0) as usize;
            while heard.len() / 96 < due {
                let mut buf = vec![0f32; 96 * 2];
                mixer.fill(&mut buf);
                heard.extend(buf.iter().step_by(2));
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    };
    route(true);
    play(2.0, &mut heard);
    route(false);
    play(3.0, &mut heard);
    route(true);
    play(4.5, &mut heard);
    paused.store(true, Ordering::Relaxed);
    play(5.5, &mut heard);
    paused.store(false, Ordering::Relaxed);
    play(7.0, &mut heard);
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    // Peak per ms, as a fraction of the tone's.
    let peaks: Vec<f32> = heard
        .chunks(48)
        .map(|c| c.iter().fold(0f32, |m, v| m.max(v.abs())) / 0.5)
        .collect();
    let mut steps = Vec::new();
    let mut starts = 0;
    for k in 1..peaks.len() {
        let (a, b) = (peaks[k - 1], peaks[k]);
        if a < 0.02 && b >= 0.02 {
            starts += 1;
        }
        // Up, more than a 5 ms fade's step (0.2 a ms); down, from full to nothing at
        // once (a source that stops fades out over what its last block left, a ms or two).
        if b - a > 0.3 || a - b > 0.7 {
            steps.push((k, a, b));
        }
    }
    assert!(
        starts >= 3,
        "the tone started {starts} times: {:?}",
        &peaks[..peaks.len().min(40)]
    );
    assert!(
        steps.is_empty(),
        "steps (ms, from, to): {steps:?}; around the first: {:?}",
        steps
            .first()
            .map(|&(k, _, _)| &peaks[k.saturating_sub(12)..(k + 12).min(peaks.len())])
    );
}

/// The IDN Mixer's Routing view, Network tab: the matrix's data from the
/// desk (its receiver and transmitter named), and routes and a latency set through the
/// desk's own commands, which go to the Node.
#[test]
fn the_desk_routes_through_its_own_panel() {
    use idn_mixer::{Console, ConsoleConfig};
    use serde_json::{Value, json};
    let (endpoint, state) = (sock("mxr"), TempDir::new("mxrstate"));
    let _child = start(&endpoint, &state);
    let node = Arc::new(Node::connect_to(&endpoint, "mixer").unwrap());
    let desk = Console::start(ConsoleConfig {
        name: "FOH".into(),
        channels: 8,
        node: Some(node.clone()),
        secure: Some(node.secure_config()),
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let receiver = |v: &Value| -> Option<Value> {
        v["audio"]["receivers"]
            .as_array()?
            .iter()
            .find(|r| r["name"] == "FOH")
            .cloned()
    };
    let until = |what: &str, pred: &dyn Fn(&Value) -> bool| -> Value {
        let held = idn_common::testing::wait_until(Duration::from_secs(10), || {
            pred(&desk.routing().unwrap())
        });
        let v = desk.routing().unwrap();
        assert!(held, "timed out waiting for {what}: {v}");
        v
    };
    let v = until("the desk's receiver", &|v| receiver(v).is_some());
    assert_eq!(v["desk"]["receiver"], "FOH");
    assert!(
        v["desk"]["transmitter"]
            .as_str()
            .unwrap()
            .ends_with("(FOH)"),
        "{v}"
    );
    assert!(v["network"].is_array());
    // A route onto the desk's input 3, then cleared.
    desk.command(
        "route",
        &json!({"receiver": "FOH", "changes": [{"ch": 2, "tx": "127.0.0.1:5999", "tx_ch": 1}]}),
    )
    .unwrap();
    until("the route", &|v| {
        receiver(v).is_some_and(|r| {
            r["routes"].as_array().is_some_and(|rs| {
                rs.iter()
                    .any(|x| x["ch"] == 2 && x["tx"] == "127.0.0.1:5999" && x["tx_ch"] == 1)
            })
        })
    });
    desk.command(
        "route",
        &json!({"receiver": "FOH", "changes": [{"ch": 2, "tx": ""}]}),
    )
    .unwrap();
    until("the route cleared", &|v| {
        receiver(v).is_some_and(|r| r["routes"].as_array().is_some_and(|rs| rs.is_empty()))
    });
    // Its latency set, then back to the program's own.
    desk.command("receiver_latency", &json!({"receiver": "FOH", "ms": 20}))
        .unwrap();
    until("the latency", &|v| {
        receiver(v).is_some_and(|r| r["latency_ms"] == 20)
    });
    desk.command("receiver_latency", &json!({"receiver": "FOH", "ms": null}))
        .unwrap();
    until("the latency cleared", &|v| {
        receiver(v).is_some_and(|r| r["latency_ms"].is_null())
    });
    // Refused without a receiver's name, or with a channel out of range.
    assert!(desk.command("route", &json!({"changes": []})).is_err());
    assert!(
        desk.command(
            "route",
            &json!({"receiver": "FOH", "changes": [{"ch": 300, "tx": ""}]})
        )
        .is_err()
    );
    drop(desk);
}
