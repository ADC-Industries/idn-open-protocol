//! The show: a cue list, run of show (spec/extensions/idn-switcher.md §S.27, G1). Each cue
//! fires its actions together on one Go — the switcher's own commands, a desk's snapshot, a
//! web request (a presentation program's API, a Companion button), an OSC message — and
//! may follow on by itself after a time. A Planning Center service plan can become a
//! show (with the user's own personal access token).
//!
//! No I/O but [`run_external`] (a mixer, HTTP, OSC) and [`Pco`] (Planning Center); the
//! switcher's own commands are run by the program that owns the switcher.

use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Something a cue does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A switcher command (the API's, `POST /api/v1/<command>`) and its body; `input`
    /// and `camera` may name an input by its label (found when the cue fires).
    Switcher { command: String, body: Value },
    /// A mixing desk's snapshot. `desk`: the desk's source name ("HOST (Mixer)"), over IDN
    /// (the switcher's connection to it; the desk takes it from devices of its site,
    /// idn-mixer §M.21). Else `url`: `http://host:29214` (its API) or `osc://host:port`
    /// (its OSC control, §M.16), for a desk that isn't in a site.
    Mixer {
        desk: String,
        url: String,
        snapshot: String,
    },
    /// A web request: `method`, `url`, a JSON or text `body` (empty: none).
    Http {
        method: String,
        url: String,
        body: String,
    },
    /// An OSC message to `to` (`host:port`), with its arguments (numbers, strings, true
    /// and false).
    Osc {
        to: String,
        path: String,
        args: Vec<Value>,
    },
    /// A message to an MQTT topic (§S.33), through the switcher's MQTT connection: its
    /// `payload` as text, kept by the broker (`retain`) or not.
    Mqtt {
        topic: String,
        payload: String,
        retain: bool,
    },
    /// A text message into an IDN Voice channel (idn-voice.md §V.10), through the
    /// switcher's MQTT connection to the site's broker (`idn/site/voice/cmd/message`).
    Voice { channel: String, text: String },
}

impl Action {
    pub fn to_json(&self) -> Value {
        match self {
            Action::Switcher { command, body } => {
                json!({"do": "switcher", "command": command, "body": body})
            }
            Action::Mixer {
                desk,
                url,
                snapshot,
            } if desk.is_empty() => {
                json!({"do": "mixer", "url": url, "snapshot": snapshot})
            }
            Action::Mixer { desk, snapshot, .. } => {
                json!({"do": "mixer", "desk": desk, "snapshot": snapshot})
            }
            Action::Http { method, url, body } => {
                json!({"do": "http", "method": method, "url": url, "body": body})
            }
            Action::Osc { to, path, args } => {
                json!({"do": "osc", "to": to, "path": path, "args": args})
            }
            Action::Mqtt {
                topic,
                payload,
                retain,
            } => json!({"do": "mqtt", "topic": topic, "payload": payload, "retain": retain}),
            Action::Voice { channel, text } => {
                json!({"do": "voice", "channel": channel, "text": text})
            }
        }
    }

    pub fn from_json(v: &Value) -> Result<Action, String> {
        let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_owned();
        let need = |k: &str| -> Result<String, String> {
            Some(s(k))
                .filter(|x| !x.is_empty())
                .ok_or(format!("a {} action needs {k}", s("do")))
        };
        Ok(match v["do"].as_str().unwrap_or("") {
            "switcher" => Action::Switcher {
                command: need("command")?,
                body: if v["body"].is_object() {
                    v["body"].clone()
                } else {
                    json!({})
                },
            },
            "mixer" => Action::Mixer {
                desk: s("desk"),
                url: if s("desk").is_empty() {
                    need("url").map_err(|_| "a mixer action needs desk (or url)".to_owned())?
                } else {
                    String::new()
                },
                snapshot: need("snapshot")?,
            },
            "http" => Action::Http {
                method: Some(s("method").to_uppercase())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "POST".into()),
                url: need("url")?,
                body: s("body"),
            },
            "osc" => {
                let path = need("path")?;
                if !path.starts_with('/') {
                    return Err("an OSC path starts with /".into());
                }
                Action::Osc {
                    to: need("to")?,
                    path,
                    args: v["args"].as_array().cloned().unwrap_or_default(),
                }
            }
            "mqtt" => {
                let topic = need("topic")?;
                if topic.contains(['+', '#']) {
                    return Err("an MQTT topic to publish to has no wildcards".into());
                }
                Action::Mqtt {
                    topic,
                    payload: v["payload"].as_str().unwrap_or("").to_owned(),
                    retain: v["retain"].as_bool().unwrap_or(false),
                }
            }
            "voice" => Action::Voice {
                channel: need("channel")?,
                text: need("text")?,
            },
            other => {
                return Err(format!(
                    "no action {other:?}: switcher, mixer, http, osc, mqtt or voice"
                ));
            }
        })
    }
}

