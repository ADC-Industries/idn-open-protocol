//! The display service's API (§D.8.3): `GET /api/v1/state` (long poll with `?after=`),
//! `POST /api/v1/<command>`, `GET /api/v1/thumbnail?screen=N`, `GET /api/v1/schema`,
//! `GET /api/v1/audit`, and the push channel `GET /api/v1/events`. Open on this machine;
//! on the network (HTTPS) for site tokens of the engine `display` only. A read-only token
//! reads and is refused changes (403); every change is written in the audit with who made
//! it.

use super::{ScreenRef, Service, typed};
use idn_open_protocol::web::{self, Request, Response, Who, audit};
use serde_json::{Value, json};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The header a change must carry (no cross-site form can send it).
pub const HEADER: &str = "X-IDN-Display";

/// The network's front (tokens only: no password, so no login page).
const LOGIN: web::ui::Login = web::ui::Login {
    title: "IDN Display",
    hint: None,
    label: "Password",
    header: HEADER,
    who: "display",
    console: false,
    user: false,
};

/// Serves the API on 127.0.0.1:`port`, and on the network on `lan` once the machine is in
/// a site (tokens can then be checked).
pub fn serve(svc: &Arc<Service>, port: u16, lan: Option<u16>) -> std::io::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let handler: web::Handler = {
        let svc = svc.clone();
        Arc::new(move |r: Request| svc.handle(r))
    };
    web::serve(listener, None, handler, stop.clone())?;
    if let (Some(lan_port), Some(t)) = (lan, svc.tokens.clone()) {
        let handler: web::Handler = {
            let svc = svc.clone();
            Arc::new(move |r: Request| svc.handle_lan(r))
        };
        web::lan::start_when(
            svc.dir(),
            "IDN Display",
            lan_port,
            stop,
            handler,
            move || t.available(),
            |m| println!("idn-display: {m}"),
        );
    }
    Ok(())
}

impl Service {
    /// A request on this machine: everything allowed, audited as `local`; a
    /// token, if it carries one, counts as its holder's.
    pub fn handle(self: &Arc<Self>, r: Request) -> Response {
        let who = match self.tokens.as_ref().and_then(|t| t.who(&r)) {
            Some(Ok(w)) => w,
            Some(Err(e)) => return web::auth::refused(&e),
            None => Who::local(&r),
        };
        self.handle_as(r, who)
    }

    /// A request from the network: site tokens only.
    pub fn handle_lan(self: &Arc<Self>, r: Request) -> Response {
        // Turned off (`lan`): nothing on the network until it is on again here.
        if self.lan_off() {
            return Response::json_err(
                "503 Service Unavailable",
                "the display service's network API is turned off on this machine",
            );
        }
        let me = self.clone();
        web::lan::front(
            self.lan.as_ref(),
            &LOGIN,
            r,
            &|m| eprintln!("idn-display: {m}"),
            move |r, who| me.handle_as(r, who),
        )
    }

    /// A command run for `who`, written in the audit as theirs.
    pub fn command_as(&self, who: &Who, cmd: &str, v: &Value) -> Result<Value, String> {
        let done = self.command_by(cmd, v, &who.name);
        self.audit
            .record(who, cmd, v, done.as_ref().err().map(String::as_str));
        done
    }

    /// A request from `who`: the shared engine front (`web::engine`) with this service's
    /// own pages (its root, the figures, a screen's thumbnail).
    pub fn handle_as(self: &Arc<Self>, r: Request, who: Who) -> Response {
        web::engine::handle_as(self, r, who)
    }
}

impl web::engine::Engine for Service {
    fn change_header(&self) -> &'static str {
        HEADER
    }

    fn audit(&self) -> &audit::Audit {
        &self.audit
    }

    fn http_sequencer(&self) -> &std::sync::Mutex<web::events::Sequencer> {
        &self.seq
    }

    fn version(&self) -> u64 {
        Service::version(self)
    }

    fn wait_change(&self, after: u64, timeout: Duration) {
        Service::wait_change(self, after, timeout);
    }

    fn state_for(&self, _who: &Who) -> Value {
        self.state()
    }

    fn schema(&self) -> Value {
        typed::schema()
    }

    fn state_schema(&self) -> Option<Value> {
        Some(super::state::state_schema())
    }

    fn run_for(&self, who: &Who, cmd: &str, body: &Value) -> Result<Value, web::engine::Refusal> {
        self.command_as(who, cmd, body).map_err(Into::into)
    }

    /// The typed events (§D.8.5): `lost`, `other_site`, `reconnecting`, `fallback`,
    /// `recovered`, `playing`, `format`, `routed`, `screens`, `stopped`, `restarted`.
    fn pushed_events(&self, _who: &Who, seen: &mut u64) -> Vec<String> {
        let (evs, now) = self.events_since(*seen);
        *seen = now;
        evs.into_iter().map(|e| e.to_string()).collect()
    }

    fn event_seq(&self) -> u64 {
        Service::event_seq(self)
    }

    /// `stats`: each running screen's figures, once a second.
    fn topic_every(&self, name: &str) -> Option<Duration> {
        (name == "stats").then_some(Duration::from_secs(1))
    }

    fn topic(&self, _name: &str, _who: &Who) -> Value {
        self.stats()
    }

    fn route(this: &Arc<Self>, r: &Request, _who: &Who) -> Option<Response> {
        Some(match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/") => {
                Response::json(json!({"service": "IDN Display", "api": "/api/v1/"}).to_string())
            }
            ("GET", "/api/v1/stats") => Response::json(this.stats().to_string()),
            ("GET", "/api/v1/thumbnail") => {
                let screen = match r.param("screen") {
                    Some(s) => ScreenRef::Name(decode(s)),
                    None => {
                        return Some(Response::json_err("400 Bad Request", "?screen=N or NAME"));
                    }
                };
                let width = r
                    .param("width")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(480u32);
                match this.thumbnail(&screen, width) {
                    Ok(j) => Response::ok("image/jpeg", j).with("Cache-Control", "no-store"),
                    Err(e) => Response::json_err("404 Not Found", &e),
                }
            }
            _ => return None,
        })
    }
}

/// A query value percent-decoded (`%5C%5C.%5CDISPLAY1`, `+` a space).
fn decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(x) => {
                        out.push(x);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_values_are_decoded() {
        assert_eq!(super::decode("%5C%5C.%5CDISPLAY1"), r"\\.\DISPLAY1");
        assert_eq!(super::decode("DP-2"), "DP-2");
        assert_eq!(super::decode("a+b%2"), "a b%2");
    }
}
