//! An engine's API front, shared (the desk, the switcher, the display service): over HTTP
//! the state (with a long poll), the commands' schema, the audit, a command by
//! `POST /api/v1/<cmd>` (the change header, per-control sequence numbers, a read-only or
//! out-of-scope caller refused and audited); and the push channel (`GET /api/v1/events`):
//! the state when what this caller sees changes, typed events, topics a client subscribes
//! to (`stats`, `meters`, `audit`), and commands answered in order. Each engine says what
//! is its own through [`Engine`].

use super::events::{self, Sequencer};
use super::{Request, Response, WebSocket, Who, audit};
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A command refused or failed: the HTTP status it answers with, and why. A refusal for
/// the caller's rights (403) or a locked engine (423) is also told on the push channel as
/// a `denied` event.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: &'static str,
    pub why: String,
}

impl Refusal {
    pub fn new(status: &'static str, why: impl Into<String>) -> Refusal {
        Refusal {
            status,
            why: why.into(),
        }
    }

    /// Said to the caller's push channel as `denied`.
    fn denies(&self) -> bool {
        self.status.starts_with("403") || self.status.starts_with("423")
    }
}

impl From<String> for Refusal {
    /// A command that failed (bad values, nothing such): 400.
    fn from(why: String) -> Refusal {
        Refusal::new("400 Bad Request", why)
    }
}

/// What an engine brings to the shared front.
pub trait Engine: Send + Sync + 'static {
    /// The header a change over HTTP must carry (`X-IDN-Mixer`): no cross-site form can
    /// send it.
    fn change_header(&self) -> &'static str;
    /// The engine's audit (every change, and every refusal, with who).
    fn audit(&self) -> &audit::Audit;
    /// HTTP's per-control sequence numbers (one for the engine; each push channel has its
    /// own).
    fn http_sequencer(&self) -> &Mutex<Sequencer>;
    /// A number that moves with every change worth sending.
    fn version(&self) -> u64;
    /// The long poll: waits up to `timeout` for the version to pass `after` (or for a
    /// change the version doesn't count).
    fn wait_change(&self, after: u64, timeout: Duration);
    /// What `who` sees of the state (a scoped token: its part).
    fn state_for(&self, who: &Who) -> Value;
    /// The commands' JSON schema.
    fn schema(&self) -> Value;
    /// The state's JSON schema (`GET /api/v1/state_schema`), when the engine has one.
    fn state_schema(&self) -> Option<Value> {
        None
    }
    /// Whether `who` may run `cmd` with `body` (beyond the default: a read-only caller
    /// refused). Refused: 403, audited and told as `denied`.
    fn permits(&self, who: &Who, cmd: &str, body: &Value) -> Result<(), String> {
        let _ = (cmd, body);
        who.may_change().map_err(str::to_owned)
    }
    /// Runs `cmd` for `who`, written in the audit as theirs by the engine: its answer (an
    /// object, merged into `{"ok": true}`), or why not.
    fn run_for(&self, who: &Who, cmd: &str, body: &Value) -> Result<Value, Refusal>;
    /// The typed failure events after `seen` for `who` (each a message), `seen` moved on;
    /// none to a caller who isn't to have them.
    fn pushed_events(&self, who: &Who, seen: &mut u64) -> Vec<String> {
        let _ = (who, seen);
        Vec::new()
    }
    /// Where the typed events are now (a channel sends those after it opened).
    fn event_seq(&self) -> u64 {
        0
    }
    /// A topic a push channel subscribes to (`stats`, `meters`): how often it is sent;
    /// `None`: no such topic here.
    fn topic_every(&self, name: &str) -> Option<Duration> {
        let _ = name;
        None
    }
    /// The topic's message for `who` (its `type` set to the topic's name here).
    fn topic(&self, name: &str, who: &Who) -> Value {
        let _ = (name, who);
        Value::Null
    }
    /// Whether `who` may have the audit (a scoped token: no).
    fn audit_for(&self, who: &Who) -> Result<(), String> {
        let _ = who;
        Ok(())
    }
    /// The engine's own pages, looked at first; `None`: the shared ones.
    fn route(this: &Arc<Self>, r: &Request, who: &Who) -> Option<Response> {
        let _ = (this, r, who);
        None
    }
}

