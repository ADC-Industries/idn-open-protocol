//! The HTTP side: the page and its files, the WebSocket, logins, the administrators' API
//! (channels, settings, recordings) and the programs' (a dispatch system's, §V.10:
//! presence, a message into a channel, a link that signs a user into the embedded
//! console).

use crate::hub::Who;
use crate::model::{Access, Allowed, Channel, Kind, Settings, valid_name};
use crate::{User, Voice, web};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use idn_common::sync::LockOk as _;
use idn_open_protocol::web::{PasswordChecks, SameSite, cookie, set_cookie};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod handsets;

/// A login lasts this long without use.
const LOGIN_FOR: Duration = Duration::from_secs(12 * 3600);
/// A console link (§V.10) works once, within this long.
const LINK_FOR: Duration = Duration::from_secs(300);
const COOKIE: &str = "idn_voice";

/// Voice's own logins (its page's sign-in) and console links.
#[derive(Default)]
pub struct Logins {
    by_token: Mutex<HashMap<String, (User, Instant)>>,
    links: Mutex<HashMap<String, (String, Instant)>>,
    /// Wrong passwords by address in the last minute (the Site Managers' own count: the
    /// check and the count in one step, deep bug run 2026-11-09).
    fails: idn_open_protocol::web::Logins,
}

/// Passwords checked at once (Argon2: each a few tens of MB and a core for a while): the
/// process's cap, shared with the Site Manager's own logins.
pub(crate) fn checks() -> &'static PasswordChecks {
    PasswordChecks::shared()
}

/// A login's or a console link's token.
fn token() -> String {
    idn_open_protocol::random::token_hex(24)
}

impl Logins {
    /// A try at a password from `ip` (a login, a channel's): whether it may, counted.
    pub(crate) fn attempt(&self, ip: IpAddr) -> bool {
        self.fails.attempt(ip)
    }

    pub(crate) fn succeeded(&self, ip: IpAddr) {
        self.fails.succeeded(ip)
    }

    fn create(&self, u: User) -> String {
        let t = token();
        let mut m = self.by_token.lock_ok();
        m.retain(|_, (_, at)| at.elapsed() < LOGIN_FOR);
        m.insert(t.clone(), (u, Instant::now()));
        t
    }

    /// The login's user, while it lasts: 12 hours from signing in, not kept going by use
    /// (a login taken stayed one for ever while used, deep bug run 2026-11-09).
    fn user(&self, t: &str) -> Option<User> {
        let mut m = self.by_token.lock_ok();
        let e = m.get(t)?;
        if e.1.elapsed() >= LOGIN_FOR {
            m.remove(t);
            return None;
        }
        Some(e.0.clone())
    }

    fn remove(&self, t: &str) {
        self.by_token.lock_ok().remove(t);
    }
}

/// A handset's credential given as `Authorization: Bearer idnv1.…` (§V.13).
pub(crate) fn handset_bearer(headers: &HeaderMap) -> Option<&str> {
    idn_open_protocol::web::bearer(headers).filter(|t| t.starts_with(crate::hub::CREDENTIAL_PREFIX))
}

/// Whether a handset's secrets (its credential, an enrolment, a provisioned credential)
/// may cross this request (§V.13.2): over this server's own TLS; from a proxy the Site
/// Manager trusts (its tunnel) that says the client came by HTTPS; or anywhere when the
/// site allows plain HTTP (`plain_handsets`, off by default).
pub(crate) fn safe_for_handsets(v: &Voice, headers: &HeaderMap, peer: IpAddr, tls: bool) -> bool {
    tls || (v.hub.host.trusted_proxy(peer)
        && headers
            .get("x-forwarded-proto")
            .and_then(|p| p.to_str().ok())
            .is_some_and(|p| p.trim().eq_ignore_ascii_case("https")))
        || v.hub.settings().plain_handsets
}

pub(crate) fn needs_https(v: &Voice) -> Response {
    let to = v
        .cfg
        .cert_sha256
        .as_ref()
        .map_or(String::new(), |_| " (this server's HTTPS port)".to_owned());
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error": format!("handsets only over HTTPS{to}")})),
    )
        .into_response()
}

