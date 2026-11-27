//! The APIs' push channel (IDN Media as a server and libraries; the UI in the host program): one
//! WebSocket a client, `GET /api/v1/events`, in place of polling. The server sends JSON
//! messages as things change (`{"type": "state", "version": …, "state": …}`, meters once
//! the client asks for them, alerts); the client sends commands
//! (`{"id": 1, "cmd": "cut", "body": {…}, "seq": 5}`) and gets a reply for each
//! (`{"type": "reply", "id": 1, "ok": true, "result": {…}}`), in the order it sent them.
//!
//! Per-control sequence numbers: a change may name its control and carry a number that
//! only grows for it (`"control": "ch1.fader", "seq": 57`); one older than the last applied
//! for that control (a late arrival) is answered `"skipped": true` and not applied
//! ([`Sequencer`]: per WebSocket; over HTTP the headers `X-IDN-Client`, `X-IDN-Control`,
//! `X-IDN-Seq`, per client).

use super::WebSocket;
use serde_json::{Value, json};
use std::time::Duration;
use tungstenite::Message;

/// How often a connection looks for something to send (its read waits this long).
pub const TICK: Duration = Duration::from_millis(50);

/// A ping this often when nothing else was sent; no frame from the client for
/// [`QUIET_LIMIT`] (a pong counts) ends the channel (bug run O2-M2: a tablet that slept
/// kept its connection, and a thread, for good).
pub const PING_EVERY: Duration = Duration::from_secs(15);
pub const QUIET_LIMIT: Duration = Duration::from_secs(45);

/// What a channel's `next` gives: messages to send, and whether to end it after them (a
/// session that ended).
pub struct Next {
    pub send: Vec<String>,
    pub end: bool,
}

impl From<Vec<String>> for Next {
    fn from(send: Vec<String>) -> Self {
        Next { send, end: false }
    }
}

