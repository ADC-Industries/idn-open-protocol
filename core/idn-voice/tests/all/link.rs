//! Voice over IDN's link (idn-voice.md §V.14) end to end: a handset's QUIC connection to
//! Voice's link listener, the certificate pinned from its provisioning answer; signing in
//! with its credential, hello, a channel joined, sound both ways; a wrong credential, a
//! first frame that isn't `auth`, a frame too long and the site's `link` off refused (and
//! its open sessions ended); a dropped connection resumed over QUIC with `have`; a
//! WebSocket session moved to QUIC; the site's program kept signed in; two listeners'
//! offers.

use crate::handsets::{API_KEY, call, handset, server};
use crate::voice::{Client, login, make_channel};
use idn_open_protocol::link::LinkConfig;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Accepts the server's certificate by its SHA-256 alone, as a handset does (§V.13.2).
#[derive(Debug)]
struct Pin {
    sha256: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if idn_open_protocol::hash::sha256_hex(end_entity.as_ref()) == self.sha256 {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the site's certificate changed".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.3 only".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A QUIC connection to `at`, its certificate pinned (`AA:BB:…`).
async fn dial(at: SocketAddr, pin: &str) -> (quinn::Endpoint, quinn::Connection) {
    let provider = idn_open_protocol::secure::crypto_provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin {
            sha256: pin.replace(':', "").to_ascii_lowercase(),
            provider,
        }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![idn_voice::link::ALPN.as_bytes().to_vec()];
    let cfg = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    ));
    let ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let conn = ep
        .connect_with(cfg, at, "idn-voice")
        .unwrap()
        .await
        .unwrap();
    (ep, conn)
}

/// One frame: its length, its kind, the payload.
fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = (payload.len() as u32 + 1).to_le_bytes().to_vec();
    f.push(kind);
    f.extend_from_slice(payload);
    f
}

/// A handset's app on the link: the voice tests' `Client`, over QUIC.
struct LinkClient {
    _ep: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    /// Frames read (kind, payload), by a task of its own (a wait that times out loses
    /// nothing).
    rx: tokio::sync::mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
    sid: u64,
    held: Vec<Value>,
    frames: Vec<Vec<u8>>,
    hello: Value,
}

