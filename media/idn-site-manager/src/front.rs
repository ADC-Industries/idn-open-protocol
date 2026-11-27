//! The Site Manager on the shared API front ([`web::engine`]), as the desk, the switcher
//! and the display service have it: a command `POST /api/v1/<cmd>` (the change header,
//! per-control sequence numbers, the caller's role checked and a refusal audited),
//! `GET /api/v1/state` (the status; `?after=VERSION` waits up to 20 s for a newer one),
//! `GET /api/v1/state_schema`, the commands' schema and the audit. The caller is the
//! session's user, their role looked up at each request (a user removed or changed has it
//! at once).
//!
//! What is the Site Manager's own stays so ([`State::answer_as`]): logging in and out, the
//! files (a backup, a restore, a release's pieces), its other pages (`status`, `matrix`,
//! `levels`, …, the machines' packages, the metrics). The push channel is the shared one,
//! with the Site Manager's topics (events.rs).

use super::*;
use idn_open_protocol::web::engine::{Engine, Refusal};
use idn_open_protocol::web::events::Sequencer;
use idn_open_protocol::web::{Who, audit};
use serde_json::Value;
use std::time::Instant;

/// POSTs the Site Manager answers itself: a session's own (logging in and out) and those
/// that send or answer a file (a backup, a restore, a release's pieces).
const OWN_POSTS: &[&str] = &[
    "/api/v1/login",
    "/api/v1/logout",
    "/api/v1/backup",
    "/api/v1/restore",
    "/api/v1/release/piece",
];

/// GETs the shared front answers in a session.
const SHARED_GETS: &[&str] = &[
    "/api/v1/state",
    "/api/v1/state_schema",
    "/api/v1/audit",
    "/api/v1/events",
];

impl State {
    /// A request: its session's user, then the shared front.
    pub(super) fn serve(self: &Arc<Self>, r: Request) -> Response {
        self.sessions_follow_password();
        let user = self.sessions.user(r.cookie("idn_site")).unwrap_or_default();
        let may_change = self
            .users
            .role(&user)
            .is_some_and(|role| role > users::Role::Viewer);
        let who = Who::session(&r, &user, may_change);
        // A push channel opened with a session goes on only while that session does
        // (logged out, or the site password changed: it went on running the admin's
        // commands; bug run 4).
        let sid = r.cookie("idn_site").map(str::to_owned);
        let resp = web::engine::handle_as(self, r, who);
        if user.is_empty() {
            return resp;
        }
        let w = Arc::downgrade(self);
        resp.while_allowed(Arc::new(move || {
            w.upgrade().is_some_and(|s| {
                s.sessions_follow_password();
                s.sessions
                    .user(sid.as_deref())
                    .is_some_and(|u| !u.is_empty())
            })
        }))
    }

    /// Whether `user` may run command `cmd` with `body` (§N.31.2): their role now against
    /// the least the command needs; why not.
    pub(super) fn may_run(&self, user: &str, cmd: &str, body: &Value) -> Result<(), String> {
        let role = (!user.is_empty())
            .then(|| self.users.role(user))
            .flatten()
            .ok_or("not logged in")?;
        if role < users::needs(cmd, body) {
            let a = if role == users::Role::Operator {
                "an"
            } else {
                "a"
            };
            return Err(format!("{a} {} may not do this", role.as_str()));
        }
        Ok(())
    }

    /// A change's line in the audit, with how it was answered: why it failed, if it did.
    pub(super) fn audit_answer(
        &self,
        user: &str,
        (peer, client): (&str, &str),
        cmd: &str,
        body: &Value,
        resp: &Response,
    ) -> Option<String> {
        let error = (!resp.status.starts_with('2')).then(|| {
            serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&resp.body).into_owned())
        });
        self.users
            .audit_store()
            .write(user, peer, client, cmd, body, error.as_deref());
        error
    }

    /// The caller's role now (none: not logged in, or no longer a user).
    fn role_of(&self, who: &Who) -> Option<users::Role> {
        if who.user.is_empty() {
            return None;
        }
        self.users.role(&who.user)
    }
}

/// An answer's status as the front's [`Refusal`] takes it.
fn status_of(s: &str) -> &'static str {
    match s.get(..3).unwrap_or("") {
        "403" => "403 Forbidden",
        "404" => "404 Not Found",
        "409" => "409 Conflict",
        "423" => "423 Locked",
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
        self.users.audit_store()
    }

    fn http_sequencer(&self) -> &Mutex<Sequencer> {
        &self.http_seq
    }

    fn version(&self) -> u64 {
        self.status_version_now()
    }

    fn wait_change(&self, after: u64, timeout: Duration) {
        let end = Instant::now() + timeout;
        while self.version() <= after && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn state_for(&self, who: &Who) -> Value {
        self.status_for(&who.user, self.role_of(who))
    }

    fn schema(&self) -> Value {
        crate::typed::schema()
    }

    fn state_schema(&self) -> Option<Value> {
        Some(crate::status::status_schema())
    }

    /// The site's typed events (§N.41) and the machines' as their Nodes pass them on.
    fn pushed_events(&self, _who: &Who, seen: &mut u64) -> Vec<String> {
        self.watch.events.pushed(seen)
    }

    fn event_seq(&self) -> u64 {
        self.watch.events.seq()
    }

    fn topic_every(&self, name: &str) -> Option<Duration> {
        crate::events::every(name)
    }

    fn topic(&self, name: &str, _who: &Who) -> Value {
        crate::events::topic(self, name)
    }

    fn permits(&self, who: &Who, cmd: &str, body: &Value) -> Result<(), String> {
        self.may_run(&who.user, cmd, body)
    }

    fn run_for(&self, who: &Who, cmd: &str, body: &Value) -> Result<Value, Refusal> {
        let resp = self.command(
            cmd,
            body.to_string().as_bytes(),
            &who.user,
            self.role_of(who),
        );
        self.status_changed();
        match self.audit_answer(&who.user, (&who.peer, &who.client), cmd, body, &resp) {
            None => Ok(
                match serde_json::from_slice(&resp.body).unwrap_or(Value::Null) {
                    Value::Object(mut m) => {
                        m.remove("ok");
                        Value::Object(m)
                    }
                    _ => Value::Null,
                },
            ),
            Some(why) => Err(Refusal::new(status_of(resp.status), why)),
        }
    }

    fn route(this: &Arc<Self>, r: &Request, who: &Who) -> Option<Response> {
        let logged_in = this.role_of(who).is_some();
        let path = r.path.as_str();
        let shared = match r.method.as_str() {
            // (The commands' schema to anyone, as before.)
            "GET" => path == "/api/v1/schema" || (logged_in && SHARED_GETS.contains(&path)),
            "POST" => logged_in && path.starts_with("/api/v1/") && !OWN_POSTS.contains(&path),
            _ => false,
        };
        if shared {
            return None;
        }
        Some(this.answer_as(r.clone(), logged_in.then(|| who.user.clone())))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn answers_keep_their_status() {
        assert_eq!(super::status_of("409 Conflict"), "409 Conflict");
        assert_eq!(super::status_of("403 Forbidden"), "403 Forbidden");
        assert_eq!(super::status_of("418 I'm a teapot"), "400 Bad Request");
    }
}