/// The site user a request is signed in as: Voice's own login, else the Site Manager's.
/// Never from a handset's credential (a handset is no site user, and never an
/// administrator).
fn user_of(v: &Voice, headers: &HeaderMap) -> Option<User> {
    if handset_bearer(headers).is_some() {
        return None;
    }
    let own = cookie(headers, COOKIE).and_then(|t| {
        let u = v.logins.user(t)?;
        // As the site has them now: a user removed is signed out, one whose roles changed
        // has the new ones (they were kept from signing in, deep bug run 2026-11-09).
        let now = v.hub.host.users().into_iter().find(|n| n.name == u.name);
        if now.is_none() {
            v.logins.remove(t);
        }
        now
    });
    own.or_else(|| v.hub.host.session_user(headers))
}

/// A change asked for by the page itself or a program: the page's own header (another
/// site's page can't send it without asking first, and isn't allowed to), or a bearer
/// token. Voice's cookie is enough for nothing that changes something.
fn asked_here(h: &HeaderMap) -> bool {
    h.contains_key("x-idn-voice") || h.contains_key(header::AUTHORIZATION)
}

/// A browser's request from a page of this server (no Origin: not a browser's).
fn same_origin(h: &HeaderMap) -> bool {
    let Some(origin) = h.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
        return true;
    };
    let host = h
        .get(header::HOST)
        .and_then(|o| o.to_str().ok())
        .unwrap_or_default();
    origin
        .split_once("://")
        .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(host))
}

fn not_here() -> Response {
    denied("send the X-IDN-Voice header (the page does)")
}

pub(crate) fn who_of(u: &User) -> Who {
    Who {
        name: u.name.clone(),
        user: true,
        admin: u.admin,
        roles: u.roles.clone(),
        feed: None,
        ..Default::default()
    }
}

fn denied(why: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"error": why}))).into_response()
}

fn bad(why: impl ToString) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": why.to_string()})),
    )
        .into_response()
}

#[allow(clippy::result_large_err)] // (a response to hand back as it is)
fn admin(v: &Voice, h: &HeaderMap) -> Result<User, Response> {
    match user_of(v, h) {
        Some(u) if u.admin => Ok(u),
        Some(_) => Err(denied("administrators only")),
        None => Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "sign in"}))).into_response()),
    }
}

pub fn router(v: Voice) -> Router {
    let pages = v.cfg.pages;
    let r = Router::new();
    // The web client (VoiceConfig::pages).
    let r = if pages {
        r.route("/", get(page))
            .route(
                "/app.js",
                get(|| async { asset("text/javascript; charset=utf-8", web::APP_JS) }),
            )
            .route(
                "/app.css",
                get(|| async { asset("text/css; charset=utf-8", web::APP_CSS) }),
            )
            .route("/qr", get(qr))
    } else {
        r
    };
    let r = handsets::routes(r);
    r.route("/ws", get(ws))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/link/{token}", get(link_open))
        .route("/api/channels", get(channels_list).post(channel_put))
        .route("/api/channels/{id}", delete(channel_remove))
        .route("/api/settings", get(settings_get).post(settings_put))
        .route("/api/recordings", get(recordings))
        .route("/api/recordings/{id}", delete(recording_remove))
        .route("/api/presence", get(presence))
        .route("/api/messages", post(post_message))
        .route("/api/links", post(link_make))
        .route("/api/status", get(status))
        .with_state(v)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn page(State(v): State<Voice>) -> Response {
    let html = web::INDEX_HTML.replace("{{BASE}}", &v.cfg.base);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        html,
    )
        .into_response()
}

#[derive(Deserialize)]
struct QrQuery {
    channel: u32,
}

/// A QR code (SVG) to the page with a channel picked: printed or shown for guests.
async fn qr(State(v): State<Voice>, headers: HeaderMap, Query(q): Query<QrQuery>) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("https");
    let url = format!("{scheme}://{host}{}/?channel={}", v.cfg.base, q.channel);
    ([(header::CONTENT_TYPE, "image/svg+xml")], web::qr_svg(&url)).into_response()
}

#[derive(Deserialize)]
struct WsQuery {
    guest: Option<String>,
    feed: Option<String>,
    /// A handset's resume token (a `hello`'s), §V.13.6.
    resume: Option<String>,
    /// ... and its last `seq` from each talker: `TALKER:SEQ,…`.
    have: Option<String>,
}

