//! The application server's side (§I.7), with an MQTT 5 client as the application:
//! messages and values arrive at once, wait for a persistent session that was away,
//! commands reach the machine's program and its answers come back, the machine's
//! requests are answered by the application; logins and topics only the Site Manager
//! writes are refused.

use crate::common;
use common::{dir, free_tcp, free_udp, wait};

use bytes::Bytes;
use idn_industrial::node::{IndustrialNode, NodeConfig, Value};
use idn_industrial::server::{Config, SiteManager};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

struct Hub {
    rt: tokio::runtime::Runtime,
    sm: SiteManager,
    mqtt: u16,
    udp: u16,
}

impl Hub {
    fn start(dir: &Path) -> Hub {
        Self::start_on(dir, free_udp(), free_tcp())
    }

    /// On given ports (a restart on the same ones).
    fn start_on(dir: &Path, udp: u16, mqtt: u16) -> Hub {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let sm = rt
            .block_on(SiteManager::start(Config {
                data_dir: dir.to_owned(),
                quic: Some(format!("127.0.0.1:{udp}").parse().unwrap()),
                public_urls: vec![format!("idn://127.0.0.1:{udp}")],
                mqtt: Some(format!("127.0.0.1:{mqtt}").parse().unwrap()),
            }))
            .unwrap();
        sm.broker().set_user("dispatch", "password123").unwrap();
        Hub { rt, sm, mqtt, udp }
    }

    fn stop(self) {
        let Hub { rt, sm, .. } = self;
        drop(sm);
        rt.shutdown_timeout(Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The application: an MQTT client whose received publishes come out of a channel.
struct App {
    rt: tokio::runtime::Runtime,
    client: AsyncClient,
    got: mpsc::UnboundedReceiver<rumqttc::v5::mqttbytes::v5::Publish>,
    acks: mpsc::UnboundedReceiver<Packet>,
    /// Publishes taken while waiting for another topic, in order: the next waits see them
    /// first.
    held: std::collections::VecDeque<rumqttc::v5::mqttbytes::v5::Publish>,
}

impl App {
    fn connect(port: u16, id: &str, password: &str, persistent: bool) -> App {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut o = MqttOptions::new(id, "127.0.0.1", port);
        o.set_credentials("dispatch", password);
        o.set_clean_start(!persistent);
        if persistent {
            o.set_session_expiry_interval(Some(3600));
        }
        o.set_keep_alive(Duration::from_secs(10));
        let (client, mut el) = AsyncClient::new(o, 100);
        let (tx, got) = mpsc::unbounded_channel();
        let (atx, acks) = mpsc::unbounded_channel();
        rt.spawn(async move {
            loop {
                match el.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let _ = tx.send(p);
                    }
                    Ok(Event::Incoming(
                        p @ (Packet::ConnAck(_) | Packet::PubAck(_) | Packet::SubAck(_)),
                    )) => {
                        let _ = atx.send(p);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let _ = atx.send(Packet::PingResp(rumqttc::v5::mqttbytes::v5::PingResp));
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        App {
            rt,
            client,
            got,
            acks,
            held: Default::default(),
        }
    }

    /// Subscribes, and waits for the broker's answer: a machine started after this sends
    /// to a subscription already there (logins are checked off the broker's threads, so a
    /// machine can join while the application is still logging in).
    fn sub(&mut self, f: &str) {
        let c = self.client.clone();
        let f = f.to_owned();
        self.rt
            .block_on(async move { c.subscribe(f, QoS::AtLeastOnce).await })
            .unwrap();
        let acks = &mut self.acks;
        let ok = self.rt.block_on(async {
            loop {
                match tokio::time::timeout(Duration::from_secs(10), acks.recv()).await {
                    Ok(Some(Packet::SubAck(_))) => break true,
                    Ok(Some(_)) => continue,
                    _ => break false,
                }
            }
        });
        assert!(ok, "no SUBACK");
    }

    fn next(&mut self, secs: u64) -> Option<rumqttc::v5::mqttbytes::v5::Publish> {
        if let Some(p) = self.held.pop_front() {
            return Some(p);
        }
        self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(secs), self.got.recv())
                .await
                .ok()
                .flatten()
        })
    }

    /// The next publish on `topic` (others kept for later waits).
    fn next_on(&mut self, topic: &str, secs: u64) -> rumqttc::v5::mqttbytes::v5::Publish {
        if let Some(i) = self.held.iter().position(|p| p.topic == topic.as_bytes()) {
            return self.held.remove(i).unwrap();
        }
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = end
                .saturating_duration_since(Instant::now())
                .as_secs()
                .max(1);
            let got = &mut self.got;
            let p = self
                .rt
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(left), got.recv())
                        .await
                        .ok()
                        .flatten()
                })
                .unwrap_or_else(|| panic!("nothing on {topic}"));
            if p.topic == topic.as_bytes() {
                return p;
            }
            self.held.push_back(p);
            assert!(Instant::now() < end, "nothing on {topic}");
        }
    }

    fn publish(&self, topic: &str, payload: &[u8], props: PublishProperties) {
        let c = self.client.clone();
        let (t, p) = (topic.to_owned(), payload.to_vec());
        self.rt
            .block_on(async move {
                c.publish_with_properties(t, QoS::AtLeastOnce, false, p, props)
                    .await
            })
            .unwrap();
    }

    fn close(self) {
        let c = self.client.clone();
        let _ = self.rt.block_on(async move { c.disconnect().await });
        std::thread::sleep(Duration::from_millis(200));
        self.rt.shutdown_timeout(Duration::from_secs(1));
    }
}

