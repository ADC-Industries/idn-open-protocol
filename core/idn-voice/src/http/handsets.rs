//! Handsets over HTTP (§V.13): enrolment with a code (the app), the administrators' and
//! the site program's handset list, codes and credentials, the driver assignment, and
//! SOS alerts. Every change is audited; credentials and codes are never logged.

use super::{OverTls, admin, asked_here, bad, needs_https, not_here, safe_for_handsets};
use crate::Voice;
use crate::model::Driver;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use idn_common::sync::LockOk as _;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;

pub(super) fn routes(r: Router<Voice>) -> Router<Voice> {
    r.route("/api/enrol", post(enrol))
        .route("/api/handsets", get(list).post(add))
        .route(
            "/api/handsets/{handset}",
            axum::routing::put(rename).delete(remove),
        )
        .route("/api/handsets/{handset}/code", post(new_code))
        .route("/api/handsets/{handset}/credential", post(new_credential))
        .route("/api/handsets/{handset}/revoke", post(revoke))
        .route("/api/driver", post(driver))
        .route("/api/sos", get(sos_list))
        .route("/api/sos/{id}/ack", post(sos_ack))
}

fn not_found(why: impl ToString) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": why.to_string()})),
    )
        .into_response()
}

/// This server's address as the request reached it: `https` over its own TLS or behind a
/// proxy that says so, else `http`; the Host header; the base path.
fn reached_as(v: &Voice, headers: &HeaderMap, tls: bool) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c == '/') {
        return None;
    }
    let https = tls
        || headers
            .get("x-forwarded-proto")
            .and_then(|p| p.to_str().ok())
            .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let scheme = if https { "https" } else { "http" };
    Some(format!("{scheme}://{host}{}", v.cfg.base))
}

/// The addresses a handset is told: the one it came by first, then the configured ones.
fn urls(v: &Voice, first: Option<String>) -> Vec<String> {
    let mut out: Vec<String> = first.into_iter().collect();
    for u in &v.cfg.urls {
        if !out.contains(u) {
            out.push(u.clone());
        }
    }
    out
}

/// The site's certificate fingerprint as the QR code's `fp` (64 hex digits).
fn fp_param(v: &Voice) -> Option<String> {
    v.cfg.cert_sha256.as_ref().map(|f| f.replace(':', ""))
}

#[derive(Deserialize, Default)]
struct CodeBody {
    /// The address to put in the QR code (`https://192.168.1.5:29203/voice`); else the
    /// first configured one, else the one this request came by.
    #[serde(default)]
    url: Option<String>,
}

/// A new enrolment code's answer: the code, its link and QR code.
fn code_answer(
    v: &Voice,
    headers: &HeaderMap,
    tls: bool,
    want: Option<String>,
    h: &crate::model::Handset,
    code: &str,
) -> Response {
    let base = want
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| {
            (u.starts_with("https://") || u.starts_with("http://"))
                && u.len() <= 300
                && !u.chars().any(|c| c.is_whitespace() || c == '?' || c == '#')
        })
        .or_else(|| v.cfg.urls.first().cloned())
        .or_else(|| reached_as(v, headers, tls));
    let Some(base) = base else {
        return bad("no address for the QR code: give `url`");
    };
    let bare: String = code.chars().filter(char::is_ascii_alphanumeric).collect();
    let mut link = format!("{base}/api/enrol?code={bare}");
    if let Some(fp) = fp_param(v) {
        link += &format!("&fp={fp}");
    }
    // Voice over IDN's link (§V.14): its addresses and its certificate's SHA-256.
    if let Some(o) = v.hub.link_offer() {
        let first = std::iter::once(base.as_str()).chain(v.cfg.urls.iter().map(String::as_str));
        let addrs = crate::link::addrs(o.port, first).join(",");
        link += &format!("&link={addrs}&link_fp={}", o.cert_sha256.replace(':', ""));
    }
    Json(json!({
        "handset": h.public(crate::hub::now_ms()),
        "code": code,
        "expires_ms": h.code_until_ms,
        "link": link,
        "qr": crate::web::qr_svg(&link),
    }))
    .into_response()
}

