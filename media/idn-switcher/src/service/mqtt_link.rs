//! The switcher on MQTT (§S.33): a connection to a broker (the site's,
//! idn-node.md §N.34, or any other), set with `mqtt_set` (`mqtt.json`). Through it:
//!
//! - **its state**, retained under its base topic (`idn/<machine>/switcher` unless set), as
//!   it changes: `online` (`true`, `false` by its will), `program` and `preview` (the
//!   inputs' labels, or empty), `tally` (`{"<label>": "program" | "preview"}`), `ftb`,
//!   `cue` (`{"current", "next", "name"}`), `streams` (as the API's state says them);
//! - **commands**: a message on `<base>/cmd/<command>` (its JSON body, or nothing) runs
//!   that command as the API's `POST /api/v1/<command>` does (`cut`, `program`
//!   `{"input": "Camera 1"}`, `cue_go`, `cue_fire` `{"cue": 3}`, …); the answer on
//!   `<base>/cmd/<command>/result`: `{"ok", "error"?, …}`;
//! - **triggers**: when a message comes on a topic (MQTT wildcards; and, if set, its
//!   payload contains some text), a command runs: a button on Home Assistant fires a cue;
//! - **the cues' MQTT actions** (`{"do": "mqtt", "topic", "payload", "retain"}`), sent
//!   through it.

use super::*;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::LastWill;
use rumqttc::v5::{Client, Event, Incoming, MqttOptions};
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The MQTT card's settings, kept in the state directory.
pub(super) const MQTT_FILE: &str = "mqtt.json";

/// One trigger: a topic (with wildcards) and, if set, text its payload must contain; the
/// command it runs and its body.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MqttTrigger {
    pub topic: String,
    pub contains: String,
    pub command: String,
    pub body: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct MqttCfg {
    /// `mqtt://host:port` (empty: off).
    pub url: String,
    pub username: String,
    pub password: String,
    /// The base topic (empty: `idn/<machine>/switcher`).
    pub base: String,
    pub triggers: Vec<MqttTrigger>,
}

impl MqttCfg {
    pub fn from_json(v: &Value) -> Result<MqttCfg, String> {
        let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_owned();
        let url = s("url");
        if !url.is_empty() && host_port(&url).is_none() {
            return Err("the broker: mqtt://host:port (e.g. mqtt://site-manager:1883)".into());
        }
        let base = s("base").trim_end_matches('/').to_owned();
        if base.contains(['+', '#']) {
            return Err("the base topic has no wildcards".into());
        }
        let triggers = v["triggers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| {
                        let topic = t["topic"].as_str().unwrap_or("").trim().to_owned();
                        let command = t["command"].as_str().unwrap_or("").trim().to_owned();
                        if topic.is_empty() || command.is_empty() {
                            return Err("a trigger needs a topic and a command".to_owned());
                        }
                        Ok(MqttTrigger {
                            topic,
                            contains: t["contains"].as_str().unwrap_or("").to_owned(),
                            command,
                            body: if t["body"].is_object() {
                                t["body"].clone()
                            } else {
                                json!({})
                            },
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(MqttCfg {
            url,
            username: s("username"),
            password: v["password"].as_str().unwrap_or("").to_owned(),
            base,
            triggers,
        })
    }

    /// As the state shows it: the password only as whether there is one.
    pub fn to_json(&self, secret: bool) -> Value {
        json!({
            "url": self.url,
            "username": self.username,
            "password": if secret { json!(self.password) } else { json!(!self.password.is_empty()) },
            "base": self.base,
            "triggers": self.triggers.iter().map(|t| json!({
                "topic": t.topic, "contains": t.contains, "command": t.command, "body": t.body,
            })).collect::<Vec<_>>(),
        })
    }

    fn base_topic(&self) -> String {
        if self.base.is_empty() {
            format!(
                "idn/{}/switcher",
                topic_level(&idn_open_protocol::discovery::hostname())
            )
        } else {
            self.base.clone()
        }
    }
}

/// `mqtt://host:port` (port 1883 if none) as (host, port).
fn host_port(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("mqtt://")
        .unwrap_or(url)
        .trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    // [v6]:port, [v6], host:port, host.
    if let Some(v6) = rest.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => 1883,
            None => return None,
        };
        return Some((h.to_owned(), port));
    }
    match rest.split_once(':') {
        Some((h, p)) if !h.is_empty() => Some((h.to_owned(), p.parse().ok()?)),
        Some(_) => None,
        None => Some((rest.to_owned(), 1883)),
    }
}

/// A machine's name as one topic level, the Site Manager's rule for a machine's topic
/// (idn-node.md §N.34): each ASCII letter in lower case, each digit, and every other
/// character (`/`, `+`, `#`, NUL and spaces among them: a level must hold none of the
/// first four) as `-`, one for one; `-` trimmed from both ends; nothing left:
/// `switcher`. (Each character on its own: lower-casing the whole name first made some
/// letters two characters, and the Kelvin sign an ASCII `k`.)
fn topic_level(name: &str) -> String {
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
        "switcher".into()
    } else {
        t.to_owned()
    }
}