async fn ws(
    State(v): State<Voice>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<OverTls>>,
    Query(q): Query<WsQuery>,
    up: WebSocketUpgrade,
) -> Response {
    let peer_ip = peer.ip();
    let who = if let Some(t) = q.feed.as_deref() {
        match v.feeds.channel_of(t) {
            Some((c, name, program)) => Who {
                name,
                user: false,
                admin: false,
                roles: vec![],
                feed: Some(c),
                program,
                ..Default::default()
            },
            None => return denied("not a feed of this server"),
        }
    } else if let Some(t) = handset_bearer(&headers) {
        // A handset (§V.13): an app, not a page, so no Origin to check; never over plain
        // HTTP on the network (its credential in the clear).
        if !safe_for_handsets(&v, &headers, peer_ip, tls.is_some()) {
            return needs_https(&v);
        }
        match v.hub.handset_by_credential(t) {
            Some(h) => v.hub.handset_who(&h),
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not an enrolled handset (revoked, or enrolled again)"})),
                )
                    .into_response();
            }
        }
    } else if let Some(u) = user_of(&v, &headers) {
        // A signed-in browser's socket only from this server's own page (another site's
        // page would otherwise talk and listen as the user).
        if !same_origin(&headers) {
            return denied("a page of another site");
        }
        // (The site's program by its credential: not followed as a site account.)
        Who {
            api: v.hub.host.is_program(&headers),
            ..who_of(&u)
        }
    } else if let Some(g) = q.guest.as_deref().map(str::trim).filter(|g| valid_name(g)) {
        Who {
            name: format!("{g} (guest)"),
            user: false,
            admin: false,
            roles: vec![],
            feed: None,
            ..Default::default()
        }
    } else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "sign in or give a name"})),
        )
            .into_response();
    };
    let resume = q.resume.filter(|_| who.handset.is_some());
    let have = q.have.filter(|_| resume.is_some());
    // (To the server 64 KiB, as the link's frames; from it up to 4 MiB, `MAX_FROM_SERVER`,
    // held to in session.rs.)
    up.max_message_size(crate::link::MAX_FRAME)
        .on_upgrade(move |socket| crate::ws::session(v, socket, who, peer_ip, resume, have))
}

#[derive(Deserialize)]
struct LoginBody {
    user: String,
    password: String,
}

async fn login(
    State(v): State<Voice>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<LoginBody>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    // The client's own address (behind the Site Manager's tunnel every client is the
    // tunnel's: one wrong password then locked everyone out).
    let host = v.hub.host.clone();
    let ip = idn_open_protocol::web::client_ip(&headers, peer.ip(), |p| host.trusted_proxy(p));
    if !v.logins.fails.attempt(ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "too many tries; wait a minute"})),
        )
            .into_response();
    }
    let (name, pw) = (b.user.clone(), b.password.clone());
    let u = checks()
        .run(move || host.login(&name, &pw, ip))
        .await
        .flatten();
    let Some(u) = u else {
        // (Counted as it was tried.)
        return denied("wrong name or password");
    };
    v.logins.fails.succeeded(ip);
    let t = v.logins.create(u.clone());
    v.hub
        .host
        .audit(&u.name, "voice.login", json!({"address": ip.to_string()}));
    (
        [(
            header::SET_COOKIE,
            set_cookie(COOKIE, &t).max_age(LOGIN_FOR).to_string(),
        )],
        Json(json!({"name": u.name, "admin": u.admin})),
    )
        .into_response()
}

async fn logout(State(v): State<Voice>, headers: HeaderMap) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    if let Some(t) = cookie(&headers, COOKIE) {
        v.logins.remove(t);
    }
    (
        [(
            header::SET_COOKIE,
            set_cookie(COOKIE, "").max_age(Duration::ZERO).to_string(),
        )],
        Json(json!({})),
    )
        .into_response()
}

