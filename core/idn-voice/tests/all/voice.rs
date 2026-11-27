//! IDN Voice end to end over its WebSocket (the fallback path): channels made by an
//! administrator, talking and forwarding, half duplex (busy, priority, the time limit),
//! a password channel, calls and instant talk, messages, recordings, missed and replay.

use futures_util::{SinkExt, StreamExt};
use idn_voice::{Host, User, Voice, VoiceConfig};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct TestHost;

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
            name: "dan".into(),
            roles: vec!["driver".into()],
            admin: false,
        },
    ]
}

impl Host for TestHost {
    fn session_user(&self, _: &axum::http::HeaderMap) -> Option<User> {
        None
    }
    fn login(&self, name: &str, password: &str, _: std::net::IpAddr) -> Option<User> {
        users()
            .into_iter()
            .find(|u| u.name == name && password == format!("pw-{name}"))
    }
    fn users(&self) -> Vec<User> {
        users()
    }
    fn audit(&self, _: &str, _: &str, _: Value) {}
}

async fn server() -> (SocketAddr, Voice) {
    let dir = idn_common::testing::TempDir::new("voice-test");
    let mut cfg = VoiceConfig::new(dir.to_path_buf());
    cfg.udp = None;
    let v = Voice::start(cfg, Arc::new(TestHost)).unwrap();
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
    (addr, v)
}

/// A plain HTTP/1.1 request: (status, Set-Cookie value, body).
pub(crate) async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &Value,
) -> (u16, String, Value) {
    let b = body.to_string();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-IDN-Voice: 1\r\nCookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
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
    let cookie = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("set-cookie: ")
                .or_else(|| l.strip_prefix("Set-Cookie: "))
        })
        .map(|c| c.split(';').next().unwrap_or("").to_owned())
        .unwrap_or_default();
    (
        status,
        cookie,
        serde_json::from_str(body.trim()).unwrap_or(Value::Null),
    )
}

pub(crate) async fn login(addr: SocketAddr, name: &str) -> String {
    let (st, cookie, _) = http(
        addr,
        "POST",
        "/voice/api/login",
        "",
        &json!({"user": name, "password": format!("pw-{name}")}),
    )
    .await;
    assert_eq!(st, 200, "login {name}");
    cookie
}

pub(crate) type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) struct Client {
    pub(crate) ws: Ws,
    pub(crate) sid: u64,
    /// Messages read while waiting for another.
    pub(crate) held: Vec<Value>,
    pub(crate) frames: Vec<Vec<u8>>,
    /// Its `hello`.
    pub(crate) hello: Value,
}

impl Client {
    pub(crate) async fn connect(addr: SocketAddr, cookie: &str, query: &str) -> Client {
        let mut req = format!("ws://{addr}/voice/ws{query}")
            .into_client_request()
            .unwrap();
        if !cookie.is_empty() {
            req.headers_mut().insert("Cookie", cookie.parse().unwrap());
        }
        Client::open(req).await
    }

    /// From a request made ready (a handset's Bearer credential, §V.13).
    pub(crate) async fn open(
        req: tokio_tungstenite::tungstenite::handshake::client::Request,
    ) -> Client {
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let mut c = Client {
            ws,
            sid: 0,
            held: vec![],
            frames: vec![],
            hello: Value::Null,
        };
        let h = c.wait("hello").await;
        c.sid = h["sid"].as_u64().unwrap();
        c.hello = h;
        c
    }

    pub(crate) async fn send(&mut self, v: Value) {
        self.ws
            .send(Message::Text(v.to_string().into()))
            .await
            .unwrap();
    }

    pub(crate) async fn talk_frame(&mut self, data: &[u8]) {
        let mut b = vec![1u8];
        b.extend_from_slice(data);
        self.ws.send(Message::Binary(b.into())).await.unwrap();
    }

    pub(crate) async fn next(&mut self, secs: u64) -> Option<Value> {
        loop {
            match tokio::time::timeout(Duration::from_secs(secs), self.ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).ok(),
                Ok(Some(Ok(Message::Binary(b)))) => self.frames.push(b.to_vec()),
                Ok(Some(Ok(_))) => {}
                _ => return None,
            }
        }
    }

    /// The next message of type `t` (others kept).
    pub(crate) async fn wait(&mut self, t: &str) -> Value {
        if let Some(i) = self.held.iter().position(|m| m["t"] == t) {
            return self.held.remove(i);
        }
        loop {
            let m = self.next(5).await.unwrap_or_else(|| panic!("no {t}"));
            if m["t"] == t {
                return m;
            }
            self.held.push(m);
        }
    }

    /// Waits until `n` binary frames have come.
    pub(crate) async fn frames(&mut self, n: usize) {
        let end = tokio::time::Instant::now() + Duration::from_secs(5);
        while self.frames.len() < n && tokio::time::Instant::now() < end {
            if let Some(m) = self.next(1).await {
                self.held.push(m);
            }
        }
        assert!(
            self.frames.len() >= n,
            "{} of {n} frames",
            self.frames.len()
        );
    }
}

