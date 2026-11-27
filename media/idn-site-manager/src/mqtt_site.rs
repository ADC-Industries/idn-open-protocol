//! The site's MQTT broker (idn-node.md §N.34): where the site's programs
//! (the switcher's state and commands, §S.33) and outside systems (Companion, Home
//! Assistant, Node-RED, a lighting or building controller) meet. Off until turned on on
//! the panel; a login for each client from another machine (the panel's MQTT card),
//! programs on this machine without one (loopback). `idn/site/…` is the Site Manager's own:
//!
//! - `idn/site/machines` (retained): `[{"name", "topic", "fingerprint", "kind", "online"}]`;
//! - `idn/site/machine/<topic>` (retained): one machine as the panel shows it (its Node's
//!   report: programs, clock, links, video);
//! - `idn/site/events`: `{"event": "online" | "offline" | "joined" | "left", "machine",
//!   "time_ms"}` as they happen.
//!
//! Every other topic is open to logged-in clients (`idn/<machine>/switcher/…` and the
//! like are the programs' own, by convention).

use idn_common::sync::LockOk as _;
use idn_mqtt::{Broker, Msg, Role};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The broker's settings (`mqtt.json` in the Site Manager's folder).
fn settings_path(dir: &Path) -> PathBuf {
    dir.join("mqtt.json")
}

/// A machine's name as a topic level, as the switcher's (§S.33): each character on its
/// own, an ASCII letter in lower case, a digit kept, anything else `-` (one for one); `-`
/// trimmed from both ends; nothing left: `machine`. (Lower-casing the whole name first
/// made `İ` two characters and the Kelvin sign an ASCII `k`.)
pub fn topic_name(name: &str) -> String {
    let t: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let t = t.trim_matches('-');
    if t.is_empty() {
        "machine".into()
    } else {
        t.to_owned()
    }
}

struct Running {
    listen: SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

/// The site's broker: off, or listening.
/// Where IDN Voice's commands go (topic, payload): set by Voice once it runs.
pub type VoiceFn = Box<dyn Fn(&str, &[u8]) + Send + Sync>;
type VoiceHook = Arc<Mutex<Option<VoiceFn>>>;

pub struct SiteMqtt {
    /// IDN Voice's commands from the broker (§N.35).
    voice: VoiceHook,
    dir: PathBuf,
    /// (Shut down safely wherever the Site Manager's last reference goes.)
    rt: crate::OwnedRuntime,
    broker: Broker,
    running: Mutex<Option<Running>>,
    /// The machines as last published (topic → JSON), to publish only what changed.
    said: Mutex<BTreeMap<String, String>>,
    online: Mutex<BTreeMap<String, bool>>,
}

impl SiteMqtt {
    /// Opens the broker's store; listens if the settings say so.
    pub fn new(dir: &Path) -> io::Result<SiteMqtt> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("idn-site-mqtt")
            .enable_all()
            .build()?;
        let (broker, mut hooks) = {
            let _g = rt.enter();
            Broker::open_as(&dir.join("mqtt.redb"), Role::Media)?
        };
        // In this role only IDN Voice's commands are handed over (`idn/site/voice/cmd/…`).
        let voice: VoiceHook = Arc::new(Mutex::new(None));
        let v2 = voice.clone();
        rt.spawn(async move {
            while let Some(h) = hooks.recv().await {
                if let idn_mqtt::Hook::Published(m) = h
                    && let Some(f) = v2.lock_ok().as_ref()
                {
                    f(&m.topic, &m.payload);
                }
            }
        });
        let m = SiteMqtt {
            voice,
            dir: dir.to_owned(),
            rt: crate::OwnedRuntime::new(rt),
            broker,
            running: Mutex::new(None),
            said: Mutex::new(BTreeMap::new()),
            online: Mutex::new(BTreeMap::new()),
        };
        let s = m.settings();
        if s["on"].as_bool() == Some(true)
            && let Err(e) = m.listen(s["listen"].as_str().unwrap_or("0.0.0.0:1883"))
        {
            idn_open_protocol::log::log(&format!("site MQTT: {e}"));
        }
        Ok(m)
    }

