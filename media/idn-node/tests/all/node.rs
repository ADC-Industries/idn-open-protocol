//! The IDN Node end to end (spec/extensions/idn-node.md §N.9): the `idn-node` program in user
//! mode, a site authority in this process, programs as Node clients in this process.

use crate::common;
use common::free_port;

use idn_common::testing::{TempDir, wait_until};
use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::node::Node;
use idn_open_protocol::secure::SecureConfig;
use idn_open_protocol::trust::{
    Authority, AuthorityConfig, DeviceRole, Enrollment, EnrollmentConfig, TrustState,
};
use idn_open_protocol::xml::Element;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A socket path short enough for `sun_path`.
fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/inode-{}-{name}.sock", std::process::id())
}

struct NodeProcess {
    child: Child,
    endpoint: String,
    state: PathBuf,
    /// The state folder, when this process made it (removed with it).
    _dir: Option<TempDir>,
}

impl NodeProcess {
    fn start(name: &str) -> NodeProcess {
        let dir = TempDir::new(&format!("{name}-state"));
        let mut n = Self::start_at(sock(name), dir.to_path_buf());
        n._dir = Some(dir);
        n
    }

    fn start_at(endpoint: String, state: PathBuf) -> NodeProcess {
        let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
            .env("IDN_CLOCK", "off") // no PTP on the host network from tests
            .args(["run", "--user", "--state"])
            .arg(&state)
            .args([
                "--endpoint",
                &endpoint,
                "--panel-port",
                &free_port().to_string(),
            ])
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_until(Duration::from_secs(10), || {
            Node::connect_to(&endpoint, "probe").is_ok()
        });
        NodeProcess {
            child,
            endpoint,
            state,
            _dir: None,
        }
    }

