//! Handsets and drivers (idn-voice.md §V.13) end to end: enrolment with a code (once,
//! within its time, wrong ones throttled), a credential given at once to the site's
//! program, signing in with it (never an administrator), revoking it, the site's program
//! putting a driver on a live handset (an Auth channel granted) and taking them off,
//! SOS to dispatch and its acknowledgement, a dropped link resumed with its token and the
//! sound that came meanwhile, a talk that goes quiet ending.

use crate::voice::{Client, login, make_channel};
use idn_common::sync::LockOk as _;
use idn_voice::{Host, User, Voice, VoiceConfig};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// What Voice told its Site Manager: audit entries and MQTT publications.
#[derive(Default)]
pub(crate) struct Told {
    audit: Mutex<Vec<(String, String, Value)>>,
    published: Mutex<Vec<(String, Value)>>,
}

struct RecHost(Arc<Told>);

fn users() -> Vec<User> {
    vec![
        User {
            name: "ann".into(),
            roles: vec!["admin".into()],
            admin: true,
        },
        User {
            name: "bob".into(),
            roles: vec!["operator".into()],
            admin: false,
        },
        User {
            name: "cy".into(),
            roles: vec!["operator".into()],
            admin: false,
        },
    ]
}

/// The site program's Bearer token here (as Industrial's API key): user `api`, who is no
/// site account.
pub(crate) const API_KEY: &str = "test-api-key";

impl Host for RecHost {
    fn session_user(&self, h: &axum::http::HeaderMap) -> Option<User> {
        self.is_program(h).then(|| User {
            name: "api".into(),
            roles: vec!["admin".into()],
            admin: true,
        })
    }
    fn is_program(&self, h: &axum::http::HeaderMap) -> bool {
        idn_open_protocol::web::bearer(h) == Some(API_KEY)
    }
    fn login(&self, name: &str, password: &str, _: std::net::IpAddr) -> Option<User> {
        users()
            .into_iter()
            .find(|u| u.name == name && password == format!("pw-{name}"))
    }
    fn users(&self) -> Vec<User> {
        users()
    }
    fn audit(&self, who: &str, action: &str, detail: Value) {
        self.0
            .audit
            .lock_ok()
            .push((who.into(), action.into(), detail));
    }
    fn publish(&self, topic: &str, body: &Value, _: bool) {
        self.0
            .published
            .lock_ok()
            .push((topic.into(), body.clone()));
    }
}

pub(crate) async fn server() -> (SocketAddr, Voice, Arc<Told>) {
    let dir = idn_common::testing::TempDir::new("voice-handsets");
    let mut cfg = VoiceConfig::new(dir.to_path_buf());
    cfg.udp = None;
    cfg.cert_sha256 = None;
    cfg.urls = vec![];
    let told = Arc::new(Told::default());
    let v = Voice::start(cfg, Arc::new(RecHost(told.clone()))).unwrap();
    // (These tests speak plain HTTP on loopback; handsets_need_https checks the default.)
    let mut s = v.hub.settings();
    s.plain_handsets = true;
    v.hub.put_settings(s).unwrap();
    let app = axum::Router::new().nest("/voice", v.router());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        // (The folder kept while the server runs: removed when the test's runtime ends.)
        let _dir = dir;
        axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    (addr, v, told)
}

/// An HTTP/1.1 request with these headers (and X-IDN-Voice): (status, body).
pub(crate) async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> (u16, Value) {
    let b = body.to_string();
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-IDN-Voice: 1\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (
        status,
        serde_json::from_str(body.trim()).unwrap_or(Value::Null),
    )
}

fn request(
    addr: SocketAddr,
    credential: &str,
    query: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = format!("ws://{addr}/voice/ws{query}")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {credential}").parse().unwrap(),
    );
    req
}

/// A handset's connection with its credential.
pub(crate) async fn handset(addr: SocketAddr, credential: &str, query: &str) -> Client {
    Client::open(request(addr, credential, query)).await
}