fn credential_answer(
    v: &Voice,
    headers: &HeaderMap,
    tls: bool,
    h: &crate::model::Handset,
    credential: &str,
) -> Response {
    let urls = urls(v, reached_as(v, headers, tls));
    // Voice over IDN's link (§V.14): each address's host at its UDP port, the one it came
    // by first; none where it isn't offered.
    let offer = v.hub.link_offer();
    let link = offer.as_ref().map_or(Vec::new(), |o| {
        crate::link::addrs(o.port, urls.iter().map(String::as_str))
    });
    Json(json!({
        "handset": {"id": h.id, "name": h.name},
        "credential": credential,
        "urls": urls,
        "cert_sha256": v.cfg.cert_sha256,
        "link": link,
        "link_cert_sha256": offer.map(|o| o.cert_sha256),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct EnrolBody {
    code: String,
    /// What the app runs on (model, OS), shown to administrators.
    #[serde(default)]
    device: String,
}

/// `POST /api/enrol {code, device}`: the app's first contact. No sign-in: the code is the
/// administrator's word for it.
async fn enrol(
    State(v): State<Voice>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<OverTls>>,
    Json(b): Json<EnrolBody>,
) -> Response {
    let ip = peer.ip();
    // (The credential comes back in the answer: never over plain HTTP on the network.)
    if !safe_for_handsets(&v, &headers, ip, tls.is_some()) {
        return needs_https(&v);
    }
    let set = v.hub.settings();
    let allowed =
        v.hub
            .enrols
            .lock_ok()
            .attempt(ip, set.enrol_per_minute, set.enrol_all_per_minute);
    if !allowed {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "too many tries; wait a minute"})),
        )
            .into_response();
    }
    match v.hub.handset_enrol(&b.code, &b.device) {
        Ok((h, cred)) => {
            v.hub.enrols.lock_ok().succeeded(ip);
            v.hub.host.audit(
                &format!("handset {}", h.name),
                "voice.handset.enrol",
                json!({"handset": h.id, "name": h.name, "device": h.device, "address": ip.to_string()}),
            );
            credential_answer(&v, &headers, tls.is_some(), &h, &cred)
        }
        Err(e) => (StatusCode::FORBIDDEN, Json(json!({"error": e}))).into_response(),
    }
}

async fn list(State(v): State<Voice>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&v, &headers) {
        return e;
    }
    Json(json!(v.hub.handset_list())).into_response()
}

