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

impl State {
    /// The status with its `version` (moved on when anything else in it changed since the
    /// last look), made again at most every [`STATUS_FRESH`] and after any change.
    pub(super) fn versioned_status(&self) -> (Value, u64) {
        let mut g = self.status_cache.lock_ok();
        if let Some((at, v, ver)) = g.as_ref()
            && at.elapsed() < STATUS_FRESH
        {
            return (v.clone(), *ver);
        }
        let mut v = self.status();
        let ver = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            v.to_string().hash(&mut h);
            let h = h.finish();
            let mut seen = self.status_seen.lock_ok();
            if seen.1 != h {
                *seen = (seen.0 + 1, h);
            }
            seen.0
        };
        v["version"] = ver.into();
        *g = Some((std::time::Instant::now(), v.clone(), ver));
        (v, ver)
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

    fn state_for(&self, who: &Who) -> Value {
        let mut v = self.versioned_status().0;
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
