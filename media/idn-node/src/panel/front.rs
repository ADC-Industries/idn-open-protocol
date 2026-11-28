//! The panel on the shared API front ([`web::engine`]), as the engines and the Site
//! Manager have it: a change `POST /api/v1/<command>` (the change header, per-control
//! sequence numbers, a read-only token's change refused, every change in the audit),
//! `GET /api/v1/state` (the status, `?after=VERSION` waiting up to 20 s for a newer one),
//! the commands' list (`GET /api/v1/schema`) and the audit. The caller: the loopback
//! session's (`local`, or a session a program opened), the LAN password's or a token's.
//!
//! What stays the panel's own (panel.rs, panel/api.rs): logging in and out, the pages, the
//! other GETs (`status`, `levels`, `logs`, …). The push channel is the shared one
//! (`{"type": "state", "version", "state"}` when the status changes, the Node's typed
//! events, commands answered in order, `denied`); it was `{"type": "status"}` each
//! second until 2026-11-19.

use super::*;
use web::engine::{Engine, Refusal};
use web::events::Sequencer;
use web::{Who, audit};

/// GETs the shared front answers.
const SHARED_GETS: &[&str] = &[
    "/api/v1/state",
    "/api/v1/state_schema",
    "/api/v1/schema",
    "/api/v1/audit",
    "/api/v1/events",
];

/// How long a status made is given again (every push channel and long poll asks).
const STATUS_FRESH: std::time::Duration = std::time::Duration::from_millis(250);

/// The hash the status's version follows: of the status without its version and the
/// [`RUNNING_NUMBERS`](crate::server::RUNNING_NUMBERS) (an uptime or a clock offset moving
/// is no change to send).
fn version_hash(v: &Value) -> u64 {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for k in crate::server::RUNNING_NUMBERS {
                    m.remove(*k);
                }
                m.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    use std::hash::{Hash, Hasher};
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("version");
    }
    strip(&mut v);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.to_string().hash(&mut h);
    h.finish()
}

/// How often the status's version also moves when only its running numbers changed: a
/// push then carries fresh ones, and a long poll (`?after=`) ends, at least this often.
const LIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether the status's version moves: something but the running numbers changed (`hash`
/// differs from the last move's), or only they did and [`LIVE_EVERY`] passed since the
/// last move.
fn version_moves(
    last_hash: u64,
    hash: u64,
    since_move: std::time::Duration,
    numbers_moved: impl FnOnce() -> bool,
) -> bool {
    last_hash != hash || (since_move >= LIVE_EVERY && numbers_moved())
}

/// Whether `a` and `b` differ in anything but their `version`.
fn differ_but_version(a: &Value, b: &Value) -> bool {
    let unversioned = |v: &Value| {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("version");
        }
        v
    };
    unversioned(a) != unversioned(b)
}

impl State {
    /// The status as it is now, with its `version` (moved on when something other than its
    /// running numbers changed since the last look, or when only they did, at most every
    /// [`LIVE_EVERY`]), made again at most every [`STATUS_FRESH`] and after any change.
    pub(super) fn versioned_status(&self) -> (Value, u64) {
        let mut g = self.status_cache.lock_ok();
        if let Some((at, v, ver)) = g.as_ref()
            && at.elapsed() < STATUS_FRESH
        {
            return (v.clone(), *ver);
        }
        let mut v = self.status();
        let ver = {
            let h = version_hash(&v);
            let mut seen = self.status_seen.lock_ok();
            let (last_ver, last_h, last, at) = &*seen;
            if version_moves(*last_h, h, at.elapsed(), || differ_but_version(last, &v)) {
                let ver = last_ver + 1;
                let mut moved = v.clone();
                moved["version"] = ver.into();
                *seen = (ver, h, moved, std::time::Instant::now());
            }
            seen.0
        };
        v["version"] = ver.into();
        *g = Some((std::time::Instant::now(), v.clone(), ver));
        (v, ver)
    }

    /// The status as it was when its version last moved (its running numbers as they were
    /// then): what the push channel compares and sends, so that an uptime ticking sends
    /// nothing until [`LIVE_EVERY`] passed; a push always carries the numbers of its
    /// change.
    fn pushed_status(&self) -> Value {
        let now = self.versioned_status().0;
        let seen = self.status_seen.lock_ok();
        if seen.2.is_null() {
            now
        } else {
            seen.2.clone()
        }
    }
}

/// An answer's status as the front's [`Refusal`] takes it.
fn status_of(s: &str) -> &'static str {
    match s.get(..3).unwrap_or("") {
        "403" => "403 Forbidden",
        "404" => "404 Not Found",
        "409" => "409 Conflict",
        "429" => "429 Too Many Requests",
        "500" => "500 Internal Server Error",
        "503" => "503 Service Unavailable",
        _ => "400 Bad Request",
    }
}

