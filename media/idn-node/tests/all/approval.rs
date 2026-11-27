//! Per-program approval (spec/extensions/idn-node.md §N.8.1), end to end: this test
//! program (not one of the IDN programs installed with the Node) is held back once
//! approval is on, signs again once allowed, and is refused once blocked; the `idn-node`
//! command next to the Node keeps working throughout.

use crate::common;
use common::start;

use idn_common::testing::TempDir;
use idn_open_protocol::node::Node;
use idn_open_protocol::xml::Element;
use std::process::Command;

/// A socket path short enough for `sun_path`.
fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/iappr-{}-{name}.sock", std::process::id())
}

/// `idn-node set …` (installed with the Node: allowed by itself).
fn cli_set(endpoint: &str, kv: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_NODE", endpoint)
        .args(["set", kv])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "idn-node set {kv}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn b64(d: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

/// Asks the Node to sign a TLS 1.3 CertificateVerify input with the link key: the reply's name.
fn sign(n: &Node) -> String {
    let mut data = vec![0x20u8; 64];
    data.extend_from_slice(b"TLS 1.3, client CertificateVerify\0");
    data.extend_from_slice(&[7u8; 32]);
    match n.request(
        Element::new("sign")
            .with_attr("key", "link")
            .with_attr("data", &b64(&data)),
    ) {
        Ok(r) => r.name.clone(),
        // Error replies come back as errors ending in their code.
        Err(e) if e.to_string().ends_with("(approval)") => "error:approval".into(),
        Err(e) => format!("error: {e}"),
    }
}

fn me_in(n: &Node) -> Element {
    let s = n.status().unwrap();
    s.children_named("client")
        .find(|c| c.attr("app") == Some("approval test"))
        .cloned()
        .expect("this program in the status")
}

#[test]
fn new_programs_wait_for_approval_and_can_be_allowed_or_blocked() {
    let endpoint = sock("a");
    let state = TempDir::new("state");
    let _node = start(&endpoint, &state);
    let me = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Off (the default): signs for anyone.
    let n = Node::connect_to(&endpoint, "approval test").unwrap();
    assert!(!n.awaiting_approval());
    assert_eq!(sign(&n), "signed");
    assert_eq!(me_in(&n).attr("exe"), Some(me.as_str()));

    // On: this program, connected already, is held back; the Node's own command is not.
    cli_set(&endpoint, "approve-programs=on");
    assert_eq!(sign(&n), "error:approval");
    assert_eq!(me_in(&n).attr("approval"), Some("pending"));
    let again = Node::connect_to(&endpoint, "approval test").unwrap();
    assert!(again.awaiting_approval());
    drop(again);

    // Allowed: signs again, and the rule is kept.
    cli_set(&endpoint, &format!("program-allow={me}"));
    assert_eq!(sign(&n), "signed");
    let s = n.status().unwrap();
    let rule = s.children_named("program_rule").next().expect("a rule");
    assert_eq!(
        (rule.attr("path"), rule.attr("allow")),
        (Some(me.as_str()), Some("true"))
    );
    assert!(
        !Node::connect_to(&endpoint, "approval test")
            .unwrap()
            .awaiting_approval()
    );

    // Blocked: refused at the door, and held back on the connection it has.
    cli_set(&endpoint, &format!("program-block={me}"));
    let e = Node::connect_to(&endpoint, "approval test").unwrap_err();
    assert!(e.to_string().contains("blocked"), "{e}");
    assert_eq!(sign(&n), "error:approval");

    // Forgotten: asked about again; approval off: as before.
    cli_set(&endpoint, &format!("program-forget={me}"));
    assert!(
        Node::connect_to(&endpoint, "approval test")
            .unwrap()
            .awaiting_approval()
    );
    cli_set(&endpoint, "approve-programs=off");
    assert_eq!(sign(&n), "signed");
}
