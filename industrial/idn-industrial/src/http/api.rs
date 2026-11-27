//! The machines' API (§I.12): enrolment tokens, machines, commands, the inbox, the
//! site broker's WebSocket and users.

use super::*;

#[derive(Deserialize)]
pub(super) struct MintBody {
    name: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    /// Hours the token is valid (24 by default, at most a year).
    hours: Option<f64>,
}

pub(super) async fn mint(State(app): State<App>, h: HeaderMap, Json(b): Json<MintBody>) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    let hours = b.hours.unwrap_or(24.0).clamp(0.01, 24.0 * 366.0);
    match app
        .sm
        .mint_token(b.name, b.tags, Duration::from_secs_f64(hours * 3600.0))
    {
        Ok((text, rec)) => {
            app.sm.audit(
                &who(&app, &h),
                "",
                "token.mint",
                serde_json::json!({ "id": rec.id, "name": rec.name, "tags": rec.tags,
                    "expires_ms": rec.expires_ms }),
            );
            Json(serde_json::json!({
            "token": text,
            "id": rec.id,
            "name": rec.name,
            "tags": rec.tags,
            "expires_ms": rec.expires_ms,
        }))
            .into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn tokens(State(app): State<App>, h: HeaderMap) -> Response {
    // Tokens are admin's (Was any signed-in user, a viewer too: deep bug run 2026-11-09.)
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.tokens() {
        // Never the secret.
        Ok(v) => Json(
            v.into_iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id, "name": t.name, "tags": t.tags, "created_ms": t.created_ms,
                        "expires_ms": t.expires_ms, "used_by": t.used_by,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn withdraw(State(app): State<App>, h: HeaderMap, Path(id): Path<String>) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.revoke_token(&id) {
        Ok(true) => {
            app.sm.audit(&who(&app, &h), "", "token.withdraw", serde_json::json!({ "id": id }));
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn machines(State(app): State<App>, h: HeaderMap) -> Response {
    if !authorised(&app, &h) {
        return refused();
    }
    match app.sm.machines() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn revoke(State(app): State<App>, h: HeaderMap, Path(fp): Path<String>) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.revoke(&fp) {
        Ok(()) => {
            app.sm.audit(&who(&app, &h), &fp, "machine.revoke", serde_json::json!({}));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub(super) struct CommandBody {
    #[serde(rename = "type")]
    kind: String,
    /// A string is sent as its UTF-8 bytes; anything else as JSON.
    #[serde(default)]
    payload: serde_json::Value,
    timeout_s: Option<f64>,
}

pub(super) async fn command(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
    Json(b): Json<CommandBody>,
) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    if !crate::wire::valid_kind(&b.kind) {
        return (
            StatusCode::BAD_REQUEST,
            "type: lower-case letters, digits, '.', '-', '_'",
        )
            .into_response();
    }
    if crate::wire::reserved_kind(&b.kind) {
        return (
            StatusCode::FORBIDDEN,
            "types starting idn. or remote. are the Site Manager's own",
        )
            .into_response();
    }
    let payload = match b.payload {
        serde_json::Value::String(s) => s.into_bytes(),
        serde_json::Value::Null => Vec::new(),
        v => serde_json::to_vec(&v).unwrap_or_default(),
    };
    let timeout = Duration::from_secs_f64(b.timeout_s.unwrap_or(30.0).clamp(1.0, 300.0));
    match app.sm.command(&fp, &b.kind, payload, timeout).await {
        Ok((st, p)) => Json(serde_json::json!({
            "status": st,
            "ok": st == crate::wire::ST_OK,
            "payload": String::from_utf8_lossy(&p),
        }))
        .into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub(super) struct InboxQuery {
    #[serde(default)]
    after: u64,
    limit: Option<usize>,
}

pub(super) async fn inbox(State(app): State<App>, h: HeaderMap, Query(q): Query<InboxQuery>) -> Response {
    // The plant's data: operators (Was any signed-in user, a viewer too: deep bug run 2026-11-09.)
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    match app.sm.inbox(q.after, q.limit.unwrap_or(100).min(1000)) {
        Ok(v) => Json(
            v.into_iter()
                .map(|(seq, fp, at, it)| {
                    serde_json::json!({
                        "seq": seq, "machine": fp, "arrived_ms": at, "op": it.op,
                        "id": it.id_hex(), "time_ms": it.time_ms, "type": it.kind,
                        "payload": String::from_utf8_lossy(&it.payload),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn inbox_done(State(app): State<App>, h: HeaderMap, Path(seq): Path<u64>) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.inbox_done(seq) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

/// MQTT over a WebSocket (subprotocol `mqtt`), through the same tunnel as the link.
pub(super) async fn mqtt_ws(
    State(app): State<App>,
    headers: HeaderMap,
    parts: axum::http::request::Parts,
    ws: WebSocketUpgrade,
) -> Response {
    let from = remote(&headers, &parts);
    let broker = app.sm.broker().clone();
    ws.protocols(["mqtt"])
        .max_message_size(17 << 20)
        .on_upgrade(move |socket| async move {
            broker
                .serve(crate::transport::axum_bytes(socket), from)
                .await;
        })
}

#[derive(Deserialize)]
pub(super) struct UserBody {
    username: String,
    password: String,
}

pub(super) async fn mqtt_user(State(app): State<App>, h: HeaderMap, Json(b): Json<UserBody>) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.broker().set_user(&b.username, &b.password) {
        Ok(()) => {
            app.sm.audit(
                &who(&app, &h),
                "",
                "mqtt.user.set",
                serde_json::json!({ "username": b.username }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn mqtt_users(State(app): State<App>, h: HeaderMap) -> Response {
    // MQTT's users are admin's (Was any signed-in user, a viewer too: deep bug run 2026-11-09.)
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.broker().users() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn mqtt_user_remove(
    State(app): State<App>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    // Changes the site: the role for it (the API key has every role).
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.broker().remove_user(&name) {
        Ok(true) => {
            app.sm.audit(
                &who(&app, &h),
                "",
                "mqtt.user.remove",
                serde_json::json!({ "username": name }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}
