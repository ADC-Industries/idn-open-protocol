//! The Site Manager's HTTP side (§I.12): `/healthz`, the WebSocket link at `/idn/link`
//! (what a Cloudflare Tunnel points at), and the application's API under `/api`, with an
//! API key as a bearer token.

use crate::server::{Role, SiteManager};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use idn_common::sync::LockOk as _;
use idn_open_protocol::web::{self, Logins};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

mod api;
mod dead;
mod remote;
mod ui;
mod updates;
mod voice;
use api::*;
use dead::*;
use remote::*;
use ui::*;
use updates::*;

#[derive(Clone)]
struct App {
    sm: SiteManager,
    key: std::sync::Arc<String>,
    ui: std::sync::Arc<Ui>,
}

/// The web UI's logins and sessions.
#[derive(Default)]
struct Ui {
    /// Session token → the live session.
    sessions: Mutex<HashMap<String, Session>>,
    /// Failed logins by address and by name (5 an address, 20 a name, in a minute). The
    /// password checks themselves take turns with the process's others
    /// (`PasswordChecks::shared`: Argon2, tens of ms and 19 MiB each).
    logins: Logins,
}

#[derive(Clone)]
struct Session {
    until: std::time::Instant,
    role: Role,
    user: String,
}

const SESSION: Duration = Duration::from_secs(12 * 3600);
const COOKIE: &str = "idn_sm_session";

/// The routes (see [`SiteManager::router`]).
pub fn router(sm: SiteManager, api_key: String) -> Router {
    let app = App {
        sm,
        key: std::sync::Arc::new(api_key),
        ui: Default::default(),
    };
    let r = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/", get(ui_page))
        .route("/ui/forms.js", get(forms_js))
        .route("/ui/{file}", get(shared_ui))
        .route("/ui/state", get(ui_state))
        .route("/ui/login", post(ui_login))
        .route("/ui/logout", post(ui_logout))
        .route("/ui/setup", post(ui_setup))
        .route("/api/site", get(site).put(site_put))
        .route("/api/machines/{fp}/values", get(machine_values))
        .route(crate::transport::WS_PATH, get(link))
        .route("/api/tokens", post(mint).get(tokens))
        .route("/api/tokens/{id}", delete(withdraw))
        .route("/api/machines", get(machines))
        .route("/api/machines/{fp}/revoke", post(revoke))
        .route("/api/machines/{fp}/command", post(command))
        .route("/api/inbox", get(inbox))
        .route("/api/inbox/{seq}/done", post(inbox_done))
        .route("/api/mqtt-users", post(mqtt_user).get(mqtt_users))
        .route("/api/mqtt-users/{name}", delete(mqtt_user_remove))
        .route("/mqtt", get(mqtt_ws))
        .route("/api/modules", get(modules))
        .route(
            "/api/modules/{scope}/{name}",
            axum::routing::put(module_put).delete(module_delete),
        )
        // Remote support (§I.11).
        .route("/api/users", get(users).post(user_set))
        .route("/api/users/{name}", delete(user_remove))
        .route("/api/audit", get(audit))
        .route("/api/dead-letters", get(dead_list).post(dead_refuse))
        .route("/api/dead-letters/{n}", get(dead_one))
        .route("/api/dead-letters/{n}/retry", post(dead_retry))
        .route("/api/dead-letters/{n}/resolve", post(dead_resolve))
        .route("/api/commands", get(held_commands))
        .route("/api/commands/{id}", delete(held_cancel))
        // Updates (§I.17): releases (a file of tens of MB, streamed to disk after the
        // role is checked: release_put), offers, pins, rollback.
        .route("/api/releases", get(releases_list).put(release_put))
        .route("/api/releases/{version}", delete(release_delete))
        .route("/api/updates", get(updates_get).post(updates_offer))
        .route("/api/machines/{fp}/rollback", post(machine_rollback))
        .route("/api/machines/{fp}/pin", post(machine_pin))
        .route("/api/machines/{fp}/logs", post(machine_logs))
        .route("/api/machines/{fp}/tags", axum::routing::put(machine_tags))
        .route("/api/screens", get(screens))
        .route("/api/machines/{fp}/screen", post(screen_start))
        .route("/api/machines/{fp}/chat", post(chat_send))
        .route("/api/screen/{session}/stop", post(screen_stop))
        .route("/api/screen/{session}/ws", get(screen_ws))
        .route("/screen", get(screen_page));
    // IDN Voice (§I.18): its own routes and state, at /voice.
    let r = match voice::start(&app) {
        Some(v) => r.nest_service("/voice", v),
        None => r,
    };
    r.with_state(app)
}

/// Where a request came from: the connection's address (when the server was started with
/// it), else unknown; a tunnel's or proxy's header only when the connection is from one
/// this Site Manager trusts (loopback: a tunnel on this machine, or an address in
/// `IDN_TRUST_PROXY`, comma-separated: a tunnel in another container). Anyone can send
/// those headers, and the login limit counts by this address.
fn remote(h: &HeaderMap, parts: &axum::http::request::Parts) -> String {
    let Some(peer) = parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0)
    else {
        return "unknown".into();
    };
    web::client_ip(h, peer.ip(), trusted_proxy).to_string()
}

