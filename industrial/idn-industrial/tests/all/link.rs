//! A Site Manager | Industrial and a Node | Industrial in one process (§I.14, stage 1):
//! token enrolment (good, used, expired, another site's, another edition), QUIC and
//! WebSocket links, store and forward across a Site Manager restart (every item once),
//! commands to the machine's program, revocation.

use crate::common;
use common::{dir, free_tcp, free_udp, wait};

use idn_industrial::node::{IndustrialNode, NodeConfig};
use idn_industrial::server::{Config, SiteManager};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// A Site Manager on its own runtime, as a separate process would be.
struct Hub {
    rt: tokio::runtime::Runtime,
    sm: SiteManager,
    udp: u16,
}

impl Hub {
    fn start(dir: &Path, http: u16, udp: u16, quic: bool, ws: bool) -> Hub {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut urls = Vec::new();
        if quic {
            urls.push(format!("idn://127.0.0.1:{udp}"));
        }
        if ws {
            urls.push(format!("ws://127.0.0.1:{http}/idn/link"));
        }
        // Started again in the same folder right after a stop, the one before may still
        // hold the database for a moment (its tasks end with the runtime, in their own
        // time): tried again for up to 5 s ("Database already open", seen once in a
        // loaded release run, 2026-11-16).
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let sm = loop {
            match rt.block_on(SiteManager::start(Config {
                data_dir: dir.to_owned(),
                quic: quic.then(|| format!("127.0.0.1:{udp}").parse().unwrap()),
                public_urls: urls.clone(),
                mqtt: None,
            })) {
                Ok(sm) => break sm,
                Err(e)
                    if e.to_string().contains("already open")
                        && std::time::Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("the Site Manager didn't start: {e}"),
            }
        };
        let router = sm.router("test-key".into());
        rt.spawn(async move {
            let l = tokio::net::TcpListener::bind(("127.0.0.1", http))
                .await
                .unwrap();
            axum::serve(l, router).await.unwrap();
        });
        Hub { rt, sm, udp }
    }

    fn token(&self, name: Option<&str>, ttl: Duration) -> String {
        let mut tags = BTreeMap::new();
        tags.insert("plant".to_owned(), "PD".to_owned());
        self.sm
            .mint_token(name.map(str::to_owned), tags, ttl)
            .unwrap()
            .0
    }

    /// Item types stored so far, in arrival order.
    fn kinds(&self) -> Vec<String> {
        self.sm
            .inbox(0, 100_000)
            .unwrap()
            .into_iter()
            .map(|(_, _, _, it)| it.kind)
            .collect()
    }

    fn stop(self) {
        self.rt.shutdown_timeout(Duration::from_secs(2));
    }
}

fn node(d: &Path) -> IndustrialNode {
    IndustrialNode::start(NodeConfig {
        state_dir: d.join("node"),
        name: "BATCH-PC".into(),
        version: "test".into(),
    })
    .unwrap()
}

fn messages_cross(quic: bool, ws: bool, want: &str) {
    let d = dir(want);
    let hub = Hub::start(&d.join("hub"), free_tcp(), free_udp(), quic, ws);
    let n = node(&d);
    let st = n
        .enrol(&hub.token(Some("Plant 1 batch"), Duration::from_secs(60)))
        .unwrap();
    assert_eq!(st.name, "Plant 1 batch");
    assert_eq!(n.tags().get("plant").map(String::as_str), Some("PD"));
    for i in 0..200 {
        n.send("batch.ticket", format!("{{\"n\":{i}}}").into_bytes())
            .unwrap();
    }
    wait("200 messages stored", 20, || hub.kinds().len() >= 200);
    wait("the outbox empty", 10, || n.status().queued == 0);
    let st = n.status();
    assert!(st.connected, "{st:?}");
    assert_eq!(
        st.transport,
        Some(match want {
            "quic" => "quic",
            _ => "websocket",
        })
    );
    let m = hub.sm.machines().unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(
        m[0].online.as_ref().map(|o| o.transport.as_str()),
        Some(st.transport.unwrap())
    );
    assert_eq!(hub.kinds().len(), 200, "each once");
    // In the order sent.
    let order: Vec<String> = hub
        .sm
        .inbox(0, 1000)
        .unwrap()
        .into_iter()
        .map(|(_, _, _, it)| String::from_utf8(it.payload).unwrap())
        .collect();
    assert_eq!(
        order,
        (0..200)
            .map(|i| format!("{{\"n\":{i}}}"))
            .collect::<Vec<_>>()
    );
    n.stop();
    hub.stop();
}

#[test]
fn messages_cross_over_quic() {
    messages_cross(true, false, "quic");
}

#[test]
fn messages_cross_over_the_websocket() {
    messages_cross(false, true, "websocket");
}

#[test]
fn tokens_work_once_and_only_for_their_site_and_time() {
    let d = dir("tokens");
    let hub = Hub::start(&d.join("hub"), free_tcp(), free_udp(), true, true);
    let tok = hub.token(None, Duration::from_secs(60));
    let a = node(&d.join("a"));
    a.enrol(&tok).unwrap();
    // Used.
    let b = node(&d.join("b"));
    let e = b.enrol(&tok).unwrap_err().to_string();
    assert!(e.contains("already used"), "{e}");
    // Expired.
    let old = hub.token(None, Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(20));
    let e = b.enrol(&old).unwrap_err().to_string();
    assert!(e.contains("expired"), "{e}");
    // Another site's token pointed at this Site Manager: the CA doesn't match.
    let other = Hub::start(&d.join("other"), free_tcp(), free_udp(), true, false);
    let mut t =
        idn_industrial::token::Token::decode(&other.token(None, Duration::from_secs(60))).unwrap();
    t.urls = vec![format!("idn://127.0.0.1:{}", hub.udp)];
    let e = b.enrol(&t.encode()).unwrap_err().to_string();
    assert!(
        e.contains("another CA") || e.contains("not the site"),
        "{e}"
    );
    // Withdrawn.
    let w = hub.token(None, Duration::from_secs(60));
    let id = idn_industrial::token::Token::decode(&w).unwrap().id_hex();
    assert!(hub.sm.revoke_token(&id).unwrap());
    let e = b.enrol(&w).unwrap_err().to_string();
    assert!(e.contains("unknown token"), "{e}");
    a.stop();
    b.stop();
    other.stop();
    hub.stop();
}

