//! The web UI (§I.12): the page, its state, login and setup, the site's settings and
//! modules, and the shared panel files.

use super::*;
use idn_open_protocol::web::PasswordChecks;

pub(super) async fn ui_page() -> Response {
    (
        [
            ("content-type", "text/html; charset=utf-8"),
            ("x-frame-options", "DENY"),
            ("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'"),
        ],
        UI,
    )
        .into_response()
}

pub(super) const UI: &str = include_str!("../ui/site.html");

pub(super) async fn ui_state(State(app): State<App>, h: HeaderMap) -> Response {
    let setup = !admin_hash_path(&app).exists();
    let session = session_of(&h).and_then(|s| app.ui.live(s));
    // For the header's "n of m online" (machines not revoked).
    let (online, machines) = match &session {
        Some(_) => app.sm.machines().map_or((0, 0), |v| {
            let v: Vec<_> = v.into_iter().filter(|m| !m.revoked).collect();
            (v.iter().filter(|m| m.online.is_some()).count(), v.len())
        }),
        None => (0, 0),
    };
    Json(serde_json::json!({
        "online": online,
        "machines": machines,
        "setup_needed": setup,
        "logged_in": session.is_some(),
        "role": session.as_ref().map(|s| s.role.as_str()),
        "user": session.as_ref().map(|s| s.user.clone()),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct LoginBody {
    #[serde(default)]
    username: String,
    password: String,
}

pub(super) fn secure_cookie(h: &HeaderMap) -> bool {
    // Behind HTTPS (a tunnel or proxy says so): the cookie only over HTTPS.
    h.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
        || h.get("cf-visitor")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("https"))
}

pub(super) fn start_session(app: &App, h: &HeaderMap, user: &str, role: Role) -> Response {
    let token = idn_open_protocol::random::token_hex(24);
    app.ui.sessions.lock_ok().insert(
        token.clone(),
        Session {
            until: std::time::Instant::now() + SESSION,
            role,
            user: user.to_owned(),
        },
    );
    let cookie = web::set_cookie(COOKIE, &token)
        .max_age(SESSION)
        .secure(secure_cookie(h))
        .to_string();
    (StatusCode::NO_CONTENT, [("set-cookie", cookie)]).into_response()
}

pub(super) async fn ui_login(
    State(app): State<App>,
    h: HeaderMap,
    parts: axum::http::request::Parts,
    Json(b): Json<LoginBody>,
) -> Response {
    // Counted by IP, not by connection (each attempt comes from another port).
    let from = client_ip(&remote(&h, &parts));
    let name = if b.username.is_empty() {
        "admin".to_owned()
    } else {
        b.username.clone()
    };
    // The attempt counts before the check (attempts sent at once all count), and is
    // taken back if it succeeds.
    if !app.ui.logins.attempt_as(from, &name) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts: wait a minute",
        )
            .into_response();
    }
    let (sm, user, pw) = (app.sm.clone(), b.username, b.password);
    let role = PasswordChecks::shared()
        .run(move || sm.verify_user(&user, &pw))
        .await
        .flatten();
    let Some(role) = role else {
        return (StatusCode::UNAUTHORIZED, "wrong name or password").into_response();
    };
    app.ui.logins.succeeded_as(from, &name);
    start_session(&app, &h, &name, role)
}

pub(super) async fn ui_logout(State(app): State<App>, h: HeaderMap) -> Response {
    if let Some(s) = session_of(&h) {
        app.ui.sessions.lock_ok().remove(s);
    }
    (
        StatusCode::NO_CONTENT,
        [(
            "set-cookie",
            web::set_cookie(COOKIE, "")
                .max_age(Duration::ZERO)
                .to_string(),
        )],
    )
        .into_response()
}

#[derive(Deserialize)]
pub(super) struct SetupBody {
    api_key: String,
    password: String,
}

/// The first administrator password (or a forgotten one): proven by the API key, so an
/// exposed Site Manager can't be claimed by whoever finds it first.
pub(super) async fn ui_setup(
    State(app): State<App>,
    h: HeaderMap,
    Json(b): Json<SetupBody>,
) -> Response {
    if app.key.is_empty() || !crate::token::same(b.api_key.trim().as_bytes(), app.key.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            "that isn't this Site Manager's API key",
        )
            .into_response();
    }
    let dir = app.sm.data_dir().to_owned();
    match tokio::task::spawn_blocking(move || set_admin_password(&dir, &b.password)).await {
        Ok(Ok(())) => {
            // A reset (a leaked password): the old password's sessions end.
            app.ui.end_user("admin");
            app.sm
                .audit("admin", "", "admin.password.reset", serde_json::json!({}));
            start_session(&app, &h, "admin", Role::Admin)
        }
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn site(State(app): State<App>, h: HeaderMap) -> Response {
    if !authorised(&app, &h) {
        return refused();
    }
    Json(serde_json::json!({
        "ca_fingerprint": app.sm.ca_fingerprint(),
        "public_urls": app.sm.public_urls(),
        "version": env!("CARGO_PKG_VERSION"),
        "mqtt_clients": app.sm.broker().clients(),
        "hold_days": app.sm.hold_days(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct SitePut {
    hold_days: u32,
}

/// The site's settings (admins): the hold for messages no application has taken.
pub(super) async fn site_put(
    State(app): State<App>,
    h: HeaderMap,
    Json(b): Json<SitePut>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    let before = app.sm.hold_days();
    match app.sm.set_hold_days(b.hold_days) {
        Ok(()) => {
            app.sm.audit(
                &who(&app, &h),
                "",
                "site.settings",
                serde_json::json!({ "hold_days": { "from": before, "to": b.hold_days } }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

/// A machine's latest values (retained), for the UI.
pub(super) async fn machine_values(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
) -> Response {
    // The plant's values: operators (Was any signed-in user, a viewer too: deep bug run 2026-11-09.)
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    let Some(m) = app
        .sm
        .machines()
        .ok()
        .and_then(|v| v.into_iter().find(|m| m.fingerprint == fp))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let prefix = format!("idn/{}/v/", m.topic);
    let mut out = serde_json::Map::new();
    for msg in app.sm.broker().retained_matching(&format!("{prefix}#")) {
        let name = msg.topic.trim_start_matches(&prefix).to_owned();
        out.insert(
            name,
            serde_json::from_slice(&msg.payload).unwrap_or(serde_json::Value::Null),
        );
    }
    Json(out).into_response()
}

/// Module settings pushed to machines (§I.8): `GET /api/modules`,
/// `PUT /api/modules/{machine:<fp>|tag:<k>=<v>}/{name}` (the TOML as the body), `DELETE`.
pub(super) async fn modules(State(app): State<App>, h: HeaderMap) -> Response {
    // Module settings (connections, queries) are admin's (Was any signed-in user, a viewer too: deep bug run 2026-11-09.)
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.module_files() {
        Ok(v) => Json(
            v.into_iter()
                .map(|(scope, name, text)| {
                    // As the forms see it (null if the text isn't valid TOML).
                    let json = toml::from_str::<toml::Value>(&text)
                        .ok()
                        .and_then(|v| serde_json::to_value(v).ok());
                    serde_json::json!({ "scope": scope, "name": name, "toml": text, "json": json })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn module_put(
    State(app): State<App>,
    h: HeaderMap,
    Path((scope, name)): Path<(String, String)>,
    body: String,
) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    // From the forms: JSON, written as TOML (§I.8.4).
    let json = h
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let body = if json {
        match serde_json::from_str::<serde_json::Value>(&body)
            .map_err(idn_common::err::invalid_input)
            .and_then(|v| crate::settings::to_toml(&v))
        {
            Ok(t) => t,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    } else {
        body
    };
    match app.sm.put_module(&scope, &name, &body).await {
        Ok(warnings) => Json(serde_json::json!({ "warnings": warnings })).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn module_delete(
    State(app): State<App>,
    h: HeaderMap,
    Path((scope, name)): Path<(String, String)>,
) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.remove_module(&scope, &name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}

/// The IDN panels' shared files (`/ui/shell.js`, `/ui/shell.css`, …): the look this page
/// shares with the Node's and the Media Site Manager's.
pub(super) async fn shared_ui(Path(file): Path<String>) -> Response {
    match idn_open_protocol::web::ui::asset_file(&format!("/ui/{file}")) {
        Some((ctype, body)) => ([("content-type", ctype)], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The settings forms' script (§I.8.4), the Node's own.
pub(super) async fn forms_js() -> Response {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        crate::local_api::FORMS_JS,
    )
        .into_response()
}