/// A request from `who` (already checked by its front: this machine, a token, a
/// password).
pub fn handle_as<E: Engine>(e: &Arc<E>, r: Request, who: Who) -> Response {
    if !who.ready() && r.path != "/api/v1/events" {
        return Response::json_err("401 Unauthorized", "not logged in");
    }
    if let Some(resp) = E::route(e, &r, &who) {
        return resp;
    }
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/api/v1/events") => {
            let e = e.clone();
            Response::websocket(&r, move |ws| events_as(&e, ws, who))
        }
        ("GET", "/api/v1/audit") => match e.audit_for(&who) {
            Ok(()) => audit::answer(e.audit(), &r),
            Err(why) => Response::json_err("403 Forbidden", &why),
        },
        ("GET", "/api/v1/schema") => Response::json(e.schema().to_string()),
        ("GET", "/api/v1/state_schema") => match e.state_schema() {
            Some(s) => Response::json(s.to_string()),
            None => Response::json_err("404 Not Found", "no state schema here"),
        },
        ("GET", "/api/v1/state") => {
            if let Some(after) = r.param("after").and_then(|v| v.parse::<u64>().ok()) {
                e.wait_change(after, Duration::from_secs(20));
            }
            Response::json(e.state_for(&who).to_string()).with("Cache-Control", "no-store")
        }
        ("POST", p) => post(e, &r, p, &who),
        _ => Response::json_err("404 Not Found", "no such page"),
    }
}

/// `POST /api/v1/<cmd>`.
fn post<E: Engine>(e: &Arc<E>, r: &Request, path: &str, who: &Who) -> Response {
    let header = e.change_header();
    if !r.may_change(header) {
        return Response::json_err("403 Forbidden", &format!("missing {header}: 1"));
    }
    let Some(cmd) = path.strip_prefix("/api/v1/") else {
        return Response::json_err("404 Not Found", "no such page");
    };
    let body: Value = if r.body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&r.body) {
            Ok(v) => v,
            Err(err) => {
                return Response::json_err("400 Bad Request", &format!("bad JSON: {err}"));
            }
        }
    };
    // A read-only caller, or one outside its scope (§N.31.5): refused, and said in the
    // audit.
    if let Err(why) = e.permits(who, cmd, &body) {
        e.audit().record(who, cmd, &body, Some(&why));
        return Response::json_err("403 Forbidden", &why);
    }
    // A change older than one already applied to its control: not applied.
    let key = Sequencer::of_request(r);
    if let Some((c, n)) = &key
        && !e.http_sequencer().lock_ok().fresh(c, *n)
    {
        return Response::json(json!({"ok": true, "skipped": true}).to_string());
    }
    match e.run_for(who, cmd, &body) {
        Ok(extra) => Response::json(merged(extra).to_string()),
        Err(no) => {
            // Refused: its number isn't used up (the client sends it again, fixed).
            if let Some((c, _)) = &key {
                e.http_sequencer().lock_ok().revert(c);
            }
            Response::json_err(no.status, &no.why)
        }
    }
}

/// `{"ok": true}` with the answer's fields.
fn merged(extra: Value) -> Value {
    let mut out = json!({"ok": true});
    if let Value::Object(m) = extra {
        for (k, x) in m {
            out[k] = x;
        }
    }
    out
}

