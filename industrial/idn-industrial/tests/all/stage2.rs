//! Stage 2 (§I.15) with a Site Manager, a Node and an MQTT 5 client as the application:
//! the local MQTT endpoint bridged both ways; module settings pushed from the Site Manager
//! (a Modbus device polled; a write refused where only a pushed file allowed it, done
//! where the machine's own file does, both reported); actions, HTTP polling, webhooks.

use crate::common;
use common::{dir, free_tcp, free_udp, wait};
use idn_common::sync::LockOk as _;

use bytes::Bytes;
use idn_industrial::modbus::sim;
use idn_industrial::modules::{MqttConfig, Supervisor};
use idn_industrial::node::{IndustrialNode, NodeConfig};
use idn_industrial::server::{Config, SiteManager};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, Publish, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

struct Hub {
    rt: tokio::runtime::Runtime,
    sm: SiteManager,
    mqtt: u16,
}

fn hub(d: &Path) -> Hub {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (udp, mqtt) = (free_udp(), free_tcp());
    let sm = rt
        .block_on(SiteManager::start(Config {
            data_dir: d.join("hub"),
            quic: Some(format!("127.0.0.1:{udp}").parse().unwrap()),
            public_urls: vec![format!("idn://127.0.0.1:{udp}")],
            mqtt: Some(format!("127.0.0.1:{mqtt}").parse().unwrap()),
        }))
        .unwrap();
    sm.broker().set_user("app", "password123").unwrap();
    Hub { rt, sm, mqtt }
}

/// An MQTT client (the application, or a local device with `login` None).
struct Client {
    rt: tokio::runtime::Runtime,
    c: AsyncClient,
    got: mpsc::UnboundedReceiver<Publish>,
    subacks: mpsc::UnboundedReceiver<()>,
    /// Publishes taken while waiting for another topic, in order (a machine's audit
    /// record comes before its command's answer, and must still be there after it).
    held: std::collections::VecDeque<Publish>,
}

impl Client {
    fn connect(port: u16, login: Option<(&str, &str)>) -> Client {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut o = MqttOptions::new(format!("c{}", free_tcp()), "127.0.0.1", port);
        if let Some((u, p)) = login {
            o.set_credentials(u, p);
        }
        let (c, mut el) = AsyncClient::new(o, 100);
        let (tx, got) = mpsc::unbounded_channel();
        let (stx, subacks) = mpsc::unbounded_channel();
        rt.spawn(async move {
            loop {
                match el.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let _ = tx.send(p);
                    }
                    Ok(Event::Incoming(Packet::SubAck(_))) => {
                        let _ = stx.send(());
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        });
        Client {
            rt,
            c,
            got,
            subacks,
            held: Default::default(),
        }
    }

    /// Subscribes, and waits for the broker's answer (a login is checked off the broker's
    /// threads and takes a while in a debug build: a machine can be sending before it).
    fn sub(&mut self, f: &str) {
        let (c, f) = (self.c.clone(), f.to_owned());
        self.rt
            .block_on(async move { c.subscribe(f, QoS::AtLeastOnce).await })
            .unwrap();
        let acks = &mut self.subacks;
        let ok = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), acks.recv())
                .await
                .ok()
                .flatten()
                .is_some()
        });
        assert!(ok, "no SUBACK");
    }

    fn publish(&self, topic: &str, payload: &[u8], retain: bool, props: PublishProperties) {
        let (c, t, p) = (self.c.clone(), topic.to_owned(), payload.to_vec());
        self.rt
            .block_on(async move {
                c.publish_with_properties(t, QoS::AtLeastOnce, retain, p, props)
                    .await
            })
            .unwrap();
    }

    fn next_on(&mut self, topic: &str, secs: u64) -> Publish {
        if let Some(i) = self.held.iter().position(|p| p.topic == topic.as_bytes()) {
            return self.held.remove(i).unwrap();
        }
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let p = self
                .rt
                .block_on(async {
                    tokio::time::timeout(left, self.got.recv())
                        .await
                        .ok()
                        .flatten()
                })
                .unwrap_or_else(|| panic!("nothing on {topic}"));
            if p.topic == topic.as_bytes() {
                return p;
            }
            self.held.push_back(p);
        }
    }

    /// A command to the machine; its answer's (status, payload).
    fn command(
        &mut self,
        machine: &str,
        kind_path: &str,
        body: serde_json::Value,
    ) -> (String, String) {
        let rt = format!("app/answers/{}", free_tcp());
        self.sub(&rt);
        self.publish(
            &format!("idn/{machine}/cmd/{kind_path}"),
            &serde_json::to_vec(&body).unwrap(),
            false,
            PublishProperties {
                response_topic: Some(rt.clone()),
                correlation_data: Some(Bytes::from_static(b"1")),
                ..Default::default()
            },
        );
        let a = self.next_on(&rt, 20);
        let st = a
            .properties
            .unwrap()
            .user_properties
            .into_iter()
            .find(|(k, _)| k == "status")
            .map(|(_, v)| v)
            .unwrap_or_default();
        (st, String::from_utf8_lossy(&a.payload).into_owned())
    }
}

