//! The small web server behind the IDN Node's panel and the IDN Site Manager's
//! (spec/extensions/idn-node.md §N.6, §N.11): one request per connection, plain HTTP on
//! the loopback or HTTPS with a self-signed certificate, sessions, login rate limits and
//! Argon2id password hashes. Not a general web server.

use idn_common::sync::LockOk as _;
pub mod audit;
pub mod auth;
pub mod engine;
pub mod events;
pub mod headers;
mod hosts;
#[cfg(feature = "http-client")]
pub mod httpc;
pub mod lan;
pub mod limits;
pub mod problems;
mod server;
mod sessions;
pub mod ui;
pub use auth::{SiteKeys, TokenCheck, Via, Who};
pub use headers::{
    Headers, SameSite, SetCookie, bearer, client_ip, cookie, is_loopback_host, is_loopback_name,
    set_cookie, split_host,
};
pub(crate) use hosts::panel_cert;
pub use hosts::{lan_host_ok, loopback_host, open_browser, open_existing, self_signed};
pub use limits::{LoginLimits, Logins, PasswordChecks, PinLock};
pub use server::{Gate, serve, serve_gated, serve_until, serve_with};
pub use sessions::{
    HandOff, PasswordGate, Role, SITE_PASSWORD_FILE, Sessions, hash_password, set_site_password,
    verify_password,
};

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Header names in lower case.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// The client's address.
    pub peer: IpAddr,
    /// Whether it came over HTTPS.
    pub tls: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// A query parameter.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .split('&')
            .find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
    }

    /// A cookie ([`cookie()`]).
    pub fn cookie(&self, name: &str) -> Option<&str> {
        cookie(self, name)
    }

    /// A change that may be made: the custom header that no cross-site form can send,
    /// and, when the browser names one, the page's own origin.
    pub fn may_change(&self, header: &str) -> bool {
        self.header(&header.to_ascii_lowercase()) == Some("1") && self.same_origin()
    }

    /// The page's own origin, when the browser names one (a program sends none): no other
    /// site's page (a WebSocket can't carry a custom header; this is its check).
    pub fn same_origin(&self) -> bool {
        match (self.header("origin"), self.header("host")) {
            (Some(o), Some(h)) => o == format!("{}://{h}", if self.tls { "https" } else { "http" }),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }
}

/// A response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: &'static str,
    pub ctype: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A WebSocket: the connection is handed over after the 101 ([`Response::websocket`]).
    pub upgrade: Option<Upgrade>,
}

/// A connection both ways (plain or TLS), as a WebSocket runs over it.
pub trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

/// A WebSocket on a connection of [`serve`]'s.
pub type WebSocket = tungstenite::WebSocket<Box<dyn Duplex>>;

/// What runs a WebSocket once the connection is upgraded (on the connection's thread), and
/// what keeps it allowed (a login checked again: the connection ends when it isn't).
#[derive(Clone)]
pub struct Upgrade(
    Arc<std::sync::Mutex<Option<RunSocket>>>,
    Option<StillAllowed>,
);

/// Whether an upgraded connection may go on (its session still valid).
pub type StillAllowed = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a WebSocket is run with, once.
type RunSocket = Box<dyn FnOnce(WebSocket) + Send>;

impl std::fmt::Debug for Upgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Upgrade")
    }
}

impl Response {
    pub fn new(status: &'static str, ctype: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            ctype,
            headers: Vec::new(),
            body: body.into(),
            upgrade: None,
        }
    }

    /// A WebSocket for request `r` (its `Upgrade: websocket` and key checked): `run` gets
    /// it on the connection's own thread once the 101 is sent, and the connection ends
    /// when `run` returns. Not a WebSocket request: 400.
    pub fn websocket(r: &Request, run: impl FnOnce(WebSocket) + Send + 'static) -> Response {
        let upgrade = r
            .header("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
        let Some(key) = r.header("sec-websocket-key").filter(|_| upgrade) else {
            return Response::json_err("400 Bad Request", "a WebSocket (Upgrade: websocket)");
        };
        if r.header("sec-websocket-version") != Some("13") {
            return Response::json_err("426 Upgrade Required", "WebSocket version 13")
                .with("Sec-WebSocket-Version", "13");
        }
        // A WebSocket can't carry the API's change header: the page's own origin is its
        // check (bug run O2-H1: any web site's page took the switcher and the desk).
        if !r.same_origin() {
            return Response::json_err("403 Forbidden", "another site's page");
        }
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        let mut resp = Response::new("101 Switching Protocols", "", Vec::new())
            .with("Upgrade", "websocket")
            .with("Connection", "Upgrade")
            .with("Sec-WebSocket-Accept", &accept);
        resp.upgrade = Some(Upgrade(
            Arc::new(std::sync::Mutex::new(Some(Box::new(run)))),
            None,
        ));
        resp
    }

    /// An upgrade that goes on only while `still` says so (a session checked again about
    /// once a second; bug run O2-M3: a logged-out client kept its WebSocket).
    pub fn while_allowed(mut self, still: StillAllowed) -> Response {
        if let Some(u) = &mut self.upgrade {
            u.1 = Some(still);
        }
        self
    }

    /// The 101's head, for an upgrade.
    fn write_upgrade(&self, w: &mut impl Write) -> io::Result<()> {
        let mut h = format!("HTTP/1.1 {}\r\n", self.status);
        for (k, v) in &self.headers {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str("\r\n");
        w.write_all(h.as_bytes())?;
        w.flush()
    }

    pub fn ok(ctype: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self::new("200 OK", ctype, body)
    }

    pub fn json(v: impl Into<Vec<u8>>) -> Self {
        Self::new("200 OK", "application/json", v)
    }

    /// An error as JSON, `{"ok": false, "error": e}`, with `status`: what every panel's
    /// API answers a refused or failed request with.
    pub fn json_err(status: &'static str, e: &str) -> Self {
        Response::new(
            status,
            "application/json",
            format!(r#"{{"ok":false,"error":{}}}"#, json_str(e)),
        )
    }

    pub fn text(status: &'static str, t: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", t.as_bytes())
    }

    pub fn redirect(to: &str) -> Self {
        Self::new("303 See Other", "text/plain; charset=utf-8", "").with("Location", to)
    }

    /// A header added. (Control characters in either part are made spaces: a value
    /// taken from a request split the answer's head; bug run 4.)
    pub fn with(mut self, k: &str, v: &str) -> Self {
        let clean = |t: &str| -> String {
            t.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect()
        };
        self.headers.push((clean(k), clean(v)));
        self
    }

    fn write(&self, w: &mut impl Write) -> io::Result<()> {
        let mut h = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\n\
Connection: close\r\n",
            self.status,
            self.ctype,
            self.body.len()
        );
        for (k, v) in &self.headers {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str("\r\n");
        w.write_all(h.as_bytes())?;
        w.write_all(&self.body)?;
        w.flush()
    }
}

/// The request handler.
pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// A JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '<' => o.push_str("\\u003c"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests;