/// The state's parts published, each as its own retained topic (by name under the base).
fn parts(s: &Value) -> Vec<(&'static str, Value)> {
    let empty = Vec::new();
    let inputs = s["inputs"].as_array().unwrap_or(&empty);
    let label_of = |id: &Value| -> Value {
        inputs
            .iter()
            .find(|i| &i["id"] == id && !id.is_null())
            .map_or(json!(""), |i| i["label"].clone())
    };
    let tally: serde_json::Map<String, Value> = inputs
        .iter()
        .filter_map(|i| {
            let label = i["label"].as_str()?.to_owned();
            if i["program"] == true {
                Some((label, json!("program")))
            } else if i["preview"] == true {
                Some((label, json!("preview")))
            } else {
                None
            }
        })
        .collect();
    let show = &s["show"];
    let current = show["current"].clone();
    // Cues count from 1; 0 or none: no cue yet.
    let name = current
        .as_u64()
        .and_then(|k| k.checked_sub(1))
        .and_then(|k| show["show"]["cues"].get(k as usize))
        .map_or(json!(""), |c| c["name"].clone());
    // (Not their sent-rate history: a graph's, not MQTT's.)
    let mut streams = s["streams"].clone();
    for d in streams.as_array_mut().into_iter().flatten() {
        if let Some(o) = d.as_object_mut() {
            o.remove("sent_history");
        }
    }
    vec![
        ("program", label_of(&s["program"])),
        ("preview", label_of(&s["preview"])),
        ("tally", Value::Object(tally)),
        ("ftb", s["ftb"]["on"].clone()),
        (
            "cue",
            json!({"current": current, "next": show["next"], "name": name}),
        ),
        ("streams", streams),
    ]
}

/// The link: the settings, and the connection's work on threads of its own.
pub(super) struct MqttLink {
    cfg: Mutex<MqttCfg>,
    /// Changed settings: the connection starts again.
    version: AtomicU64,
    client: Mutex<Option<Client>>,
    status: Mutex<String>,
    /// Commands and triggers for the worker ([`work`]).
    jobs: std::sync::mpsc::SyncSender<Job>,
}

/// What MQTT asked for, run in order by the link's worker, off the connection's thread:
/// commands ran on it, so a slow one (a preset input put on preview recalls its camera's
/// preset; a camera that doesn't answer: up to 8 s) held up the connection past its keepalive and the broker dropped it (bug review
/// 3).
enum Job {
    Command {
        topic: String,
        cmd: String,
        body: Value,
        client: Client,
    },
    Trigger {
        topic: String,
        command: String,
        body: Value,
    },
}

/// Commands waiting for the worker; more are answered "busy".
const JOBS_WAITING: usize = 64;

