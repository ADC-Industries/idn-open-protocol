//! Remote support (§I.11): users, the audit log, screens and chat.

use super::*;

pub(super) async fn users(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.users() {
        Ok(v) => Json(
            v.into_iter()
                .map(|(name, role)| serde_json::json!({ "name": name, "role": role.as_str() }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub(super) struct UserSetBody {
    username: String,
    password: String,
    #[serde(default)]
    role: String,
}

pub(super) async fn user_set(
    State(app): State<App>,
    h: HeaderMap,
    Json(b): Json<UserSetBody>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    let Some(role) = Role::parse(if b.role.is_empty() { "viewer" } else { &b.role }) else {
        return (StatusCode::BAD_REQUEST, "role is viewer, operator or admin").into_response();
    };
    match app.sm.set_user(&b.username, &b.password, role) {
        Ok(()) => {
            app.ui.end_user(&b.username);
            app.sm.audit(
                &who(&app, &h),
                "",
                "user.set",
                serde_json::json!({ "username": b.username, "role": b.role,
                    "password_changed": !b.password.is_empty() }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn user_remove(
    State(app): State<App>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.remove_user(&name) {
        Ok(true) => {
            app.ui.end_user(&name);
            app.sm.audit(
                &who(&app, &h),
                "",
                "user.remove",
                serde_json::json!({ "username": name }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub(super) struct AuditQuery {
    #[serde(default)]
    after: u64,
    n: Option<usize>,
}

pub(super) async fn audit(
    State(app): State<App>,
    h: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    match app.sm.audit_log(q.after, q.n.unwrap_or(200).min(1000)) {
        Ok(v) => Json(
            v.into_iter()
                .map(|(seq, mut rec)| {
                    if let Some(o) = rec.as_object_mut() {
                        o.insert("seq".into(), seq.into());
                    }
                    rec
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

pub(super) async fn screens(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    Json(app.sm.screens()).into_response()
}

pub(super) async fn screen_start(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    // Controlling needs operator; watching needs viewer.
    let wants_control = body
        .get("control")
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    let need = if wants_control {
        Role::Operator
    } else {
        Role::Viewer
    };
    if let Err(e) = require(&app, &h, need) {
        return e;
    }
    if !body.is_object() {
        body = serde_json::json!({});
    }
    let user = who(&app, &h);
    match app.sm.start_screen(&fp, body, &user).await {
        Ok((session, w, hgt)) => {
            Json(serde_json::json!({ "session": session, "width": w, "height": hgt }))
                .into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn screen_stop(
    State(app): State<App>,
    h: HeaderMap,
    Path(session): Path<String>,
) -> Response {
    let role = match require(&app, &h, Role::Viewer) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let user = who(&app, &h);
    match app
        .sm
        .stop_screen_as(&session, &user, role >= Role::Operator)
        .await
    {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            (StatusCode::FORBIDDEN, e.to_string()).into_response()
        }
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct ChatBody {
    text: String,
}

pub(super) async fn chat_send(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
    Json(b): Json<ChatBody>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    let from = who(&app, &h);
    match app.sm.send_chat(&fp, &from, &b.text).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

/// A browser watching a screen session: cookie-authenticated (a WebSocket can't send the
/// UI header, but the session id is itself a secret and the cookie must be valid). The
/// pictures go down as binary; input comes up as JSON (operators only).
pub(super) async fn screen_ws(
    State(app): State<App>,
    h: HeaderMap,
    Path(session): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(sid) = session_of(&h).map(str::to_owned) else {
        return refused();
    };
    let Some(live) = app.ui.live(&sid) else {
        return refused();
    };
    let Some((rx, keyframe, control, _fp)) = app.sm.screen_watch(&session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let may_control = control && live.role >= Role::Operator;
    ws.on_upgrade(move |socket| {
        screen_socket(app, session, socket, rx, keyframe, may_control, Some(sid))
    })
}

pub(super) async fn screen_socket(
    app: App,
    session: String,
    socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<u8>>>,
    keyframe: Option<std::sync::Arc<Vec<u8>>>,
    may_control: bool,
    // The viewer's sign-in, checked as it goes: signed out, removed or made a viewer, the
    // screen (and its control) went on until the tab closed (deep bug run 2026-11-09).
    ui_session: Option<String>,
) {
    let still = |need: Role| {
        ui_session
            .as_deref()
            .is_none_or(|s| app.ui.live(s).is_some_and(|l| l.role >= need))
    };
    let mut checked = std::time::Instant::now();
    use futures::{SinkExt, StreamExt};
    let sm = app.sm.clone();
    // Count this viewer so the session ends when the last one leaves (§I.11.3).
    sm.viewer_joined(&session);
    let (mut tx, mut inbound) = socket.split();
    // The latest keyframe first, so decoding can start before the next one.
    if let Some(k) = keyframe
        && tx.send(Message::Binary(k.to_vec().into())).await.is_err()
    {
        sm.viewer_left(&session).await;
        return;
    }
    let send = &mut tx;
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(rec) => {
                    if checked.elapsed() > std::time::Duration::from_secs(2) {
                        checked = std::time::Instant::now();
                        if !still(Role::Viewer) {
                            break;
                        }
                    }
                    if send.send(Message::Binary(rec.to_vec().into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            msg = inbound.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if !may_control || !still(Role::Operator) {
                        continue;
                    }
                    if let Ok(events) = serde_json::from_str::<serde_json::Value>(&t)
                        && let Ok(ans) = sm.send_input(&session, events).await
                        && ans.get("clipboard").is_some()
                        && send
                            .send(Message::Text(ans.to_string().into()))
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    sm.viewer_left(&session).await;
}

pub(super) async fn screen_page() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        SCREEN,
    )
        .into_response()
}

pub(super) const SCREEN: &str = include_str!("../ui/screen.html");