impl Engine for State {
    fn change_header(&self) -> &'static str {
        "X-IDN-Node"
    }

    fn audit(&self) -> &audit::Audit {
        &self.audit
    }

    fn http_sequencer(&self) -> &Mutex<Sequencer> {
        &self.seq
    }

    fn version(&self) -> u64 {
        self.versioned_status().1
    }

    fn wait_change(&self, after: u64, timeout: std::time::Duration) {
        let end = std::time::Instant::now() + timeout;
        while self.version() <= after && std::time::Instant::now() < end {
            std::thread::sleep(STATUS_FRESH);
        }
    }

    /// The push channel's state (`GET /api/v1/state` is answered in [`Engine::route`]
    /// below, with the numbers as they are now).
    fn state_for(&self, who: &Who) -> Value {
        let mut v = self.pushed_status();
        v["lan"] = Value::Bool(who.via != web::Via::Local);
        v
    }

    fn schema(&self) -> Value {
        json!({
            "title": "IDN Node panel commands",
            "description": "POST /api/v1/<cmd> with a JSON body (idn-node.md); over the push channel {\"cmd\", \"body\"}.",
            "type": "object",
            "properties": {
                "cmd": {"enum": commands::COMMANDS},
                "body": {"type": "object"},
            },
            "required": ["cmd"],
        })
    }

    fn state_schema(&self) -> Option<Value> {
        Some(crate::panel_state::state_schema())
    }

    /// The Node's typed events (§N.41) as they happen.
    fn pushed_events(&self, _who: &Who, seen: &mut u64) -> Vec<String> {
        self.core.problem_events.pushed(seen)
    }

    fn event_seq(&self) -> u64 {
        self.core.problem_events.seq()
    }

    fn run_for(&self, who: &Who, cmd: &str, body: &Value) -> Result<Value, Refusal> {
        let limited = who.via == web::Via::Local && who.user == LIMITED;
        let lan = who.via != web::Via::Local;
        let resp = self.command(cmd, body.to_string().as_bytes(), limited, lan);
        *self.status_cache.lock_ok() = None;
        let answer: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let error = (!resp.status.starts_with('2')).then(|| {
            answer["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| String::from_utf8_lossy(&resp.body).into_owned())
        });
        self.audit.record(who, cmd, body, error.as_deref());
        match error {
            None => Ok(match answer {
                Value::Object(mut m) => {
                    m.remove("ok");
                    Value::Object(m)
                }
                _ => Value::Null,
            }),
            Some(why) => Err(Refusal::new(status_of(resp.status), why)),
        }
    }

    fn route(this: &Arc<Self>, r: &Request, who: &Who) -> Option<Response> {
        let path = r.path.as_str();
        // The state as the shared front gives it, its running numbers (the uptime, the
        // clock's offset) as they are now rather than as at the last change.
        if r.method == "GET" && path == "/api/v1/state" {
            if let Some(after) = r.param("after").and_then(|v| v.parse::<u64>().ok()) {
                this.wait_change(after, std::time::Duration::from_secs(20));
            }
            let mut v = this.versioned_status().0;
            v["lan"] = Value::Bool(who.via != web::Via::Local);
            return Some(Response::json(v.to_string()).with("Cache-Control", "no-store"));
        }
        let shared = match r.method.as_str() {
            "GET" => SHARED_GETS.contains(&path),
            "POST" => path.starts_with("/api/v1/"),
            _ => false,
        };
        if shared {
            return None;
        }
        Some(this.route(r.clone(), true, who.via != web::Via::Local))
    }
}

#[cfg(test)]
mod tests {
    /// An uptime, a clock's offset and a service's time running moving don't move the
    /// status's version; anything else does.
    #[test]
    fn the_version_ignores_the_running_numbers() {
        use super::version_hash;
        let a = serde_json::json!({
            "version": 3,
            "name": "BOOTH",
            "uptime_s": 61,
            "clock": {"state": "slave", "locked": true, "offset_ns": 120.0, "delay_ns": 40.0},
            "services": [{"id": "display", "state": "running", "for_s": 11}],
        });
        let mut b = a.clone();
        b["version"] = 4.into();
        b["uptime_s"] = 62.into();
        b["clock"]["offset_ns"] = (-85.0).into();
        b["clock"]["delay_ns"] = 41.0.into();
        b["services"][0]["for_s"] = 12.into();
        assert_eq!(version_hash(&a), version_hash(&b));
        b["clock"]["locked"] = false.into();
        assert_ne!(version_hash(&a), version_hash(&b));
        let mut c = a.clone();
        c["services"][0]["state"] = "stopped".into();
        assert_ne!(version_hash(&a), version_hash(&c));
    }

    /// The running numbers alone move the version only once [`super::LIVE_EVERY`] passed
    /// since the last move (and only if they changed); anything else moves it at once.
    #[test]
    fn the_running_numbers_move_the_version_every_ten_seconds() {
        use super::{LIVE_EVERY, differ_but_version, version_moves};
        use std::time::Duration;
        let a = serde_json::json!({"version": 3, "uptime_s": 61});
        let b = serde_json::json!({"uptime_s": 70});
        assert!(!version_moves(
            1,
            1,
            LIVE_EVERY - Duration::from_secs(1),
            || { differ_but_version(&a, &b) }
        ));
        assert!(version_moves(1, 1, LIVE_EVERY, || differ_but_version(
            &a, &b
        )));
        assert!(!version_moves(1, 1, LIVE_EVERY, || differ_but_version(
            &a, &a
        )));
        assert!(version_moves(1, 2, Duration::ZERO, || false));
    }

    /// Every command listed has its branch, and every branch is listed.
    #[test]
    fn the_commands_listed_are_the_branches() {
        let src = include_str!("commands.rs");
        let body = &src[src.find("match cmd {").unwrap()..];
        for c in super::commands::COMMANDS {
            assert!(body.contains(&format!("\"{c}\"")), "{c} has no branch");
        }
        for line in body.lines() {
            let t = line.trim_start();
            if line.starts_with("            \"") {
                for name in t
                    .split(" =>")
                    .next()
                    .unwrap()
                    .split(" if ")
                    .next()
                    .unwrap()
                    .split(" | ")
                {
                    let name = name.trim().trim_matches('"');
                    assert!(
                        super::commands::COMMANDS.contains(&name),
                        "{name} isn't listed"
                    );
                }
            }
        }
    }
}