/// The link's worker: each job in order, with the service while it runs (it ends when the
/// link or the service goes).
fn work(jobs: std::sync::mpsc::Receiver<Job>, api: Weak<Service>, link: Weak<MqttLink>) {
    while let Ok(job) = jobs.recv() {
        let Some(api) = api.upgrade() else { return };
        match job {
            Job::Command {
                topic,
                cmd,
                body,
                client,
            } => {
                // Written in the audit as MQTT's (§S.35).
                let who = idn_open_protocol::web::Who::mqtt(&topic);
                let mut answer = match api.command_as(&who, &cmd, &body) {
                    Ok(v) if v.is_object() => v,
                    Ok(_) => json!({}),
                    Err(e) => json!({"ok": false, "error": e}),
                };
                if answer.get("ok").is_none() {
                    answer["ok"] = json!(true);
                }
                let _ = client.try_publish(
                    format!("{topic}/result"),
                    QoS::AtLeastOnce,
                    false,
                    answer.to_string(),
                );
            }
            Job::Trigger {
                topic,
                command,
                body,
            } => {
                let who = idn_open_protocol::web::Who::mqtt(&format!("trigger {topic}"));
                let run = match not_from_mqtt(&command) {
                    Some(why) => Err(why.to_owned()),
                    None => api.command_as(&who, &command, &body).map(|_| ()),
                };
                if let Err(e) = run
                    && let Some(l) = link.upgrade()
                {
                    l.set_status(format!("trigger on {topic}: {e}"));
                }
            }
        }
    }
}

impl MqttLink {
    /// (One that can't be read is set aside and said: it turned MQTT off without a word,
    /// and the next change, which never sends the password back, saved it empty; bug
    /// review 3.)
    pub fn load() -> MqttCfg {
        super::saved::load_saved_value(MQTT_FILE)
            .and_then(|v| MqttCfg::from_json(&v).ok())
            .unwrap_or_default()
    }

    /// Starts the link for `api` (it ends with it).
    pub fn start(api: Weak<Service>) -> Arc<MqttLink> {
        let (jobs, waiting) = std::sync::mpsc::sync_channel(JOBS_WAITING);
        let link = Arc::new(MqttLink {
            cfg: Mutex::new(Self::load()),
            version: AtomicU64::new(1),
            client: Mutex::new(None),
            status: Mutex::new("off".into()),
            jobs,
        });
        {
            let (l, a) = (Arc::downgrade(&link), api.clone());
            let _ = std::thread::Builder::new()
                .name("idn-switcher-mqtt-cmd".into())
                .spawn(move || work(waiting, a, l));
        }
        // The cues' MQTT actions go out through it.
        {
            let l = Arc::downgrade(&link);
            crate::show::set_mqtt_publisher(Some(Box::new(move |topic, payload, retain| {
                let l = l.upgrade().ok_or("the switcher is stopping")?;
                let c = l.client.lock_ok().clone();
                let c =
                    c.ok_or_else(|| format!("MQTT isn't connected ({})", l.status.lock_ok()))?;
                c.publish(topic, QoS::AtLeastOnce, retain, payload.to_vec())
                    .map_err(|e| e.to_string())
            })));
        }
        let (l, a) = (link.clone(), api.clone());
        let _ = std::thread::Builder::new()
            .name("idn-switcher-mqtt".into())
            .spawn(move || l.run(a));
        let (l, a) = (Arc::downgrade(&link), api);
        let _ = std::thread::Builder::new()
            .name("idn-switcher-mqtt-state".into())
            .spawn(move || publish_state(l, a));
        link
    }

    /// The card's view: the settings (no password) and how the connection is.
    pub fn view(&self) -> Value {
        let mut v = self.cfg.lock_ok().to_json(false);
        v["status"] = json!(*self.status.lock_ok());
        v["base_topic"] = json!(self.cfg.lock_ok().base_topic());
        v
    }

