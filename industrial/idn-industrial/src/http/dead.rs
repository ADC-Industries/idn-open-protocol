//! Dead letters (§I.7.5) and held commands (§I.7.3).

use super::*;

#[derive(Deserialize)]
pub(super) struct DeadQuery {
    #[serde(default)]
    all: bool,
}

pub(super) async fn dead_list(
    State(app): State<App>,
    h: HeaderMap,
    Query(q): Query<DeadQuery>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    match app.sm.dead_letters(q.all) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn dead_one(State(app): State<App>, h: HeaderMap, Path(n): Path<u64>) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    match app.sm.dead_letter(n) {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub(super) struct RefuseBody {
    id: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    reason: String,
}

/// An application refusing an item over HTTP (as `idn/<machine>/nack/<id>` over MQTT).
pub(super) async fn dead_refuse(
    State(app): State<App>,
    h: HeaderMap,
    Json(b): Json<RefuseBody>,
) -> Response {
    // The application (the API key) or an operator: not a viewer.
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    let body = serde_json::json!({ "code": b.code, "reason": b.reason });
    match app.sm.refuse(&b.id, &body, &who(&app, &h)) {
        Ok(n) => Json(serde_json::json!({ "dead_letter": n })).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn dead_retry(
    State(app): State<App>,
    h: HeaderMap,
    Path(n): Path<u64>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    match app.sm.retry_dead(n, &who(&app, &h)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub(super) struct ResolveBody {
    #[serde(default)]
    note: String,
}

pub(super) async fn dead_resolve(
    State(app): State<App>,
    h: HeaderMap,
    Path(n): Path<u64>,
    Json(b): Json<ResolveBody>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    match app.sm.resolve_dead(n, &who(&app, &h), &b.note) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn held_commands(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    Json(app.sm.held_commands()).into_response()
}

/// Cancels a held command (§I.7.3): an administrator's.
pub(super) async fn held_cancel(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    let (sm, by) = (app.sm.clone(), who(&app, &h));
    match tokio::task::spawn_blocking(move || sm.cancel_command(&id, &by)).await {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => fail(e),
        Err(e) => fail(idn_common::err::other(e)),
    }
}