pub(crate) async fn make_channel(addr: SocketAddr, admin: &str, body: Value) -> u64 {
    let (st, _, v) = http(addr, "POST", "/voice/api/channels", admin, &body).await;
    assert_eq!(st, 200, "{v}");
    v["id"].as_u64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn channels_talk_calls_messages_and_recordings() {
    let (addr, _v) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let dan = login(addr, "dan").await;
    // Only administrators make channels.
    let (st, _, _) = http(
        addr,
        "POST",
        "/voice/api/channels",
        &bob,
        &json!({"name": "X", "kind": "walkie", "access": "open"}),
    )
    .await;
    assert_eq!(st, 403);
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let gate = make_channel(addr, &ann, json!({"name": "Gate", "kind": "walkie", "half_duplex": true, "limit_s": 2, "access": "open"})).await;
    let locked = make_channel(
        addr,
        &ann,
        json!({"name": "Locked", "kind": "walkie", "access": "password", "password": "abc"}),
    )
    .await;
    let band = make_channel(
        addr,
        &ann,
        json!({"name": "Band", "kind": "iem", "source": "PC (Aux 1)", "access": "guest"}),
    )
    .await;

    let mut a = Client::connect(addr, &ann, "").await;
    let mut b = Client::connect(addr, &bob, "").await;
    let mut d = Client::connect(addr, &dan, "").await;

    // Talking in a full-duplex channel reaches the other members, unchanged.
    for c in [&mut b, &mut d] {
        c.send(json!({"t": "join", "channel": crew})).await;
        c.wait("joined").await;
    }
    b.send(json!({"t": "talk", "on": true, "channel": crew}))
        .await;
    while b.wait("talking").await["on"] != true {}
    for i in 0..3u8 {
        b.talk_frame(&[0xF8, i, i]).await;
    }
    d.frames(3).await;
    let f = &d.frames[0];
    assert_eq!(f[0], 1);
    assert_eq!(
        u32::from_le_bytes(f[1..5].try_into().unwrap()) as u64,
        b.sid
    );
    assert_eq!(u32::from_le_bytes(f[5..9].try_into().unwrap()) as u64, crew);
    assert_eq!(&f[13..], &[0xF8, 0, 0]);
    b.send(json!({"t": "talk", "on": false})).await;
    // Ann isn't in Crew: nothing reached her.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = a.next(1).await;
    assert!(a.frames.is_empty());

    // Half duplex: busy for another, taken by priority (ann is an administrator).
    for c in [&mut a, &mut b, &mut d] {
        c.send(json!({"t": "join", "channel": gate})).await;
        c.wait("joined").await;
    }
    b.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    while b.wait("talking").await["on"] != true {}
    d.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    assert_eq!(d.wait("busy").await["by"], "bob");
    a.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    assert_eq!(b.wait("taken").await["by"], "ann");
    while a.wait("talking").await["on"] != true {}
    a.send(json!({"t": "talk", "on": false})).await;
    while a.wait("talking").await["on"] != false {}
    // The time limit (2 s).
    d.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    loop {
        let m = d.wait("talking").await;
        if m["on"] == false {
            assert_eq!(m["why"], "time limit");
            break;
        }
    }

    // A password channel.
    let mut g = Client::connect(addr, "", "?guest=Visitor").await;
    g.send(json!({"t": "join", "channel": locked, "password": "nope"}))
        .await;
    assert_eq!(g.wait("error").await["error"], "wrong password");
    g.send(json!({"t": "join", "channel": locked, "password": "abc"}))
        .await;
    g.wait("joined").await;
    // Guests may enter a guest IEM channel, listen-only.
    g.send(json!({"t": "join", "channel": band})).await;
    assert_eq!(g.wait("joined").await["may_talk"], false);
    // ... but not an Open one.
    g.send(json!({"t": "join", "channel": crew})).await;
    g.wait("error").await;

    // Calls: ring, answer, talk into the call.
    b.send(json!({"t": "call", "user": "dan"})).await;
    let ring = d.wait("ring").await;
    assert_eq!(ring["from"], "bob");
    d.send(json!({"t": "answer", "call": ring["call"], "accept": true}))
        .await;
    loop {
        if b.wait("call").await["state"] == "answered" {
            break;
        }
    }
    let before = d.frames.len();
    b.send(json!({"t": "talk", "on": true, "call": ring["call"]}))
        .await;
    while b.wait("talking").await["on"] != true {}
    b.talk_frame(&[1, 2, 3]).await;
    d.frames(before + 1).await;
    assert_eq!(
        u32::from_le_bytes(d.frames[before][5..9].try_into().unwrap()),
        0
    );
    b.send(json!({"t": "talk", "on": false})).await;
    b.send(json!({"t": "hangup", "call": ring["call"]})).await;
    // Instant talk refused when the other takes calls only.
    d.send(json!({"t": "settings", "settings": {"instant_from": "never"}}))
        .await;
    d.wait("settings").await;
    b.send(json!({"t": "talk", "on": true, "user": "dan"}))
        .await;
    assert!(
        b.wait("error").await["error"]
            .as_str()
            .unwrap()
            .contains("calls only")
    );

    // Messages and their history.
    b.send(json!({"t": "message", "channel": crew, "text": "truck 7 at the gate"}))
        .await;
    assert_eq!(
        d.wait("message").await["message"]["text"],
        "truck 7 at the gate"
    );
    d.send(json!({"t": "history", "channel": crew})).await;
    assert_eq!(
        d.wait("history").await["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Recordings: bob's Crew talk was recorded; dan was missing nothing of his own.
    d.send(json!({"t": "missed", "since_ms": 0})).await;
    let missed = d.wait("missed").await;
    let list = missed["transmissions"].as_array().unwrap();
    let crew_talk = list
        .iter()
        .find(|t| t["channel"] == crew && t["from"] == "bob")
        .expect("bob's talk in Crew recorded");
    assert_eq!(crew_talk["frames"], 3);
    let before = d.frames.len();
    d.send(json!({"t": "replay", "id": crew_talk["id"]})).await;
    d.wait("replay_end").await;
    d.frames(before + 3).await;
    assert_eq!(d.frames[before][0], 2);
    assert_eq!(&d.frames[before][13..], &[0xF8, 0, 0]);
    // A guest can't hear a recording of a channel it isn't in.
    g.send(json!({"t": "replay", "id": crew_talk["id"]})).await;
    g.wait("error").await;
}

/// Another site's page can't change anything (no X-IDN-Voice header) nor open a signed-in
/// socket (its Origin isn't this server).
#[tokio::test(flavor = "multi_thread")]
async fn other_sites_pages_are_refused() {
    let (addr, _v) = server().await;
    let ann = login(addr, "ann").await;
    // No header: refused.
    let b = json!({"name": "X", "kind": "walkie", "access": "open"}).to_string();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /voice/api/channels HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nCookie: {ann}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 403"));
    // A signed-in socket from another origin: refused.
    let mut req = format!("ws://{addr}/voice/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("Cookie", ann.parse().unwrap());
    req.headers_mut()
        .insert("Origin", "https://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    // From its own origin: fine.
    let mut req = format!("ws://{addr}/voice/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("Cookie", ann.parse().unwrap());
    req.headers_mut()
        .insert("Origin", format!("http://{addr}").parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_ok());
}

/// A command on the site's MQTT (a cue's "Voice message", an operations system) posts into a channel.
#[tokio::test(flavor = "multi_thread")]
async fn an_mqtt_command_posts_a_message() {
    let (addr, v) = server().await;
    let ann = login(addr, "ann").await;
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let mut a = Client::connect(addr, &ann, "").await;
    a.send(json!({"t": "join", "channel": crew})).await;
    a.wait("joined").await;
    v.hub.mqtt_command(
        "idn/site/voice/cmd/message",
        br#"{"channel": "crew", "text": "Doors open", "from": "cue"}"#,
    );
    let m = a.wait("message").await;
    assert_eq!(m["message"]["text"], "Doors open");
    assert_eq!(m["message"]["from"], "cue");
}

/// Review 2026-11-07: a password set on an open channel takes out who didn't give it, and
/// a channel's floor held by an all-call is let go when the dispatcher leaves it. An
/// empty posted message is refused.
#[tokio::test(flavor = "multi_thread")]
async fn access_changes_and_floors_held_by_an_all_call() {
    let (addr, v) = server().await;
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let gate = make_channel(addr, &ann, json!({"name": "Gate", "kind": "walkie", "half_duplex": true, "limit_s": 0, "access": "open"})).await;
    let mut a = Client::connect(addr, &ann, "").await;
    let mut b = Client::connect(addr, &bob, "").await;
    for ch in [crew, gate] {
        for c in [&mut a, &mut b] {
            c.send(json!({"t": "join", "channel": ch})).await;
            c.wait("joined").await;
        }
    }
    // A password set on Crew: bob (no password given) is taken out; ann (administrator)
    // stays.
    let (st, _, _) = http(addr, "POST", "/voice/api/channels", &ann, &json!({"id": crew, "name": "Crew", "kind": "walkie", "access": "password", "password": "new"})).await;
    assert_eq!(st, 200);
    let left = b.wait("left").await;
    assert_eq!(left["channel"].as_u64(), Some(crew));
    assert_eq!(left["why"], "access changed");
    // Ann's all-call holds Gate's floor; she leaves Gate while still talking: bob may
    // talk there at once.
    a.send(json!({"t": "talk", "on": true, "all": true})).await;
    while a.wait("talking").await["on"] != true {}
    b.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    assert_eq!(b.wait("busy").await["by"], "ann");
    a.send(json!({"t": "leave", "channel": gate})).await;
    a.wait("left").await;
    b.send(json!({"t": "talk", "on": true, "channel": gate}))
        .await;
    while b.wait("talking").await["on"] != true {}
    // Posts are messages: 1 to 2,000 characters.
    assert!(v.hub.post(crew as u32, "cue", "  ").is_err());
}