/// What a new subscriber gets on `topic` (a retained message).
fn retained(h: &Hub, topic: &str, secs: u64) -> Publish {
    let mut c = Client::connect(h.mqtt, Some(("app", "password123")));
    c.sub(topic);
    c.next_on(topic, secs)
}

fn node(d: &Path, h: &Hub, name: &str) -> IndustrialNode {
    let n = IndustrialNode::start(NodeConfig {
        state_dir: d.join("node"),
        name: name.into(),
        version: "test".into(),
    })
    .unwrap();
    let mut tags = std::collections::BTreeMap::new();
    tags.insert("plant".into(), "PD".into());
    n.enrol(
        &h.sm
            .mint_token(None, tags, Duration::from_secs(60))
            .unwrap()
            .0,
    )
    .unwrap();
    wait("online", 20, || n.status().connected);
    n
}

#[test]
fn local_mqtt_is_bridged_both_ways() {
    let d = dir("bridge");
    let h = hub(&d);
    let mut app = Client::connect(h.mqtt, Some(("app", "password123")));
    app.sub("idn/+/local/#");
    let n = node(&d, &h, "Batch");
    let port = free_tcp();
    idn_industrial::bridge::start(
        &n,
        &d.join("node"),
        &MqttConfig {
            loopback: Some(format!("127.0.0.1:{port}")),
            ..Default::default()
        },
    )
    .unwrap();
    // A device on the machine, no login on loopback.
    let mut dev = Client::connect(port, None);
    dev.sub("setpoint/#");
    dev.publish("plc/temp", b"21.5", true, PublishProperties::default());
    let up = app.next_on("idn/batch/local/plc/temp", 15);
    assert_eq!(&up.payload[..], b"21.5");
    // Retained at the Site Manager, as the device published it.
    let r = retained(&h, "idn/batch/local/plc/temp", 10);
    assert!(r.retain && &r.payload[..] == b"21.5");
    app.publish(
        "idn/batch/to/setpoint/silo3",
        b"80",
        false,
        PublishProperties::default(),
    );
    let down = dev.next_on("setpoint/silo3", 15);
    assert_eq!(&down.payload[..], b"80");
    n.stop();
    h.rt.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn pushed_modbus_settings_poll_but_only_the_machines_own_files_allow_writes() {
    let d = dir("modbus");
    let h = hub(&d);
    let mut app = Client::connect(h.mqtt, Some(("app", "password123")));
    app.sub("idn/+/v/#");
    app.sub("idn/+/m/audit.modbus");
    let s = Arc::new(sim::Sim::default());
    s.regs.lock_ok().insert(100, 725);
    let addr = sim::start(s.clone());
    let n = node(&d, &h, "Batch");
    let sup = Supervisor::start(n.clone(), d.join("node"));
    let _cmds = idn_industrial::modules::dispatch(&n, sup.clone());
    let fp = h.sm.machines().unwrap()[0].fingerprint.clone();
    // Pushed for this machine (by its tag): a device, a point it tries to make writable.
    let warnings = h
        .rt
        .block_on(h.sm.put_module(
            "tag:plant=PD",
            "plc",
            &format!(
                "[[modbus]]\nname = \"plc\"\naddress = \"{addr}\"\nevery_ms = 200\n[[modbus.points]]\nname = \"level\"\naddress = 100\nscale = 0.1\nwritable = true\n"
            ),
        ))
        .unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("writable is ignored")),
        "{warnings:?}"
    );
    // Actions can't be pushed at all.
    let e =
        h.rt.block_on(h.sm.put_module(
            &format!("machine:{fp}"),
            "x",
            "[[action]]\nname = \"a\"\nrun = [\"true\"]\n",
        ))
        .unwrap_err();
    assert!(e.to_string().contains("actions can only"), "{e}");
    let v = app.next_on("idn/batch/v/plc/level", 20);
    let v: serde_json::Value = serde_json::from_slice(&v.payload).unwrap();
    assert_eq!(v["value"], 72.5);
    s.regs.lock_ok().insert(100, 730);
    let v = app.next_on("idn/batch/v/plc/level", 20);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&v.payload).unwrap()["value"],
        73.0
    );
    // A write the pushed file "allowed": refused, reported.
    let (st, why) = app.command(
        "batch",
        "modbus/write",
        serde_json::json!({"device": "plc", "point": "level", "value": 80}),
    );
    assert_ne!(st, "0", "{why}");
    assert!(why.contains("isn't writable"), "{why}");
    let audit = app.next_on("idn/batch/m/audit.modbus", 10);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&audit.payload).unwrap()["ok"],
        false
    );
    assert_eq!(s.regs.lock_ok()[&100], 730);
    // The machine's own file: written.
    std::fs::create_dir_all(d.join("node/modules")).unwrap();
    std::fs::write(
        d.join("node/modules/local.toml"),
        format!(
            "[[modbus]]\nname = \"line\"\naddress = \"{addr}\"\n[[modbus.points]]\nname = \"setpoint\"\naddress = 101\nwritable = true\nmin = 0\nmax = 500\n"
        ),
    )
    .unwrap();
    sup.reload();
    let (st, why) = app.command(
        "batch",
        "modbus/write",
        serde_json::json!({"device": "line", "point": "setpoint", "value": 50}),
    );
    assert_eq!(st, "0", "{why}");
    assert_eq!(s.regs.lock_ok()[&101], 50);
    let (st, _) = app.command(
        "batch",
        "modbus/write",
        serde_json::json!({"device": "line", "point": "setpoint", "value": 900}),
    );
    assert_ne!(st, "0", "over max");
    n.stop();
    h.rt.shutdown_timeout(Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn actions_polls_and_webhooks() {
    let d = dir("actions");
    let h = hub(&d);
    let mut app = Client::connect(h.mqtt, Some(("app", "password123")));
    app.sub("idn/+/v/#");
    app.sub("idn/+/m/#");
    // A web service to poll.
    let web = free_tcp();
    idn_industrial::node::runtime().spawn(async move {
        let r = axum::Router::new().route(
            "/now",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"current": {"temp": 12.5}}))
            }),
        );
        let l = tokio::net::TcpListener::bind(("127.0.0.1", web))
            .await
            .unwrap();
        axum::serve(l, r).await.unwrap();
    });
    std::fs::create_dir_all(d.join("node/modules")).unwrap();
    std::fs::write(
        d.join("node/modules/local.toml"),
        format!(
            r#"
            [[action]]
            name = "greet"
            run = ["sh", "-c", "printf 'hello %s' \"$IDN_PARAM_WHO\""]
            params = {{ who = "[a-z]{{1,10}}" }}

            [[http]]
            name = "weather"
            url = "http://127.0.0.1:{web}/now"
            every_s = 1
            values = [ {{ name = "outside/temp_c", pointer = "/current/temp" }} ]

            [[webhook]]
            name = "scale"
            type = "scale.weight"
            token = "scale-secret"
            "#
        ),
    )
    .unwrap();
    let n = node(&d, &h, "Batch");
    let sup = Supervisor::start(n.clone(), d.join("node"));
    let cmds = idn_industrial::modules::dispatch(&n, sup.clone());
    let (st, out) = app.command(
        "batch",
        "action/run",
        serde_json::json!({"name": "greet", "params": {"who": "plant"}}),
    );
    assert_eq!(st, "0", "{out}");
    assert!(out.contains("hello plant"), "{out}");
    let (st, out) = app.command(
        "batch",
        "action/run",
        serde_json::json!({"name": "greet", "params": {"who": "x; reboot"}}),
    );
    assert_ne!(st, "0");
    assert!(out.contains("isn't allowed"), "{out}");
    let v = retained(&h, "idn/batch/v/outside/temp_c", 15);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&v.payload).unwrap()["value"],
        12.5
    );
    // The webhook through the local API.
    let port = free_tcp();
    let router = idn_industrial::local_api::router_with(n.clone(), "api".into(), cmds, Some(sup));
    idn_industrial::node::runtime().spawn(async move {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        axum::serve(l, router).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    let post = |token: &str| {
        let (st, _) = idn_industrial::httpc::request(
            "POST",
            &format!("http://127.0.0.1:{port}/webhooks/scale?token={token}"),
            &[("Content-Type".into(), "application/json".into())],
            Some(br#"{"kg": 9100}"#),
            false,
            Duration::from_secs(5),
        )
        .unwrap();
        st
    };
    assert_eq!(post("wrong"), 401);
    assert_eq!(post("scale-secret"), 200);
    let m = app.next_on("idn/batch/m/scale.weight", 15);
    assert_eq!(&m.payload[..], br#"{"kg": 9100}"#);
    n.stop();
    h.rt.shutdown_timeout(Duration::from_secs(2));
}

/// The hold for messages no application has taken (§I.7.3): 7 days unless set, kept on
/// the volume across a restart, 0 = until taken, out of range refused.
#[test]
fn the_hold_is_a_site_setting() {
    let d = dir("hold");
    {
        let h = hub(&d);
        assert_eq!(h.sm.hold_days(), 7);
        h.sm.set_hold_days(0).unwrap();
        assert!(h.sm.set_hold_days(4000).is_err());
        assert_eq!(h.sm.hold_days(), 0);
    }
    let h = hub(&d);
    assert_eq!(h.sm.hold_days(), 0, "kept across a restart");
    h.sm.set_hold_days(30).unwrap();
    assert_eq!(h.sm.hold_days(), 30);
    drop(h);
}

/// An update end to end up to installing (§I.15): a release of IDN Industrial signed by a
/// key the test trusts, kept by the Site Manager, offered to the machine for its next
/// start; the Node fetches its package over the link in pieces, checks it, and stages it,
/// and says so (`node/update`). A release another key signed, or IDN Media's, isn't
/// taken; a pinned machine isn't offered one for every machine.
#[test]
fn an_update_reaches_the_machine_and_is_staged() {
    use idn_open_protocol::release;
    let d = dir("update");
    let h = hub(&d);
    let n = node(&d, &h, "Update test");
    let sup = Supervisor::start(n.clone(), d.join("node"));
    let _cmds = idn_industrial::modules::dispatch(&n, sup.clone());
    // A package bigger than one piece (1 MiB).
    let pkg = d.join("idn-industrial-linux.tar.gz");
    let body: Vec<u8> = (0..2_500_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&pkg, &body).unwrap();
    let key = d.join("test.key");
    let public = release::keygen(&key, None).unwrap();
    let make = |product: &str, version: &str, out: &Path| {
        release::make(
            &key,
            None,
            product,
            version,
            "a test",
            &[(release::platform(), pkg.clone())],
            out,
        )
        .unwrap()
    };
    let (good, media) = (d.join("good.idnrelease"), d.join("media.idnrelease"));
    make(release::INDUSTRIAL, "0.2.0-Test9", &good);
    make(release::MEDIA, "0.2.0-TestM", &media);
    // Not trusted yet: refused.
    let up = d.join("upload.part");
    std::fs::copy(&good, &up).unwrap();
    assert!(h.sm.add_release(&up, "test").is_err());
    release::trust_for_tests(public);
    std::fs::copy(&media, &up).unwrap();
    let e = h.sm.add_release(&up, "test").unwrap_err();
    assert!(e.to_string().contains("not of industrial"), "{e}");
    std::fs::copy(&good, &up).unwrap();
    h.sm.add_release(&up, "test").unwrap();
    assert_eq!(h.sm.releases()[0]["version"], "0.2.0-Test9");
    // Pinned: not offered to every machine; named, it is.
    let fp = h.sm.machines().unwrap()[0].fingerprint.clone();
    h.sm.set_pin(&fp, true, "test").unwrap();
    let _g = h.rt.enter();
    assert!(
        h.sm.offer_update("0.2.0-Test9", "all", "restart", "test")
            .unwrap()
            .is_empty()
    );
    let to =
        h.sm.offer_update("0.2.0-Test9", &format!("machine:{fp}"), "restart", "test")
            .unwrap();
    assert_eq!(to, ["Update test"]);
    // The machine fetches it, checks it, and keeps it for its next start.
    let staged = d.join("node").join("updates").join("staged.json");
    wait("staged", 60, || staged.exists());
    let s: serde_json::Value = serde_json::from_slice(&std::fs::read(&staged).unwrap()).unwrap();
    assert_eq!(s["version"], "0.2.0-Test9");
    let got = std::fs::read(s["package"].as_str().unwrap()).unwrap();
    assert_eq!(got, body, "the package as it was signed");
    let topic = h.sm.machines().unwrap()[0].topic.clone();
    let mut said = String::new();
    wait("node/update staged", 20, || {
        let p = retained(&h, &format!("idn/{topic}/v/node/update"), 5);
        said = String::from_utf8_lossy(&p.payload).into_owned();
        said.contains("\"staged\"")
    });
    assert!(said.contains("0.2.0-Test9"), "{said}");
    // The offer is answered (in the audit) and no longer held.
    wait("released", 10, || h.sm.held_commands().is_empty());
    // "Send me the logs": the Node's logs come back, the update helper's among them once
    // it has one.
    let (st, body) =
        h.rt.block_on(h.sm.command(&fp, "idn.logs", b"{}".to_vec(), Duration::from_secs(20)))
            .unwrap();
    assert_eq!(st, 0);
    let logs: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(logs["files"].is_object(), "{logs}");
    n.stop();
}