fn node(d: &Path, hub: &Hub) -> IndustrialNode {
    let n = IndustrialNode::start(NodeConfig {
        state_dir: d.join("node"),
        name: "BATCH".into(),
        version: "test".into(),
    })
    .unwrap();
    let mut tags = BTreeMap::new();
    tags.insert("plant".into(), "PD".into());
    let (tok, _) = hub
        .sm
        .mint_token(Some("Plant 1 batch".into()), tags, Duration::from_secs(60))
        .unwrap();
    n.enrol(&tok).unwrap();
    wait("online", 20, || n.status().connected);
    n
}

#[test]
fn messages_and_values_arrive_at_once_and_wait_for_an_application_that_was_away() {
    let d = dir("deliver");
    let hub = Hub::start(&d.join("hub"));
    let mut app = App::connect(hub.mqtt, "dispatch", "password123", true);
    app.sub("idn/+/m/#");
    app.sub("idn/+/v/#");
    app.sub("idn/+/status");
    let n = node(&d, &hub);
    // Enrolled (offline), then online.
    let s = loop {
        let st = app.next_on("idn/plant-1-batch/status", 10);
        let s: serde_json::Value = serde_json::from_slice(&st.payload).unwrap();
        if s["online"] == true {
            break s;
        }
    };
    assert_eq!(s["tags"]["plant"], "PD");
    n.send("batch.ticket", b"{\"ticket\":1001}".to_vec())
        .unwrap();
    let p = app.next_on("idn/plant-1-batch/m/batch.ticket", 10);
    assert_eq!(&p.payload[..], b"{\"ticket\":1001}");
    let user = p.properties.unwrap().user_properties;
    assert!(user.iter().any(|(k, _)| k == "id"));
    n.send_values(&[Value {
        name: "silo/3/level".into(),
        value: serde_json::json!(72.5),
        time_ms: 0,
        quality: "good".into(),
    }])
    .unwrap();
    let v = app.next_on("idn/plant-1-batch/v/silo/3/level", 10);
    let v: serde_json::Value = serde_json::from_slice(&v.payload).unwrap();
    assert_eq!(v["value"], 72.5);
    // The application goes away; the machine goes on sending.
    app.close();
    for i in 0..20 {
        n.send("batch.ticket", format!("{i}").into_bytes()).unwrap();
    }
    wait("sent", 20, || n.status().queued == 0);
    std::thread::sleep(Duration::from_millis(300));
    // Back: everything that was sent meanwhile, in order.
    let mut app = App::connect(hub.mqtt, "dispatch", "password123", true);
    let mut got = Vec::new();
    while got.len() < 20 {
        let p = app.next(10).expect("the waiting messages");
        if p.topic == b"idn/plant-1-batch/m/batch.ticket".as_slice() {
            got.push(String::from_utf8(p.payload.to_vec()).unwrap());
        }
    }
    assert_eq!(got, (0..20).map(|i| i.to_string()).collect::<Vec<_>>());
    app.close();
    n.stop();
    hub.rt.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn commands_and_requests_go_both_ways() {
    let d = dir("commands");
    let hub = Hub::start(&d.join("hub"));
    let mut app = App::connect(hub.mqtt, "dispatch", "password123", false);
    app.sub("app/answers");
    app.sub("idn/+/req/#");
    let n = node(&d, &hub);
    let mut cmds = n.commands().unwrap();
    idn_industrial::node::runtime().spawn(async move {
        while let Some(c) = cmds.recv().await {
            let p = c.payload.to_ascii_uppercase();
            c.answer(true, p);
        }
    });
    // A command from the application; the answer comes to its response topic.
    app.publish(
        "idn/plant-1-batch/cmd/plant/scan",
        b"customers",
        PublishProperties {
            response_topic: Some("app/answers".into()),
            correlation_data: Some(Bytes::from_static(&[7])),
            ..Default::default()
        },
    );
    let a = app.next_on("app/answers", 10);
    assert_eq!(&a.payload[..], b"CUSTOMERS");
    let pr = a.properties.unwrap();
    assert_eq!(pr.correlation_data.as_deref(), Some(&[7u8][..]));
    assert!(
        pr.user_properties
            .iter()
            .any(|(k, v)| k == "status" && v == "0")
    );
    // A request from the machine; the application answers.
    let n2 = n.clone();
    let asking = std::thread::spawn(move || {
        n2.request(
            "customers.lookup",
            b"C100".to_vec(),
            Duration::from_secs(10),
        )
    });
    let q = app.next_on("idn/plant-1-batch/req/customers.lookup", 10);
    assert_eq!(&q.payload[..], b"C100");
    let rt = q.properties.unwrap().response_topic.unwrap();
    app.publish(&rt, b"Acme Concrete", PublishProperties::default());
    let (ok, answer) = asking.join().unwrap().unwrap();
    assert!(ok);
    assert_eq!(answer, b"Acme Concrete");
    app.close();
    n.stop();
    hub.rt.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn logins_are_checked_and_machine_topics_are_the_site_managers() {
    let d = dir("acl");
    let hub = Hub::start(&d.join("hub"));
    // A wrong password: no session.
    let mut bad = App::connect(hub.mqtt, "intruder", "wrong-password", false);
    let first = bad
        .rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), bad.acks.recv()).await });
    assert!(
        !matches!(first, Ok(Some(Packet::ConnAck(ref c))) if c.code == rumqttc::v5::mqttbytes::v5::ConnectReturnCode::Success),
        "{first:?}"
    );
    bad.close();
    // Pretending to be a machine: refused.
    let mut app = App::connect(hub.mqtt, "dispatch", "password123", false);
    app.sub("idn/#");
    app.publish(
        "idn/plant-1-batch/m/batch.ticket",
        b"forged",
        PublishProperties::default(),
    );
    let ack = app.rt.block_on(async {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), app.acks.recv()).await {
                Ok(Some(Packet::PubAck(a))) => break Some(a),
                Ok(Some(_)) => continue,
                _ => break None,
            }
        }
    });
    assert_eq!(
        ack.map(|a| a.reason),
        Some(rumqttc::v5::mqttbytes::v5::PubAckReason::NotAuthorized)
    );
    assert!(app.next(1).is_none(), "the forged message went nowhere");
    app.close();
    hub.rt.shutdown_timeout(Duration::from_secs(2));
}