    /// New settings (a password left out keeps the one there is); kept, and the
    /// connection starts again.
    pub fn set(&self, v: &Value) -> Result<(), String> {
        let mut new = MqttCfg::from_json(v)?;
        if let Some(why) = new.triggers.iter().find_map(|t| not_from_mqtt(&t.command)) {
            return Err(format!("a trigger: {why}"));
        }
        if v.get("password").is_none_or(|p| !p.is_string()) {
            new.password = self.cfg.lock_ok().password.clone();
        }
        idn_common::fsutil::write_atomic(
            &state_dir().join(MQTT_FILE),
            new.to_json(true).to_string().as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        *self.cfg.lock_ok() = new;
        self.version.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock_ok() = s.into();
    }

    /// The connection, made again when it drops or the settings change.
    fn run(self: Arc<Self>, api: Weak<Service>) {
        loop {
            if api.upgrade().is_none() {
                return;
            }
            let at_version = self.version.load(Ordering::SeqCst);
            let cfg = self.cfg.lock_ok().clone();
            let Some((host, port)) = (!cfg.url.is_empty()).then(|| host_port(&cfg.url)).flatten()
            else {
                self.set_status("off");
                *self.client.lock_ok() = None;
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };
            let base = cfg.base_topic();
            // The host and this switcher's base topic: two switchers on one machine, or
            // two machines with one name, would otherwise take the session from each
            // other for ever.
            let mut o = MqttOptions::new(
                format!(
                    "idn-switcher-{}-{:08x}",
                    topic_level(&idn_open_protocol::discovery::hostname()),
                    {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        base.hash(&mut h);
                        h.finish() as u32
                    }
                ),
                host.clone(),
                port,
            );
            o.set_keep_alive(Duration::from_secs(15));
            if !cfg.username.is_empty() {
                o.set_credentials(cfg.username.clone(), cfg.password.clone());
            }
            o.set_last_will(LastWill::new(
                format!("{base}/online"),
                "false",
                QoS::AtLeastOnce,
                true,
                None,
            ));
            let (client, mut conn) = Client::new(o, 64);
            self.set_status(format!("connecting to {host}:{port}"));
            let mut subscribed = false;
            loop {
                if self.version.load(Ordering::SeqCst) != at_version || api.upgrade().is_none() {
                    let _ =
                        client.publish(format!("{base}/online"), QoS::AtLeastOnce, true, "false");
                    let _ = client.disconnect();
                    break;
                }
                match conn.recv_timeout(Duration::from_millis(500)) {
                    Ok(Ok(Event::Incoming(Incoming::ConnAck(_)))) => {
                        self.set_status(format!("connected to {host}:{port}"));
                        let _ = client.subscribe(format!("{base}/cmd/+"), QoS::AtLeastOnce);
                        for t in &cfg.triggers {
                            let _ = client.subscribe(t.topic.clone(), QoS::AtLeastOnce);
                        }
                        let _ = client.publish(
                            format!("{base}/online"),
                            QoS::AtLeastOnce,
                            true,
                            "true",
                        );
                        *self.client.lock_ok() = Some(client.clone());
                        subscribed = true;
                        // Everything said again on the new connection.
                        SAID.lock_ok().clear();
                    }
                    // A retained message is an old one the broker hands every new
                    // subscription: run, it would cut or advance the cues again at each
                    // reconnect (a broker restart, Wi-Fi). Commands and triggers are live
                    // messages only.
                    Ok(Ok(Event::Incoming(Incoming::Publish(p)))) if subscribed && p.retain => {}
                    Ok(Ok(Event::Incoming(Incoming::Publish(p)))) if subscribed => {
                        let topic = String::from_utf8_lossy(&p.topic).into_owned();
                        let payload = String::from_utf8_lossy(&p.payload).into_owned();
                        if api.strong_count() > 0 {
                            self.on_message(&client, &cfg, &base, &topic, &payload);
                        }
                    }
                    Ok(Ok(Event::Incoming(Incoming::Disconnect(d)))) => {
                        // The broker ended it (turned off, lab 396-C): not connected, and
                        // a cue's MQTT action says so until it's back.
                        *self.client.lock_ok() = None;
                        self.set_status(format!(
                            "{host}:{port}: the broker disconnected ({:?})",
                            d.reason_code
                        ));
                        std::thread::sleep(Duration::from_secs(3));
                        break;
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        // The broker closing the link reads as the library's "bytes remaining on
                        // stream" (lab 397): said plainly, the library's words after.
                        self.set_status(format!("not connected to {host}:{port}: {e}"));
                        *self.client.lock_ok() = None;
                        std::thread::sleep(Duration::from_secs(3));
                        break;
                    }
                    Err(_) => {}
                }
            }
            *self.client.lock_ok() = None;
        }
    }

    fn on_message(&self, client: &Client, cfg: &MqttCfg, base: &str, topic: &str, payload: &str) {
        // A command: `<base>/cmd/<command>`, the operator's ones only (§S.33): anyone the
        // broker lets publish would otherwise have the whole API without the switcher's
        // password (its MQTT settings, streams to anywhere, deleting setups).
        if let Some(cmd) = topic
            .strip_prefix(base)
            .and_then(|t| t.strip_prefix("/cmd/"))
            .filter(|c| !c.contains('/'))
        {
            let body: Value = if payload.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(payload).unwrap_or_else(|_| json!({}))
            };
            let refused = if let Some(why) = not_from_mqtt(cmd) {
                Some(why.to_owned())
            } else if !MQTT_COMMANDS.contains(&cmd) {
                Some(format!("{cmd} isn't an MQTT command (the HTTP API's)"))
            } else {
                // Run by the worker, in order; answered from there.
                self.jobs
                    .try_send(Job::Command {
                        topic: topic.to_owned(),
                        cmd: cmd.to_owned(),
                        body,
                        client: client.clone(),
                    })
                    .err()
                    .map(|_| "busy: too many commands waiting".to_owned())
            };
            // Not waiting: this thread is the one that empties the client's queue, so a
            // full queue would wait on itself for ever.
            if let Some(why) = refused {
                let _ = client.try_publish(
                    format!("{topic}/result"),
                    QoS::AtLeastOnce,
                    false,
                    json!({"ok": false, "error": why}).to_string(),
                );
            }
            return;
        }
        for t in cfg
            .triggers
            .iter()
            .filter(|t| rumqttc::v5::mqttbytes::matches(topic, &t.topic))
        {
            if !t.contains.is_empty() && !payload.contains(&t.contains) {
                continue;
            }
            if self
                .jobs
                .try_send(Job::Trigger {
                    topic: topic.to_owned(),
                    command: t.command.clone(),
                    body: t.body.clone(),
                })
                .is_err()
            {
                self.set_status(format!("trigger on {topic}: busy, too many waiting"));
            }
        }
    }
}

