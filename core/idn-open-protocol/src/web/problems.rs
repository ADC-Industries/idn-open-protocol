//! Typed failure events and the problems now (idn-node.md §N.41): what an engine, a
//! Node or the Site Manager says went wrong, three ways at once.
//!
//! - **The push channel**: each event as `{"type": "event", "event": NAME, "level",
//!   "text", "at", …fields}` ([`Events::emit`], [`Events::pushed`]).
//! - **The state**: what is wrong now, `problems: [{"problem": NAME, "level", "text",
//!   …fields}]`, kept by whoever owns the state (a problem is a condition, an event a
//!   change).
//! - **The Site Manager's log**: through the machine's IDN Node, as an alert carrying the
//!   event's name and fields (§N.18). Rate-limited and deduplicated ([`LogForward`]): the
//!   same event about the same thing (an input, a screen, a destination) goes at most once
//!   a minute, the next one saying how many were held back (`repeats`), so a source that
//!   comes and goes doesn't flood the log.
//!
//! Not MQTT.

use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The same event about the same thing goes to the Site Manager's log at most this often.
pub const LOG_EVERY: Duration = Duration::from_secs(60);

/// Events kept for the push channels (a client's place moves past older ones).
const KEPT: usize = 500;

/// The fields that say what an event is about (the first one there is its subject): an
/// event about another of these is a different event for the log.
pub const SUBJECTS: &[&str] = &[
    "input",
    "screen",
    "destination",
    "id",
    "machine",
    "fingerprint",
    "feed",
    "output",
    "desk",
    "channel",
    "camera",
    "receiver",
    "source",
    "cmd",
];

/// A typed event: `{"type": "event", "event", "level", "text", "at", …fields}`.
/// `level`: `error`, `warning`, `info` or `ok` (a problem over).
pub fn event(name: &str, level: &str, text: &str, fields: Value) -> Value {
    let mut v = match fields {
        Value::Object(m) => Value::Object(m),
        _ => json!({}),
    };
    v["type"] = json!("event");
    v["event"] = json!(name);
    v["level"] = json!(level);
    v["text"] = json!(text);
    if v.get("at").is_none() {
        v["at"] = json!(idn_common::epoch::secs());
    }
    v
}

/// A problem for a state's `problems` list: `{"problem", "level", "text", …fields}`.
pub fn problem(name: &str, level: &str, text: &str, fields: Value) -> Value {
    let mut v = match fields {
        Value::Object(m) => Value::Object(m),
        _ => json!({}),
    };
    v["problem"] = json!(name);
    v["level"] = json!(level);
    v["text"] = json!(text);
    v
}

/// What makes two events the same one for the log: the name and its subject's value.
pub fn key_of(ev: &Value) -> String {
    let name = ev["event"].as_str().unwrap_or("");
    let subject = SUBJECTS
        .iter()
        .find_map(|k| ev.get(*k).filter(|v| !v.is_null()).map(Value::to_string))
        .unwrap_or_default();
    format!("{name}\u{1f}{subject}")
}

/// One line for the Site Manager's log.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub level: String,
    pub event: String,
    pub text: String,
    /// The event's fields (without `type`, `event`, `level`, `text`), and `repeats` when
    /// some were held back since the last one.
    pub detail: Value,
}

type Sink = Box<dyn Fn(&LogLine) + Send + Sync>;

/// The rate limit and deduplication in front of the Site Manager's log.
#[derive(Default)]
pub struct LogForward {
    /// Per event key: when one last went, and how many were held back since.
    sent: Mutex<HashMap<String, (Instant, u64)>>,
    sink: Mutex<Option<Sink>>,
}

impl std::fmt::Debug for LogForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LogForward")
    }
}

impl LogForward {
    /// Where lines go (the machine's Node; the Site Manager's own log).
    pub fn set_sink(&self, f: impl Fn(&LogLine) + Send + Sync + 'static) {
        *self.sink.lock_ok() = Some(Box::new(f));
    }