/// A handset made with its credential at once (the site's program provisioning it).
async fn provision(addr: SocketAddr, admin: &str, name: &str) -> (String, String) {
    let (st, r) = call(
        addr,
        "POST",
        "/voice/api/handsets",
        &[("Cookie", admin)],
        &json!({"name": name, "issue": "credential"}),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    (
        r["handset"]["id"].as_str().unwrap().to_owned(),
        r["credential"].as_str().unwrap().to_owned(),
    )
}

/// Waits for what Voice publishes on `topic` with `event`.
async fn published(told: &Told, topic: &str, event: &str) -> Value {
    for _ in 0..60 {
        if let Some((_, v)) = told
            .published
            .lock_ok()
            .iter()
            .find(|(t, v)| t == topic && v["event"] == event)
        {
            return v.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nothing published on {topic} for {event}");
}

fn audited(told: &Told, action: &str) -> Option<(String, Value)> {
    told.audit
        .lock_ok()
        .iter()
        .find(|(_, a, _)| a == action)
        .map(|(w, _, d)| (w.clone(), d.clone()))
}

#[tokio::test(flavor = "multi_thread")]
async fn enrolment_codes_work_once_within_their_time_and_wrong_ones_are_throttled() {
    let (addr, v, told) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    // Administrators only.
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/handsets",
        &[("Cookie", &bob)],
        &json!({"name": "Truck 12"}),
    )
    .await;
    assert_eq!(st, 403);
    let (st, r) = call(
        addr,
        "POST",
        "/voice/api/handsets",
        &[("Cookie", &ann)],
        &json!({"name": "Truck 12"}),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    let code = r["code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 11, "ten characters and a dash: {code}");
    assert!(
        r["link"]
            .as_str()
            .unwrap()
            .contains("/voice/api/enrol?code=")
    );
    assert!(r["qr"].as_str().unwrap().starts_with("<svg"));
    // A name is unique.
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/handsets",
        &[("Cookie", &ann)],
        &json!({"name": "truck 12"}),
    )
    .await;
    assert_eq!(st, 400);

    // Enrolled with the code as typed (lower case, no dash).
    let typed = code.replace('-', "").to_lowercase();
    let (st, e) = call(
        addr,
        "POST",
        "/voice/api/enrol",
        &[],
        &json!({"code": typed, "device": "Inrico B01 / Android 9"}),
    )
    .await;
    assert_eq!(st, 200, "{e}");
    let cred = e["credential"].as_str().unwrap();
    assert!(cred.starts_with("idnv1."));
    assert_eq!(e["handset"]["name"], "Truck 12");
    assert_eq!(e["urls"][0], format!("http://{addr}/voice"));
    assert!(e["cert_sha256"].is_null());
    assert!(audited(&told, "voice.handset.enrol").is_some());
    // The credential is kept as a hash only.
    let stored = v.hub.handset_find("Truck 12").unwrap();
    assert!(!stored.secret_sha256.contains(cred) && stored.secret_sha256.len() == 64);
    assert_eq!(stored.device, "Inrico B01 / Android 9");

    // Once only (1), and not past its time (2).
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/enrol",
        &[],
        &json!({"code": code}),
    )
    .await;
    assert_eq!(st, 403);
    v.hub.handset_add("Truck 9").unwrap();
    let (_, old) = v.hub.handset_code("Truck 9", Duration::ZERO).unwrap();
    let (st, _) = call(addr, "POST", "/voice/api/enrol", &[], &json!({"code": old})).await;
    assert_eq!(st, 403);
    // Three more wrong (five a minute): the next is refused before it is checked.
    for _ in 0..3 {
        let (st, _) = call(
            addr,
            "POST",
            "/voice/api/enrol",
            &[],
            &json!({"code": "AAAAA-AAAAA"}),
        )
        .await;
        assert_eq!(st, 403);
    }
    let (_, good) = v
        .hub
        .handset_code("Truck 9", Duration::from_secs(60))
        .unwrap();
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/enrol",
        &[],
        &json!({"code": good}),
    )
    .await;
    assert_eq!(st, 429);

    // The list: enrolled or not, never a hash.
    let (st, list) = call(
        addr,
        "GET",
        "/voice/api/handsets",
        &[("Cookie", &ann)],
        &json!({}),
    )
    .await;
    assert_eq!(st, 200);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(!list.iter().any(|h| h.to_string().contains("sha256")));
    let t12 = list.iter().find(|h| h["name"] == "Truck 12").unwrap();
    assert_eq!(t12["enrolled"], true);
    assert_eq!(t12["connected"], 0);
}

/// A handset's secrets never cross plain HTTP from a connection that isn't the Site
/// Manager's trusted proxy (IDN Industrial's plant network: its HTTPS port instead), not
/// even one that says `X-Forwarded-Proto: https`.
#[tokio::test(flavor = "multi_thread")]
async fn handsets_need_https() {
    let (addr, v, _told) = server().await;
    let ann = login(addr, "ann").await;
    let (_, cred) = provision(addr, &ann, "Truck 1").await;
    let (_, code) = v
        .hub
        .handset_code("Truck 1", Duration::from_secs(60))
        .unwrap();
    let mut s = v.hub.settings();
    s.plain_handsets = false;
    v.hub.put_settings(s).unwrap();
    let (st, e) = call(
        addr,
        "POST",
        "/voice/api/handsets",
        &[("Cookie", &ann), ("X-Forwarded-Proto", "https")],
        &json!({"name": "Truck 2", "issue": "credential"}),
    )
    .await;
    assert_eq!(st, 403, "{e}");
    assert!(e["error"].as_str().unwrap().contains("HTTPS"));
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/enrol",
        &[],
        &json!({"code": code}),
    )
    .await;
    assert_eq!(st, 403);
    let bearer = format!("Bearer {cred}");
    let (st, _) = call(
        addr,
        "GET",
        "/voice/api/me",
        &[("Authorization", &bearer)],
        &json!({}),
    )
    .await;
    assert_eq!(st, 403);
    assert!(
        tokio_tungstenite::connect_async(request(addr, &cred, ""))
            .await
            .is_err()
    );
    // A code is no secret of the handset's: still made.
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/handsets",
        &[("Cookie", &ann)],
        &json!({"name": "Truck 3"}),
    )
    .await;
    assert_eq!(st, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handset_signs_in_with_its_credential_is_never_admin_and_revoking_ends_it() {
    let (addr, _v, told) = server().await;
    let ann = login(addr, "ann").await;
    let (id, cred) = provision(addr, &ann, "Truck 7").await;
    let bearer = format!("Bearer {cred}");

    let mut h = handset(addr, &cred, "").await;
    let me = &h.hello["me"];
    assert_eq!(me["name"], "Handset Truck 7");
    assert_eq!(me["admin"], false);
    assert_eq!(me["handset"]["name"], "Truck 7");
    assert_eq!(me["roles"], json!(["handset"]));
    assert_eq!(h.hello["media"], "websocket");
    assert!(h.hello["resume"].as_str().is_some_and(|t| t.len() > 20));
    assert_eq!(h.hello["link"]["ping_s"], 10);
    assert_eq!(h.hello["link"]["jitter_start_ms"], 200);

    // Never an administrator, whatever it asks.
    let (st, _) = call(
        addr,
        "GET",
        "/voice/api/handsets",
        &[("Authorization", &bearer)],
        &json!({}),
    )
    .await;
    assert_eq!(st, 401);
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/channels",
        &[("Authorization", &bearer)],
        &json!({"name": "X", "kind": "walkie", "access": "open"}),
    )
    .await;
    assert_eq!(st, 401);
    let (st, m) = call(
        addr,
        "GET",
        "/voice/api/me",
        &[("Authorization", &bearer)],
        &json!({}),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(m["handset"]["id"], id.as_str());
    assert_eq!(m["admin"], false);

    // A credential that isn't one: refused.
    let wrong = format!("{}x", &cred[..cred.len() - 1]);
    assert!(
        tokio_tungstenite::connect_async(request(addr, &wrong, ""))
            .await
            .is_err()
    );

    // Revoked: its live session ends at once, and it can't come back.
    let (st, _) = call(
        addr,
        "POST",
        &format!("/voice/api/handsets/{id}/revoke"),
        &[("Cookie", &ann)],
        &json!({}),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(h.wait("bye").await["why"], "this handset was revoked");
    assert!(
        tokio_tungstenite::connect_async(request(addr, &cred, ""))
            .await
            .is_err()
    );
    assert!(audited(&told, "voice.handset.revoke").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sites_program_puts_a_driver_on_a_live_handset_and_takes_them_off() {
    let (addr, v, told) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let dispatch = make_channel(
        addr,
        &ann,
        json!({"name": "Dispatch", "kind": "walkie", "access": "auth", "allow": [{"role": "dispatcher"}]}),
    )
    .await;
    let secret = make_channel(
        addr,
        &ann,
        json!({"name": "Secret", "kind": "walkie", "access": "password", "password": "abc"}),
    )
    .await;
    let yard = make_channel(
        addr,
        &ann,
        json!({"name": "Yard", "kind": "walkie", "access": "open"}),
    )
    .await;
    let (_, cred) = provision(addr, &ann, "Truck 12").await;
    let mut h = handset(addr, &cred, "").await;
    let mut a = Client::connect(addr, &ann, "").await;
    a.send(json!({"t": "join", "channel": dispatch})).await;
    a.wait("joined").await;

    // The dispatch system (an administrator here) puts Dan on Truck 12.
    let (st, r) = call(
        addr,
        "POST",
        "/voice/api/driver",
        &[("Cookie", &ann)],
        &json!({"handset": "Truck 12", "driver": {
            "id": "D1042", "name": "Dan Smith", "roles": ["driver"],
            "channels": ["Dispatch", "secret", "Yard", "Nowhere"],
        }}),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["past_gates"], json!(["Dispatch", "Secret"]));
    assert_eq!(r["unknown_channels"], json!(["Nowhere"]));
    // At once: a fresh hello as Dan, the assigned channels joined past their gates.
    let hello = h.wait("hello").await;
    assert_eq!(hello["me"]["name"], "Dan Smith");
    assert_eq!(hello["me"]["driver"]["id"], "D1042");
    assert_eq!(hello["me"]["key"], "driver:D1042");
    assert_eq!(hello["me"]["admin"], false);
    let mut joined = Vec::new();
    for _ in 0..3 {
        joined.push(h.wait("joined").await["channel"].as_u64().unwrap());
    }
    joined.sort();
    assert_eq!(joined, vec![dispatch, secret, yard]);
    let (_, d) = audited(&told, "voice.driver.assign").unwrap();
    assert_eq!(d["past_gates"], json!(["Dispatch", "Secret"]));
    // The gate stays for everyone else.
    let mut b = Client::connect(addr, &bob, "").await;
    b.send(json!({"t": "join", "channel": dispatch})).await;
    assert_eq!(
        b.wait("error").await["error"],
        "not allowed in this channel"
    );

    // What Dan says is his, from Truck 12: messages and recordings.
    h.send(json!({"t": "message", "channel": dispatch, "text": "at the plant"}))
        .await;
    let m = a.wait("message").await;
    assert_eq!(m["message"]["from"], "Dan Smith");
    assert_eq!(m["message"]["handset"], "Truck 12");
    h.send(json!({"t": "talk", "on": true, "channel": dispatch}))
        .await;
    while h.wait("talking").await["on"] != true {}
    h.talk_frame(&[0xF8, 1, 2]).await;
    a.frames(1).await;
    h.send(json!({"t": "talk", "on": false})).await;
    while h.wait("talking").await["on"] != false {}
    a.send(json!({"t": "missed", "since_ms": 0})).await;
    let missed = a.wait("missed").await;
    let t = &missed["transmissions"][0];
    assert_eq!(t["from"], "Dan Smith");
    assert_eq!(t["handset"], "Truck 12");
    // Presence shows the handset and its driver.
    let on = v.hub.status();
    assert_eq!(on["handsets"][0]["driver"]["name"], "Dan Smith");

    // Off shift, over MQTT: back to "Handset Truck 12", out of the channels.
    v.hub.mqtt_command(
        "idn/site/voice/cmd/driver",
        br#"{"handset": "Truck 12", "driver": null}"#,
    );
    // Every channel the assignment put it in, the Open one too (bug run).
    let mut left = Vec::new();
    for _ in 0..3 {
        let l = h.wait("left").await;
        assert_eq!(l["why"], "driver changed");
        left.push(l["channel"].as_u64().unwrap());
    }
    left.sort();
    assert_eq!(left, vec![dispatch, secret, yard]);
    let hello = h.wait("hello").await;
    assert_eq!(hello["me"]["name"], "Handset Truck 12");
    assert!(hello["me"]["driver"].is_null());
    assert_eq!(
        published(&told, "events", "driver_result").await["ok"],
        true
    );
    assert!(audited(&told, "voice.driver.clear").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn sos_reaches_dispatch_once_until_acknowledged() {
    let (addr, _v, told) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let (_, cred) = provision(addr, &ann, "Truck 3").await;
    let mut a = Client::connect(addr, &ann, "").await;
    let mut b = Client::connect(addr, &bob, "").await;
    let mut h = handset(addr, &cred, "").await;

    // Only a handset sends one.
    b.send(json!({"t": "sos"})).await;
    assert_eq!(b.wait("error").await["what"], "sos");

    h.send(
        json!({"t": "sos", "position": {"lat": 53.35, "lon": -6.26}, "text": "slid off the road"}),
    )
    .await;
    let id = h.wait("sos_sent").await["id"].as_u64().unwrap();
    let s = a.wait("sos").await;
    assert_eq!(s["sos"]["id"].as_u64(), Some(id));
    assert_eq!(s["sos"]["handset"]["name"], "Truck 3");
    assert_eq!(s["sos"]["position"]["lat"], 53.35);
    // Pressed again at once: the same alert, not sent to dispatch again.
    h.send(json!({"t": "sos"})).await;
    assert_eq!(h.wait("sos_sent").await["id"].as_u64(), Some(id));
    a.send(json!({"t": "ping"})).await;
    a.wait("pong").await;
    assert!(a.held.iter().all(|m| m["t"] != "sos"));
    // Not dispatch: can't acknowledge it.
    b.send(json!({"t": "sos_ack", "id": id})).await;
    assert_eq!(b.wait("error").await["what"], "sos_ack");
    h.send(json!({"t": "sos_ack", "id": id})).await;
    assert_eq!(h.wait("error").await["what"], "sos_ack");
    // Dispatch acknowledges: the handset is told.
    a.send(json!({"t": "sos_ack", "id": id})).await;
    assert_eq!(h.wait("sos_ack").await["by"], "ann");
    a.wait("sos_ack").await;
    let ev = published(&told, "events", "sos").await;
    assert_eq!(ev["handset"]["name"], "Truck 3");
    assert_eq!(published(&told, "events", "sos_ack").await["by"], "ann");
    assert!(audited(&told, "voice.sos").is_some());
    assert_eq!(audited(&told, "voice.sos.ack").unwrap().0, "ann");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_handset_resumes_with_its_token_and_hears_what_came_meanwhile() {
    let (addr, _v, _told) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let (_, cred) = provision(addr, &ann, "Truck 5").await;
    let mut b = Client::connect(addr, &bob, "").await;
    b.send(json!({"t": "join", "channel": crew})).await;
    b.wait("joined").await;
    let mut h = handset(addr, &cred, "").await;
    let token = h.hello["resume"].as_str().unwrap().to_owned();
    h.send(json!({"t": "join", "channel": crew})).await;
    h.wait("joined").await;

    // The link goes (no close): the session waits, shown "reconnecting", still a member.
    drop(h);
    loop {
        let p = b.wait("presence").await;
        if p["handsets"][0]["state"] == "reconnecting" {
            break;
        }
    }
    // Bob talks meanwhile: kept for the handset.
    b.send(json!({"t": "talk", "on": true, "channel": crew}))
        .await;
    while b.wait("talking").await["on"] != true {}
    for i in 0..3u8 {
        b.talk_frame(&[0xF8, i]).await;
    }
    b.send(json!({"t": "talk", "on": false})).await;
    b.send(json!({"t": "ping"})).await;
    b.wait("pong").await;

    // Back with its token, saying it got bob's first packet (seq 1) before the link went:
    // the same session, no join, the two it didn't get first.
    let have = format!("?resume={token}&have={}:1", b.sid);
    let mut h = handset(addr, &cred, &have).await;
    assert_eq!(h.hello["resumed"], true);
    assert_eq!(h.hello["backlog"], 2);
    assert_eq!(h.hello["joined"][0]["channel"].as_u64(), Some(crew));
    h.frames(2).await;
    assert_eq!(&h.frames[0][13..], &[0xF8, 1]);
    assert_eq!(&h.frames[1][13..], &[0xF8, 2]);
    let fresh = h.hello["resume"].as_str().unwrap().to_owned();
    assert_ne!(fresh, token);

    // Its answer lost too (the link went again): the token it came with still works,
    // until the new one has been used; the connection it left is let go.
    let mut h3 = handset(addr, &cred, &format!("?resume={token}")).await;
    assert_eq!(h3.hello["resumed"], true);
    assert_eq!(h.wait("bye").await["why"], "resumed on another connection");

    // A wrong token: a fresh session that takes the other over, its channels joined.
    let mut h2 = handset(addr, &cred, "?resume=nope").await;
    assert_eq!(h2.hello["resumed"], false);
    assert_eq!(h2.wait("rejoined").await["channels"], json!([crew]));
    h3.wait("bye").await;
}

/// Bug run: a driver's name was who they were, so a driver named as a site user got that
/// user's Auth channels, direct messages and calls. Now a handset is its key
/// (`driver:ID`), whatever its name; two drivers of one name are two people.
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_named_as_a_site_user_is_not_them() {
    let (addr, v, _told) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let ops = make_channel(
        addr,
        &ann,
        json!({"name": "Ops", "kind": "walkie", "access": "auth", "allow": [{"user": "bob"}]}),
    )
    .await;
    let (_, c2) = provision(addr, &ann, "Truck 2").await;
    let (_, c3) = provision(addr, &ann, "Truck 3").await;
    for (truck, id) in [("Truck 2", "D7"), ("Truck 3", "D8")] {
        let d = json!({"id": id, "name": "bob"});
        v.hub
            .assign_driver(truck, serde_json::from_value(d).ok(), "test")
            .unwrap();
    }
    let mut t2 = handset(addr, &c2, "").await;
    let mut t3 = handset(addr, &c3, "").await;
    assert_eq!(t2.hello["me"]["name"], "bob");
    assert_eq!(t2.hello["me"]["key"], "driver:D7");
    // Not on bob's Auth list.
    t2.send(json!({"t": "join", "channel": ops})).await;
    assert_eq!(
        t2.wait("error").await["error"],
        "not allowed in this channel"
    );
    // Ann's message to bob is bob's alone.
    let mut a = Client::connect(addr, &ann, "").await;
    let mut b = Client::connect(addr, &bob, "").await;
    a.send(json!({"t": "message", "user": "bob", "text": "for bob"}))
        .await;
    assert_eq!(b.wait("message").await["message"]["text"], "for bob");
    t2.send(json!({"t": "history", "user": "ann"})).await;
    assert!(
        t2.wait("history").await["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    t2.send(json!({"t": "ping"})).await;
    t2.wait("pong").await;
    assert!(t2.held.iter().all(|m| m["t"] != "message"));
    // A call to one driver rings that driver alone; bob isn't rung.
    a.send(json!({"t": "call", "user": "driver:D8"})).await;
    let ring = t3.wait("ring").await;
    assert_eq!(ring["from"], "ann");
    t2.send(json!({"t": "ping"})).await;
    t2.wait("pong").await;
    b.send(json!({"t": "ping"})).await;
    b.wait("pong").await;
    assert!(t2.held.iter().all(|m| m["t"] != "ring"));
    assert!(b.held.iter().all(|m| m["t"] != "ring"));
    // ... who answers it, and writes back by key.
    t3.send(json!({"t": "answer", "call": ring["call"], "accept": true}))
        .await;
    loop {
        if a.wait("call").await["state"] == "answered" {
            break;
        }
    }
    t3.send(json!({"t": "message", "user": "ann", "text": "on my way"}))
        .await;
    // (Ann's own "for bob" comes back to her first.)
    let m = loop {
        let m = a.wait("message").await;
        if m["message"]["text"] == "on my way" {
            break m;
        }
    };
    assert_eq!(m["message"]["from"], "bob");
    assert_eq!(m["message"]["from_id"], "driver:D8");
    // Presence: bob once (the site user), the drivers by key.
    let p = v.hub.status();
    assert_eq!(p["users"], json!(["ann", "bob"]));
    let keys: Vec<&str> = p["handsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"driver:D7") && keys.contains(&"driver:D8"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_talk_that_goes_quiet_ends_and_frees_the_floor() {
    let (addr, _v, _told) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    // Settings changed one field at a time; the others kept; ranges checked.
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/settings",
        &[("Cookie", &ann)],
        &json!({"talk_silence_s": 1, "talk_first_s": 1}),
    )
    .await;
    assert_eq!(st, 200);
    let (st, _) = call(
        addr,
        "POST",
        "/voice/api/settings",
        &[("Cookie", &ann)],
        &json!({"store_forward_s": 301}),
    )
    .await;
    assert_eq!(st, 400);
    let (_, s) = call(
        addr,
        "GET",
        "/voice/api/settings",
        &[("Cookie", &ann)],
        &json!({}),
    )
    .await;
    assert_eq!(s["settings"]["keep_days"], 30);
    assert_eq!(s["settings"]["talk_silence_s"], 1);
    assert_eq!(s["settings"]["store_forward_s"], 30);
    // A setting that isn't one: refused by name, not taken in silence.
    let (st, e) = call(
        addr,
        "POST",
        "/voice/api/settings",
        &[("Cookie", &ann)],
        &json!({"talk_silense_s": 1}),
    )
    .await;
    assert_eq!(st, 400);
    assert!(e["error"].as_str().unwrap().contains("talk_silense_s"));

    let gate = make_channel(addr, &ann, json!({"name": "Gate", "kind": "walkie", "half_duplex": true, "limit_s": 0, "access": "open"})).await;
    let mut b = Client::connect(addr, &bob, "").await;
    b.send(json!({"t": "join", "channel": gate})).await;
    b.wait("joined").await;
    b.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    while b.wait("talking").await["on"] != true {}
    b.talk_frame(&[0xF8, 0]).await;
    // Another member is busy while bob holds the floor...
    let cy = login(addr, "cy").await;
    let mut c = Client::connect(addr, &cy, "").await;
    c.send(json!({"t": "join", "channel": gate})).await;
    c.wait("joined").await;
    c.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    assert_eq!(c.wait("busy").await["by"], "bob");
    // ... nothing more from bob: his talk ends within a couple of seconds,
    loop {
        let m = b.wait("talking").await;
        if m["on"] == false {
            assert_eq!(m["why"], "no sound");
            break;
        }
    }
    // ... and the floor is free: cy talks.
    c.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    while c.wait("talking").await["on"] != true {}
}