#[test]
fn nothing_is_lost_or_doubled_across_a_site_manager_restart() {
    let d = dir("restart");
    let (http, udp) = (free_tcp(), free_udp());
    let hub = Hub::start(&d.join("hub"), http, udp, true, true);
    let n = node(&d);
    n.enrol(&hub.token(None, Duration::from_secs(60))).unwrap();
    for i in 0..50 {
        n.send("a.before", vec![i]).unwrap();
    }
    wait("the first 50", 20, || hub.kinds().len() == 50);
    hub.stop();
    // Offline: kept on disk.
    for i in 0..50 {
        n.send("a.during", vec![i]).unwrap();
    }
    assert!(n.status().queued >= 50);
    let hub = Hub::start(&d.join("hub"), http, udp, true, true);
    wait("the other 50", 40, || hub.kinds().len() >= 100);
    std::thread::sleep(Duration::from_millis(300));
    let k = hub.kinds();
    assert_eq!(k.len(), 100, "each once");
    assert_eq!(k.iter().filter(|k| *k == "a.during").count(), 50);
    n.stop();
    hub.stop();
}

#[test]
fn commands_reach_the_program_and_revocation_cuts_the_machine_off() {
    let d = dir("commands");
    let hub = Hub::start(&d.join("hub"), free_tcp(), free_udp(), true, true);
    let n = node(&d);
    n.enrol(&hub.token(None, Duration::from_secs(60))).unwrap();
    let mut cmds = n.commands().unwrap();
    // The machine's program: echoes in capitals.
    idn_industrial::node::runtime().spawn(async move {
        while let Some(c) = cmds.recv().await {
            let p = c.payload.to_ascii_uppercase();
            c.answer(true, p);
        }
    });
    wait("online", 20, || n.status().connected);
    let fp = hub.sm.machines().unwrap()[0].fingerprint.clone();
    let (st, p) = hub
        .rt
        .block_on(hub.sm.command(
            &fp,
            "plant.scan",
            b"customers".to_vec(),
            Duration::from_secs(5),
        ))
        .unwrap();
    assert_eq!(
        (st, p.as_slice()),
        (idn_industrial::wire::ST_OK, b"CUSTOMERS".as_slice())
    );
    hub.sm.revoke(&fp).unwrap();
    wait("disconnected", 10, || !n.status().connected);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!n.status().connected, "a revoked machine stays out");
    assert!(hub.sm.machines().unwrap()[0].revoked);
    n.stop();
    hub.stop();
}

#[test]
fn a_media_node_cant_join_an_industrial_site() {
    use idn_industrial::{node, tls, token::Token, wire};
    let d = dir("edition");
    let hub = Hub::start(&d.join("hub"), free_tcp(), free_udp(), true, false);
    let t = Token::decode(&hub.token(None, Duration::from_secs(60))).unwrap();
    let (st, why) = node::runtime().block_on(async {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let link = node::connect_any(&t.urls, tls::join_config(t.ca_fingerprint, seen).unwrap())
            .await
            .unwrap();
        let key = idn_open_protocol::trust::pki::KeyFile::generate().unwrap();
        let body = serde_json::json!({
            "token": t.id_hex(), "name": "CHURCH", "edition": "media",
            "key": base64_std(&key.spki()), "mac": "",
        });
        let (mut s, mut r) = link.open().await.unwrap();
        let it = wire::Item::new(
            wire::OP_JOIN,
            "idn.join",
            serde_json::to_vec(&body).unwrap(),
        );
        wire::write_item(&mut s, &it).await.unwrap();
        let a = wire::read_answer(&mut r).await.unwrap();
        link.close("done");
        a
    });
    assert_eq!(st, wire::ST_REFUSED);
    assert!(String::from_utf8_lossy(&why).contains("IDN Site Manager | Industrial"));
    // The token is still good for an industrial machine.
    assert!(hub.sm.tokens().unwrap()[0].used_by.is_none());
    hub.stop();
}

fn base64_std(b: &[u8]) -> String {
    idn_common::text::base64(b)
}

#[test]
fn a_program_sends_through_the_local_api_and_needs_the_token() {
    let d = dir("localapi");
    let hub = Hub::start(&d.join("hub"), free_tcp(), free_udp(), true, false);
    let n = node(&d);
    n.enrol(&hub.token(None, Duration::from_secs(60))).unwrap();
    let token = idn_industrial::local_api::token(&d.join("node")).unwrap();
    let port = free_tcp();
    let router = idn_industrial::local_api::router(n.clone(), token.clone());
    idn_industrial::node::runtime().spawn(async move {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        axum::serve(l, router).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    let call = |auth: &str, body: &str| -> String {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "POST /messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    assert!(call("wrong", r#"{"type":"a.b","payload":1}"#).starts_with("HTTP/1.1 401"));
    assert!(call(&token, r#"{"type":"Bad Type","payload":1}"#).starts_with("HTTP/1.1 400"));
    let ok = call(&token, r#"{"type":"batch.ticket","payload":{"ticket":7}}"#);
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    wait("stored", 20, || hub.kinds() == ["batch.ticket"]);
    n.stop();
    hub.stop();
}