/// One cue.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cue {
    pub name: String,
    pub notes: String,
    /// What it is, as the plan says: `item`, `song`, `media`, or `header` (a heading in
    /// the list: Go passes over it).
    pub kind: String,
    /// Its planned length, seconds.
    pub length: Option<f64>,
    /// Goes on to the next cue by itself this long after it fired, seconds.
    pub follow: Option<f64>,
    pub actions: Vec<Action>,
}

impl Cue {
    pub fn header(&self) -> bool {
        self.kind == "header"
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name, "notes": self.notes, "kind": self.kind,
            "length": self.length, "follow": self.follow,
            "actions": self.actions.iter().map(Action::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Result<Cue, String> {
        // At most a day: a follow of 1e300 s panicked the show thread when it became a
        // Duration.
        let secs = |k: &str| {
            v[k].as_f64()
                .filter(|s| *s > 0.0 && s.is_finite())
                .map(|s| s.min(86_400.0))
        };
        Ok(Cue {
            name: v["name"].as_str().unwrap_or("").to_owned(),
            notes: v["notes"].as_str().unwrap_or("").to_owned(),
            kind: v["kind"]
                .as_str()
                .filter(|k| !k.is_empty())
                .unwrap_or("item")
                .to_owned(),
            length: secs("length"),
            follow: secs("follow"),
            actions: v["actions"]
                .as_array()
                .map(|a| a.iter().map(Action::from_json).collect::<Result<_, _>>())
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

/// A show: its cues in order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Show {
    pub name: String,
    /// Where it came from (`Planning Center: Sunday 9:00, 2026-11-01`), if imported.
    pub source: String,
    pub cues: Vec<Cue>,
}

/// At most this many cues in a show.
pub const MAX_CUES: usize = 1000;

impl Show {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "source": self.source,
               "cues": self.cues.iter().map(Cue::to_json).collect::<Vec<_>>()})
    }

    /// A saved show read back: an action this build can't read (an older build's file
    /// with a newer action, a check added since) is left out of its cue and named in the
    /// notes, rather than the whole show lost (bug round 2026-11-09).
    pub fn from_saved(v: &Value) -> (Show, Vec<String>) {
        let mut notes = Vec::new();
        let mut v = v.clone();
        if let Some(cues) = v["cues"].as_array_mut() {
            for (k, c) in cues.iter_mut().enumerate() {
                if let Some(acts) = c["actions"].as_array() {
                    let kept: Vec<Value> = acts
                        .iter()
                        .filter(|a| match Action::from_json(a) {
                            Ok(_) => true,
                            Err(e) => {
                                notes.push(format!("cue {}: {e}", k + 1));
                                false
                            }
                        })
                        .cloned()
                        .collect();
                    c["actions"] = Value::Array(kept);
                }
            }
            cues.truncate(MAX_CUES);
        }
        match Show::from_json(&v) {
            Ok(s) => (s, notes),
            Err(e) => {
                notes.push(e);
                (Show::default(), notes)
            }
        }
    }

    pub fn from_json(v: &Value) -> Result<Show, String> {
        let cues: Vec<Cue> = v["cues"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(k, c)| Cue::from_json(c).map_err(|e| format!("cue {}: {e}", k + 1)))
                    .collect::<Result<_, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        if cues.len() > MAX_CUES {
            return Err(format!("at most {MAX_CUES} cues"));
        }
        Ok(Show {
            name: v["name"].as_str().unwrap_or("").to_owned(),
            source: v["source"].as_str().unwrap_or("").to_owned(),
            cues,
        })
    }
}

/// Where a show is: the cue last fired and the one Go fires next (standby).
#[derive(Debug, Clone, Default)]
pub struct Runner {
    pub show: Show,
    /// The cue last fired, and when.
    pub current: Option<(usize, Instant)>,
    /// The cue Go fires (`None`: the end).
    pub next: Option<usize>,
}

impl Runner {
    /// A show at its start (standing by on its first cue that isn't a header).
    pub fn new(show: Show) -> Runner {
        let next = Self::playable_from(&show, 0);
        Runner {
            show,
            current: None,
            next,
        }
    }

    fn playable_from(show: &Show, k: usize) -> Option<usize> {
        (k..show.cues.len()).find(|&i| !show.cues[i].header())
    }

    /// Fires the next cue: its index (`None`: nothing left). The one after stands by.
    pub fn go(&mut self) -> Option<usize> {
        let k = self.next?;
        self.fire(k);
        Some(k)
    }

    /// Fires cue `k` (its actions are the caller's to run) and stands by on the one after.
    pub fn fire(&mut self, k: usize) {
        self.current = Some((k, Instant::now()));
        self.next = Self::playable_from(&self.show, k + 1);
    }

    /// Fires the cue before the one last fired (to put back what it set): its index.
    pub fn back(&mut self) -> Option<usize> {
        let (cur, _) = self.current?;
        let k = (0..cur).rev().find(|&i| !self.show.cues[i].header())?;
        self.fire(k);
        Some(k)
    }

    /// Stands by on cue `k` (nothing fires).
    pub fn standby(&mut self, k: usize) -> Result<(), String> {
        if k >= self.show.cues.len() {
            return Err(format!("cues are 1 to {}", self.show.cues.len()));
        }
        self.next = Self::playable_from(&self.show, k);
        Ok(())
    }

    /// The show changed (edited, imported again): keeps the places as far as they still
    /// exist.
    pub fn replace(&mut self, show: Show) {
        let n = show.cues.len();
        self.show = show;
        self.current = self.current.filter(|(k, _)| *k < n);
        self.next = match self.next {
            Some(k) if k < n => Self::playable_from(&self.show, k),
            _ if self.current.is_none() => Self::playable_from(&self.show, 0),
            _ => None,
        };
    }

    /// The show's cues added to, removed or moved: `old_of_new[i]` is where cue `i` of
    /// `show` was before (`None`: a new one). The current cue stays the same cue; the
    /// next one too, or (removed) the cue that took its place. At the show's end (a
    /// current cue, no next) a cue added after the current one is next; the current one
    /// removed there leaves no next (not cue 1).
    pub fn rearrange(&mut self, show: Show, old_of_new: &[Option<usize>]) {
        let new_of = |k: usize| old_of_new.iter().position(|o| *o == Some(k));
        // Where old cue `k` was, gone: the cues before it that are still there (new ones
        // before it too).
        let in_place_of = |k: usize| {
            old_of_new
                .iter()
                .take_while(|o| o.is_none_or(|o| o < k))
                .count()
        };
        let was = self.current;
        self.current = self.current.and_then(|(k, at)| new_of(k).map(|j| (j, at)));
        self.next = match (self.next, was) {
            (Some(k), _) => Self::playable_from(&show, new_of(k).unwrap_or_else(|| in_place_of(k))),
            (None, None) => Self::playable_from(&show, 0),
            (None, Some((k, _))) => {
                let after = new_of(k).map_or_else(|| in_place_of(k), |j| j + 1);
                Self::playable_from(&show, after)
            }
        };
        self.show = show;
    }

    /// The current cue's follow time has passed: Go now.
    pub fn follow_due(&self) -> bool {
        self.current.is_some_and(|(k, at)| {
            self.next.is_some()
                && self
                    .show
                    .cues
                    .get(k)
                    .and_then(|c| c.follow)
                    .is_some_and(|f| {
                        Duration::try_from_secs_f64(f).is_ok_and(|d| at.elapsed() >= d)
                    })
        })
    }

    /// For the state: the places, and how long the current cue has run.
    pub fn status(&self) -> Value {
        json!({
            "current": self.current.map(|(k, _)| k + 1),
            "elapsed": self.current.map(|(_, at)| (at.elapsed().as_secs_f64() * 10.0).round() / 10.0),
            "next": self.next.map(|k| k + 1),
        })
    }
}

/// Runs a mixer (by `url`), web or OSC action (the switcher's own, and a desk's by name,
/// are the caller's: [`crate::Switcher::desk_snapshot`]).
pub fn run_external(a: &Action) -> Result<(), String> {
    match a {
        Action::Switcher { .. } => Err("a switcher action runs in the switcher".into()),
        Action::Mixer { desk, .. } if !desk.is_empty() => {
            Err("a desk's snapshot by name goes over the switcher's connection".into())
        }
        Action::Mixer { url, snapshot, .. } => match url.strip_prefix("osc://") {
            Some(to) => send_osc(to, "/snapshot/recall", &[json!(snapshot)]),
            None => {
                let url = format!("{}/api/v1/snapshot_recall", url.trim_end_matches('/'));
                let body = json!({"name": snapshot}).to_string();
                web(&url, "POST", &body, &[("X-IDN-Mixer".into(), "1".into())])
            }
        },
        Action::Http { method, url, body } => web(url, method, body, &[]),
        Action::Osc { to, path, args } => send_osc(to, path, args),
        Action::Mqtt {
            topic,
            payload,
            retain,
        } => match MQTT_PUBLISH.lock_ok().as_ref() {
            Some(f) => f(topic, payload.as_bytes(), *retain),
            None => Err("the switcher has no MQTT connection (its MQTT card)".into()),
        },
        Action::Voice { channel, text } => {
            let body = json!({"channel": channel, "text": text, "from": "cue"}).to_string();
            match MQTT_PUBLISH.lock_ok().as_ref() {
                Some(f) => f("idn/site/voice/cmd/message", body.as_bytes(), false),
                None => Err(
                    "a Voice message goes over the switcher's MQTT connection (its MQTT card)"
                        .into(),
                ),
            }
        }
    }
}

/// How an MQTT action is sent: set by the program that holds the switcher's MQTT
/// connection (§S.33).
type Publisher = Box<dyn Fn(&str, &[u8], bool) -> Result<(), String> + Send + Sync>;
static MQTT_PUBLISH: std::sync::Mutex<Option<Publisher>> = std::sync::Mutex::new(None);

/// Sets how MQTT actions are sent (none: they say there's no connection).
pub fn set_mqtt_publisher(f: Option<Publisher>) {
    *MQTT_PUBLISH.lock_ok() = f;
}

fn web(url: &str, method: &str, body: &str, extra: &[(String, String)]) -> Result<(), String> {
    let mut headers = extra.to_vec();
    if !body.is_empty() {
        let json = body.trim_start().starts_with(['{', '[']);
        headers.push((
            "Content-Type".into(),
            if json {
                "application/json"
            } else {
                "text/plain"
            }
            .into(),
        ));
    }
    let b = (!body.is_empty()).then_some(body.as_bytes());
    match idn_open_protocol::web::httpc::request(
        method,
        url,
        &headers,
        b,
        false,
        Duration::from_secs(5),
    ) {
        Ok((s, _)) if (200..300).contains(&s) => Ok(()),
        Ok((s, b)) => Err(format!(
            "{url}: {s} {}",
            String::from_utf8_lossy(&b)
                .chars()
                .take(120)
                .collect::<String>()
        )),
        Err(e) => Err(format!("{url}: {e}")),
    }
}

fn send_osc(to: &str, path: &str, args: &[Value]) -> Result<(), String> {
    use idn_open_protocol::osc::{Arg, Message, encode};
    let args: Vec<Arg> = args
        .iter()
        .map(|v| match v {
            Value::Bool(b) => Arg::Bool(*b),
            Value::Number(n) if n.is_i64() => Arg::Int(
                n.as_i64()
                    .unwrap_or(0)
                    .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            ),
            Value::Number(n) => Arg::Float(n.as_f64().unwrap_or(0.0) as f32),
            Value::String(s) => Arg::Str(s.clone()),
            _ => Arg::Nil,
        })
        .collect();
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.send_to(&encode(&Message::new(path, args)), to)
        .map(|_| ())
        .map_err(|e| format!("OSC to {to}: {e}"))
}

/// Planning Center Services, with the user's own personal access token (an application
/// id and secret from api.planningcenteronline.com; nothing of ours in between).
pub struct Pco {
    pub app_id: String,
    pub secret: String,
}

const PCO: &str = "https://api.planningcenteronline.com/services/v2";

impl Pco {
    fn get(&self, path: &str) -> Result<Value, String> {
        let auth = format!(
            "Basic {}",
            idn_common::text::base64(format!("{}:{}", self.app_id, self.secret).as_bytes())
        );
        let url = format!("{PCO}{path}");
        match idn_open_protocol::web::httpc::request(
            "GET",
            &url,
            &[("Authorization".into(), auth)],
            None,
            false,
            Duration::from_secs(15),
        ) {
            Ok((200, b)) => serde_json::from_slice(&b).map_err(|e| format!("Planning Center: {e}")),
            Ok((401, _)) => Err(
                "Planning Center refused the token (check the application id and secret)".into(),
            ),
            Ok((s, _)) => Err(format!("Planning Center: {s}")),
            Err(e) => Err(format!("Planning Center: {e}")),
        }
    }

    /// The coming plans of every service type: `[{service_type, service_type_name, plan,
    /// title, dates}]`, soonest first within each type.
    pub fn plans(&self) -> Result<Vec<Value>, String> {
        let types = self.get("/service_types?per_page=25")?;
        let mut out = Vec::new();
        for t in types["data"].as_array().into_iter().flatten() {
            let id = t["id"].as_str().unwrap_or("");
            let name = t["attributes"]["name"].as_str().unwrap_or("");
            let plans = self.get(&format!(
                "/service_types/{id}/plans?filter=future&order=sort_date&per_page=8"
            ))?;
            for p in plans["data"].as_array().into_iter().flatten() {
                let a = &p["attributes"];
                out.push(json!({
                    "service_type": id, "service_type_name": name, "plan": p["id"],
                    "title": a["title"].as_str().filter(|t| !t.is_empty()).or(a["series_title"].as_str()).unwrap_or(""),
                    "dates": a["dates"],
                }));
            }
        }
        Ok(out)
    }

    /// A plan as a show: one cue an item, in the plan's order.
    pub fn import(&self, service_type: &str, plan: &str) -> Result<Show, String> {
        let p = self.get(&format!("/service_types/{service_type}/plans/{plan}"))?;
        let items = self.get(&format!(
            "/service_types/{service_type}/plans/{plan}/items?per_page=100&order=sequence"
        ))?;
        Ok(show_from_plan(&p["data"], &items))
    }
}

/// A plan and its items (Planning Center's JSON) as a show; headers stay as headings.
pub fn show_from_plan(plan: &Value, items: &Value) -> Show {
    let a = &plan["attributes"];
    let title = [
        a["series_title"].as_str(),
        a["title"].as_str(),
        a["dates"].as_str(),
    ]
    .into_iter()
    .flatten()
    .filter(|t| !t.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    let mut list: Vec<&Value> = items["data"]
        .as_array()
        .map(|v| v.iter().collect())
        .unwrap_or_default();
    list.sort_by_key(|i| i["attributes"]["sequence"].as_i64().unwrap_or(0));
    Show {
        name: title.clone(),
        source: format!("Planning Center: {title}"),
        cues: list
            .into_iter()
            .take(MAX_CUES)
            .map(|i| {
                let a = &i["attributes"];
                Cue {
                    name: a["title"].as_str().unwrap_or("").to_owned(),
                    notes: a["description"].as_str().unwrap_or("").to_owned(),
                    kind: match a["item_type"].as_str().unwrap_or("item") {
                        k @ ("song" | "media" | "header") => k.to_owned(),
                        _ => "item".to_owned(),
                    },
                    length: a["length"].as_f64().filter(|l| *l > 0.0),
                    follow: None,
                    actions: Vec::new(),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {

    /// Cues added, removed and moved: the current and next stay on their cues.
    #[test]
    fn a_rearranged_show_keeps_its_places() {
        let cue = |n: &str| super::Cue {
            name: n.into(),
            kind: "item".into(),
            ..Default::default()
        };
        let show = |names: &[&str]| super::Show {
            cues: names.iter().map(|n| cue(n)).collect(),
            ..Default::default()
        };
        let mut r = super::Runner::new(show(&["a", "b", "c", "d"]));
        r.fire(1); // b current, c next
        // A cue put first: b and c move down one.
        r.rearrange(
            show(&["x", "a", "b", "c", "d"]),
            &[None, Some(0), Some(1), Some(2), Some(3)],
        );
        assert_eq!((r.current.map(|c| c.0), r.next), (Some(2), Some(3)));
        // The next (c) removed: d (now at 3) stands by.
        r.rearrange(
            show(&["x", "a", "b", "d"]),
            &[Some(0), Some(1), Some(2), Some(4)],
        );
        assert_eq!((r.current.map(|c| c.0), r.next), (Some(2), Some(3)));
        // d moved first: it is still the next.
        r.rearrange(
            show(&["d", "x", "a", "b"]),
            &[Some(3), Some(0), Some(1), Some(2)],
        );
        assert_eq!((r.current.map(|c| c.0), r.next), (Some(3), Some(0)));
    }

    /// Lab 471(d): one action this build can't read costs that action, not the show.
    #[test]
    fn a_saved_show_keeps_everything_but_the_action_it_cant_read() {
        let saved = serde_json::json!({"name": "Sunday", "cues": [
            {"name": "Welcome", "actions": [
                {"do": "switcher", "command": "cut"},
                {"do": "nonsense"},
                {"do": "mqtt", "topic": "church/lights", "payload": "1"},
            ]},
            {"name": "Song", "actions": [{"do": "switcher", "command": "auto"}]},
        ]});
        let (show, notes) = Show::from_saved(&saved);
        assert_eq!(show.name, "Sunday");
        assert_eq!(show.cues.len(), 2);
        assert_eq!(show.cues[0].actions.len(), 2);
        assert_eq!(show.cues[1].actions.len(), 1);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("cue 1: no action \"nonsense\""),
            "{notes:?}"
        );
        // Nothing wrong: no notes.
        assert!(Show::from_saved(&show.to_json()).1.is_empty());
    }

    #[test]
    fn an_mqtt_action_round_trips_and_is_checked() {
        let a = Action::from_json(&serde_json::json!({
            "do": "mqtt", "topic": "church/lights/scene", "payload": "2", "retain": true,
        }))
        .unwrap();
        assert_eq!(
            a,
            Action::Mqtt {
                topic: "church/lights/scene".into(),
                payload: "2".into(),
                retain: true
            }
        );
        assert_eq!(Action::from_json(&a.to_json()).unwrap(), a);
        assert!(Action::from_json(&serde_json::json!({"do": "mqtt", "topic": "a/#"})).is_err());
        assert!(Action::from_json(&serde_json::json!({"do": "mqtt"})).is_err());
        // Without a connection it says so.
        set_mqtt_publisher(None);
        assert!(run_external(&a).unwrap_err().contains("MQTT"));
    }

    use super::*;

    fn cue(name: &str, kind: &str) -> Cue {
        Cue {
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    #[test]
    fn go_passes_over_headers_back_and_standby() {
        let show = Show {
            cues: vec![
                cue("Pre-service", "header"),
                cue("Welcome", "item"),
                cue("Worship", "header"),
                cue("Song 1", "song"),
                cue("Song 2", "song"),
            ],
            ..Default::default()
        };
        let mut r = Runner::new(show);
        assert_eq!(r.next, Some(1));
        assert_eq!(r.go(), Some(1));
        assert_eq!(r.next, Some(3), "the header is passed over");
        assert_eq!(r.go(), Some(3));
        assert_eq!(r.go(), Some(4));
        assert_eq!(r.go(), None, "the end");
        assert_eq!(r.back(), Some(3));
        assert_eq!(r.next, Some(4));
        r.standby(0).unwrap();
        assert_eq!(r.next, Some(1));
        assert!(r.standby(9).is_err());
        // Edited shorter: the places that no longer exist go.
        r.replace(Show {
            cues: vec![cue("Only", "item")],
            ..Default::default()
        });
        assert_eq!((r.current.map(|c| c.0), r.next), (None, Some(0)));
    }

    #[test]
    fn a_cue_follows_on_after_its_time() {
        let mut c = cue("Countdown", "item");
        c.follow = Some(0.05);
        let mut r = Runner::new(Show {
            cues: vec![c, cue("Welcome", "item")],
            ..Default::default()
        });
        r.go();
        assert!(!r.follow_due());
        std::thread::sleep(Duration::from_millis(80));
        assert!(r.follow_due());
        r.go();
        assert!(!r.follow_due(), "the last cue has nothing to follow on to");
    }

    #[test]
    fn shows_round_trip_and_bad_actions_are_named() {
        let v = json!({"name": "Sunday", "cues": [
            {"name": "Welcome", "length": 120, "actions": [
                {"do": "switcher", "command": "program", "body": {"input": "Cam 1"}},
                {"do": "mixer", "url": "http://127.0.0.1:29214", "snapshot": "Speech"},
                {"do": "http", "url": "http://show.local/api/next", "method": "post"},
                {"do": "osc", "to": "127.0.0.1:9000", "path": "/go", "args": [1, 0.5, "x", true]}]}]});
        let s = Show::from_json(&v).unwrap();
        assert_eq!(s.cues[0].actions.len(), 4);
        assert_eq!(s.cues[0].kind, "item");
        assert!(matches!(&s.cues[0].actions[2], Action::Http { method, .. } if method == "POST"));
        assert_eq!(Show::from_json(&s.to_json()).unwrap(), s);
        let bad = json!({"cues": [{}, {"actions": [{"do": "teleport"}]}]});
        assert!(Show::from_json(&bad).unwrap_err().starts_with("cue 2:"));
        assert!(
            Show::from_json(
                &json!({"cues": [{"actions": [{"do": "osc", "to": "x:1", "path": "go"}]}]})
            )
            .is_err()
        );
    }

    #[test]
    fn a_planning_center_plan_becomes_a_show() {
        // Shaped as Planning Center's API answers (JSON:API).
        let plan = json!({"id": "77", "attributes": {"title": "Hope", "series_title": "Advent", "dates": "November 29, 2026"}});
        let items = json!({"data": [
            {"id": "3", "attributes": {"title": "Amazing Grace", "item_type": "song", "length": 300, "sequence": 3, "description": null}},
            {"id": "1", "attributes": {"title": "Pre-service", "item_type": "header", "length": 0, "sequence": 1}},
            {"id": "2", "attributes": {"title": "Countdown", "item_type": "media", "length": 300, "sequence": 2, "description": "5 min"}},
            {"id": "4", "attributes": {"title": "Sermon", "item_type": "item", "length": 1800, "sequence": 4}}]});
        let s = show_from_plan(&plan, &items);
        assert_eq!(s.name, "Advent · Hope · November 29, 2026");
        let names: Vec<_> = s
            .cues
            .iter()
            .map(|c| (c.name.as_str(), c.kind.as_str(), c.length))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Pre-service", "header", None),
                ("Countdown", "media", Some(300.0)),
                ("Amazing Grace", "song", Some(300.0)),
                ("Sermon", "item", Some(1800.0))
            ]
        );
        assert_eq!(s.cues[1].notes, "5 min");
        assert_eq!(idn_common::text::base64(b"app:secret"), "YXBwOnNlY3JldA==");
        assert_eq!(idn_common::text::base64(b"ab"), "YWI=");
    }

    /// A desk by name (over IDN) or by address (desks outside a site).
    #[test]
    fn mixer_actions_by_desk_or_address() {
        let by_name = json!({"do": "mixer", "desk": "FOH (Mixer)", "snapshot": "Sermon"});
        let a = Action::from_json(&by_name).unwrap();
        assert_eq!(a.to_json(), by_name);
        assert!(run_external(&a).is_err());
        let by_url = json!({"do": "mixer", "url": "http://10.0.0.5:29214", "snapshot": "Band"});
        assert_eq!(Action::from_json(&by_url).unwrap().to_json(), by_url);
        let e = Action::from_json(&json!({"do": "mixer", "snapshot": "Band"})).unwrap_err();
        assert!(e.contains("desk"), "{e}");
        let e = Action::from_json(&json!({"do": "mixer", "desk": "FOH (Mixer)"})).unwrap_err();
        assert!(e.contains("snapshot"), "{e}");
    }

    #[test]
    fn osc_and_mixer_actions_reach_the_network() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let to = rx.local_addr().unwrap().to_string();
        run_external(&Action::Mixer {
            desk: String::new(),
            url: format!("osc://{to}"),
            snapshot: "Speech".into(),
        })
        .unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        let m = idn_open_protocol::osc::decode(&buf[..n]).unwrap();
        assert_eq!(m[0].addr, "/snapshot/recall");
        assert_eq!(
            m[0].args,
            vec![idn_open_protocol::osc::Arg::Str("Speech".into())]
        );
        // A web request to nowhere says where it failed.
        let e = run_external(&Action::Http {
            method: "POST".into(),
            url: "http://127.0.0.1:9/x".into(),
            body: String::new(),
        })
        .unwrap_err();
        assert!(e.starts_with("http://127.0.0.1:9/x"), "{e}");
    }
}
