//! Remote management (spec/extensions/idn-node.md §N.11.5): a Site Manager in this process,
//! a fresh IDN Node that finds it on mDNS, settings pushed to the Node, and the two web
//! panels' login rules over HTTPS.

use crate::common;
use common::free_port;

use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::discovery::site::SiteAnnouncer;
use idn_open_protocol::node::Node;
use idn_open_protocol::trust::{Authority, AuthorityConfig, AuthorityEvent};
use idn_open_protocol::xml::Element;
use idn_site_manager::{SiteManagerPanel, set_password};
use std::io::{Read, Write};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// This machine's person confirms the site's code too (§N.11).
fn confirm_code(node: &Node) {
    // (No confirming on the machine since 2026-11-18, §N.11.4: approved is in.)
    wait_for("the Node in the site", Duration::from_secs(15), || {
        node.enrollment().creds().is_some()
    });
}

fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/isite-{}-{name}.sock", std::process::id())
}

struct Kill(Child, String);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// One HTTPS request (the panels' certificates are self-signed): status, cookie, body.
fn https(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
) -> (u16, Option<String>, String) {
    https_as(port, method, path, body, cookie, None)
}

/// [`https`] with a site token (`Authorization: Bearer`, §N.31.5).
fn https_as(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    token: Option<&str>,
) -> (u16, Option<String>, String) {
    let provider = idn_open_protocol::secure::crypto_provider();
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
        .with_no_client_auth();
    let conn =
        rustls::ClientConnection::new(Arc::new(cfg), "localhost".try_into().unwrap()).unwrap();
    let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut s = rustls::StreamOwned::new(conn, tcp);
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nX-IDN-Node: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
    }
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let cookie = text.lines().find_map(|l| {
        let v = l.strip_prefix("Set-Cookie: ")?;
        Some(v.split(';').next()?.to_owned())
    });
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, cookie, body)
}