impl LinkClient {
    /// Connects and signs in with `auth`: the client, or the error frame it was answered.
    async fn open(at: SocketAddr, pin: &str, auth: Value) -> Result<LinkClient, Value> {
        let (ep, conn) = dial(at, pin).await;
        let (send, mut recv) = conn.open_bi().await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let mut len = [0u8; 4];
                if recv.read_exact(&mut len).await.is_err() {
                    return;
                }
                let mut b = vec![0u8; u32::from_le_bytes(len) as usize];
                if recv.read_exact(&mut b).await.is_err() {
                    return;
                }
                let kind = b.remove(0);
                if tx.send((kind, b)).is_err() {
                    return;
                }
            }
        });
        let mut c = LinkClient {
            _ep: ep,
            conn,
            send,
            rx,
            sid: 0,
            held: vec![],
            frames: vec![],
            hello: Value::Null,
        };
        c.send(auth).await;
        let first = c.next(5).await.expect("an answer to auth");
        if first["t"] != "hello" {
            // (Closed at once: the server waits for that before its own close.)
            c.conn.close(0u32.into(), b"refused");
            return Err(first);
        }
        c.sid = first["sid"].as_u64().unwrap();
        c.hello = first;
        Ok(c)
    }

    async fn send(&mut self, v: Value) {
        let f = frame(0, v.to_string().as_bytes());
        self.send.write_all(&f).await.unwrap();
    }

    async fn talk_frame(&mut self, data: &[u8]) {
        let mut b = vec![1u8];
        b.extend_from_slice(data);
        self.send.write_all(&frame(1, &b)).await.unwrap();
    }

    async fn next(&mut self, secs: u64) -> Option<Value> {
        loop {
            match tokio::time::timeout(Duration::from_secs(secs), self.rx.recv()).await {
                Ok(Some((0, p))) => return serde_json::from_slice(&p).ok(),
                Ok(Some((1, p))) => self.frames.push(p),
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    /// The next message of type `t` (others kept).
    async fn wait(&mut self, t: &str) -> Value {
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

    /// Waits until `n` sound frames have come.
    async fn frames(&mut self, n: usize) {
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

/// Voice's link listener on loopback, with a key of the test's own.
fn serve_link(v: &idn_voice::Voice) -> idn_voice::VoiceLink {
    v.serve_link(
        "127.0.0.1:0".parse().unwrap(),
        &LinkConfig::ephemeral(),
        &tokio::runtime::Handle::current(),
    )
    .unwrap()
}

/// A handset provisioned by the site's program: its credential, where the link is and its
/// certificate, as the answer gives them.
async fn provision(addr: SocketAddr, admin: &str, name: &str) -> (String, SocketAddr, String) {
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
        r["credential"].as_str().unwrap().to_owned(),
        r["link"][0].as_str().unwrap().parse().unwrap(),
        r["link_cert_sha256"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handset_signs_in_and_talks_over_the_link() {
    let (addr, v, _told) = server().await;
    let link = serve_link(&v);
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let (cred, at, pin) = provision(addr, &ann, "Truck 7").await;
    // The answer names the link at the address the program came by, and its certificate.
    assert_eq!(at.port(), link.port());
    assert_eq!(pin, link.cert_sha256());

    let mut b = Client::connect(addr, &bob, "").await;
    assert_eq!(b.hello["link"]["quic"]["port"], link.port());
    assert_eq!(b.hello["link"]["link_connect_ms"], 3000);
    b.send(json!({"t": "join", "channel": crew})).await;
    b.wait("joined").await;

    let mut h = LinkClient::open(at, &pin, json!({"t": "auth", "credential": cred}))
        .await
        .unwrap();
    assert_eq!(h.hello["me"]["handset"]["name"], "Truck 7");
    assert!(h.hello["resume"].is_string());
    assert_eq!(h.hello["link"]["quic"]["cert_sha256"], pin.as_str());
    h.send(json!({"t": "join", "channel": crew})).await;
    h.wait("joined").await;

    // The handset talks: bob hears it.
    h.send(json!({"t": "talk", "on": true, "channel": crew}))
        .await;
    while h.wait("talking").await["on"] != true {}
    for i in 0..3u8 {
        h.talk_frame(&[0xF8, i]).await;
    }
    b.frames(3).await;
    assert_eq!(&b.frames[0][13..], &[0xF8, 0]);
    assert_eq!(&b.frames[2][13..], &[0xF8, 2]);
    h.send(json!({"t": "talk", "on": false})).await;
    h.send(json!({"t": "ping"})).await;
    h.wait("pong").await;

    // Bob talks: the handset hears it over the link, with the WebSocket's header.
    b.send(json!({"t": "talk", "on": true, "channel": crew}))
        .await;
    while b.wait("talking").await["on"] != true {}
    b.talk_frame(&[0xF8, 9]).await;
    b.talk_frame(&[0xF8, 10]).await;
    h.frames(2).await;
    assert_eq!(&h.frames[0][1..5], &(b.sid as u32).to_le_bytes());
    assert_eq!(&h.frames[1][13..], &[0xF8, 10]);
    b.send(json!({"t": "talk", "on": false})).await;

    // A wrong credential: an error frame, then closed.
    let e = LinkClient::open(
        at,
        &pin,
        json!({"t": "auth", "credential": "idnv1.hnope.x"}),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(e["t"], "error");
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("not an enrolled handset")
    );
    // No cookies or guests over the link.
    let e = LinkClient::open(at, &pin, json!({"t": "auth", "credential": ""}))
        .await
        .err()
        .unwrap();
    assert_eq!(e["t"], "error");
    // The first frame isn't auth.
    let e = LinkClient::open(at, &pin, json!({"t": "ping"}))
        .await
        .err()
        .unwrap();
    assert_eq!(e["error"], "the first frame is auth");

    // A frame longer than 64 KiB: a protocol error, the connection closed.
    let (_ep, conn) = dial(at, &pin).await;
    let (mut s, _r) = conn.open_bi().await.unwrap();
    s.write_all(&(64 * 1024 + 1u32).to_le_bytes())
        .await
        .unwrap();
    match tokio::time::timeout(Duration::from_secs(5), conn.closed()).await {
        Ok(quinn::ConnectionError::ApplicationClosed(c)) => {
            assert_eq!(c.error_code, quinn::VarInt::from_u32(2))
        }
        other => panic!("not closed for the frame: {other:?}"),
    }

    // A wrong pin: no connection (the handset refuses the server).
    let wrong = "00".repeat(32);
    let ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let provider = idn_open_protocol::secure::crypto_provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin {
            sha256: wrong,
            provider,
        }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![idn_voice::link::ALPN.as_bytes().to_vec()];
    let cfg = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    ));
    assert!(
        ep.connect_with(cfg, at, "idn-voice")
            .unwrap()
            .await
            .is_err()
    );

    // The site's `link` off: the session on it ended (the app resumes on the WebSocket),
    // new ones refused, and not offered.
    let mut s = v.hub.settings();
    s.link = false;
    v.hub.put_settings(s).unwrap();
    assert_eq!(h.wait("bye").await["why"], "QUIC is off on this site");
    let e = LinkClient::open(at, &pin, json!({"t": "auth", "credential": cred}))
        .await
        .err()
        .unwrap();
    assert!(e["error"].as_str().unwrap().contains("off"));
    let b2 = Client::connect(addr, &bob, "").await;
    assert!(b2.hello["link"]["quic"].is_null());
    drop(h);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_link_resumes_over_quic_with_what_it_has() {
    let (addr, v, _told) = server().await;
    let _link = serve_link(&v);
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let (cred, at, pin) = provision(addr, &ann, "Truck 8").await;
    let mut b = Client::connect(addr, &bob, "").await;
    b.send(json!({"t": "join", "channel": crew})).await;
    b.wait("joined").await;
    let mut h = LinkClient::open(at, &pin, json!({"t": "auth", "credential": cred}))
        .await
        .unwrap();
    let token = h.hello["resume"].as_str().unwrap().to_owned();
    h.send(json!({"t": "join", "channel": crew})).await;
    h.wait("joined").await;

    // The connection dies (no bye): the session waits, shown "reconnecting".
    h.conn.close(0u32.into(), b"gone");
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

    // Back over QUIC with its token, having had bob's first packet: the same session,
    // the two it didn't get first.
    let auth = json!({
        "t": "auth", "credential": cred, "resume": token, "have": format!("{}:1", b.sid),
    });
    let mut h = LinkClient::open(at, &pin, auth).await.unwrap();
    assert_eq!(h.hello["resumed"], true);
    assert_eq!(h.hello["backlog"], 2);
    assert_eq!(h.hello["joined"][0]["channel"].as_u64(), Some(crew));
    h.frames(2).await;
    assert_eq!(&h.frames[0][13..], &[0xF8, 1]);
    assert_eq!(&h.frames[1][13..], &[0xF8, 2]);
    assert_ne!(h.hello["resume"], token.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_websocket_session_moves_to_quic_with_its_token() {
    let (addr, v, _told) = server().await;
    let _link = serve_link(&v);
    let ann = login(addr, "ann").await;
    let bob = login(addr, "bob").await;
    let crew = make_channel(
        addr,
        &ann,
        json!({"name": "Crew", "kind": "walkie", "access": "open"}),
    )
    .await;
    let (cred, at, pin) = provision(addr, &ann, "Truck 9").await;
    let mut b = Client::connect(addr, &bob, "").await;
    b.send(json!({"t": "join", "channel": crew})).await;
    b.wait("joined").await;

    // On the WebSocket first (UDP blocked, say); told where QUIC is.
    let mut w = handset(addr, &cred, "").await;
    assert_eq!(w.hello["link"]["quic"]["port"], at.port());
    let token = w.hello["resume"].as_str().unwrap().to_owned();
    w.send(json!({"t": "join", "channel": crew})).await;
    w.wait("joined").await;

    // QUIC connects later: the session moves over, the WebSocket let go.
    let auth = json!({"t": "auth", "credential": cred, "resume": token});
    let mut q = LinkClient::open(at, &pin, auth).await.unwrap();
    assert_eq!(q.hello["resumed"], true);
    assert_eq!(q.hello["joined"][0]["channel"].as_u64(), Some(crew));
    assert_eq!(w.wait("bye").await["why"], "resumed on another connection");

    // Its sound comes over QUIC now.
    b.send(json!({"t": "talk", "on": true, "channel": crew}))
        .await;
    while b.wait("talking").await["on"] != true {}
    b.talk_frame(&[0xF8, 7]).await;
    q.frames(1).await;
    assert_eq!(&q.frames[0][13..], &[0xF8, 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sites_program_stays_signed_in() {
    let (addr, v, _told) = server().await;
    let link = serve_link(&v);
    let at: SocketAddr = format!("127.0.0.1:{}", link.port()).parse().unwrap();
    let pin = link.cert_sha256();
    // Its user (`api`) is no site account: sessions are checked against the site's users
    // every 5 s, and its were closed ("your account was removed").
    let mut q = LinkClient::open(at, &pin, json!({"t": "auth", "credential": API_KEY}))
        .await
        .unwrap();
    let mut w = handset(addr, API_KEY, "").await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    q.send(json!({"t": "ping"})).await;
    q.wait("pong").await;
    assert!(q.held.iter().all(|m| m["t"] != "bye"), "{:?}", q.held);
    w.send(json!({"t": "ping"})).await;
    w.wait("pong").await;
    assert!(w.held.iter().all(|m| m["t"] != "bye"), "{:?}", w.held);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listener_clears_only_its_own_offer() {
    let (_addr, v, _told) = server().await;
    let first = serve_link(&v);
    let second = serve_link(&v);
    drop(first);
    assert_eq!(v.hub.link_offer().map(|o| o.port), Some(second.port()));
    drop(second);
    assert!(v.hub.link_offer().is_none());
}