/// The commands MQTT may give (§S.33): running the show (switching, transitions, a scene's
/// layer shown or hidden, sound, the monitor, captions, cues, mixes, virtual shots, timers,
/// slideshows, lists, media playback, titles' live fields). Not configuration, saved setups and shows, streams,
/// inputs, Planning Center or MQTT's own settings: those go over the HTTP API, behind the
/// switcher's password.
const MQTT_COMMANDS: &[&str] = &[
    "preview",
    "program",
    "cut",
    "auto",
    "tbar",
    "transition",
    "stinger",
    "ftb",
    "scene_layer_visible",
    "audio",
    "monitor",
    "monitor_solo",
    "caption",
    "captions",
    "cue_go",
    "cue_back",
    "cue_standby",
    "cue_fire",
    "mix_program",
    "mix_preview",
    "mix_cut",
    "mix_auto",
    "mix_transition",
    // (Not `ptz`: cameras move over the IDN control channel only, see [`not_from_mqtt`].
    // Not `camera`: it points a camera's control at any address, or takes it away;
    // configuration, behind the switcher's password; bug review 3.)
    "virtual_view",
    "timer",
    "slideshow",
    "slide",
    "media",
    "list",
    "title_fields",
    "title_row",
];

/// Commands MQTT never runs, as a command or a trigger, and why: a camera's moves go over
/// the IDN control channel only (the switcher's API, panel and cues).
fn not_from_mqtt(cmd: &str) -> Option<&'static str> {
    (cmd == "ptz").then_some(
        "ptz isn't run from MQTT: cameras move over the IDN control channel (the switcher's API)",
    )
}

/// What was last published, by part (to publish only what changed).
static SAID: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