#[derive(Deserialize)]
struct AddBody {
    name: String,
    /// `code` (default): an enrolment code and its QR to give the handset; `credential`:
    /// its credential at once (a program provisioning its own app).
    #[serde(default)]
    issue: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// `POST /api/handsets {name, issue, url}`: a new handset with its code or credential.
async fn add(
    State(v): State<Voice>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<OverTls>>,
    Json(b): Json<AddBody>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let credential = match b.issue.as_deref().unwrap_or("code") {
        "code" => false,
        "credential" => true,
        _ => return bad("issue: code or credential"),
    };
    if credential && !safe_for_handsets(&v, &headers, peer.ip(), tls.is_some()) {
        return needs_https(&v);
    }
    let h = match v.hub.handset_add(&b.name) {
        Ok(h) => h,
        Err(e) => return bad(e),
    };
    v.hub.host.audit(
        &who.name,
        "voice.handset.add",
        json!({"handset": h.id, "name": h.name, "issue": if credential { "credential" } else { "code" }}),
    );
    if credential {
        match v.hub.handset_issue(&h.id) {
            Ok((h, cred)) => credential_answer(&v, &headers, tls.is_some(), &h, &cred),
            Err(e) => bad(e),
        }
    } else {
        let hours = v.hub.settings().code_hours as u64;
        match v.hub.handset_code(&h.id, Duration::from_secs(hours * 3600)) {
            Ok((h, code)) => code_answer(&v, &headers, tls.is_some(), b.url, &h, &code),
            Err(e) => bad(e),
        }
    }
}

/// `POST /api/handsets/{handset}/code {url}`: a new enrolment code (enrolling with it
/// replaces the handset's credential).
async fn new_code(
    State(v): State<Voice>,
    headers: HeaderMap,
    tls: Option<axum::Extension<OverTls>>,
    Path(key): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let hours = v.hub.settings().code_hours as u64;
    match v.hub.handset_code(&key, Duration::from_secs(hours * 3600)) {
        Ok((h, code)) => {
            v.hub.host.audit(
                &who.name,
                "voice.handset.code",
                json!({"handset": h.id, "name": h.name}),
            );
            // (A body is optional: `{"url"}`.)
            let url = serde_json::from_slice::<CodeBody>(&body)
                .unwrap_or_default()
                .url;
            code_answer(&v, &headers, tls.is_some(), url, &h, &code)
        }
        Err(e) => not_found(e),
    }
}

/// `POST /api/handsets/{handset}/credential`: a new credential at once; the old one stops
/// working and its sessions end.
async fn new_credential(
    State(v): State<Voice>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<OverTls>>,
    Path(key): Path<String>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    if !safe_for_handsets(&v, &headers, peer.ip(), tls.is_some()) {
        return needs_https(&v);
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    match v.hub.handset_issue(&key) {
        Ok((h, cred)) => {
            v.hub.host.audit(
                &who.name,
                "voice.handset.credential",
                json!({"handset": h.id, "name": h.name}),
            );
            credential_answer(&v, &headers, tls.is_some(), &h, &cred)
        }
        Err(e) => not_found(e),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(v): State<Voice>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(b): Json<RenameBody>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let Some(before) = v.hub.handset_find(&key) else {
        return not_found("no such handset");
    };
    match v.hub.handset_rename(&key, &b.name) {
        Ok(h) => {
            v.hub.host.audit(
                &who.name,
                "voice.handset.rename",
                json!({"handset": h.id, "from": before.name, "to": h.name}),
            );
            Json(h.public(crate::hub::now_ms())).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn revoke(State(v): State<Voice>, headers: HeaderMap, Path(key): Path<String>) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    match v.hub.handset_revoke(&key) {
        Ok(h) => {
            v.hub.host.audit(
                &who.name,
                "voice.handset.revoke",
                json!({"handset": h.id, "name": h.name}),
            );
            Json(h.public(crate::hub::now_ms())).into_response()
        }
        Err(e) => not_found(e),
    }
}

async fn remove(State(v): State<Voice>, headers: HeaderMap, Path(key): Path<String>) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    match v.hub.handset_remove(&key) {
        Ok(h) => {
            v.hub.host.audit(
                &who.name,
                "voice.handset.remove",
                json!({"handset": h.id, "name": h.name}),
            );
            Json(json!({})).into_response()
        }
        Err(e) => not_found(e),
    }
}

#[derive(Deserialize)]
struct DriverBody {
    /// The handset's id or name.
    handset: String,
    /// The driver, or `null` (off shift).
    driver: Option<Driver>,
}

/// `POST /api/driver {handset, driver}`: the site's program puts a driver on a handset,
/// or takes them off (`driver: null`). The same body as MQTT `idn/site/voice/cmd/driver`.
async fn driver(State(v): State<Voice>, headers: HeaderMap, Json(b): Json<DriverBody>) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    if v.hub.handset_find(&b.handset).is_none() {
        return not_found(format!("no handset {}", b.handset.trim()));
    }
    match v.hub.assign_driver(&b.handset, b.driver, &who.name) {
        Ok(r) => Json(r).into_response(),
        Err(e) => bad(e),
    }
}

async fn sos_list(State(v): State<Voice>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&v, &headers) {
        return e;
    }
    Json(json!(v.hub.sos_list())).into_response()
}

async fn sos_ack(State(v): State<Voice>, headers: HeaderMap, Path(id): Path<u64>) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    match v.hub.sos_ack(id, &who.name, None) {
        Ok(()) => Json(json!({})).into_response(),
        Err(e) => not_found(e),
    }
}