/// Runs one client's channel until it closes, a send fails, it goes quiet past
/// [`QUIET_LIMIT`] or `next` says to end: each tick `next` gives what to send now (each a
/// JSON text); each message the client sends goes to `on_message`, whose answer (when there
/// is one) is sent back at once. A message that isn't JSON, or a binary one, is answered
/// with an error.
pub fn run(
    mut ws: WebSocket,
    mut next: impl FnMut() -> Next,
    mut on_message: impl FnMut(Value) -> Option<Value>,
) {
    use std::time::Instant;
    let (mut heard, mut sent) = (Instant::now(), Instant::now());
    loop {
        let got = ws.read();
        if got.is_ok() {
            heard = Instant::now();
        }
        match got {
            Ok(Message::Text(t)) => {
                let reply = match serde_json::from_str::<Value>(t.as_str()) {
                    Ok(v) => on_message(v),
                    Err(e) => Some(json!({"type": "error", "error": format!("bad JSON: {e}")})),
                };
                if let Some(r) = reply {
                    if ws.send(Message::text(r.to_string())).is_err() {
                        return;
                    }
                    sent = Instant::now();
                }
            }
            Ok(Message::Binary(_)) => {
                let e = json!({"type": "error", "error": "JSON text messages only"});
                if ws.send(Message::text(e.to_string())).is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) => {
                let _ = ws.close(None);
                let _ = ws.flush();
                return;
            }
            Ok(_) => {}
            // Nothing this tick (a read's timeout).
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
        if heard.elapsed() >= QUIET_LIMIT {
            let _ = ws.close(None);
            let _ = ws.flush();
            return;
        }
        let n = next();
        for m in n.send {
            if ws.send(Message::text(m)).is_err() {
                return;
            }
            sent = Instant::now();
        }
        if n.end {
            let _ = ws.close(None);
            let _ = ws.flush();
            return;
        }
        if sent.elapsed() >= PING_EVERY {
            if ws.send(Message::Ping(Default::default())).is_err() {
                return;
            }
            sent = Instant::now();
        }
    }
}

/// The reply to a message that isn't a command (no `cmd`), so a client waiting on its id
/// isn't left waiting (bug run O2-L9).
pub fn not_a_command(v: &Value) -> Value {
    with_id(
        json!({"type": "reply", "ok": false, "error": "not a command: {\"id\", \"cmd\", \"body\"}"}),
        v["id"].clone(),
    )
}

/// The last sequence number applied per control (one client's, or per client over HTTP):
/// a change older than it is skipped. A control not changed for [`SEQ_IDLE`] is forgotten
/// (a client started again counts from 1; bug run O2-M6). Bounded: the oldest controls
/// forgotten past 4096.
#[derive(Debug, Default)]
pub struct Sequencer {
    /// Per control: the last number taken, when, and the one before (given back by
    /// [`Sequencer::revert`]).
    last: std::collections::HashMap<String, (u64, std::time::Instant, Option<u64>)>,
}

/// How long a control's last number counts (late arrivals come within milliseconds).
pub const SEQ_IDLE: Duration = Duration::from_secs(30);

impl Sequencer {
    /// Whether the change to `control` numbered `seq` is to be applied (newer than the last
    /// applied, or the control idle); it is then the last.
    pub fn fresh(&mut self, control: &str, seq: u64) -> bool {
        let now = std::time::Instant::now();
        let before = self
            .last
            .get(control)
            .filter(|(_, at, _)| now.duration_since(*at) < SEQ_IDLE)
            .map(|(n, _, _)| *n);
        if before.is_some_and(|last| seq <= last) {
            return false;
        }
        if self.last.len() >= 4096
            && !self.last.contains_key(control)
            && let Some(oldest) = self
                .last
                .iter()
                .min_by_key(|(_, (_, at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            self.last.remove(&oldest);
        }
        self.last.insert(control.to_owned(), (seq, now, before));
        true
    }

    /// The last [`fresh`](Self::fresh) for `control` undone: a change refused (the desk
    /// locked, a bad body) doesn't use up its number (bug run O2-L7).
    pub fn revert(&mut self, control: &str) {
        let prev = self.last.get(control).map(|(_, _, p)| *p);
        match prev {
            Some(Some(p)) => {
                if let Some(e) = self.last.get_mut(control) {
                    *e = (p, e.1, None);
                }
            }
            Some(None) => {
                self.last.remove(control);
            }
            None => {}
        }
    }

    /// A message's control and number, when it names both.
    pub fn of(v: &Value) -> Option<(String, u64)> {
        Some((v["control"].as_str()?.to_owned(), v["seq"].as_u64()?))
    }

    /// An HTTP request's (`X-IDN-Client`, `X-IDN-Control`, `X-IDN-Seq`): the key (client
    /// and control) and the number.
    pub fn of_request(r: &super::Request) -> Option<(String, u64)> {
        Some((
            format!(
                "{}\u{1f}{}",
                r.header("x-idn-client")?,
                r.header("x-idn-control")?
            ),
            r.header("x-idn-seq")?.parse().ok()?,
        ))
    }
}

/// The reply to a change skipped as older than one already applied.
pub fn skipped(id: Value) -> Value {
    with_id(json!({"type": "reply", "ok": true, "skipped": true}), id)
}

/// A reply with the message's `id`, left out when the message had none (Alpha87 lab 544:
/// `"id": null` before).
fn with_id(mut reply: Value, id: Value) -> Value {
    if !id.is_null()
        && let Some(o) = reply.as_object_mut()
    {
        o.insert("id".into(), id);
    }
    reply
}

/// A command message's parts (`{"id", "cmd", "body", "seq"}`): the command, its body
/// (`{}` when none) and its id (echoed in the reply).
pub fn command_of(v: &Value) -> Option<(String, Value, Value)> {
    let cmd = v["cmd"].as_str()?.to_owned();
    let body = if v["body"].is_null() {
        json!({})
    } else {
        v["body"].clone()
    };
    Some((cmd, body, v["id"].clone()))
}

/// The reply to command `id`: `ok` and what the command answered (`result`: as the HTTP
/// API's reply holds it beside `ok`; apart here, so a command's own `id` doesn't meet the
/// message's), or its error.
pub fn reply(id: Value, r: Result<Value, String>) -> Value {
    match r {
        Ok(result) => with_id(json!({"type": "reply", "ok": true, "result": result}), id),
        Err(e) => with_id(json!({"type": "reply", "ok": false, "error": e}), id),
    }
}

/// A topic a channel pushes ([`run_as_requests`]): its name, the GET whose answer it is,
/// how often it is looked at, and whether it is sent without being asked for.
#[derive(Debug, Clone, Copy)]
pub struct Topic {
    pub name: &'static str,
    pub path: &'static str,
    pub every: Duration,
    pub always: bool,
}

/// A channel whose every message is answered as the same request over HTTP would be, in
/// the session `base` (the WebSocket's request) carried: `handle` is the API's own handler,
/// so its logins, roles and audit apply.
/// - Pushed: each topic's GET when its answer changed (`{"type": name, name: answer}`);
///   `always` ones from the start, the others after `{"subscribe": [names]}` (until
///   `{"unsubscribe": [names]}`). An answer that isn't 2xx (the session ended) is said as
///   `{"type": "error"}`.
/// - Commands `{"id", "cmd", "body"}`: `POST /api/v1/<cmd>` with that body (and `with`'s
///   headers: the API's change header), replied `{"type": "reply", "id", "ok", …}`.
/// - `{"token": "idn1…"}` (a site-signed token, idn-node.md §N.31.5): the requests carry
///   it from then on (`Authorization: Bearer`), so the API checks it as over HTTP.
pub fn run_as_requests(
    ws: WebSocket,
    base: &super::Request,
    with: &[(&str, &str)],
    topics: &[Topic],
    handle: impl Fn(super::Request) -> super::Response,
) {
    run_as_requests_awaiting(ws, base, with, topics, false, handle)
}

/// [`run_as_requests`]; with `awaiting`, nothing is pushed or done until the client's
/// first message gives its token (`{"token": …}`; a browser can't set a WebSocket's
/// headers), and the channel ends if none comes within
/// [`TOKEN_WAIT`](super::auth::TOKEN_WAIT).
pub fn run_as_requests_awaiting(
    ws: WebSocket,
    base: &super::Request,
    with: &[(&str, &str)],
    topics: &[Topic],
    awaiting: bool,
    handle: impl Fn(super::Request) -> super::Response,
) {
    run_as_requests_with(ws, base, with, topics, awaiting, None, handle)
}

/// [`run_as_requests_awaiting`], also sending the typed events of `feed` as they come
/// (`{"type": "event", …}`, idn-node.md §N.41; those after the channel opened). A command
/// refused (403) gets a `denied` event on this channel beside its reply.
pub fn run_as_requests_with(
    ws: WebSocket,
    base: &super::Request,
    with: &[(&str, &str)],
    topics: &[Topic],
    awaiting: bool,
    feed: Option<&super::problems::Events>,
    handle: impl Fn(super::Request) -> super::Response,
) {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::time::Instant;
    let opened = Instant::now();
    // A token given in a message: carried by every request from then on.
    let token: RefCell<Option<String>> = RefCell::new(None);
    let like = |method: &str, path: &str, body: Vec<u8>| {
        let mut headers = base.headers.clone();
        if let Some(t) = token.borrow().as_ref() {
            headers.insert("authorization".into(), format!("Bearer {t}"));
        }
        // Not an upgrade any more; a body of this length.
        headers.retain(|k, _| {
            !matches!(
                k.as_str(),
                "upgrade" | "connection" | "content-length" | "origin"
            ) && !k.starts_with("sec-websocket")
        });
        for (k, v) in with {
            headers.insert(k.to_ascii_lowercase(), (*v).to_owned());
        }
        super::Request {
            method: method.into(),
            path: path.into(),
            query: String::new(),
            headers,
            body,
            peer: base.peer,
            tls: base.tls,
        }
    };
    let wanted = RefCell::new(
        topics
            .iter()
            .map(|t| (t.name, t.always))
            .collect::<BTreeMap<_, _>>(),
    );
    let last: RefCell<BTreeMap<&str, (Instant, Vec<u8>)>> = RefCell::default();
    let mut seq = Sequencer::default();
    let ev_seen = std::cell::Cell::new(feed.map_or(0, |f| f.seq()));
    // A refused command's `denied` event, sent after its reply.
    let denied: RefCell<Vec<String>> = RefCell::default();
    run(
        ws,
        || {
            let mut out = Vec::new();
            let mut end = false;
            if awaiting && token.borrow().is_none() {
                return Next {
                    send: out,
                    end: opened.elapsed() >= super::auth::TOKEN_WAIT,
                };
            }
            for t in topics {
                if !wanted.borrow().get(t.name).copied().unwrap_or(false)
                    || last
                        .borrow()
                        .get(t.name)
                        .is_some_and(|(at, _)| at.elapsed() < t.every)
                {
                    continue;
                }
                let resp = handle(like("GET", t.path, Vec::new()));
                let before = last
                    .borrow_mut()
                    .insert(t.name, (Instant::now(), resp.body.clone()));
                if before.is_some_and(|(_, b)| b == resp.body) {
                    continue;
                }
                if !resp.status.starts_with('2') {
                    out.push(
                        json!({"type": "error", "topic": t.name, "error": String::from_utf8_lossy(&resp.body)})
                            .to_string(),
                    );
                    // The session ended (logged out, expired, the password changed): the
                    // channel ends too (bug run O2-M3); the client opens it again after a
                    // login.
                    end |= resp.status.starts_with("401") || resp.status.starts_with("403");
                    continue;
                }
                let mut m = json!({"type": t.name});
                m[t.name] = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                out.push(m.to_string());
            }
            if let Some(f) = feed {
                let mut seen = ev_seen.get();
                out.extend(f.pushed(&mut seen));
                ev_seen.set(seen);
            }
            out.append(&mut denied.borrow_mut());
            Next { send: out, end }
        },
        |v| {
            if let Some(t) = v.get("token").and_then(Value::as_str) {
                let before = token.replace(Some(t.trim().to_owned()));
                // Tried on the first topic before saying it's taken (bug run 2026-11-25: an
                // expired or wrong token was "ok", and the channel died at the first push).
                if let Some(topic) = topics.first() {
                    let r = handle(like("GET", topic.path, Vec::new()));
                    if r.status.starts_with("401") {
                        *token.borrow_mut() = before;
                        let why = serde_json::from_slice::<Value>(&r.body)
                            .ok()
                            .and_then(|b| b["error"].as_str().map(str::to_owned))
                            .unwrap_or_else(|| "the token wasn't taken".into());
                        return Some(json!({"type": "token", "ok": false, "error": why}));
                    }
                }
                // Every topic sent again, as this caller now sees it.
                last.borrow_mut().clear();
                return Some(json!({"type": "token", "ok": true}));
            }
            if awaiting && token.borrow().is_none() {
                return Some(with_id(
                    json!({"type": "reply", "ok": false, "error": "not logged in: send {\"token\": …} first"}),
                    v["id"].clone(),
                ));
            }
            for (k, on) in [("subscribe", true), ("unsubscribe", false)] {
                if let Some(l) = v[k].as_array() {
                    for name in l.iter().filter_map(Value::as_str) {
                        if let Some(x) = wanted.borrow_mut().get_mut(name) {
                            *x = on;
                        }
                        // Asked again: sent at once.
                        last.borrow_mut().remove(name);
                    }
                    return Some(json!({"type": k, "ok": true}));
                }
            }
            let Some((cmd, body, id)) = command_of(&v) else {
                return Some(not_a_command(&v));
            };
            let numbered = Sequencer::of(&v);
            if let Some((c, n)) = &numbered
                && !seq.fresh(c, *n)
            {
                return Some(skipped(id));
            }
            let resp = handle(like(
                "POST",
                &format!("/api/v1/{cmd}"),
                body.to_string().into_bytes(),
            ));
            let answer: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
            let r = if resp.status.starts_with('2') {
                Ok(answer)
            } else {
                if let Some((c, _)) = &numbered {
                    seq.revert(c);
                }
                let e = answer["error"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| String::from_utf8_lossy(&resp.body).into_owned());
                if resp.status.starts_with("403") {
                    denied.borrow_mut().push(super::problems::denied(&cmd, &e));
                }
                Err(e)
            };
            Some(reply(id, r))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply echoes the message's id, and has none when the message had none.
    #[test]
    fn replies_echo_an_id_only_when_given() {
        assert_eq!(
            reply(json!(3), Ok(json!({}))),
            json!({"type": "reply", "id": 3, "ok": true, "result": {}})
        );
        assert_eq!(
            reply(Value::Null, Err("no".into())),
            json!({"type": "reply", "ok": false, "error": "no"})
        );
        assert_eq!(
            skipped(Value::Null),
            json!({"type": "reply", "ok": true, "skipped": true})
        );
        assert_eq!(not_a_command(&json!({"id": "a"}))["id"], "a");
        assert!(not_a_command(&json!({})).get("id").is_none());
    }

    /// A change older than the last applied to its control is skipped; other controls are
    /// apart; numbers named in a message or in HTTP headers.
    #[test]
    fn late_changes_are_skipped_per_control() {
        let mut s = Sequencer::default();
        assert!(s.fresh("ch1.fader", 5));
        assert!(!s.fresh("ch1.fader", 4));
        assert!(!s.fresh("ch1.fader", 5));
        assert!(s.fresh("ch2.fader", 1));
        assert!(s.fresh("ch1.fader", 6));
        assert_eq!(
            Sequencer::of(&json!({"cmd": "channel", "control": "ch1.fader", "seq": 7})),
            Some(("ch1.fader".to_owned(), 7))
        );
        assert_eq!(Sequencer::of(&json!({"cmd": "channel"})), None);
        // Bounded.
        for k in 0..5000 {
            s.fresh(&format!("c{k}"), 1);
        }
        assert!(s.last.len() <= 4096);
    }
}
