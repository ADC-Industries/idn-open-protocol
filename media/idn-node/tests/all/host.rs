//! The Node hosted in another program (`idn_node::host::start`; IDN Media as a server and
//! libraries, the UI in the host): it starts in this process, IDN's clients reach it on its
//! endpoint, its panel answers, and it stops.

use crate::common;
use common::free_port;

use idn_node::host::{Config, Place, start};
use idn_open_protocol::node::Node;
use std::time::Duration;

#[test]
fn a_node_hosted_in_this_process_starts_answers_and_stops() {
    // SAFETY: before any thread of this test program reads the environment (this test is
    // the file's only one). No PTP on the host network from tests.
    unsafe { std::env::set_var("IDN_CLOCK", "off") };
    let state = idn_common::testing::TempDir::new("idn-node-host");
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    let endpoint = format!("{dir}/inode-{}-host.sock", std::process::id());
    let config = Config {
        place: Place {
            state: state.to_path_buf(),
            endpoint: endpoint.clone(),
            system: false,
        },
        programs_dir: None,
        panel_port: Some(free_port()),
        lan_panel_port: None,
        service: false,
        diagnostics: false,
    };
    let node = start(config.clone()).expect("the hosted Node starts");
    assert_eq!(node.endpoint(), endpoint);
    // A client, as every IDN program is one.
    let client = Node::connect_to(&endpoint, "host-test").expect("its endpoint answers");
    assert!(client.status().is_ok());
    drop(client);
    // Its panel.
    assert!(
        std::net::TcpStream::connect_timeout(
            &([127, 0, 0, 1], node.panel_port()).into(),
            Duration::from_secs(2)
        )
        .is_ok()
    );
    // The push channel: a panel session from a one-time link, its status at once.
    let client = Node::connect_to(&endpoint, "host-test").unwrap();
    let url = client
        .request(idn_open_protocol::xml::Element::new("panel_token"))
        .unwrap()
        .attr("url")
        .expect("a panel link")
        .to_owned();
    drop(client);
    let path = url
        .split_once("127.0.0.1")
        .map(|x| x.1)
        .unwrap_or("")
        .trim_start_matches(|c: char| c == ':' || c.is_ascii_digit());
    let (code, page) = http_get(node.panel_port(), path);
    assert_eq!(code, 200, "{page}");
    let sid = page
        .split("','")
        .nth(1)
        .and_then(|x| x.split('\'').next())
        .expect("the tab's session")
        .to_owned();
    assert!(
        tungstenite::connect(format!(
            "ws://127.0.0.1:{}/api/v1/events",
            node.panel_port()
        ))
        .is_err(),
        "no session: refused"
    );
    let ws_url = format!(
        "ws://127.0.0.1:{}/api/v1/events?session={sid}",
        node.panel_port()
    );
    let (mut ws, _) = tungstenite::connect(&ws_url).expect("it upgrades");
    let end = std::time::Instant::now() + Duration::from_secs(5);
    let mut status = None;
    while status.is_none() && std::time::Instant::now() < end {
        if let Ok(tungstenite::Message::Text(t)) = ws.read() {
            let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
            if v["type"] == "state" {
                status = Some(v);
            }
        }
    }
    // The status as the shared front sends a state (§N.42), typed: read into its type and
    // back, nothing left out.
    let st = status.expect("its state")["state"].clone();
    assert!(st["version"].as_u64().is_some_and(|v| v > 0), "{st}");
    let typed: idn_node::panel_state::NodeStatus =
        serde_json::from_value(st.clone()).expect("the status reads as NodeStatus");
    let back = serde_json::to_value(&typed).unwrap();
    let left_out: Vec<&String> = st
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| back.get(*k).is_none())
        .collect();
    assert!(left_out.is_empty(), "not in NodeStatus: {left_out:?}");
    let _ = ws.close(None);
    // Per-control sequence numbers over HTTP: an older change than one applied to its
    // control is skipped, not applied.
    let post = |seq: &str| {
        http_post(
            node.panel_port(),
            "/api/v1/settings",
            &[
                ("X-IDN-Session", sid.as_str()),
                ("X-IDN-Node", "1"),
                ("X-IDN-Client", "host-test"),
                ("X-IDN-Control", "settings"),
                ("X-IDN-Seq", seq),
            ],
            "{}",
        )
    };
    let (code, a) = post("5");
    assert_eq!(code, 200, "{a}");
    assert!(!a.contains("skipped"), "{a}");
    let (code, b) = post("4");
    assert_eq!(code, 200, "{b}");
    assert!(b.contains(r#""skipped":true"#), "{b}");
    assert!(!post("6").1.contains("skipped"));
    // A push channel left open, as a host's UI might: the stop ends it.
    let (mut open_ws, _) = tungstenite::connect(&ws_url).expect("it upgrades");
    // The session agent (what the tray did besides its icon; a host's tray item can embed
    // it) against this Node: it connects and follows its status.
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent =
        idn_open_protocol::node::session::Agent::start(idn_open_protocol::node::session::Config {
            app: "host-test session".into(),
            endpoint: Some(endpoint.clone()),
            status: {
                let seen = seen.clone();
                std::sync::Arc::new(move |_| seen.store(true, std::sync::atomic::Ordering::Relaxed))
            },
            ..Default::default()
        })
        .unwrap();
    assert!(idn_common::testing::wait_until(
        Duration::from_secs(5),
        || seen.load(std::sync::atomic::Ordering::Relaxed)
    ));
    agent.stop();
    let port = node.panel_port();
    node.stop().expect("it stops cleanly");
    assert!(Node::connect_to(&endpoint, "host-test").is_err());
    // Stopped is stopped: its panel's port free at once, the channel left open ended.
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "the panel's port is free once stop() returns"
    );
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = open_ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
    }
    let end = std::time::Instant::now() + Duration::from_secs(3);
    let ended = loop {
        match open_ws.read() {
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break true,
        }
        if std::time::Instant::now() >= end {
            break false;
        }
    };
    assert!(ended, "the push channel ended with the Node");
    // Started again on the same ports, in the same process.
    let again = start(config).expect("it starts again on the same ports");
    assert_eq!(again.panel_port(), port);
    assert!(Node::connect_to(&endpoint, "host-test").is_ok());
    again.stop().expect("it stops again");
}

/// A POST on the panel's port with `headers`: (status code, body).
fn http_post(port: u16, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut a = String::new();
    let _ = s.read_to_string(&mut a);
    let code = a
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (
        code,
        a.split_once("\r\n\r\n")
            .map_or(String::new(), |x| x.1.to_owned()),
    )
}

/// A GET on the panel's port: (status code, body).
fn http_get(port: u16, path: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut a = String::new();
    let _ = s.read_to_string(&mut a);
    let code = a
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (
        code,
        a.split_once("\r\n\r\n")
            .map_or(String::new(), |x| x.1.to_owned()),
    )
}
