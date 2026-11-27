//! Updates (§I.17): releases, offers, rollback and pins; a machine's logs and tags.

use super::*;

pub(super) async fn releases_list(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    Json(app.sm.releases()).into_response()
}

/// A release file as the body (`idn-release make --product industrial …`): the role checked
/// first, then the body written to the file as it comes (at most 1 GiB), never held in
/// memory: the route is open to the network, and a body read before the check let anyone
/// fill the Site Manager's memory.
pub(super) async fn release_put(
    State(app): State<App>,
    h: HeaderMap,
    body: axum::body::Body,
) -> Response {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    const MAX: u64 = 1 << 30;
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    let tmp = app.sm.data_dir().join(format!(
        "release-upload-{}.part",
        idn_open_protocol::random::token_hex(8)
    ));
    let written: Result<(), Response> = async {
        let mut f = tokio::fs::File::create(&tmp).await.map_err(fail)?;
        let (mut stream, mut n) = (body.into_data_stream(), 0u64);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                (StatusCode::BAD_REQUEST, format!("the upload: {e}")).into_response()
            })?;
            n += chunk.len() as u64;
            if n > MAX {
                return Err(
                    (StatusCode::PAYLOAD_TOO_LARGE, "a release is at most 1 GiB").into_response(),
                );
            }
            f.write_all(&chunk).await.map_err(fail)?;
        }
        f.flush().await.map_err(fail)
    }
    .await;
    if let Err(r) = written {
        let _ = std::fs::remove_file(&tmp);
        return r;
    }
    let (sm, by) = (app.sm.clone(), who(&app, &h));
    match tokio::task::spawn_blocking(move || sm.add_release(&tmp, &by)).await {
        Ok(Ok(m)) => Json(m).into_response(),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Ok(Err(e)) => fail(e),
        Err(e) => fail(idn_common::err::other(e)),
    }
}

pub(super) async fn release_delete(
    State(app): State<App>,
    h: HeaderMap,
    Path(version): Path<String>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.remove_release(&version, &who(&app, &h)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub(super) async fn updates_get(State(app): State<App>, h: HeaderMap) -> Response {
    if let Err(e) = require(&app, &h, Role::Viewer) {
        return e;
    }
    match app.sm.update_pins() {
        Ok(p) => Json(serde_json::json!({ "pinned": p })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct OfferBody {
    version: String,
    #[serde(default = "all_scope")]
    scope: String,
    #[serde(default = "now_when")]
    when: String,
}

pub(super) fn all_scope() -> String {
    "all".into()
}
pub(super) fn now_when() -> String {
    "now".into()
}

pub(super) async fn updates_offer(
    State(app): State<App>,
    h: HeaderMap,
    Json(b): Json<OfferBody>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app
        .sm
        .offer_update(&b.version, &b.scope, &b.when, &who(&app, &h))
    {
        Ok(m) => Json(serde_json::json!({ "machines": m })).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}

pub(super) async fn machine_rollback(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.rollback_machine(&fp, &who(&app, &h)) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct PinBody {
    pinned: bool,
}

pub(super) async fn machine_pin(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
    Json(b): Json<PinBody>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    match app.sm.set_pin(&fp, b.pinned, &who(&app, &h)) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

/// "Send me the logs" (§I.12): the machine's recent logs, if it is connected (operators).
pub(super) async fn machine_logs(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Operator) {
        return e;
    }
    app.sm
        .audit(&who(&app, &h), &fp, "logs", serde_json::json!({}));
    match app
        .sm
        .command(&fp, "idn.logs", b"{}".to_vec(), Duration::from_secs(15))
        .await
    {
        Ok((st, body)) if st == crate::wire::ST_OK => {
            ([("content-type", "application/json")], body).into_response()
        }
        Ok((_, body)) => (StatusCode::BAD_GATEWAY, body).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => (
            StatusCode::GATEWAY_TIMEOUT,
            "no answer in 15 s: the machine may have just gone (it shows as offline within a minute)",
        )
            .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

/// A machine's tags, changed (admins): `{"plant": "PD", "role": "batch"}`.
pub(super) async fn machine_tags(
    State(app): State<App>,
    h: HeaderMap,
    Path(fp): Path<String>,
    Json(tags): Json<BTreeMap<String, String>>,
) -> Response {
    if let Err(e) = require(&app, &h, Role::Admin) {
        return e;
    }
    let before = app
        .sm
        .machines()
        .ok()
        .and_then(|v| v.into_iter().find(|m| m.fingerprint == fp))
        .map(|m| m.tags);
    match app.sm.set_tags(&fp, tags.clone()).await {
        Ok(()) => {
            app.sm.audit(
                &who(&app, &h),
                &fp,
                "machine.tags",
                serde_json::json!({ "from": before, "to": tags }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => fail(e),
    }
}