/// A value without its numbers (counters, rates): what a stream *is* (started, failed,
/// where), for [`publish_state`].
fn shape(v: &Value) -> Value {
    match v {
        Value::Number(_) => Value::Null,
        Value::Array(a) => Value::Array(a.iter().map(shape).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        v => v.clone(),
    }
}

/// The state, as it changes, every quarter of a second. The streams' counters change all
/// the time: a change of their state goes at once, their numbers at most every 5 s (each
/// retained message is a write to the broker's disk).
fn publish_state(link: Weak<MqttLink>, api: Weak<Service>) {
    let mut streams_at: Option<(Value, std::time::Instant)> = None;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let (Some(l), Some(api)) = (link.upgrade(), api.upgrade()) else {
            return;
        };
        let Some(c) = l.client.lock_ok().clone() else {
            continue;
        };
        let base = l.cfg.lock_ok().base_topic();
        let s = api.state();
        let mut said = SAID.lock_ok();
        for (name, v) in parts(&s) {
            let text = v.to_string();
            if said.iter().any(|(n, t)| *n == name && *t == text) {
                continue;
            }
            if name == "streams" {
                let now_shape = shape(&v);
                // (Said once on each connection: a new one starts with nothing said.)
                let numbers_only = said.iter().any(|(n, _)| *n == "streams")
                    && streams_at.as_ref().is_some_and(|(sh, at)| {
                        *sh == now_shape && at.elapsed() < Duration::from_secs(5)
                    });
                if numbers_only {
                    continue;
                }
                streams_at = Some((now_shape, std::time::Instant::now()));
            }
            if c.try_publish(
                format!("{base}/{name}"),
                QoS::AtLeastOnce,
                true,
                text.clone(),
            )
            .is_ok()
            {
                said.retain(|(n, _)| *n != name);
                said.push((name, text));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_addresses_and_settings() {
        assert_eq!(topic_level("Sanctuary PC"), "sanctuary-pc");
        assert_eq!(topic_level("booth/2+#\0"), "booth-2");
        assert_eq!(topic_level("\u{212A}iosk İ"), "iosk");
        assert_eq!(topic_level(" -/- "), "switcher");
        assert_eq!(host_port("mqtt://sm:1883"), Some(("sm".into(), 1883)));
        assert_eq!(host_port("mqtt://sm"), Some(("sm".into(), 1883)));
        assert_eq!(host_port("10.0.0.5:1884"), Some(("10.0.0.5".into(), 1884)));
        assert_eq!(host_port("mqtt://sm/x"), None);
        let c = MqttCfg::from_json(&json!({
            "url": "mqtt://sm:1883", "username": "sw", "password": "pw", "base": "church/switcher/",
            "triggers": [{"topic": "ha/button", "contains": "single", "command": "cue_go"}],
        }))
        .unwrap();
        assert_eq!(c.base_topic(), "church/switcher");
        assert_eq!(c.triggers[0].body, json!({}));
        assert_eq!(c.to_json(false)["password"], true);
        assert!(MqttCfg::from_json(&json!({"url": "mqtt://x/y"})).is_err());
        assert!(MqttCfg::from_json(&json!({"triggers": [{"topic": "t"}]})).is_err());
        assert!(MqttCfg::from_json(&json!({"base": "a/#"})).is_err());
        // PTZ: never over MQTT, as a command or a trigger.
        assert!(!MQTT_COMMANDS.contains(&"ptz"));
        assert!(not_from_mqtt("ptz").is_some());
        assert!(MQTT_COMMANDS.iter().all(|c| not_from_mqtt(c).is_none()));
    }

    #[test]
    fn the_state_as_parts() {
        let s = json!({
            "inputs": [
                {"id": 1, "label": "Camera 1", "program": true, "preview": false},
                {"id": 2, "label": "Slides", "program": false, "preview": true},
                {"id": 3, "label": "Camera 2", "program": false, "preview": false},
            ],
            "program": 1, "preview": 2, "ftb": {"on": false},
            "show": {"current": 1, "next": 2, "show": {"cues": [{"name": "Welcome"}, {"name": "Song"}]}},
            "streams": {"recording": false},
        });
        let p: std::collections::HashMap<&str, Value> = parts(&s).into_iter().collect();
        assert_eq!(p["program"], "Camera 1");
        assert_eq!(p["preview"], "Slides");
        assert_eq!(
            p["tally"],
            json!({"Camera 1": "program", "Slides": "preview"})
        );
        assert_eq!(p["cue"]["name"], "Welcome");
        assert_eq!(p["ftb"], false);
    }
}