fn trusted_proxy(ip: std::net::IpAddr) -> bool {
    static LIST: std::sync::OnceLock<Vec<std::net::IpAddr>> = std::sync::OnceLock::new();
    ip.is_loopback()
        || LIST
            .get_or_init(|| {
                idn_common::switches::value("IDN_TRUST_PROXY")
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|a| a.trim().parse().ok())
                    .collect()
            })
            .contains(&ip)
}

async fn link(
    State(app): State<App>,
    headers: HeaderMap,
    parts: axum::http::request::Parts,
    ws: WebSocketUpgrade,
) -> Response {
    let from = remote(&headers, &parts);
    let tls = app.sm.tls();
    ws.protocols([crate::transport::WS_PROTOCOL])
        .max_message_size(1 << 20)
        .on_upgrade(move |socket| async move {
            match crate::transport::accept_ws(socket, tls, from.clone()).await {
                Ok(l) => app.sm.link(l).await,
                Err(e) => idn_open_protocol::log::log(&format!(
                    "site manager: WebSocket link from {from}: {e}"
                )),
            }
        })
}

/// The API key (compared in constant time), or the web UI's session: its cookie, and
/// the UI's own header, which a page on another site can't send (so it can't act through
/// a logged-in browser).
fn authorised(app: &App, h: &HeaderMap) -> bool {
    if api_key_given(app, h) {
        return true;
    }
    h.contains_key("x-idn-ui") && session_of(h).is_some_and(|s| app.ui.valid(s))
}

/// Whether the request carries the API key (`Authorization: Bearer …`, compared in
/// constant time).
fn api_key_given(app: &App, h: &HeaderMap) -> bool {
    let given = web::bearer(h).unwrap_or("");
    !app.key.is_empty() && crate::token::same(given.as_bytes(), app.key.as_bytes())
}

/// An address's IP alone (`1.2.3.4:5678` → `1.2.3.4`, `[::1]:80` → `::1`); one that
/// isn't an address ("unknown") is the unspecified one, all such counted together.
fn client_ip(addr: &str) -> std::net::IpAddr {
    addr.parse::<std::net::SocketAddr>()
        .map(|a| a.ip())
        .or_else(|_| addr.parse())
        .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into())
}

/// The web UI's session token, from its cookie.
fn session_of(h: &HeaderMap) -> Option<&str> {
    web::cookie(h, COOKIE)
}

impl Ui {
    fn valid(&self, token: &str) -> bool {
        self.live(token).is_some()
    }
    /// A live session, if the token names one.
    fn live(&self, token: &str) -> Option<Session> {
        let mut s = self.sessions.lock_ok();
        s.retain(|_, v| v.until > std::time::Instant::now());
        s.get(token).cloned()
    }
    /// Ends every session of `user`: removed, given another role or password, or (the
    /// administrator) a password reset. Its browsers log in again with what it is now.
    fn end_user(&self, user: &str) {
        self.sessions.lock_ok().retain(|_, v| v.user != user);
    }
}

/// The role for a request: the API key is a full administrator; otherwise the UI
/// session's role. `None` when neither is present.
fn role_of(app: &App, h: &HeaderMap) -> Option<Role> {
    if api_key_given(app, h) {
        return Some(Role::Admin);
    }
    if !h.contains_key("x-idn-ui") {
        return None;
    }
    session_of(h).and_then(|s| app.ui.live(s)).map(|s| s.role)
}

/// Requires at least `need`; the response to send back when the role is too low.
#[allow(clippy::result_large_err)] // the Err is an axum Response, returned straight on.
fn require(app: &App, h: &HeaderMap, need: Role) -> Result<Role, Response> {
    match role_of(app, h) {
        Some(r) if r >= need => Ok(r),
        Some(_) => Err((StatusCode::FORBIDDEN, "your role can't do that").into_response()),
        None => Err(refused()),
    }
}

/// The user's name for the audit: the API key is "api", a UI session its user name.
fn who(app: &App, h: &HeaderMap) -> String {
    if !h.contains_key("x-idn-ui") {
        return "api".into();
    }
    session_of(h)
        .and_then(|s| app.ui.live(s))
        .map(|s| s.user)
        .unwrap_or_else(|| "?".into())
}

fn admin_hash_path(app: &App) -> std::path::PathBuf {
    app.sm.data_dir().join("admin.hash")
}

/// Sets the administrator password (Argon2id, kept on the volume). `IDN_ADMIN_PASSWORD`
/// sets it at start too.
pub fn set_admin_password(data_dir: &std::path::Path, password: &str) -> std::io::Result<()> {
    if password.len() < 10 {
        return Err(idn_common::err::invalid_input(
            "the password needs 10 characters or more",
        ));
    }
    let h = idn_open_protocol::web::hash_password(password)?;
    idn_open_protocol::trust::pki::write_private(&data_dir.join("admin.hash"), h.as_bytes())
}

fn refused() -> Response {
    (StatusCode::UNAUTHORIZED, "API key needed").into_response()
}

fn fail(e: std::io::Error) -> Response {
    let code = match e.kind() {
        std::io::ErrorKind::NotConnected => StatusCode::SERVICE_UNAVAILABLE,
        std::io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string()).into_response()
}