async fn me(
    State(v): State<Voice>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<OverTls>>,
) -> Response {
    if let Some(t) = handset_bearer(&headers) {
        if !safe_for_handsets(&v, &headers, peer.ip(), tls.is_some()) {
            return needs_https(&v);
        }
        return match v.hub.handset_by_credential(t) {
            Some(h) => {
                let who = v.hub.handset_who(&h);
                Json(json!({
                    "name": who.name, "admin": false, "dispatch": v.hub.dispatch(&who),
                    "handset": {"id": h.id, "name": h.name, "driver": h.driver},
                    "cert_sha256": v.cfg.cert_sha256,
                }))
                .into_response()
            }
            None => (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "not an enrolled handset (revoked, or enrolled again)"})),
            )
                .into_response(),
        };
    }
    match user_of(&v, &headers) {
        Some(u) => {
            Json(json!({"name": u.name, "admin": u.admin, "dispatch": v.hub.dispatch(&who_of(&u))}))
                .into_response()
        }
        None => Json(json!({"name": null})).into_response(),
    }
}

// ---- channels (administrators)

async fn channels_list(State(v): State<Voice>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&v, &headers) {
        return e;
    }
    let list: Vec<Value> = v
        .hub
        .channels()
        .iter()
        .map(|c| {
            let mut x = c.public();
            x["feed"] = json!(v.feeds.state(c.id));
            x
        })
        .collect();
    Json(json!(list)).into_response()
}

#[derive(Deserialize)]
struct ChannelBody {
    #[serde(default)]
    id: u32,
    name: String,
    #[serde(default)]
    description: String,
    /// `iem` or `walkie`.
    kind: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    half_duplex: bool,
    limit_s: Option<u32>,
    /// `guest`, `open`, `password`, `auth`.
    access: String,
    /// For `password`: a new password (empty: keep the one set).
    #[serde(default)]
    password: String,
    #[serde(default)]
    allow: Vec<Allowed>,
    record: Option<bool>,
    /// Walkie: an IDN source that talks into it while it has sound.
    #[serde(default)]
    talk_source: String,
    /// The channel as an IDN source.
    #[serde(default)]
    as_source: bool,
    /// Its own store-and-forward window (s) and jitter advice (ms): a number sets it,
    /// `null` takes it off (the site's then), left out keeps it (the web client's
    /// channel form doesn't send them).
    store_forward_s: Option<Value>,
    jitter_start_ms: Option<Value>,
    jitter_max_ms: Option<Value>,
}

/// A channel override as given (see `ChannelBody`): `Ok(new value)`.
fn override_of(
    given: &Option<Value>,
    before: Option<u32>,
    name: &str,
) -> Result<Option<u32>, String> {
    match given {
        None => Ok(before),
        Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| format!("{name}: a whole number or null")),
    }
}