    /// `{"on", "listen", "users", "clients", "listening"}`.
    pub fn settings(&self) -> Value {
        // (A damaged file is set aside and said, not read as "off" and saved over.)
        let saved = idn_common::fsutil::load_json::<Value>(&settings_path(&self.dir))
            .unwrap_or_else(|e| {
                idn_open_protocol::log::log(&format!("site MQTT: {e}"));
                None
            });
        let mut v = saved.unwrap_or_else(|| json!({ "on": false, "listen": "0.0.0.0:1883" }));
        v["users"] = json!(self.broker.users().unwrap_or_default());
        v["clients"] = json!(self.broker.clients());
        v["listening"] = json!(
            self.running
                .lock_ok()
                .as_ref()
                .map(|r| r.listen.to_string())
        );
        v
    }

    fn listen(&self, addr: &str) -> io::Result<()> {
        let addr: SocketAddr = addr.parse().map_err(|_| {
            idn_common::err::invalid_input("listen: an address and port, e.g. 0.0.0.0:1883")
        })?;
        // On already at that address: nothing to do. Elsewhere on the same port: the old
        // listener let go first (bound first, the new one found the port in use, and "On"
        // failed while the broker was on; bug round F, F6-5), and taken again if the new
        // address can't be.
        let old = {
            let mut r = self.running.lock_ok();
            if r.as_ref().is_some_and(|x| x.listen == addr) {
                return Ok(());
            }
            r.take()
        };
        let bind = |a: SocketAddr| {
            self.rt.block_on(async {
                // (An aborted accept lets its listener go when the runtime next drops it.)
                let mut tries = 0;
                loop {
                    match tokio::net::TcpListener::bind(a).await {
                        Err(e) if e.kind() == io::ErrorKind::AddrInUse && tries < 20 => {
                            tries += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        r => return r,
                    }
                }
            })
        };
        let old_listen = old.as_ref().map(|o| o.listen);
        if let Some(o) = old {
            o.accept.abort();
        }
        let l = match bind(addr) {
            Ok(l) => l,
            Err(e) => {
                if let Some(a) = old_listen {
                    let _ = self.listen(&a.to_string());
                }
                return Err(io::Error::new(
                    e.kind(),
                    format!("the MQTT port {addr}: {e}"),
                ));
            }
        };
        let listen = l.local_addr()?;
        let b = self.broker.clone();
        let accept = self.rt.spawn(async move {
            // One failed accept (a client reset before it was taken, too many open
            // files) isn't the end of the broker: it stopped for good, the panel still
            // saying it listened (deep bug run 2026-11-09).
            loop {
                let (s, from) = match l.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        idn_open_protocol::log::log(&format!("mqtt: a connection not taken: {e}"));
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let _ = s.set_nodelay(true);
                let b = b.clone();
                tokio::spawn(async move {
                    // Programs on this machine without a login; everyone else with one.
                    if from.ip().is_loopback() {
                        b.serve_open(s, from.to_string()).await
                    } else {
                        b.serve(s, from.to_string()).await
                    }
                });
            }
        });
        if let Some(old) = self.running.lock_ok().replace(Running { listen, accept }) {
            old.accept.abort();
        }
        idn_open_protocol::log::log(&format!("site MQTT: listening on {listen}"));
        Ok(())
    }

    /// Turns the broker on (at `listen`) or off, and keeps it so.
    pub fn set(&self, on: bool, listen: &str) -> io::Result<()> {
        let listen = if listen.trim().is_empty() {
            "0.0.0.0:1883"
        } else {
            listen.trim()
        };
        if on {
            self.listen(listen)?;
        } else if let Some(old) = self.running.lock_ok().take() {
            // Off means off: no new connections, and the connected ones told and closed
            // (lab 395-A: they stayed).
            old.accept.abort();
            self.broker.disconnect_all();
        }
        idn_common::fsutil::write_atomic(
            &settings_path(&self.dir),
            &serde_json::to_vec_pretty(&json!({ "on": on, "listen": listen })).unwrap_or_default(),
        )
    }

    /// Adds (or changes) a login.
    pub fn set_user(&self, name: &str, password: &str) -> io::Result<()> {
        self.broker.set_user(name, password)
    }

    pub fn remove_user(&self, name: &str) -> io::Result<bool> {
        self.broker.remove_user(name)
    }

    /// The site's machines from the panel's status (its `machines`): published where they
    /// changed, and an event for each machine that came, went, joined or left.
    pub fn publish_machines(&self, status: &Value) {
        if self.running.lock_ok().is_none() {
            return;
        }
        let empty = Vec::new();
        let machines = status["machines"].as_array().unwrap_or(&empty);
        let mut list = Vec::new();
        let mut now: BTreeMap<String, String> = BTreeMap::new();
        let mut online_now: BTreeMap<String, bool> = BTreeMap::new();
        for m in machines.iter().filter(|m| m["revoked"] != true) {
            let topic = topic_name(m["name"].as_str().unwrap_or(""));
            let online = m["online"] == true;
            list.push(json!({
                "name": m["name"], "topic": topic, "fingerprint": m["fingerprint"],
                "kind": m["kind"], "online": online,
            }));
            now.insert(topic.clone(), m.to_string());
            online_now.insert(topic, online);
        }
        let publish = |topic: &str, payload: Vec<u8>, retain: bool| {
            let _ = self.broker.publish(&Msg::new(topic, payload, 1, retain));
        };
        let mut said = self.said.lock_ok();
        let mut was_online = self.online.lock_ok();
        let first = said.is_empty() && was_online.is_empty();
        let list_text = serde_json::to_vec(&list).unwrap_or_default();
        if said.get("") != Some(&String::from_utf8_lossy(&list_text).into_owned()) {
            publish("idn/site/machines", list_text.clone(), true);
            said.insert(
                String::new(),
                String::from_utf8_lossy(&list_text).into_owned(),
            );
        }
        let event = |what: &str, machine: &str| {
            publish(
                "idn/site/events",
                serde_json::to_vec(&json!({
                    "event": what, "machine": machine, "time_ms": idn_mqtt::now_ms(),
                }))
                .unwrap_or_default(),
                false,
            );
        };
        for (topic, text) in &now {
            if said.get(topic) != Some(text) {
                publish(
                    &format!("idn/site/machine/{topic}"),
                    text.clone().into_bytes(),
                    true,
                );
                said.insert(topic.clone(), text.clone());
            }
            let on = online_now[topic];
            match was_online.get(topic) {
                None if !first => event("joined", topic),
                Some(&before) if before != on => {
                    event(if on { "online" } else { "offline" }, topic)
                }
                _ => {}
            }
        }
        // Machines gone from the site: their retained topic cleared.
        let gone: Vec<String> = said
            .keys()
            .filter(|k| !k.is_empty() && !now.contains_key(*k))
            .cloned()
            .collect();
        for topic in gone {
            publish(&format!("idn/site/machine/{topic}"), Vec::new(), true);
            said.remove(&topic);
            event("left", &topic);
        }
        *was_online = online_now;
    }

    /// Publishes as the Site Manager (tests, and a program on it that wants a retained
    /// topic of the site's).
    /// Hands IDN Voice's commands (`idn/site/voice/cmd/…`) to `f` from now on.
    pub fn on_voice(&self, f: VoiceFn) {
        *self.voice.lock_ok() = Some(f);
    }

    pub fn broker(&self) -> &Broker {
        &self.broker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_names_are_ascii_one_character_each() {
        assert_eq!(topic_name("Sanctuary PC"), "sanctuary-pc");
        // The Kelvin sign is not a `k`; `İ` stays one character.
        assert_eq!(topic_name("\u{212A}iosk"), "iosk");
        assert_eq!(topic_name("A\u{130}B"), "a-b");
        assert_eq!(topic_name("a/b+c#"), "a-b-c");
        assert_eq!(topic_name(" -/ "), "machine");
    }

    /// The broker on loopback: a program connects without a login, gets the machines
    /// (retained) and the events; `idn/site/…` is the Site Manager's alone; other topics
    /// pass between clients.
    #[test]
    fn the_sites_broker_carries_machines_and_programs_topics() {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
        let dir = idn_common::testing::TempDir::new("idn-site-mqtt");
        let m = SiteMqtt::new(&dir).unwrap();
        m.set(true, "127.0.0.1:0").unwrap();
        let addr: SocketAddr = m.settings()["listening"].as_str().unwrap().parse().unwrap();
        m.publish_machines(&json!({"machines": [
            {"name": "Sanctuary PC", "fingerprint": "ab", "kind": "node", "online": true, "revoked": false}
        ]}));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let got = rt.block_on(async {
            let mut o = MqttOptions::new("test-program", "127.0.0.1", addr.port());
            o.set_keep_alive(std::time::Duration::from_secs(5));
            let (c, mut el) = AsyncClient::new(o, 20);
            c.subscribe("idn/#", QoS::AtLeastOnce).await.unwrap();
            // Not the Site Manager's: dropped. A program's own: passed on.
            c.publish("idn/site/machines", QoS::AtLeastOnce, false, "forged")
                .await
                .unwrap();
            c.publish(
                "idn/sanctuary-pc/switcher/program",
                QoS::AtLeastOnce,
                true,
                "Camera 1",
            )
            .await
            .unwrap();
            let mut got = Vec::new();
            let end = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while tokio::time::Instant::now() < end && got.len() < 3 {
                if let Ok(Ok(Event::Incoming(Incoming::Publish(p)))) =
                    tokio::time::timeout(std::time::Duration::from_millis(500), el.poll()).await
                {
                    got.push((
                        String::from_utf8_lossy(&p.topic).into_owned(),
                        String::from_utf8_lossy(&p.payload).into_owned(),
                    ));
                }
            }
            got
        });
        assert!(
            got.iter()
                .any(|(t, p)| t == "idn/site/machines" && p.contains("sanctuary-pc")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(t, _)| t == "idn/site/machine/sanctuary-pc"),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(t, p)| t == "idn/sanctuary-pc/switcher/program" && p == "Camera 1"),
            "{got:?}"
        );
        assert!(!got.iter().any(|(_, p)| p == "forged"), "{got:?}");
        // Off, then on again from its settings.
        m.set(false, "127.0.0.1:0").unwrap();
        assert!(m.settings()["listening"].is_null());
    }

    /// A machine that goes away and comes back: "offline", then "online" (lab 395-B).
    #[test]
    fn events_for_a_machine_going_and_coming_back() {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
        let dir = idn_common::testing::TempDir::new("idn-site-mqtt-ev");
        let m = SiteMqtt::new(&dir).unwrap();
        m.set(true, "127.0.0.1:0").unwrap();
        let addr: SocketAddr = m.settings()["listening"].as_str().unwrap().parse().unwrap();
        let machine = |online: bool| {
            json!({"machines": [{"name": "Booth", "fingerprint": "ab", "kind": "app",
                "online": online, "revoked": false}]})
        };
        m.publish_machines(&machine(true));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let got = rt.block_on(async {
            let (c, mut el) =
                AsyncClient::new(MqttOptions::new("ev", "127.0.0.1", addr.port()), 20);
            c.subscribe("idn/site/events", QoS::AtLeastOnce)
                .await
                .unwrap();
            let mut got = Vec::new();
            let mut step = 0;
            let end = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while tokio::time::Instant::now() < end && got.len() < 2 {
                match tokio::time::timeout(std::time::Duration::from_millis(200), el.poll()).await {
                    Ok(Ok(Event::Incoming(Incoming::SubAck(_)))) => {
                        m.publish_machines(&machine(false));
                        step = 1;
                    }
                    Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                        got.push(String::from_utf8_lossy(&p.payload).into_owned());
                        if step == 1 {
                            m.publish_machines(&machine(true));
                            step = 2;
                        }
                    }
                    _ => {}
                }
            }
            got
        });
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains(r#""event":"offline""#), "{got:?}");
        assert!(got[1].contains(r#""event":"online""#), "{got:?}");
    }

    /// Off ends the connected clients too, not only new connections (lab 395-A).
    #[test]
    fn off_disconnects_the_connected() {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
        let dir = idn_common::testing::TempDir::new("idn-site-mqtt-off");
        let m = SiteMqtt::new(&dir).unwrap();
        m.set(true, "127.0.0.1:0").unwrap();
        let addr: SocketAddr = m.settings()["listening"].as_str().unwrap().parse().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ended = rt.block_on(async {
            let (c, mut el) =
                AsyncClient::new(MqttOptions::new("off", "127.0.0.1", addr.port()), 20);
            c.subscribe("x/#", QoS::AtLeastOnce).await.unwrap();
            let end = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while tokio::time::Instant::now() < end {
                match tokio::time::timeout(std::time::Duration::from_millis(200), el.poll()).await {
                    Ok(Ok(Event::Incoming(Incoming::SubAck(_)))) => {
                        m.set(false, "127.0.0.1:0").unwrap();
                    }
                    Ok(Ok(Event::Incoming(Incoming::Disconnect(_)))) | Ok(Err(_)) => return true,
                    _ => {}
                }
            }
            false
        });
        assert!(ended, "still connected after off");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(m.settings()["clients"].as_array().unwrap().is_empty());
    }

    #[test]
    fn machine_names_as_topics() {
        assert_eq!(topic_name("Sanctuary PC"), "sanctuary-pc");
        assert_eq!(topic_name("  "), "machine");
        assert_eq!(topic_name("Booth/2"), "booth-2");
    }
}