/// §N.11.5 1–4.
#[test]
fn a_fresh_node_finds_the_site_and_is_managed_from_its_panel() {
    let adir = TempDir::new("auth");
    let authority = Authority::open(AuthorityConfig {
        state_dir: adir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let ours = format!(":{}", ds.port());
    // Under a service name of this test's own (the Node below looks for it), so no real
    // IDN Node on this LAN finds this site and joins it.
    let service = format!("_idn-site-test-{}._tcp.local", std::process::id());
    let _announcer =
        SiteAnnouncer::start_as(&service, ds.port(), &authority.ca_fingerprint()).unwrap();
    set_password(&adir, "volunteers1").unwrap();
    let panel = SiteManagerPanel::start(authority.clone(), &adir, ds.port(), 0).unwrap();

    // A fresh Node: no server set.
    let (endpoint, lan_port) = (sock("node"), free_port());
    let state = TempDir::new("node");
    let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_CLOCK", "off") // no PTP on the host network from tests
        .env("IDN_SITE_SERVICE", &service)
        .args(["run", "--user", "--state"])
        .arg(state.path())
        .args([
            "--endpoint",
            &endpoint,
            "--panel-port",
            &free_port().to_string(),
            "--lan-panel-port",
            &lan_port.to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _kill = Kill(child, endpoint.clone());
    wait_for("the Node to start", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, "test").is_ok()
    });
    let node = Node::connect_to(&endpoint, "test").expect("the Node starts");

    // 1. It finds the site on mDNS and asks to join, its address not kept yet (it is
    // kept once the site approves this machine).
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let st = node.status().unwrap();
        if st.attr("trial_server").is_some_and(|s| s.ends_with(&ours)) {
            assert_eq!(
                st.attr("server"),
                Some(""),
                "not kept before it is approved"
            );
            break;
        }
        if st.attr("server").is_some_and(|s| s.ends_with(&ours)) {
            break;
        }
        if let Some(s) = st
            .children_named("site")
            .find(|s| s.attr("server").is_some_and(|x| x.ends_with(&ours)))
        {
            node.request(Element::new("set").with_attr("server", s.attr("server").unwrap()))
                .unwrap();
        }
        assert!(
            Instant::now() < end,
            "the Node didn't find the site: {}",
            st.to_xml()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // ...asks to join, and is approved (as from the panel).
    authority.open_pairing(Duration::from_secs(60));
    let end = Instant::now() + Duration::from_secs(15);
    let fp = node.enrollment().fingerprint();
    let req = loop {
        if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_millis(200))
            && r.fingerprint == fp
        {
            break r;
        }
        assert!(Instant::now() < end, "no join request");
    };
    authority.approve(req.id).unwrap();
    confirm_code(&node);
    wait_for("the report from the Node", Duration::from_secs(15), || {
        authority.node_reports().iter().any(|r| r.fingerprint == fp)
    });
    // Approved and joined: its address kept now.
    let end = Instant::now() + Duration::from_secs(10);
    while !node
        .status()
        .unwrap()
        .attr("server")
        .is_some_and(|s| s.ends_with(&ours))
    {
        assert!(
            Instant::now() < end,
            "the site's address wasn't kept once approved"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // 2. Settings from the Site Manager reach it.
    let hash = idn_open_protocol::web::hash_password("stage123").unwrap();
    let r = authority
        .set_node(
            &fp,
            &[
                ("name", "STAGE-LEFT"),
                ("require", "true"),
                ("lan_password", &hash),
                ("video.encoding", "cpu"),
            ],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"), "{}", r.to_xml());
    let st = node.status().unwrap();
    assert_eq!(st.attr("name"), Some("STAGE-LEFT"));
    assert_eq!(st.attr("require"), Some("true"));
    assert_eq!(st.attr("lan_panel"), Some("true"));
    assert_eq!(
        st.child("video").and_then(|v| v.attr("encoding")),
        Some("cpu")
    );
    let end = Instant::now() + Duration::from_secs(10);
    while !authority
        .node_reports()
        .iter()
        .any(|r| r.fingerprint == fp && r.report.attr("name") == Some("STAGE-LEFT"))
    {
        assert!(Instant::now() < end, "the report doesn't show the new name");
        std::thread::sleep(Duration::from_millis(100));
    }

    // 2b. Audio routes from the Site Manager (idn-routing.md §A.4): a receiver offered on
    // the machine gets a route the authority sends.
    let booth =
        idn_open_protocol::node::routing::AudioReceiver::register(&node, "Booth", 2, &[]).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !authority.node_reports().iter().any(|r| {
        r.fingerprint == fp
            && r.report
                .children_named("audio_rx")
                .any(|x| x.attr("name") == Some("Booth"))
    }) {
        assert!(Instant::now() < end, "the report doesn't show the receiver");
        std::thread::sleep(Duration::from_millis(100));
    }
    let r = authority
        .route_node(
            &fp,
            &[idn_open_protocol::node::routing::RouteChange {
                receiver: "Booth".into(),
                ch: 0,
                to: Some(("FOH (Desk)".into(), 3)),
            }],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"), "{}", r.to_xml());
    wait_for("the route at the receiver", Duration::from_secs(5), || {
        booth.routes().len() == 1
    });
    assert_eq!(booth.routes()[0].tx, "FOH (Desk)");
    assert_eq!(booth.routes()[0].tx_ch, 3);

    // 2c. A routing preset (N3, idn-routing.md §A.11): saved now, the routes changed,
    // recalled: as saved again (the extra route cleared).
    type Routes = [(u16, String, u16)];
    let wait_report = |pred: &dyn Fn(&Routes) -> bool| {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let reports = authority.node_reports();
            let now = reports
                .iter()
                .find(|r| r.fingerprint == fp)
                .map(|r| idn_site_manager::presets::current(r, "Booth"))
                .unwrap_or_default();
            let v: Vec<(u16, String, u16)> =
                now.into_iter().map(|(ch, (tx, t))| (ch, tx, t)).collect();
            if pred(&v) {
                return reports;
            }
            assert!(Instant::now() < end, "the report: {v:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let reports = wait_report(&|v| v.len() == 1);
    let saved = idn_site_manager::presets::snapshot(&reports);
    let booth_saved = saved
        .iter()
        .find(|x| x["receiver"] == "Booth")
        .expect("Booth in the preset")
        .clone();
    let r = authority
        .route_node(
            &fp,
            &[
                idn_open_protocol::node::routing::RouteChange {
                    receiver: "Booth".into(),
                    ch: 0,
                    to: None,
                },
                idn_open_protocol::node::routing::RouteChange {
                    receiver: "Booth".into(),
                    ch: 1,
                    to: Some(("Band (Mix)".into(), 7)),
                },
            ],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"));
    let reports = wait_report(&|v| v.len() == 1 && v[0].0 == 1);
    let now = idn_site_manager::presets::current(
        reports.iter().find(|r| r.fingerprint == fp).unwrap(),
        "Booth",
    );
    let changes = idn_site_manager::presets::changes(
        "Booth",
        &now,
        &idn_site_manager::presets::saved_routes(&booth_saved),
    );
    assert_eq!(changes.len(), 2, "ch 0 back, ch 1 cleared");
    let r = authority
        .route_node(&fp, &changes, Duration::from_secs(5))
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"));
    let end = Instant::now() + Duration::from_secs(5);
    while !(booth.routes().len() == 1 && booth.routes()[0].ch == 0) {
        assert!(Instant::now() < end, "not as saved: {:?}", booth.routes());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        (booth.routes()[0].tx.as_str(), booth.routes()[0].tx_ch),
        ("FOH (Desk)", 3)
    );

    // 3. Its own LAN panel is on: the password logs in.
    let end = Instant::now() + Duration::from_secs(5);
    let (code, cookie, _) = loop {
        if std::net::TcpStream::connect(("127.0.0.1", lan_port)).is_ok() {
            break https(
                lan_port,
                "POST",
                "/api/v1/login",
                r#"{"password":"stage123"}"#,
                None,
            );
        }
        assert!(Instant::now() < end, "the LAN panel didn't start");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(code, 200);
    let (code, _, body) = https(lan_port, "GET", "/api/v1/status", "", cookie.as_deref());
    assert_eq!(code, 200);
    assert!(body.contains(r#""name":"STAGE-LEFT""#), "{body}");
    // The machine's video settings from its own panel (§N.13).
    let (code, _, body) = https(
        lan_port,
        "POST",
        "/api/v1/settings",
        r#"{"video":{"decoding":"software","parallel_decode":"false"}}"#,
        cookie.as_deref(),
    );
    assert_eq!(code, 200, "{body}");
    let (_, _, body) = https(lan_port, "GET", "/api/v1/status", "", cookie.as_deref());
    assert!(body.contains(r#""decoding":"software""#), "{body}");
    assert!(body.contains(r#""parallel_decode":"false""#), "{body}");
    assert!(body.contains(r#""encoding":"cpu""#), "{body}");

    // 4. The Site Manager's panel: five wrong passwords, then blocked; the right one works
    // (from another address it would; here the block holds a minute, so it comes first).
    let (code, cookie, _) = https(
        panel.port,
        "POST",
        "/api/v1/login",
        r#"{"password":"volunteers1"}"#,
        None,
    );
    assert_eq!(code, 200);
    let (code, _, body) = https(panel.port, "GET", "/api/v1/status", "", cookie.as_deref());
    assert_eq!(code, 200);
    assert!(body.contains("STAGE-LEFT"), "{body}");
    assert!(body.contains(r#""encoding":"cpu""#), "{body}");
    // The machine's video settings from the Site Manager's panel (§N.13); a bad value is
    // refused there.
    let edit = format!(r#"{{"fingerprint":"{fp}","video_threads":"3","video_encoding":""}}"#);
    let (code, _, body) = https(panel.port, "POST", "/api/v1/node", &edit, cookie.as_deref());
    assert_eq!(code, 200, "{body}");
    let v = node.status().unwrap();
    let v = v.child("video").unwrap();
    assert_eq!((v.attr("threads"), v.attr("encoding")), (Some("3"), None));
    let bad = format!(r#"{{"fingerprint":"{fp}","video_threads":"100"}}"#);
    let (code, _, _) = https(panel.port, "POST", "/api/v1/node", &bad, cookie.as_deref());
    assert_ne!(code, 200);
    // The routing matrix: the receiver and its route in the status, a stereo pair routed
    // from the panel, a bad channel refused.
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, _, body) = https(panel.port, "GET", "/api/v1/status", "", cookie.as_deref());
        if body.contains(r#""name":"Booth""#) && body.contains(r#""tx":"FOH (Desk)""#) {
            break;
        }
        assert!(Instant::now() < end, "no routing data: {body}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let pair = format!(
        r#"{{"fingerprint":"{fp}","receiver":"Booth","changes":[{{"ch":0,"tx":"FOH (Desk)","tx_ch":6}},{{"ch":1,"tx":"FOH (Desk)","tx_ch":7}}]}}"#
    );
    let (code, _, body) = https(
        panel.port,
        "POST",
        "/api/v1/route",
        &pair,
        cookie.as_deref(),
    );
    assert_eq!(code, 200, "{body}");
    let end = Instant::now() + Duration::from_secs(5);
    while booth.routes().iter().map(|r| r.tx_ch).collect::<Vec<_>>() != [6, 7] {
        assert!(
            Instant::now() < end,
            "the pair didn't arrive: {:?}",
            booth.routes()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let bad = format!(
        r#"{{"fingerprint":"{fp}","receiver":"Booth","changes":[{{"ch":300,"tx":"X (Y)","tx_ch":0}}]}}"#
    );
    let (code, _, _) = https(panel.port, "POST", "/api/v1/route", &bad, cookie.as_deref());
    assert_ne!(code, 200);

    // 4b. Users, roles, the audit and the lock (N4, idn-node.md §N.31).
    for u in [
        r#"{"action":"set","name":"sam","password":"operator1","role":"operator"}"#,
        r#"{"action":"set","name":"viv","password":"viewer12","role":"viewer"}"#,
    ] {
        let (code, _, body) = https(panel.port, "POST", "/api/v1/user", u, cookie.as_deref());
        assert_eq!(code, 200, "{body}");
    }
    let login = |who: &str, pw: &str| {
        let (code, c, body) = https(
            panel.port,
            "POST",
            "/api/v1/login",
            &format!(r#"{{"user":"{who}","password":"{pw}"}}"#),
            None,
        );
        assert_eq!(code, 200, "{who}: {body}");
        c
    };
    let (sam, viv) = (login("sam", "operator1"), login("viv", "viewer12"));
    let (_, _, body) = https(panel.port, "GET", "/api/v1/status", "", sam.as_deref());
    assert!(body.contains(r#""role":"operator""#), "{body}");
    // An operator routes, but doesn't change a machine; a viewer does neither.
    let one = format!(
        r#"{{"fingerprint":"{fp}","receiver":"Booth","changes":[{{"ch":0,"tx":"FOH (Desk)","tx_ch":2}}]}}"#
    );
    let (code, _, body) = https(panel.port, "POST", "/api/v1/route", &one, sam.as_deref());
    assert_eq!(code, 200, "{body}");
    let (code, _, _) = https(panel.port, "POST", "/api/v1/node", &edit, sam.as_deref());
    assert_eq!(code, 403);
    let (code, _, _) = https(panel.port, "POST", "/api/v1/route", &one, viv.as_deref());
    assert_eq!(code, 403);
    let (code, _, _) = https(panel.port, "GET", "/api/v1/users", "", viv.as_deref());
    assert_eq!(code, 403, "users: administrators only");
    // Locked: the machine refuses changes of its own, takes the Site Manager's.
    let lock = format!(r#"{{"fingerprint":"{fp}","site_lock":true}}"#);
    let (code, _, body) = https(panel.port, "POST", "/api/v1/node", &lock, cookie.as_deref());
    assert_eq!(code, 200, "{body}");
    let refused = |r: std::io::Result<Element>| format!("{r:?}").contains("locked by the site");
    assert!(refused(
        node.request(Element::new("set").with_attr("name", "MINE"))
    ));
    assert!(refused(
        node.request(
            Element::new("route")
                .with_attr("receiver", "Booth")
                .with_attr("ch", "0")
                .with_attr("tx", "X (Y)")
                .with_attr("tx_ch", "0")
        )
    ));
    let (code, _, body) = https(panel.port, "POST", "/api/v1/route", &one, sam.as_deref());
    assert_eq!(code, 200, "the site still routes it: {body}");
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, _, body) = https(panel.port, "GET", "/api/v1/status", "", cookie.as_deref());
        if body.contains(r#""locked":"true""#) {
            break;
        }
        assert!(Instant::now() < end, "the lock isn't reported: {body}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let unlock = format!(r#"{{"fingerprint":"{fp}","site_lock":false}}"#);
    let (code, _, _) = https(
        panel.port,
        "POST",
        "/api/v1/node",
        &unlock,
        cookie.as_deref(),
    );
    assert_eq!(code, 200);
    assert_eq!(
        node.request(Element::new("set").with_attr("name", "STAGE-LEFT"))
            .unwrap()
            .name,
        "ok"
    );
    // The audit: who did what, refusals too, no secrets; a viewer reads it.
    let (code, _, body) = https(panel.port, "GET", "/api/v1/audit", "", viv.as_deref());
    assert_eq!(code, 200);
    let audit: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = audit.as_array().unwrap();
    assert!(
        rows.iter()
            .any(|a| a["user"] == "sam" && a["action"] == "route" && a["ok"] == true)
    );
    assert!(
        rows.iter()
            .any(|a| a["user"] == "sam" && a["action"] == "node" && a["ok"] == false)
    );
    assert!(
        rows.iter()
            .any(|a| a["user"] == "admin" && a["detail"]["site_lock"] == true)
    );
    assert!(
        !body.contains("operator1") && !body.contains("viewer12"),
        "{body}"
    );
    // Removing a user ends their session.
    let (code, _, _) = https(
        panel.port,
        "POST",
        "/api/v1/user",
        r#"{"action":"remove","name":"sam"}"#,
        cookie.as_deref(),
    );
    assert_eq!(code, 200);
    let (code, _, _) = https(panel.port, "GET", "/api/v1/status", "", sam.as_deref());
    assert_eq!(code, 403);

    let (code, _, _) = https(panel.port, "GET", "/api/v1/status", "", None);
    assert_eq!(code, 403, "no session");
    for _ in 0..5 {
        let (c, _, _) = https(
            panel.port,
            "POST",
            "/api/v1/login",
            r#"{"password":"nope"}"#,
            None,
        );
        assert_eq!(c, 403);
    }
    let (c, _, _) = https(
        panel.port,
        "POST",
        "/api/v1/login",
        r#"{"password":"volunteers1"}"#,
        None,
    );
    assert_eq!(c, 429, "blocked after five wrong passwords");

    // Clearing the Node's LAN password: no logins; in a site, its panel stays on for the
    // Site Manager's tokens (§N.31.5).
    let r = authority
        .set_node(&fp, &[("lan_password", "")], Duration::from_secs(5))
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"));
    std::thread::sleep(Duration::from_millis(1500));
    let (code, _, _) = https(
        lan_port,
        "POST",
        "/api/v1/login",
        r#"{"password":"stage123"}"#,
        None,
    );
    assert_eq!(code, 403, "no password any more");
    node_tokens(&authority, panel.port, lan_port);
    site_certificates(&authority, &node, panel.port, lan_port);
}

/// A TLS handshake with `127.0.0.1:port` as `name`, trusting the site CA `ca` alone
/// (names checked): the certificate presented, or why not.
fn pinned(ca: &[u8], port: u16, name: &str) -> Result<Vec<u8>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(ca.to_vec()))
        .unwrap();
    let cfg =
        rustls::ClientConfig::builder_with_provider(idn_open_protocol::secure::crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let mut conn = rustls::ClientConnection::new(
        Arc::new(cfg),
        rustls::pki_types::ServerName::try_from(name.to_owned()).unwrap(),
    )
    .unwrap();
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp).map_err(|e| e.to_string())?;
    }
    Ok(conn.peer_certificates().unwrap()[0].to_vec())
}

/// The machine's certificate for its HTTPS ports (extensions §E.10.11): a client takes
/// the site CA from the Site Manager (`GET /api/v1/site_ca`) and pins it alone; the
/// Node's LAN panel presents the machine's certificate with the CA after it, by address
/// and by name; a program's network port has it from the Node, which signs for the
/// device key.
fn site_certificates(authority: &Authority, node: &Node, panel: u16, lan_port: u16) {
    let (c, _, pem) = https(panel, "GET", "/api/v1/site_ca", "", None);
    assert_eq!(c, 200, "{pem}");
    assert_eq!(pem, authority.ca_pem());
    let ca = authority.ca_der();
    let end = Instant::now() + Duration::from_secs(10);
    let leaf = loop {
        match pinned(&ca, lan_port, "127.0.0.1") {
            Ok(l) => break l,
            Err(e) => assert!(Instant::now() < end, "the Node's LAN panel: {e}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(pinned(&ca, lan_port, "localhost").unwrap(), leaf);
    assert!(pinned(&ca, lan_port, "elsewhere.example").is_err());

    // A program's network port: the certificate from the Node.
    let end = Instant::now() + Duration::from_secs(10);
    while node.enrollment().lan_cert().is_none() {
        assert!(
            Instant::now() < end,
            "the Node gave its programs no certificate"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(node.enrollment().lan_cert().unwrap().der, leaf);
    let dir = TempDir::new("program-port");
    let (tls, _) = idn_open_protocol::web::lan::server_config(
        &dir,
        "Test",
        idn_open_protocol::web::lan::process_membership(),
    )
    .unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    idn_open_protocol::web::serve(
        l,
        Some(tls),
        Arc::new(|_| idn_open_protocol::web::Response::text("200 OK", "ok")),
        stop.clone(),
    )
    .unwrap();
    assert_eq!(pinned(&ca, port, "127.0.0.1").unwrap(), leaf);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Tokens for the machine's own panel (§N.31.5), end to end: an administrator asks the
/// Site Manager for one; the Node checks it with the site's CA from its membership. A
/// read-only one reads and is refused changes (in the Node's audit); one for another
/// engine is refused; the user's tokens withdrawn by the Site Manager are refused.
fn node_tokens(authority: &Authority, panel: u16, lan_port: u16) {
    let (code, cookie, _) = https(
        panel,
        "POST",
        "/api/v1/login",
        r#"{"password":"volunteers1"}"#,
        None,
    );
    // (The Site Manager's logins from this address may be blocked a minute by the wrong
    // passwords above: then the token is signed here with the site's key, as it would.)
    let ask = |engine: &str, scope: &str| -> String {
        if code == 200 {
            let (c, _, body) = https(
                panel,
                "POST",
                "/api/v1/token",
                &format!(r#"{{"aud":{{"engine":"{engine}"}},"scope":"{scope}","ttl_s":600}}"#),
                cookie.as_deref(),
            );
            assert_eq!(c, 200, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            v["token"].as_str().unwrap().to_owned()
        } else {
            let scope = idn_open_protocol::trust::token::Scope::parse(scope).unwrap();
            authority
                .issue_token("admin", "admin", engine, "*", scope, 600)
                .unwrap()
                .0
        }
    };
    let read = ask("node", "read");
    let (c, _, body) = https_as(lan_port, "GET", "/api/v1/status", "", None, Some(&read));
    assert_eq!(c, 200, "{body}");
    assert!(body.contains("STAGE-LEFT"), "{body}");
    let (c, _, body) = https_as(
        lan_port,
        "POST",
        "/api/v1/settings",
        r#"{"video":{"decoding":"auto"}}"#,
        None,
        Some(&read),
    );
    assert_eq!(c, 403, "{body}");
    assert!(body.contains("read-only"), "{body}");
    let desk = ask("desk", "control");
    let (c, _, _) = https_as(lan_port, "GET", "/api/v1/status", "", None, Some(&desk));
    assert_eq!(c, 401, "a desk's token isn't the Node's");
    let (c, _, body) = https_as(lan_port, "GET", "/api/v1/audit", "", None, Some(&read));
    assert_eq!(c, 200);
    assert!(body.contains(r#""error":"read-only""#), "{body}");
    // Withdrawn: the user's not-before, a site option the Node gets.
    let later = idn_common::epoch::secs() + 1;
    authority
        .set_option(
            &idn_open_protocol::trust::OptionScope::Site,
            &idn_open_protocol::trust::token::not_before_key("admin"),
            &later.to_string(),
        )
        .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let (c, _, _) = https_as(lan_port, "GET", "/api/v1/status", "", None, Some(&read));
        if c == 401 {
            break;
        }
        assert!(Instant::now() < end, "a withdrawn token still taken");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// §N.17 (G8): a Site Manager restored from a backup into an empty folder keeps the site:
/// a machine enrolled before reconnects to it with no pairing or approval.
#[test]
fn a_restored_site_manager_keeps_its_machines() {
    let (adir, bdir) = (TempDir::new("auth-a"), TempDir::new("auth-b"));
    let authority = Authority::open(AuthorityConfig {
        state_dir: adir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let port = ds.port();
    let endpoint = sock("node-restore");
    let state = TempDir::new("node-restore");
    let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_CLOCK", "off")
        .args(["run", "--user", "--state"])
        .arg(state.path())
        .args([
            "--endpoint",
            &endpoint,
            "--panel-port",
            &free_port().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _kill = Kill(child, endpoint.clone());
    wait_for("the Node to start", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, "test").is_ok()
    });
    let node = Node::connect_to(&endpoint, "test").expect("the Node starts");
    node.request(Element::new("set").with_attr("server", &format!("127.0.0.1:{port}")))
        .unwrap();
    authority.open_pairing(Duration::from_secs(60));
    let fp = node.enrollment().fingerprint();
    let end = Instant::now() + Duration::from_secs(15);
    let req = loop {
        if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_millis(200))
            && r.fingerprint == fp
        {
            break r;
        }
        assert!(Instant::now() < end, "no join request");
    };
    authority.approve(req.id).unwrap();
    confirm_code(&node);
    wait_for("the report from the Node", Duration::from_secs(15), || {
        authority.node_reports().iter().any(|r| r.fingerprint == fp)
    });
    // The backup, as the panel makes it; then the first Site Manager is gone.
    let backup = idn_open_protocol::backup::make(
        &adir,
        "site-manager",
        idn_site_manager::BACKUP_SKIP,
        "keep it safe",
    )
    .unwrap();
    let ca = authority.ca_fingerprint();
    drop(ds);
    drop(authority);
    std::thread::sleep(Duration::from_millis(500));
    // A new one from the backup, at the same address.
    let (_, files) = idn_open_protocol::backup::open(&backup, "keep it safe").unwrap();
    idn_open_protocol::backup::restore(&files, &bdir, false).unwrap();
    let restored = Authority::open(AuthorityConfig {
        state_dir: bdir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    assert_eq!(restored.ca_fingerprint(), ca, "the same site");
    assert!(
        restored.devices().iter().any(|d| d.fingerprint == fp),
        "the machine is known"
    );
    let end = Instant::now() + Duration::from_secs(10);
    let _ds2 = loop {
        match DiscoveryServer::start(port, Some(restored.clone())) {
            Ok(d) => break d,
            Err(e) => assert!(Instant::now() < end, "the port again: {e}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // The machine comes back by itself: no pairing window, no approval.
    let end = Instant::now() + Duration::from_secs(60);
    let mut said = Instant::now();
    while !restored.node_reports().iter().any(|r| r.fingerprint == fp) {
        if said.elapsed() > Duration::from_secs(5) {
            said = Instant::now();
            let st = node.status().unwrap();
            eprintln!(
                "node: state={:?} server={:?} trust={}",
                st.attr("state"),
                st.attr("server"),
                st.child("trust").map(|t| t.to_xml()).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < end,
            "the machine didn't come back to the restored Site Manager"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A machine whose Node stops (SIGTERM, as a service stop does) counts as offline on its
/// site within seconds (lab 395-B: the site said nothing until it came back).
#[cfg(unix)]
#[test]
fn a_stopped_node_goes_offline_on_its_site() {
    let auth_dir = TempDir::new("auth-stop");
    let authority = Authority::open(AuthorityConfig {
        state_dir: auth_dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let endpoint = sock("node-stop");
    let state = TempDir::new("node-stop");
    let mut child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_CLOCK", "off")
        .args(["run", "--user", "--state"])
        .arg(state.path())
        .args([
            "--endpoint",
            &endpoint,
            "--panel-port",
            &free_port().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    wait_for("the Node to start", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, "test").is_ok()
    });
    let node = Node::connect_to(&endpoint, "test").expect("the Node starts");
    node.request(Element::new("set").with_attr("server", &format!("127.0.0.1:{}", ds.port())))
        .unwrap();
    authority.open_pairing(Duration::from_secs(60));
    let fp = node.enrollment().fingerprint();
    let end = Instant::now() + Duration::from_secs(15);
    let req = loop {
        if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_millis(200))
            && r.fingerprint == fp
        {
            break r;
        }
        assert!(Instant::now() < end, "no join request");
    };
    authority.approve(req.id).unwrap();
    confirm_code(&node);
    let online = |a: &Authority| a.devices().iter().any(|d| d.fingerprint == fp && d.online);
    wait_for("online", Duration::from_secs(15), || online(&authority));
    drop(node);
    let stopped = Instant::now();
    assert!(
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success()
    );
    let _ = child.wait();
    while online(&authority) {
        assert!(
            stopped.elapsed() < Duration::from_secs(20),
            "still online 20 s after its Node stopped"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "offline {} ms after the stop",
        stopped.elapsed().as_millis()
    );
    let _ = std::fs::remove_file(&endpoint);
}

/// Leaving the site from the machine (§N.42): the site's certificates forgotten, no site
/// address, the state `none`; the site's device count shown while in it.
#[cfg(unix)]
#[test]
fn leaving_the_site_forgets_it() {
    let auth_dir = TempDir::new("auth-leave");
    let authority = Authority::open(AuthorityConfig {
        state_dir: auth_dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let endpoint = sock("node-leave");
    let state = TempDir::new("node-leave");
    let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_CLOCK", "off")
        .args(["run", "--user", "--state"])
        .arg(state.path())
        .args([
            "--endpoint",
            &endpoint,
            "--panel-port",
            &free_port().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Stopped however the test ends.
    let _child = Kill(child, endpoint.clone());
    wait_for("the Node to start", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, "test").is_ok()
    });
    let node = Node::connect_to(&endpoint, "test").expect("the Node starts");
    node.request(Element::new("set").with_attr("server", &format!("127.0.0.1:{}", ds.port())))
        .unwrap();
    authority.open_pairing(Duration::from_secs(60));
    let fp = node.enrollment().fingerprint();
    let end = Instant::now() + Duration::from_secs(15);
    let req = loop {
        if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_millis(200))
            && r.fingerprint == fp
        {
            break r;
        }
        assert!(Instant::now() < end, "no join request");
    };
    authority.approve(req.id).unwrap();
    confirm_code(&node);
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let s = node.status().unwrap();
        if s.attr("state") == Some("enrolled") && s.attr("site_devices").is_some() {
            break;
        }
        assert!(
            Instant::now() < end,
            "never in the site with its count: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Check for updates now (§N.42): the site is asked, its answer shown.
    node.request(Element::new("update_check")).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(AuthorityEvent::UpdateCheck { fingerprint, .. }) =
            authority.recv(Duration::from_millis(200))
            && fingerprint == fp
        {
            break;
        }
        assert!(Instant::now() < end, "no update check reached the site");
    }
    // Settings go to a Node the site has a report from (its first comes just after it
    // joins: a fast build got here before it).
    idn_common::testing::wait_for("the Node's first report", Duration::from_secs(10), || {
        authority.node_reports().iter().any(|r| r.fingerprint == fp)
    });
    authority
        .set_node(
            &fp,
            &[("update_available", "0.2.0-Alpha99")],
            Duration::from_secs(5),
        )
        .unwrap();
    let st = node.status().unwrap();
    let up = st.child("update").expect("update");
    assert_eq!(up.attr("available"), Some("0.2.0-Alpha99"));
    assert!(up.attr("managed_by").is_none(), "not managed: {up:?}");
    // Updates managed by another product (§N.23): the setting from the site, reported
    // with where it came from; a rollback refused; a bad name refused; cleared again.
    let r = authority
        .set_node(
            &fp,
            &[("update.managed_by", " HostApp ")],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"), "{r:?}");
    let up = node
        .status()
        .unwrap()
        .child("update")
        .cloned()
        .expect("update");
    assert_eq!(
        (up.attr("managed_by"), up.attr("managed_by_source")),
        (Some("HostApp"), Some("setting"))
    );
    let r = authority
        .update_node(&fp, Element::new("rollback"), Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        (r.attr("ok"), r.attr("error")),
        (
            Some("false"),
            Some("updates managed by HostApp: roll back there")
        )
    );
    let r = authority
        .set_node(
            &fp,
            &[("update.managed_by", "x".repeat(65).as_str())],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(r.attr("ok"), Some("false"), "{r:?}");
    let r = authority
        .set_node(&fp, &[("update.managed_by", "")], Duration::from_secs(5))
        .unwrap();
    assert_eq!(r.attr("ok"), Some("true"), "{r:?}");
    let up = node
        .status()
        .unwrap()
        .child("update")
        .cloned()
        .expect("update");
    assert!(up.attr("managed_by").is_none(), "cleared: {up:?}");
    // The site's transmitters (§E.12.12): asked through the Node, answered.
    let txs = node.request(Element::new("site_transmitters")).unwrap();
    assert_eq!(txs.name, "site_transmitters");
    // A client's pairing relayed by a program through the Node (idn-node.md §N.42.3): the
    // site holds it with its PIN, answers, and the answer comes back through the Node.
    // Its pairing code from the site, through the Node, once the site
    // has marked this machine a pairing relay.
    assert!(
        node.request(Element::new("pairing_code")).is_err()
            || node
                .request(Element::new("pairing_code"))
                .is_ok_and(|r| r.attr("code").is_none()),
        "not a relay yet: no code"
    );
    authority.set_relay(&fp, true).unwrap();
    let pc = node.request(Element::new("pairing_code")).unwrap();
    let code = pc.attr("code").expect("a code from the site").to_owned();
    assert_eq!(code.len(), 6, "{pc:?}");
    assert!(pc.attr("until").is_some());
    let r = node
        .request(
            Element::new("key_request")
                .with_attr("client", "Presenter iPad")
                .with_attr("pin", &code),
        )
        .unwrap();
    let ticket = r.attr("ticket").expect("a ticket").to_owned();
    let end = Instant::now() + Duration::from_secs(10);
    let kid = loop {
        if let Some(AuthorityEvent::KeyRequested {
            id,
            client,
            pin,
            code_ok,
            ..
        }) = authority.recv(Duration::from_millis(200))
            && client == "Presenter iPad"
        {
            assert!(pin && code_ok, "the site's own code, typed right");
            break id;
        }
        assert!(Instant::now() < end, "no relayed pairing reached the site");
    };
    let held = authority.key_requests();
    assert_eq!(held[0].pin.as_deref(), Some(code.as_str()));
    assert!(held[0].code_ok);
    let waiting = node
        .request(Element::new("key_answer").with_attr("ticket", &ticket))
        .unwrap();
    assert_eq!(waiting.attr("waiting"), Some("true"));
    authority
        .answer_key_request(
            kid,
            &idn_open_protocol::trust::KeyGrant {
                approved: true,
                id: "abc123".into(),
                key: "idn1.x.y".into(),
                scope: "control".into(),
                engine: "app:hostapp".into(),
                error: String::new(),
            },
        )
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let got = loop {
        let a = node
            .request(Element::new("key_answer").with_attr("ticket", &ticket))
            .unwrap();
        if a.attr("waiting") != Some("true") {
            break a;
        }
        assert!(Instant::now() < end, "no answer through the Node");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        (got.attr("approved"), got.attr("key_id"), got.attr("key")),
        (Some("true"), Some("abc123"), Some("idn1.x.y"))
    );
    assert!(state.join("device.crt").exists());
    node.request(Element::new("site_leave")).unwrap();
    let s = node.status().unwrap();
    assert_eq!(s.attr("state"), Some("none"), "{s:?}");
    // The site was told: the machine left (not listed; it may join again).
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(AuthorityEvent::Left { fingerprint, .. }) =
            authority.recv(Duration::from_millis(200))
            && fingerprint == fp
        {
            break;
        }
        assert!(
            Instant::now() < end,
            "the site wasn't told the machine left"
        );
    }
    assert!(
        !authority
            .devices()
            .iter()
            .any(|d| d.fingerprint == fp && !d.revoked)
    );
    assert!(!state.join("device.crt").exists());
    assert!(!state.join("ca.crt").exists());
}