async fn channel_put(
    State(v): State<Voice>,
    headers: HeaderMap,
    Json(b): Json<ChannelBody>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let name = b.name.trim().to_owned();
    if !valid_name(&name) {
        return bad("a name: 1 to 64 characters");
    }
    let existing = v.hub.channels();
    if existing
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(&name) && c.id != b.id)
    {
        return bad("there is a channel of that name");
    }
    if b.id == 0 && existing.len() >= 100 {
        return bad("100 channels at most");
    }
    let old = existing.iter().find(|c| c.id == b.id && b.id != 0);
    if b.id != 0 && old.is_none() {
        return bad("no such channel");
    }
    let kind = match b.kind.as_str() {
        "iem" => {
            if b.source.trim().is_empty() {
                return bad("an IEM channel needs its source");
            }
            Kind::Iem {
                source: b.source.trim().to_owned(),
            }
        }
        "walkie" => Kind::Walkie {
            half_duplex: b.half_duplex,
            limit_s: b.limit_s.unwrap_or(60).min(3600),
        },
        _ => return bad("kind: iem or walkie"),
    };
    let access = match b.access.as_str() {
        "guest" => Access::Guest,
        "open" => Access::Open,
        "password" => {
            let keep = old.and_then(|c| match &c.access {
                Access::Password { hash } => Some(hash.clone()),
                _ => None,
            });
            let hash = if b.password.is_empty() {
                match keep {
                    Some(h) => h,
                    None => return bad("a password channel needs a password"),
                }
            } else {
                match idn_open_protocol::web::hash_password(&b.password) {
                    Ok(h) => h,
                    Err(e) => return bad(e),
                }
            };
            Access::Password { hash }
        }
        "auth" => Access::Auth { allow: b.allow },
        _ => return bad("access: guest, open, password or auth"),
    };
    let iem = matches!(kind, Kind::Iem { .. });
    let mut c = Channel {
        id: b.id,
        name,
        description: b.description.chars().take(500).collect(),
        kind,
        access,
        record: b.record.unwrap_or(!iem),
        talk_source: Some(b.talk_source.trim().to_owned()).filter(|s| !s.is_empty() && !iem),
        as_source: b.as_source,
        store_forward_s: None,
        jitter_start_ms: None,
        jitter_max_ms: None,
    };
    let o = |g: &Option<Value>, f: fn(&Channel) -> Option<u32>, n: &str| {
        override_of(g, old.and_then(f), n)
    };
    match (
        o(&b.store_forward_s, |c| c.store_forward_s, "store_forward_s"),
        o(&b.jitter_start_ms, |c| c.jitter_start_ms, "jitter_start_ms"),
        o(&b.jitter_max_ms, |c| c.jitter_max_ms, "jitter_max_ms"),
    ) {
        (Ok(a), Ok(b2), Ok(c2)) => {
            c.store_forward_s = a;
            c.jitter_start_ms = b2;
            c.jitter_max_ms = c2;
        }
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return bad(e),
    }
    if let Err(e) = c.check_overrides() {
        return bad(e);
    }
    match v.hub.put_channel(c) {
        Ok(c) => {
            v.hub.host.audit(
                &who.name,
                if b.id == 0 {
                    "voice.channel.add"
                } else {
                    "voice.channel.change"
                },
                c.public(),
            );
            v.feeds.sync(&v.hub.channels());
            Json(c.public()).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn channel_remove(
    State(v): State<Voice>,
    headers: HeaderMap,
    Path(id): Path<u32>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    match v.hub.remove_channel(id) {
        Ok(true) => {
            v.hub
                .host
                .audit(&who.name, "voice.channel.remove", json!({"id": id}));
            v.feeds.sync(&v.hub.channels());
            Json(json!({})).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such channel"})),
        )
            .into_response(),
        Err(e) => bad(e),
    }
}

async fn settings_get(State(v): State<Voice>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&v, &headers) {
        return e;
    }
    let users: Vec<Value> = v
        .hub
        .host
        .users()
        .into_iter()
        .map(|u| json!({"name": u.name, "roles": u.roles, "admin": u.admin}))
        .collect();
    // (`quic`: where Voice over IDN's link is offered, §V.14; `null` where it isn't.)
    Json(json!({
        "settings": v.hub.settings(), "users": users,
        "webrtc": v.rtc.as_ref().map(|r| r.port), "quic": v.hub.link_offer(),
    }))
    .into_response()
}

/// The site's settings changed: the fields given, the others kept (each range checked).
async fn settings_put(
    State(v): State<Voice>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let Some(given) = body.as_object() else {
        return bad("the settings: an object");
    };
    let mut now = json!(v.hub.settings());
    for (k, x) in given {
        // (A misspelt setting was taken without a word, and nothing changed.)
        if now.get(k).is_none() {
            return bad(format!("no setting {k:?}"));
        }
        now[k] = x.clone();
    }
    let s: Settings = match serde_json::from_value(now) {
        Ok(s) => s,
        Err(e) => return bad(e),
    };
    if let Err(e) = s.check() {
        return bad(e);
    }
    match v.hub.put_settings(s.clone()) {
        Ok(()) => {
            v.hub.host.audit(&who.name, "voice.settings", json!(s));
            Json(json!(s)).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct RecQuery {
    #[serde(default)]
    since_ms: i64,
}

async fn recordings(
    State(v): State<Voice>,
    headers: HeaderMap,
    Query(q): Query<RecQuery>,
) -> Response {
    if let Err(e) = admin(&v, &headers) {
        return e;
    }
    match v.hub.store.transmissions_since(q.since_ms, 1000, |_| true) {
        Ok(list) => Json(json!(list)).into_response(),
        Err(e) => bad(e),
    }
}

async fn recording_remove(
    State(v): State<Voice>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let _ = v.hub.store.remove_transmission(id);
    crate::record::remove(&v.hub.dir, id);
    v.hub
        .host
        .audit(&who.name, "voice.recording.remove", json!({"id": id}));
    Json(json!({})).into_response()
}

// ---- programs (§V.10)

async fn presence(State(v): State<Voice>, headers: HeaderMap) -> Response {
    if let Err(e) = admin(&v, &headers) {
        return e;
    }
    Json(v.hub.status()).into_response()
}

async fn status(State(v): State<Voice>, headers: HeaderMap) -> Response {
    presence(State(v), headers).await
}

#[derive(Deserialize)]
struct PostBody {
    /// The channel's name or number.
    channel: Value,
    text: String,
    from: Option<String>,
}

async fn post_message(
    State(v): State<Voice>,
    headers: HeaderMap,
    Json(b): Json<PostBody>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let chans = v.hub.channels();
    let c = match &b.channel {
        Value::Number(n) => chans.iter().find(|c| Some(c.id as u64) == n.as_u64()),
        Value::String(s) => chans.iter().find(|c| c.name.eq_ignore_ascii_case(s)),
        _ => None,
    };
    let Some(c) = c else {
        return bad("no such channel");
    };
    let from = b
        .from
        .filter(|f| valid_name(f))
        .unwrap_or_else(|| who.name.clone());
    match v.hub.post(c.id, &from, &b.text) {
        Ok(m) => Json(json!(m)).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct LinkBody {
    user: String,
}

/// A link that signs `user` into the web client once (a dispatch system's embedded console).
async fn link_make(
    State(v): State<Voice>,
    headers: HeaderMap,
    Json(b): Json<LinkBody>,
) -> Response {
    if !asked_here(&headers) {
        return not_here();
    }
    let who = match admin(&v, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    if !v.hub.host.users().iter().any(|u| u.name == b.user) {
        return bad("no such user");
    }
    let t = token();
    {
        let mut l = v.logins.links.lock_ok();
        l.retain(|_, (_, at)| at.elapsed() < LINK_FOR);
        l.insert(t.clone(), (b.user.clone(), Instant::now()));
    }
    v.hub
        .host
        .audit(&who.name, "voice.link", json!({"user": b.user}));
    Json(json!({"path": format!("{}/api/link/{t}", v.cfg.base), "expires_s": LINK_FOR.as_secs()}))
        .into_response()
}

/// A request that came over this server's own TLS (the IDN Site Manager's port).
#[derive(Clone, Copy)]
pub(crate) struct OverTls;

async fn link_open(
    State(v): State<Voice>,
    Path(t): Path<String>,
    tls: Option<axum::Extension<OverTls>>,
    headers: HeaderMap,
) -> Response {
    let Some((name, at)) = v.logins.links.lock_ok().remove(&t) else {
        return denied("this link was used or has expired");
    };
    if at.elapsed() >= LINK_FOR {
        return denied("this link has expired");
    }
    let Some(u) = v.hub.host.users().into_iter().find(|u| u.name == name) else {
        return denied("no such user");
    };
    let tok = v.logins.create(u);
    // Over HTTPS (this server's own, or a tunnel's): usable inside another site's page
    // (SameSite=None, which needs Secure). Over plain HTTP (IDN Industrial on the plant's
    // network) browsers drop a Secure cookie, so it is the page's own (Strict: set by
    // following the link from elsewhere, then sent by the page's own requests; the page
    // itself needs none).
    let https = tls.is_some()
        || headers
            .get("x-forwarded-proto")
            .and_then(|p| p.to_str().ok())
            .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let same_site = if https {
        SameSite::None
    } else {
        SameSite::Strict
    };
    let cookie = [(
        header::SET_COOKIE,
        set_cookie(COOKIE, &tok)
            .max_age(LOGIN_FOR)
            .same_site(same_site)
            .to_string(),
    )];
    // To the web client when it is served here (IDN Industrial); without pages (IDN
    // Media: the client is the host program's) `{base}/` has no route: signed in, said
    // as JSON (bug run O).
    if v.cfg.pages {
        (cookie, Redirect::to(&format!("{}/", v.cfg.base))).into_response()
    } else {
        (cookie, Json(json!({"ok": true, "user": name}))).into_response()
    }
}