    fn client(&self, app: &str) -> Node {
        Node::connect_to(&self.endpoint, app).unwrap()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

fn wait_state(e: &Enrollment, secs: u64, pred: impl Fn(&TrustState) -> bool) -> TrustState {
    wait_until(Duration::from_secs(secs), || pred(&e.state()));
    e.state()
}

fn output(
    port: u16,
    secure: SecureConfig,
    link: Option<idn_open_protocol::link::LinkConfig>,
) -> (Arc<Output>, Arc<AtomicBool>, Publisher) {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(secure),
        link,
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &publisher,
            OutputConfig {
                port,
                ..OutputConfig::new("Node test")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let _ = o.send(VideoFrame::rgba(
                160,
                90,
                vec![90u8; 160 * 90 * 4].into(),
                AlphaMode::Opaque,
            ));
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    (out, stop, publisher)
}

fn videos(input: &Input, secs: u64) -> usize {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut n = 0;
    while Instant::now() < end {
        if let Some(InputEvent::Video(_)) = input.recv(Duration::from_millis(50)) {
            n += 1;
        }
    }
    n
}

/// §N.9 1 and 2: one approval for the machine; two programs reach an enrolled peer with
/// device certificates, both as the same device; revoking the machine cuts both off.
#[test]
fn two_programs_share_one_enrolment_and_revocation_reaches_both() {
    let auth_dir = TempDir::new("auth");
    let authority = Authority::open(AuthorityConfig {
        state_dir: auth_dir.to_path_buf(),
        passphrase: Some("hall".into()),
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let server = format!("127.0.0.1:{}", ds.port());

    let node = NodeProcess::start("site");
    let admin = node.client("test");
    admin
        .request(
            Element::new("set")
                .with_attr("name", "BOOTH")
                .with_attr("server", &server)
                .with_attr("passphrase", "hall"),
        )
        .unwrap();
    // Two programs, each its own connection to the Node.
    let (a, b) = (node.client("program A"), node.client("program B"));
    for h in [&a, &b] {
        assert!(
            matches!(
                wait_state(h.enrollment(), 15, |s| matches!(
                    s,
                    TrustState::Enrolled { connected: true }
                )),
                TrustState::Enrolled { connected: true }
            ),
            "{:?}",
            h.enrollment().state()
        );
    }
    assert_eq!(a.enrollment().fingerprint(), b.enrollment().fingerprint());
    let booth: Vec<_> = authority
        .devices()
        .into_iter()
        .filter(|d| d.name == "BOOTH")
        .collect();
    assert_eq!(booth.len(), 1, "one enrolment for the machine");

    // An enrolled peer of its own.
    let peer_dir = TempDir::new("peer");
    let peer = Enrollment::start(EnrollmentConfig {
        discovery_server: server.clone(),
        state_dir: peer_dir.to_path_buf(),
        name: "PEER (App)".into(),
        role: DeviceRole::App,
        passphrase: Some("hall".into()),
        admin_code: None,
    })
    .unwrap();
    assert!(matches!(
        wait_state(&peer, 10, |s| matches!(s, TrustState::Enrolled { .. })),
        TrustState::Enrolled { .. }
    ));

    // Program A sends; the peer receives with secure/2. Its output takes the machine's
    // name from the Node, and registers with the Node's Discovery Server.
    let pa = free_port();
    let (oa, stop_a, _pa) = output(pa, a.secure_config(), Some(a.link_config()));
    let finder =
        idn_open_protocol::discovery::Browser::start(idn_open_protocol::discovery::BrowserConfig {
            interface: None,
            discovery_server: Some(server.clone()),
        })
        .unwrap();
    wait_until(Duration::from_secs(5), || {
        finder
            .sources()
            .iter()
            .any(|s| s.name == "BOOTH (Node test)")
    });
    let listed = finder.sources();
    assert!(
        listed.iter().any(|s| s.name == "BOOTH (Node test)"),
        "{:?} / {listed:?}",
        oa.port()
    );
    let peer_in = Input::open(InputConfig {
        secure: Some(SecureConfig::with_site(peer.clone())),
        ..InputConfig::new(&format!("127.0.0.1:{pa}"))
    })
    .unwrap();
    assert!(videos(&peer_in, 4) > 20, "peer receives program A");
    assert!(
        peer_in.stats().site_trust,
        "{:?}",
        peer_in.stats().extensions
    );

    // The peer sends; program B receives with secure/2.
    let pp = free_port();
    let (_op, stop_p, _pp) = output(pp, SecureConfig::with_site(peer.clone()), None);
    let b_in = Input::open(InputConfig {
        secure: Some(b.secure_config()),
        link: Some(b.link_config()),
        ..InputConfig::new(&format!("127.0.0.1:{pp}"))
    })
    .unwrap();
    assert!(videos(&b_in, 4) > 20, "program B receives the peer");
    assert!(b_in.stats().site_trust);

    // Revoking the machine: both programs hear it and both sessions end.
    authority.revoke(&a.enrollment().fingerprint()).unwrap();
    for h in [&a, &b] {
        assert_eq!(
            wait_state(h.enrollment(), 5, |s| *s == TrustState::Revoked),
            TrustState::Revoked
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    while peer_in.try_recv().is_some() {}
    while b_in.try_recv().is_some() {}
    assert_eq!(
        videos(&b_in, 4),
        0,
        "program B still receiving after revocation"
    );
    assert_eq!(
        videos(&peer_in, 2),
        0,
        "the peer still receiving the revoked machine"
    );
    stop_a.store(true, Ordering::Relaxed);
    stop_p.store(true, Ordering::Relaxed);
}

/// §N.9 3 and 4: a passphrase set in the Node gives programs `secure/1` without them
/// holding it, and the Node signs nothing but TLS handshakes.
#[test]
fn passphrase_through_the_node_and_signing_only_handshakes() {
    let node = NodeProcess::start("pass");
    let h = node.client("test");
    h.request(Element::new("set").with_attr("passphrase", "St Mark"))
        .unwrap();
    let p = node.client("program");
    wait_until(Duration::from_secs(10), || {
        p.secure_config().passphrase_key().is_some()
    });
    assert!(
        p.secure_config().passphrase_key().is_some(),
        "the program learns the key id"
    );
    let port = free_port();
    let (_o, stop, _p) = output(port, p.secure_config(), Some(p.link_config()));
    let input = Input::open(InputConfig {
        secure: Some(SecureConfig::new("St Mark").unwrap()),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    assert!(videos(&input, 4) > 20);
    let st = input.stats();
    assert!(
        st.encrypted && !st.site_trust,
        "secure/1: {:?}",
        st.extensions
    );
    // The Node keeps the stretched key, never the passphrase.
    let file = std::fs::read_to_string(node.state.join("node.xml")).unwrap();
    assert!(
        !file.contains("St Mark") && file.contains("psk=\""),
        "{file}"
    );
    // The output is listed under its program in the Node's status (the panel's list).
    let status = h.status().unwrap();
    let listed = status.children_named("client").any(|c| {
        c.attr("app") == Some("program")
            && c.children_named("reg").any(|r| {
                r.attr("kind") == Some("output")
                    && r.attr("name").is_some_and(|n| n.ends_with("(Node test)"))
            })
    });
    assert!(listed, "{}", status.to_xml());
    stop.store(true, Ordering::Relaxed);

    // Anything but a CertificateVerify input is refused, for every key.
    for key in ["device", "site", "link"] {
        let r = h.request(
            Element::new("sign")
                .with_attr("key", key)
                .with_attr("data", "PGlkbl9hZG1pbiBvcD0icmV2b2tlIi8+"),
        );
        let e = r.expect_err("signed arbitrary data").to_string();
        assert!(e.contains("denied"), "{key}: {e}");
    }
}

/// §N.9 7: a Node restart leaves running sessions alone; programs reconnect and new
/// handshakes work again.
#[test]
fn programs_ride_out_a_node_restart() {
    let mut node = NodeProcess::start("restart");
    node.client("test")
        .request(Element::new("set").with_attr("passphrase", "hall"))
        .unwrap();
    let p = node.client("program");
    wait_until(Duration::from_secs(10), || {
        p.secure_config().passphrase_key().is_some()
    });
    let port = free_port();
    let (_o, stop, _pub) = output(port, p.secure_config(), Some(p.link_config()));
    let first = Input::open(InputConfig {
        secure: Some(SecureConfig::new("hall").unwrap()),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    assert!(videos(&first, 3) > 15);

    let (endpoint, state) = (node.endpoint.clone(), node.state.clone());
    node.kill();
    assert!(
        videos(&first, 3) > 15,
        "the running session goes on without the Node"
    );
    assert_eq!(
        wait_state(p.enrollment(), 5, |s| matches!(s, TrustState::Error(_))),
        TrustState::Error("IDN Node not running".into())
    );
    let _node2 = NodeProcess::start_at(endpoint, state);
    let back = wait_state(p.enrollment(), 8, |s| !matches!(s, TrustState::Error(_)));
    assert!(!matches!(back, TrustState::Error(_)), "{back:?}");
    let second = Input::open(InputConfig {
        secure: Some(SecureConfig::new("hall").unwrap()),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    assert!(videos(&second, 4) > 15, "a new handshake after the restart");
    assert!(second.stats().encrypted);
    stop.store(true, Ordering::Relaxed);
}

/// A program that ends is forgotten: its entry and its registrations go (route1
/// regression: the Node kept every program that had ever connected).
#[test]
fn programs_that_end_are_forgotten() {
    let node = NodeProcess::start("gone");
    let watcher = node.client("watcher");
    for i in 0..5 {
        let c = node.client(&format!("short-lived {i}"));
        let _reg = c.register("output", "brief", None).unwrap();
        drop(_reg);
        drop(c);
    }
    let apps = || -> Vec<String> {
        watcher
            .status()
            .unwrap()
            .children_named("client")
            .filter_map(|c| c.attr("app").map(str::to_owned))
            .collect()
    };
    let gone = wait_until(Duration::from_secs(5), || apps() == ["watcher"]);
    assert!(gone, "programs still listed: {:?}", apps());
}

/// "Require encryption" set in the Node applies to a program already running: plain
/// receivers are let in before and refused after, without the program starting again
/// (it read the setting only when it connected).
#[test]
fn require_encryption_reaches_running_programs() {
    let node = NodeProcess::start("require");
    let h = node.client("test");
    h.request(Element::new("set").with_attr("passphrase", "St Mark"))
        .unwrap();
    let p = node.client("program");
    wait_until(Duration::from_secs(10), || {
        p.secure_config().passphrase_key().is_some()
    });
    let port = free_port();
    let (_o, stop, _p) = output(port, p.secure_config(), None);
    let plain = || Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
    assert!(
        videos(&plain(), 3) > 10,
        "a plain receiver, not required yet"
    );
    h.request(Element::new("set").with_attr("require", "true"))
        .unwrap();
    wait_until(Duration::from_secs(10), || p.secure_config().requires());
    assert!(p.secure_config().requires(), "the program heard it");
    assert_eq!(videos(&plain(), 3), 0, "a plain receiver refused now");
    h.request(Element::new("set").with_attr("require", "false"))
        .unwrap();
    wait_until(Duration::from_secs(10), || !p.secure_config().requires());
    assert!(videos(&plain(), 3) > 10, "and let in again");
    stop.store(true, Ordering::Relaxed);
}