fn user_prop(p: &rumqttc::v5::mqttbytes::v5::Publish, k: &str) -> Option<String> {
    p.properties
        .as_ref()?
        .user_properties
        .iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.clone())
}

/// An application refuses an item (§I.7.5): kept with its reason and the item, counted
/// again if refused again, sent again on request, resolved (kept, marked), and still there
/// after a Site Manager restart.
#[test]
fn a_refused_item_becomes_a_dead_letter() {
    let d = dir("dead");
    let hub = Hub::start(&d.join("hub"));
    let mut app = App::connect(hub.mqtt, "dispatch", "password123", false);
    app.sub("idn/+/m/#");
    app.sub("idn/site/events");
    let n = node(&d, &hub);
    n.send("scada.silos", br#"{"version":2,"items":[]}"#.to_vec())
        .unwrap();
    let m = app.next_on("idn/plant-1-batch/m/scada.silos", 10);
    let id = user_prop(&m, "id").unwrap();
    // The machine's tags, stamped by the Site Manager as the item arrived.
    assert_eq!(user_prop(&m, "tags").as_deref(), Some("plant=PD"));
    // Taken, then refused: the item is still had.
    std::thread::sleep(Duration::from_millis(300));
    app.publish(
        &format!("idn/plant-1-batch/nack/{id}"),
        br#"{"code":"unsupported_version","reason":"unsupported version 2 for scada.silos"}"#,
        PublishProperties::default(),
    );
    // The site's events since subscribing (the machine's enrolment first): the dead letter's.
    let ev = loop {
        let ev = app.next_on("idn/site/events", 10);
        let ev: serde_json::Value = serde_json::from_slice(&ev.payload).unwrap();
        if ev["event"] == "dead_letter" {
            break ev;
        }
    };
    assert_eq!(ev["type"], "scada.silos");
    let list = hub.sm.dead_letters(false).unwrap();
    assert_eq!(list.len(), 1);
    let nr = list[0]["n"].as_u64().unwrap();
    assert_eq!(list[0]["code"], "unsupported_version");
    assert_eq!(list[0]["machine"], "plant-1-batch");
    assert_eq!(list[0]["kept"], true);
    let one = hub.sm.dead_letter(nr).unwrap().unwrap();
    assert_eq!(one["text"], r#"{"version":2,"items":[]}"#);
    // Refused again: the same dead letter, counted.
    hub.sm
        .refuse(&id, &serde_json::json!({"reason": "still"}), "test")
        .unwrap();
    assert_eq!(hub.sm.dead_letters(false).unwrap()[0]["times"], 2);
    // Sent again: the same message, marked as a retry.
    assert!(hub.sm.retry_dead(nr, "operator").unwrap());
    let again = app.next_on("idn/plant-1-batch/m/scada.silos", 10);
    assert_eq!(user_prop(&again, "id").unwrap(), id);
    assert_eq!(user_prop(&again, "retry").as_deref(), Some("1"));
    // Resolved: not open any more, kept.
    assert!(
        hub.sm
            .resolve_dead(nr, "operator", "fixed the parser")
            .unwrap()
    );
    assert!(hub.sm.dead_letters(false).unwrap().is_empty());
    // A refusal of an id never seen is kept too, without the item.
    let unknown = hub
        .sm
        .refuse(
            &"ab".repeat(16),
            &serde_json::json!({"reason": "??"}),
            "test",
        )
        .unwrap();
    assert!(
        hub.sm
            .dead_letters(true)
            .unwrap()
            .iter()
            .any(|d| d["n"] == unknown && d["kept"] == false)
    );
    assert!(
        hub.sm
            .refuse("nothex", &serde_json::json!({}), "test")
            .is_err()
    );
    app.close();
    n.stop();
    let (udp, mqtt) = (hub.udp, hub.mqtt);
    hub.stop();
    let hub = Hub::start_on(&d.join("hub"), udp, mqtt);
    let all = hub.sm.dead_letters(true).unwrap();
    assert_eq!(all.len(), 2, "kept across a restart");
    assert!(
        all.iter()
            .any(|x| x["n"] == nr && x["resolved"]["note"] == "fixed the parser")
    );
    hub.stop();
}

/// A command held across a Site Manager restart (§I.7.3): sent again after it, and the
/// machine's claim makes the program run it once; the answer reaches the application.
#[test]
fn a_held_command_survives_a_restart_and_runs_once() {
    let d = dir("held");
    let hub = Hub::start(&d.join("hub"));
    let n = node(&d, &hub);
    let runs = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let r2 = runs.clone();
    let mut cmds = n.commands().unwrap();
    idn_industrial::node::runtime().spawn(async move {
        while let Some(c) = cmds.recv().await {
            r2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // A slow program: the Site Manager restarts meanwhile.
            tokio::time::sleep(Duration::from_secs(4)).await;
            c.answer(true, b"done".to_vec());
        }
    });
    let app = App::connect(hub.mqtt, "dispatch-cmd", "password123", false);
    app.publish(
        "idn/plant-1-batch/cmd/batch/start",
        b"load 7",
        PublishProperties {
            response_topic: Some("app/answers".into()),
            correlation_data: Some(Bytes::from_static(&[9])),
            message_expiry_interval: Some(120),
            ..Default::default()
        },
    );
    wait("the program has it", 10, || {
        runs.load(std::sync::atomic::Ordering::SeqCst) == 1
    });
    assert_eq!(hub.sm.held_commands().len(), 1);
    app.close();
    let (udp, mqtt) = (hub.udp, hub.mqtt);
    hub.stop();
    let hub = Hub::start_on(&d.join("hub"), udp, mqtt);
    assert_eq!(hub.sm.held_commands().len(), 1, "held across the restart");
    let mut app = App::connect(hub.mqtt, "dispatch-cmd2", "password123", false);
    app.sub("app/answers");
    let a = app.next_on("app/answers", 40);
    assert_eq!(&a.payload[..], b"done");
    assert_eq!(user_prop(&a, "status").as_deref(), Some("0"));
    assert_eq!(
        a.properties.unwrap().correlation_data.as_deref(),
        Some(&[9u8][..])
    );
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "run once"
    );
    wait("released", 5, || hub.sm.held_commands().is_empty());
    app.close();
    n.stop();
    hub.stop();
}

/// Re-tagging a machine (§I.7.2): what it sends from then on carries the new tags; an item
/// that was waiting for an application keeps the tags it arrived with.
#[test]
fn a_retag_doesnt_change_items_already_waiting() {
    let d = dir("retag");
    let hub = Hub::start(&d.join("hub"));
    // A persistent application, away while the first item arrives.
    let mut app = App::connect(hub.mqtt, "dispatch-tags", "password123", true);
    app.sub("idn/+/m/#");
    std::thread::sleep(Duration::from_millis(300));
    app.close();
    let n = node(&d, &hub);
    n.send("scada.silos", b"before".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let fp = hub.sm.machines().unwrap()[0].fingerprint.clone();
    let mut tags = BTreeMap::new();
    tags.insert("plant".to_owned(), "NS".to_owned());
    hub.rt.block_on(hub.sm.set_tags(&fp, tags)).unwrap();
    n.send("scada.silos", b"after".to_vec()).unwrap();
    let mut app = App::connect(hub.mqtt, "dispatch-tags", "password123", true);
    let mut got = Vec::new();
    while got.len() < 2 {
        let p = app.next(15).expect("both items");
        if p.topic == b"idn/plant-1-batch/m/scada.silos".as_slice() {
            got.push((
                String::from_utf8_lossy(&p.payload).into_owned(),
                user_prop(&p, "tags").unwrap_or_default(),
            ));
        }
    }
    assert_eq!(
        got,
        [
            ("before".to_owned(), "plant=PD".to_owned()),
            ("after".to_owned(), "plant=NS".to_owned())
        ]
    );
    // A bad tag is refused.
    let mut bad = BTreeMap::new();
    bad.insert("plant".to_owned(), "P D".to_owned());
    assert!(hub.rt.block_on(hub.sm.set_tags(&fp, bad)).is_err());
    app.close();
    n.stop();
    hub.stop();
}