/// One client's push channel: the state when what `who` sees changes (`{"type":
/// "state", "version", "state"}`; at most every 40 ms, and looked at again each second
/// for changes the version doesn't count), the typed events after it opened, a `denied`
/// event when its command is refused for its rights or a lock, the topics it subscribes
/// to (`{"subscribe": ["stats" | "meters" | "audit", …]}`), and its commands (`{"id",
/// "cmd", "body"}`) answered in order. Opened on the network without a token, it sends
/// nothing until its first message gives one (`{"token": …}`, idn-node.md §N.31.5).
pub fn events_as<E: Engine>(e: &Arc<E>, ws: WebSocket, who: Who) {
    // (The channel's 50 ms tick, less its jitter: a tick a little early isn't skipped.)
    const EVERY: Duration = Duration::from_millis(40);
    const RECHECK: Duration = Duration::from_secs(1);
    let caller = super::auth::Channel::new(who);
    let audit_seen = std::cell::Cell::new(None::<u64>);
    // Subscribed topics, each with when it was last sent.
    let topics: std::cell::RefCell<Vec<(String, Duration, Option<Instant>)>> =
        std::cell::RefCell::default();
    // The state last sent (its version and text without the version), when, and when last
    // looked at.
    let mut sent: Option<(u64, String)> = None;
    let (mut sent_at, mut looked_at) = (None::<Instant>, Instant::now());
    let mut seq = Sequencer::default();
    let mut ev_seen = e.event_seq();
    let denied: std::cell::RefCell<Vec<String>> = std::cell::RefCell::default();
    events::run(
        ws,
        || {
            let mut out = Vec::new();
            if caller.ended() {
                return events::Next {
                    send: out,
                    end: true,
                };
            }
            if !caller.ready() {
                return out.into();
            }
            let who = caller.who();
            let v = e.version();
            let fresh = sent.as_ref().is_none_or(|(sv, _)| *sv != v);
            if sent_at.is_none_or(|t| t.elapsed() >= EVERY)
                && (fresh || looked_at.elapsed() >= RECHECK)
            {
                looked_at = Instant::now();
                let mut st = e.state_for(&who);
                // Compared without its version (which may grow with changes this caller
                // doesn't see).
                let version = st.as_object_mut().and_then(|o| o.remove("version"));
                let seen = st.to_string();
                if sent.as_ref().is_none_or(|(_, t)| *t != seen) {
                    if let (Some(o), Some(ver)) = (st.as_object_mut(), version) {
                        o.insert("version".into(), ver);
                    }
                    out.push(
                        json!({"type": "state", "version": st["version"], "state": st}).to_string(),
                    );
                    sent_at = Some(Instant::now());
                }
                sent = Some((v, seen));
            }
            for (name, every, at) in topics.borrow_mut().iter_mut() {
                if at.is_none_or(|t| t.elapsed() >= *every) {
                    *at = Some(Instant::now());
                    let mut m = e.topic(name, &who);
                    if m.is_object() {
                        m["type"] = json!(name);
                    }
                    out.push(m.to_string());
                }
            }
            if let Some(mut seen) = audit_seen.get() {
                out.extend(audit::pushed(e.audit(), &mut seen));
                audit_seen.set(Some(seen));
            }
            out.extend(e.pushed_events(&who, &mut ev_seen));
            out.append(&mut denied.borrow_mut());
            out.into()
        },
        |v| {
            if let Some(r) = caller.intercept(&v) {
                return Some(r);
            }
            for (k, on) in [("subscribe", true), ("unsubscribe", false)] {
                if let Some(l) = v[k].as_array() {
                    let who = caller.who();
                    for name in l.iter().filter_map(Value::as_str) {
                        if name == "audit" {
                            if on && let Err(why) = e.audit_for(&who) {
                                return Some(json!({"type": k, "ok": false, "error": why}));
                            }
                            audit_seen.set(on.then(|| e.audit().seq()));
                        } else if let Some(every) = e.topic_every(name) {
                            let mut t = topics.borrow_mut();
                            t.retain(|(n, _, _)| n != name);
                            if on {
                                t.push((name.to_owned(), every, None));
                            }
                        }
                    }
                    return Some(json!({"type": k, "ok": true}));
                }
            }
            let Some((cmd, body, id)) = events::command_of(&v) else {
                return Some(events::not_a_command(&v));
            };
            let who = caller.who();
            if let Err(why) = e.permits(&who, &cmd, &body) {
                e.audit().record(&who, &cmd, &body, Some(&why));
                denied
                    .borrow_mut()
                    .push(super::problems::denied(&cmd, &why));
                return Some(events::reply(id, Err(why)));
            }
            let key = Sequencer::of(&v);
            if let Some((c, n)) = &key
                && !seq.fresh(c, *n)
            {
                return Some(events::skipped(id));
            }
            let done = e.run_for(&who, &cmd, &body);
            if let Err(no) = &done {
                if no.denies() {
                    denied
                        .borrow_mut()
                        .push(super::problems::denied(&cmd, &no.why));
                }
                // Refused: its number isn't used up.
                if let Some((c, _)) = &key {
                    seq.revert(c);
                }
            }
            Some(events::reply(id, done.map_err(|no| no.why)))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_merge_into_ok_and_refusals_keep_their_status() {
        assert_eq!(merged(json!({"id": "e1"})), json!({"ok": true, "id": "e1"}));
        assert_eq!(merged(Value::Null), json!({"ok": true}));
        let r: Refusal = "no such channel".to_owned().into();
        assert_eq!(r.status, "400 Bad Request");
        assert!(!r.denies());
        assert!(Refusal::new("423 Locked", "the desk is locked").denies());
        assert!(Refusal::new("403 Forbidden", "read-only").denies());
    }
}
