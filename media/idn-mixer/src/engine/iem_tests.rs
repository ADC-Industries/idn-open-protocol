//! The IEM flow, its phones and talkback, and the mixes in snapshots (§M.26).

use super::*;
use idn_iem::wire::{self, Packet, Status};
use std::net::{SocketAddr, UdpSocket};

fn console(name: &str, f: impl FnOnce(&mut ConsoleConfig)) -> Arc<Console> {
    let mut cfg = ConsoleConfig {
        name: name.into(),
        channels: 4,
        inputs: 4,
        mdns: Some(false),
        // (A roomy buffer: the test's feed is paced by sleeps.)
        latency_ms: 60,
        adaptive: false,
        iem: crate::iem::IemConfig {
            port: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    f(&mut cfg);
    Arc::new(Console::start(cfg).unwrap())
}

/// A sine on desk inputs `first..first+chans` in real time, until dropped.
struct Feeder {
    stop: Arc<AtomicBool>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

fn feed(c: &Arc<Console>, first: usize, chans: usize, amp: f32) -> Feeder {
    let stop = Arc::new(AtomicBool::new(false));
    let t = {
        let (c, stop) = (c.clone(), stop.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut pushed = 0usize;
            while !stop.load(Ordering::Relaxed) {
                // 40 ms ahead of the clock, in blocks of 96.
                let want = ((start.elapsed().as_secs_f64() + 0.04) * 48_000.0) as usize;
                while pushed < want {
                    let block: Vec<f32> = (pushed..pushed + 96)
                        .map(|i| {
                            amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin()
                        })
                        .collect();
                    let _ = c.feed("test", first, 48_000, &vec![block; chans]);
                    pushed += 96;
                }
                std::thread::sleep(Duration::from_millis(3));
            }
        })
    };
    Feeder { stop, t: Some(t) }
}

/// A phone on this machine.
struct Phone {
    sock: UdpSocket,
    desk: SocketAddr,
    client: u64,
}

impl Phone {
    fn new(c: &Console, client: u64) -> Phone {
        let port = c.iem_description()["unicast"]["port"].as_u64().unwrap() as u16;
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        Phone {
            sock,
            desk: ([127, 0, 0, 1], port).into(),
            client,
        }
    }

    fn send(&self, p: &Packet) {
        self.sock.send_to(&p.to_bytes(), self.desk).unwrap();
    }

    /// The next REPLY within 2 s (audio skipped).
    fn reply(&self) -> wire::Reply {
        let end = Instant::now() + Duration::from_secs(2);
        let mut buf = [0u8; 2048];
        while Instant::now() < end {
            if let Ok((n, _)) = self.sock.recv_from(&mut buf)
                && let Some(Packet::Reply(r)) = wire::parse(&buf[..n])
            {
                assert_eq!(r.client, self.client);
                return r;
            }
        }
        panic!("no reply");
    }

    fn subscribe(&self, flags: u8, mix: &str, token: &str) -> wire::Reply {
        // (Paced: the desk takes five requests a second from one address.)
        std::thread::sleep(Duration::from_millis(220));
        self.send(&Packet::Subscribe(wire::Subscribe {
            flow: 0,
            client: self.client,
            flags,
            mix: mix.into(),
            token: token.into(),
        }));
        self.reply()
    }

    fn keepalive(&self) -> wire::Reply {
        self.send(&Packet::Keepalive {
            flow: 0,
            client: self.client,
        });
        self.reply()
    }

    /// The AUDIO packets that come within `d`.
    fn audio(&self, d: Duration) -> Vec<wire::Audio> {
        let end = Instant::now() + d;
        let mut buf = [0u8; 2048];
        let mut out = Vec::new();
        while Instant::now() < end {
            if let Ok((n, _)) = self.sock.recv_from(&mut buf)
                && let Some(Packet::Audio(a)) = wire::parse(&buf[..n])
            {
                out.push(a);
            }
        }
        out
    }
}

/// Each channel's frames decoded in order (a missing frame concealed).
fn decode(packets: &[wire::Audio]) -> BTreeMap<u16, Vec<f32>> {
    let mut frames: BTreeMap<u16, BTreeMap<u32, Vec<u8>>> = BTreeMap::new();
    for p in packets {
        for e in &p.entries {
            frames
                .entry(e.id)
                .or_default()
                .insert(p.seq, e.data.clone());
        }
    }
    frames
        .into_iter()
        .map(|(id, f)| {
            let mut d = idn_iem::codec::Decoder::new().unwrap();
            let mut out = Vec::new();
            let (first, last) = (*f.keys().next().unwrap(), *f.keys().last().unwrap());
            for seq in first..=last {
                let pcm = d
                    .decode(f.get(&seq).map(Vec::as_slice), idn_iem::FRAME_SAMPLES)
                    .unwrap();
                out.extend_from_slice(pcm);
            }
            (id, out)
        })
        .collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// The last `secs` of a channel's sound.
fn tail(x: &[f32], secs: f64) -> &[f32] {
    let n = (secs * 48_000.0) as usize;
    &x[x.len().saturating_sub(n)..]
}

/// Waits up to 5 s for `f`, failing the test naming `what`.
fn wait_for(what: &str, f: impl FnMut() -> bool) {
    idn_common::testing::wait_for(what, Duration::from_secs(5), f);
}

/// A marked channel goes out after its processing and before its fader and mute (its
/// fader off, it still sends; its trim changes it); an unmarked one doesn't; a linked
/// pair goes as its two sides; the flow's channel list has a new version each change.
#[test]
fn marked_channels_go_out_before_their_faders() {
    let c = console("IEM flow test", |_| {});
    let _f = feed(&c, 1, 2, 0.25);
    c.channel(
        1,
        &json!({"name": "Kick", "iem": true, "fader": -144.0, "mute": true}),
    )
    .unwrap();
    let d = c.iem_description();
    assert_eq!(
        d["channels"],
        json!([{"id": 1, "name": "Kick", "color": "", "pair": null, "side": null}])
    );
    assert_eq!(d["codec"]["frame_samples"], 240);
    assert!(
        d["multicast"]["group"]
            .as_str()
            .unwrap()
            .starts_with("239.255.")
    );
    assert!(d["latency"]["desk_ms"].as_f64().unwrap() < 15.0);
    let layout = d["layout"].as_u64().unwrap() as u16;
    wait_for("the IDN output", || {
        c.iem_description()["idn_source"].is_string()
    });
    let phone = Phone::new(&c, 11);
    assert_eq!(
        phone.subscribe(wire::WANT_UNICAST, "", "").status,
        Status::Ok
    );
    let got = phone.audio(Duration::from_millis(1200));
    assert!(got.len() > 150, "{} packets", got.len());
    for p in &got {
        assert_eq!(p.flow, c.iem_description()["flow"].as_u64().unwrap() as u32);
        assert_eq!(p.layout, layout);
        assert_eq!((p.part, p.parts, p.frame_samples), (0, 1, 240));
        assert!(
            p.entries.iter().all(|e| e.id == 1),
            "only the marked channel"
        );
    }
    let loud = rms(tail(&decode(&got)[&1], 0.5));
    assert!(
        loud > 0.08,
        "fader off and muted, the IEM flow still has it: {loud}"
    );
    // Its processing: 12 dB less trim, a quarter of the level.
    c.channel(1, &json!({"trim": -12.0})).unwrap();
    phone.audio(Duration::from_millis(300));
    let quiet = rms(tail(
        &decode(&phone.audio(Duration::from_millis(800)))[&1],
        0.5,
    ));
    let ratio = quiet / loud;
    assert!((0.17..0.35).contains(&ratio), "trim −12 dB: {ratio}");
    // A linked pair, marked as one.
    c.channel(3, &json!({"link": true, "iem": true, "name": "Keys"}))
        .unwrap();
    let d = c.iem_description();
    let ids: Vec<u64> = d["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [1, 3, 4]);
    assert_eq!(
        (
            d["channels"][1]["pair"].as_u64(),
            d["channels"][1]["side"].as_str()
        ),
        (Some(4), Some("L"))
    );
    assert_eq!(d["channels"][2]["side"], "R");
    let layout2 = d["layout"].as_u64().unwrap() as u16;
    assert_ne!(layout2, layout);
    phone.audio(Duration::from_millis(300));
    let got = phone.audio(Duration::from_millis(300));
    let last = got.last().unwrap();
    assert_eq!(last.layout, layout2);
    assert_eq!(
        last.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        [1, 3, 4]
    );
    // The state lists them too.
    assert_eq!(c.state()["iem"]["channels"].as_array().unwrap().len(), 3);
    // None marked: the IDN output goes.
    c.channels(&[1, 3], &json!({"iem": false})).unwrap();
    assert_eq!(c.iem_description()["channels"], json!([]));
    wait_for("the IDN output gone", || {
        c.iem_description()["idn_source"].is_null()
    });
}

/// From the network a phone needs a token (an IEM mix's, read or control; a bus token
/// and another engine's are refused); the unicast copy goes to at most `max_phones`;
/// multicast listeners don't count; a session not renewed ends; talking is only into
/// the token's own mix; requests are bounded; the audit has them.
#[test]
fn phones_need_a_token_and_a_place() {
    use idn_open_protocol::trust::pki::KeyFile;
    use idn_open_protocol::trust::token::{self, Claims, Scope};
    use idn_open_protocol::web::{SiteKeys, TokenCheck};
    let key = KeyFile::generate().unwrap();
    let site = key.fingerprint()[..16].to_owned();
    let (public, s2) = (key.public, site.clone());
    let check = TokenCheck::new("desk", move || {
        Some(SiteKeys {
            ca_public: public,
            site: s2.clone(),
            machine: "m1".into(),
            options: Default::default(),
        })
    });
    let tok = |sub: &str, engine: &str, scope: &str| {
        let c = Claims::new(
            &site,
            sub,
            sub,
            engine,
            "m1",
            Scope::parse(scope).unwrap(),
            idn_common::epoch::secs(),
            600,
        )
        .unwrap();
        token::sign(&c, &key).unwrap()
    };
    let c = console("IEM phones test", |cfg| {
        cfg.token_check = Some(check);
        cfg.iem.trust_loopback = false;
        cfg.iem.max_phones = 2;
        cfg.iem.timeout = Duration::from_millis(3000);
    });
    let (a, b, x) = (Phone::new(&c, 1), Phone::new(&c, 2), Phone::new(&c, 3));
    let uni = wire::WANT_UNICAST;
    assert_eq!(a.subscribe(uni, "", "").status, Status::TokenNeeded);
    assert_eq!(
        a.subscribe(uni, "", "idn1.no.good").status,
        Status::TokenRefused
    );
    let other_engine = tok("alice", "switcher", "iem:drums");
    assert_eq!(
        a.subscribe(uni, "", &other_engine).status,
        Status::TokenRefused
    );
    let bus = tok("streamer", "desk", "bus:aux:1");
    assert_eq!(a.subscribe(uni, "", &bus).status, Status::NotAllowed);
    let alice = tok("alice", "desk", "iem:drums");
    assert_eq!(a.subscribe(uni, "", &alice).status, Status::Ok);
    assert_eq!(
        b.subscribe(uni, "", &tok("bob", "desk", "read")).status,
        Status::Ok
    );
    let control = tok("carol", "desk", "control");
    // A control token: refused on the flow until the desk allows it (an
    // option, off by default), then taken.
    assert_eq!(x.subscribe(uni, "", &control).status, Status::NotAllowed);
    c.desk(&serde_json::json!({"iem_control": true})).unwrap();
    let full = x.subscribe(uni, "", &control);
    assert_eq!(full.status, Status::Full, "{}", full.message);
    // Multicast listeners aren't capped by the unicast's limit.
    assert_eq!(
        x.subscribe(wire::WANT_MULTICAST, "", &control).status,
        Status::Ok
    );
    // Talking: only into the token's own mix, and a mix that is there.
    let talk = uni | wire::WANT_TALK;
    assert_eq!(a.subscribe(talk, "keys", &alice).status, Status::NotAllowed);
    assert_eq!(
        a.subscribe(talk, "", &alice).status,
        Status::NotAllowed,
        "no mix drums yet"
    );
    assert_eq!(c.iem_mix_add("Drums", None).unwrap(), "drums");
    assert_eq!(a.subscribe(talk, "", &alice).status, Status::Ok);
    assert_eq!(
        x.subscribe(wire::WANT_TALK, "", &control).status,
        Status::Bad,
        "control: which mix"
    );
    assert_eq!(
        b.subscribe(uni | wire::WANT_TALK, "drums", &tok("bob", "desk", "read"))
            .status,
        Status::NotAllowed
    );
    let st = c.state();
    assert_eq!(st["iem"]["phones"].as_array().unwrap().len(), 3);
    assert_eq!(st["iem"]["unicast"], 2);
    // The desk no longer allowing control tokens: the control session ends (bug run
    // 2026-11-24), the others go on.
    c.desk(&serde_json::json!({"iem_control": false})).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(c.state()["iem"]["phones"].as_array().unwrap().len(), 2);
    c.desk(&serde_json::json!({"iem_control": true})).unwrap();
    // Kept alive, a session stays; not, it ends.
    let end = Instant::now() + Duration::from_millis(3500);
    while Instant::now() < end {
        assert_eq!(a.keepalive().status, Status::Ok);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(b.keepalive().status, Status::Unknown);
    assert_eq!(a.keepalive().status, Status::Ok);
    assert_eq!(c.state()["iem"]["phones"].as_array().unwrap().len(), 1);
    // Bounded: a sixth request within a second from one address waits.
    std::thread::sleep(Duration::from_millis(1100));
    let mut last = Status::Ok;
    for _ in 0..6 {
        x.send(&Packet::Subscribe(wire::Subscribe {
            flow: 0,
            client: 3,
            flags: wire::WANT_MULTICAST,
            mix: String::new(),
            token: control.clone(),
        }));
        last = x.reply().status;
    }
    assert_eq!(last, Status::Busy);
    // The audit: alice's session, and the refusals.
    let log = c.audit().newest(100);
    assert!(
        log.iter()
            .any(|l| l["user"] == "alice" && l["action"] == "iem_subscribe" && l["ok"] == true),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l["action"] == "iem_subscribe" && l["ok"] == false)
    );
    assert!(
        log.iter()
            .any(|l| l["user"] == "bob" && l["action"] == "iem_session_end")
    );
}

/// A phone's push-to-talk arrives on its mix's talkback input, which a channel takes;
/// silent once the button is let go. A session that may not talk is told so.
#[test]
fn talkback_is_a_desk_input_while_the_button_is_held() {
    let c = console("IEM talkback test", |_| {});
    assert_eq!(c.iem_mix_add("Drums", None).unwrap(), "drums");
    let st = c.state();
    assert_eq!(st["iem"]["mixes"][0]["talkback_input"], 5);
    assert_eq!(st["iem"]["talkback_inputs"], json!([5, 20]));
    assert_eq!(st["inputs"], 4 + crate::iem::TALKBACK_INPUTS);
    c.channel(4, &json!({"source": 5, "name": "Drummer TB"}))
        .unwrap();
    let phone = Phone::new(&c, 21);
    assert_eq!(
        phone.subscribe(wire::WANT_TALK, "drums", "").status,
        Status::Ok
    );
    let level = |c: &Console| c.meters()["ch"][3][0].as_f64().unwrap();
    let talker = {
        let phone = Phone {
            sock: phone.sock.try_clone().unwrap(),
            ..phone
        };
        std::thread::spawn(move || {
            let mut enc = idn_iem::codec::Encoder::new(480, 64_000).unwrap();
            let start = Instant::now();
            let frames = 150u32;
            for k in 0..frames {
                let pcm: Vec<f32> = (0..480)
                    .map(|i| {
                        let t = (k as usize * 480 + i) as f32 / 48_000.0;
                        0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    })
                    .collect();
                let data = enc.encode(&pcm).unwrap().to_vec();
                phone.send(&Packet::Talk(wire::Talk {
                    flow: 0,
                    client: phone.client,
                    seq: k,
                    frame_samples: 480,
                    flags: if k + 1 == frames { wire::TALK_LAST } else { 0 },
                    data,
                }));
                // Paced at 10 ms a frame.
                let next = start + Duration::from_millis(10 * (u64::from(k) + 1));
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        })
    };
    wait_for("the talkback on channel 4", || level(&c) > -20.0);
    let st = c.state();
    assert_eq!(st["iem"]["mixes"][0]["talking"], true);
    wait_for("the input's name", || {
        c.state()["input_names"][4] == "Talkback Drums"
    });
    talker.join().unwrap();
    wait_for("silence after the button", || level(&c) < -60.0);
    assert_eq!(c.state()["iem"]["mixes"][0]["talking"], false);
    // A listening session that may not talk.
    let other = Phone::new(&c, 22);
    assert_eq!(
        other.subscribe(wire::WANT_MULTICAST, "", "").status,
        Status::Ok
    );
    other.send(&Packet::Talk(wire::Talk {
        flow: 0,
        client: 22,
        seq: 0,
        frame_samples: 480,
        flags: 0,
        data: vec![0; 10],
    }));
    assert_eq!(other.reply().status, Status::NotAllowed);
}

/// A snapshot holds the IEM mixes and marks only when saved with `include_iem`; its
/// recall then puts them back, else the desk's stay.
#[test]
fn snapshots_hold_iem_mixes_only_when_asked() {
    let c = console("IEM snapshot test", |cfg| cfg.iem.port = None);
    c.channel(1, &json!({"iem": true})).unwrap();
    let a = c.iem_mix_add("Alto", None).unwrap();
    c.iem_mix(&json!({"id": a, "master_db": -4.0, "channels": {"1": {"level_db": -8.0}}}))
        .unwrap();
    c.run(crate::Command::SnapshotSave {
        name: "with".into(),
        include_iem: Some(true),
    })
    .unwrap();
    c.snapshot_save("without").unwrap();
    // The IEM setup changes.
    c.channel(1, &json!({"iem": false})).unwrap();
    c.channel(2, &json!({"iem": true})).unwrap();
    c.iem_mix_delete(&a).unwrap();
    let b = c.iem_mix_add("Bass", None).unwrap();
    let marks = |c: &Console| {
        c.settings()
            .channels
            .iter()
            .map(|x| x.iem)
            .collect::<Vec<_>>()
    };
    let mixes = |c: &Console| {
        c.settings()
            .iem_mixes
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>()
    };
    c.snapshot_recall("without").unwrap();
    assert_eq!(marks(&c), [false, true, false, false]);
    assert_eq!(mixes(&c), std::slice::from_ref(&b));
    assert_eq!(c.state()["snapshot"]["changed"], false);
    c.snapshot_recall("with").unwrap();
    assert_eq!(marks(&c), [true, false, false, false]);
    assert_eq!(mixes(&c), std::slice::from_ref(&a));
    let m = &c.settings().iem_mixes[0];
    assert_eq!((m.master_db, m.channels["1"].level_db), (-4.0, -8.0));
    assert!(!c.settings().includes_iem);
    let st = c.state();
    assert_eq!(st["snapshot"]["changed"], false);
    assert_eq!(st["snapshots_iem"], json!(["with"]));
    // A mix's change shows the snapshot holding mixes as changed.
    c.iem_mix(&json!({"id": a, "master_db": 0.0})).unwrap();
    assert_eq!(c.state()["snapshot"]["changed"], true);
    assert!(
        c.command("snapshot_save", &json!({"name": "x", "include_iem": 1}))
            .is_err()
    );
}

/// The mixes' commands: add (an id from the name, or given), partial updates, delete;
/// at most 16, each with its own talkback input; every change a new state version.
#[test]
fn iem_mixes_are_kept_on_the_desk() {
    let c = console("IEM mixes test", |cfg| cfg.iem.port = None);
    let v0 = c.version();
    assert_eq!(c.iem_mix_add("Lead Vocal", None).unwrap(), "lead-vocal");
    assert!(c.version() > v0);
    assert_eq!(c.iem_mix_add("Lead Vocal", None).unwrap(), "lead-vocal-2");
    assert_eq!(c.iem_mix_add("Keys", Some("k1")).unwrap(), "k1");
    assert!(c.iem_mix_add("Keys", Some("k1")).is_err());
    assert!(c.iem_mix_add("Keys", Some("bad id")).is_err());
    assert!(c.iem_mix_add("  ", None).is_err());
    c.run(crate::Command::IemMix {
        id: "k1".into(),
        name: Some("Keys 1".into()),
        master_db: None,
        channels: Some(
            [(
                "2".to_owned(),
                Some(crate::engine::typed::IemSendPatch {
                    pan: Some(-0.5),
                    ..Default::default()
                }),
            )]
            .into(),
        ),
    })
    .unwrap();
    c.iem_mix(&json!({"id": "k1", "channels": {"2": {"level_db": -3.0}}}))
        .unwrap();
    let k = c.settings().iem_mixes[2].clone();
    assert_eq!(k.name, "Keys 1");
    assert_eq!(
        (k.channels["2"].pan, k.channels["2"].level_db),
        (-0.5, -3.0)
    );
    assert!(c.iem_mix(&json!({"id": "nope", "master_db": 0.0})).is_err());
    assert!(
        c.iem_mix(&json!({"id": "k1", "channels": {"9": {}}}))
            .is_err()
    );
    c.iem_mix_delete("lead-vocal").unwrap();
    assert!(c.iem_mix_delete("lead-vocal").is_err());
    let slots: Vec<usize> = c.settings().iem_mixes.iter().map(|m| m.talkback).collect();
    assert_eq!(slots, [2, 3]);
    // A new one takes the free talkback input.
    c.iem_mix_add("Drums", None).unwrap();
    assert_eq!(c.settings().iem_mixes[2].talkback, 1);
    for k in 0..13 {
        c.iem_mix_add(&format!("M{k}"), None).unwrap();
    }
    assert!(c.iem_mix_add("One too many", None).is_err());
    // Kept with the desk: in the state, with the talkback inputs.
    let st = c.state();
    assert_eq!(st["settings"]["iem_mixes"].as_array().unwrap().len(), 16);
    assert_eq!(st["iem"]["mixes"][0]["id"], "lead-vocal-2");
}

/// A phone that stops renewing its session is dropped at the timeout: an
/// `iem_phone_dropped` event with its mix and address (§M.27).
#[test]
fn a_phone_going_quiet_is_an_event() {
    let c = console("IEM drop test", |cfg| {
        cfg.iem.timeout = Duration::from_millis(600);
    });
    assert_eq!(c.iem_mix_add("Drums", None).unwrap(), "drums");
    let phone = Phone::new(&c, 31);
    assert_eq!(
        phone
            .subscribe(wire::WANT_UNICAST | wire::WANT_TALK, "drums", "")
            .status,
        Status::Ok
    );
    let peer = phone.sock.local_addr().unwrap().to_string();
    // No keepalive: gone after 600 ms.
    let mut found = None;
    wait_for("the iem_phone_dropped event", || {
        found = c
            .failure_events()
            .since(0)
            .0
            .into_iter()
            .find(|e| e["event"] == "iem_phone_dropped");
        found.is_some()
    });
    let e = found.unwrap();
    assert_eq!(
        (e["mix"].as_str(), e["peer"].as_str(), e["reason"].as_str()),
        (Some("drums"), Some(peer.as_str()), Some("timeout")),
        "{e}"
    );
    assert_eq!(e["client"], format!("{:016x}", 31));
    assert_eq!(c.state()["iem"]["phones"].as_array().map(Vec::len), Some(0));
}

/// The multicast copy leaves by the interface asked for (IP_MULTICAST_IF, idn-iem.md
/// §I.5), and the state says which.
#[test]
fn the_multicast_copy_leaves_by_the_interface_asked_for() {
    let lo = std::net::Ipv4Addr::LOCALHOST;
    let c = console("IEM interface test", |cfg| cfg.iem.interface = Some(lo));
    let sock = c.shared.iem.socket.clone().unwrap();
    assert_eq!(
        socket2::SockRef::from(&*sock).multicast_if_v4().unwrap(),
        lo
    );
    assert_eq!(
        c.state()["iem"]["multicast"]["interface"].as_str(),
        Some("127.0.0.1")
    );
    // Not asked: the default route's (or none on a machine without one), never 0.0.0.0.
    let c = console("IEM interface test 2", |_| {});
    let st = c.state();
    let i = &st["iem"]["multicast"]["interface"];
    assert!(i.is_null() || i.as_str() != Some("0.0.0.0"), "{i}");
}