    /// Lines go to the machine's Node (`Node::event`), as `app`'s: from a thread of their
    /// own (a Node slow to answer holds up no engine), at most 256 waiting.
    pub fn to_node(&self, node: Arc<crate::node::Node>) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<LogLine>(256);
        let spawned = std::thread::Builder::new()
            .name("idn-events-node".into())
            .spawn(move || {
                while let Ok(l) = rx.recv() {
                    let _ = node.event(&l.level, &l.event, &l.text, &l.detail);
                }
            });
        if spawned.is_ok() {
            self.set_sink(move |l| {
                let _ = tx.try_send(l.clone());
            });
        }
    }

    /// An event offered to the log: the line that goes now, or `None` (held back: the
    /// same one went less than [`LOG_EVERY`] ago; counted for the next).
    pub fn offer(&self, ev: &Value) -> Option<LogLine> {
        let key = key_of(ev);
        let now = Instant::now();
        let repeats = {
            let mut sent = self.sent.lock_ok();
            match sent.get_mut(&key) {
                Some((at, held)) if now.duration_since(*at) < LOG_EVERY => {
                    *held += 1;
                    return None;
                }
                Some((at, held)) => {
                    let r = *held;
                    *at = now;
                    *held = 0;
                    r
                }
                None => {
                    // Bounded: the oldest let go past 4096 subjects.
                    if sent.len() >= 4096
                        && let Some(old) = sent
                            .iter()
                            .min_by_key(|(_, (at, _))| *at)
                            .map(|(k, _)| k.clone())
                    {
                        sent.remove(&old);
                    }
                    sent.insert(key, (now, 0));
                    0
                }
            }
        };
        let mut detail = ev.clone();
        if let Some(m) = detail.as_object_mut() {
            for k in ["type", "event", "level", "text"] {
                m.remove(k);
            }
            if repeats > 0 {
                m.insert("repeats".into(), json!(repeats));
            }
        }
        let line = LogLine {
            level: ev["level"].as_str().unwrap_or("warning").to_owned(),
            event: ev["event"].as_str().unwrap_or("").to_owned(),
            text: ev["text"].as_str().unwrap_or("").to_owned(),
            detail,
        };
        if let Some(s) = self.sink.lock_ok().as_ref() {
            s(&line);
        }
        Some(line)
    }
}

#[derive(Default)]
struct Inner {
    seq: u64,
    recent: VecDeque<(u64, Value)>,
}

/// An engine's (a Node's, the Site Manager's) typed events: kept for its push channels,
/// and offered to the Site Manager's log.
#[derive(Default)]
pub struct Events {
    inner: Mutex<Inner>,
    /// Told when an event is put (for [`Events::wait`]).
    put_cv: std::sync::Condvar,
    /// The log's rate limit, and where its lines go.
    pub log: LogForward,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Events({})", self.seq())
    }
}

impl Events {
    pub fn new() -> Arc<Events> {
        Arc::new(Events::default())
    }

    /// An event ([`event`]): to the push channels, and to the log (rate-limited). The
    /// event as sent.
    pub fn emit(&self, name: &str, level: &str, text: &str, fields: Value) -> Value {
        let ev = event(name, level, text, fields);
        self.put(ev.clone());
        self.log.offer(&ev);
        ev
    }

    /// An event to the push channels only (one the log has already, e.g. another
    /// program's passed on by the Node).
    pub fn put(&self, ev: Value) {
        let mut g = self.inner.lock_ok();
        g.seq += 1;
        let s = g.seq;
        g.recent.push_back((s, ev));
        while g.recent.len() > KEPT {
            g.recent.pop_front();
        }
        drop(g);
        self.put_cv.notify_all();
    }

    /// The events after place `seq`, oldest first, each with its place; the place now;
    /// and whether some after `seq` are no longer kept (only the newest 500 are: the
    /// caller then reads the whole state again).
    pub fn since_numbered(&self, seq: u64) -> (Vec<(u64, Value)>, u64, bool) {
        let g = self.inner.lock_ok();
        let oldest = g.recent.front().map_or(g.seq + 1, |(s, _)| *s);
        (
            g.recent.iter().filter(|(s, _)| *s > seq).cloned().collect(),
            g.seq,
            seq + 1 < oldest && seq < g.seq,
        )
    }

    /// [`since_numbered`](Self::since_numbered), waiting up to `timeout` for one when
    /// there is none after `seq` yet.
    pub fn wait(&self, seq: u64, timeout: Duration) -> (Vec<(u64, Value)>, u64, bool) {
        let end = Instant::now() + timeout;
        let mut g = self.inner.lock_ok();
        while g.seq <= seq {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            g = self
                .put_cv
                .wait_timeout(g, left)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        drop(g);
        self.since_numbered(seq)
    }

    /// The place of the newest event.
    pub fn seq(&self) -> u64 {
        self.inner.lock_ok().seq
    }

    /// The events after place `seq`, oldest first, and the place now.
    pub fn since(&self, seq: u64) -> (Vec<Value>, u64) {
        let g = self.inner.lock_ok();
        (
            g.recent
                .iter()
                .filter(|(s, _)| *s > seq)
                .map(|(_, v)| v.clone())
                .collect(),
            g.seq,
        )
    }

    /// The newest `n` events, newest first (`GET …/events/recent`).
    pub fn newest(&self, n: usize) -> Vec<Value> {
        self.inner
            .lock_ok()
            .recent
            .iter()
            .rev()
            .take(n)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// A push channel's new events since `seen` (each a message), `seen` moved on.
    pub fn pushed(&self, seen: &mut u64) -> Vec<String> {
        let (evs, now) = self.since(*seen);
        *seen = now;
        evs.into_iter().map(|e| e.to_string()).collect()
    }
}

/// The `denied` event for a push channel whose command was refused (403: a read-only
/// token, a scope that doesn't cover it): sent to that caller's channel only, beside the
/// reply. The audit has the refusal already.
pub fn denied(cmd: &str, error: &str) -> String {
    event(
        "denied",
        "warning",
        &format!("{cmd}: {error}"),
        json!({"cmd": cmd, "error": error}),
    )
    .to_string()
}

/// What a lost source's reason says, for a typed event's `cause`: `other_site` (the source
/// is enrolled with another site), `refused` (its certificate or this one's was refused:
/// revoked, or not a member), `not_found` (not on the network), `unreachable` (found, no
/// answer), `ended` (it stopped), else `other`.
pub fn loss_cause(reason: &str) -> &'static str {
    let r = reason.to_ascii_lowercase();
    if r.contains("another site") {
        "other_site"
    } else if r.contains("refused") && (r.contains("certificate") || r.contains("revoked")) {
        "refused"
    } else if r.contains("not found") || r.contains("no source") || r.contains("isn't on") {
        "not_found"
    } else if r.contains("timed out")
        || r.contains("unreachable")
        || r.contains("no answer")
        || r.contains("connection refused")
    {
        "unreachable"
    } else if r.contains("ended") || r.contains("closed") || r.contains("gone") {
        "ended"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same event about the same thing reaches the log once a minute, the next one
    /// with how many were held back; another subject or another event goes at once; the
    /// push channel gets every one.
    #[test]
    fn the_log_gets_an_event_once_and_the_channel_every_time() {
        let e = Events::new();
        let lines = Arc::new(Mutex::new(Vec::<LogLine>::new()));
        {
            let l = lines.clone();
            e.log.set_sink(move |x| l.lock_ok().push(x.clone()));
        }
        for _ in 0..5 {
            e.emit(
                "lost",
                "warning",
                "Cam 1 lost",
                json!({"input": 1, "reason": "x"}),
            );
        }
        e.emit("lost", "warning", "Cam 2 lost", json!({"input": 2}));
        e.emit("recovered", "ok", "Cam 1 back", json!({"input": 1}));
        let l = lines.lock_ok().clone();
        assert_eq!(l.len(), 3, "{l:?}");
        assert_eq!(l[0].event, "lost");
        assert_eq!(l[0].detail["input"], 1);
        assert!(l[0].detail.get("type").is_none());
        let (all, seq) = e.since(0);
        assert_eq!((all.len(), seq), (7, 7));
        assert_eq!(all[0]["type"], "event");
        assert_eq!(all[0]["event"], "lost");
        let mut seen = 6;
        assert_eq!(e.pushed(&mut seen).len(), 1);
        assert_eq!(seen, 7);
        // Held back ones are counted on the next that goes.
        let f = LogForward::default();
        let ev = event("lost", "warning", "t", json!({"screen": "HDMI-1"}));
        assert!(f.offer(&ev).is_some());
        assert!(f.offer(&ev).is_none());
        assert!(f.offer(&ev).is_none());
        f.sent
            .lock_ok()
            .values_mut()
            .for_each(|(at, _)| *at = Instant::now() - LOG_EVERY - Duration::from_secs(1));
        assert_eq!(f.offer(&ev).unwrap().detail["repeats"], 2);
    }

    #[test]
    fn a_losss_reason_has_a_cause() {
        assert_eq!(
            loss_cause("BOOTH (Cam) is enrolled with another site authority"),
            "other_site"
        );
        assert_eq!(loss_cause("source not found"), "not_found");
        assert_eq!(loss_cause("connect: timed out"), "unreachable");
        assert_eq!(loss_cause("something odd"), "other");
        let d: Value = serde_json::from_str(&denied("cut", "read-only")).unwrap();
        assert_eq!(
            (d["event"].as_str(), d["cmd"].as_str()),
            (Some("denied"), Some("cut"))
        );
    }
}
