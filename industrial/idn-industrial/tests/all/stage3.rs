//! Stage 3 (§I.11, §I.14): remote support's testable core without a real display or
//! FFmpeg — the synthetic capture backend and the recording input backend stand in for
//! the platform ones. A Site Manager and a Node: users and roles; a screen session whose
//! pictures reach the relay; input injected and the clipboard read back; chat shown on
//! the station; and the append-only audit.

use crate::common;
use common::{dir, free_tcp, free_udp, wait};

use idn_industrial::modules::Supervisor;
use idn_industrial::node::{IndustrialNode, NodeConfig};
use idn_industrial::server::{Config, Role, SiteManager};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

struct Hub {
    rt: tokio::runtime::Runtime,
    sm: SiteManager,
}

fn hub(d: &Path) -> Hub {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let udp = free_udp();
    let sm = rt
        .block_on(SiteManager::start(Config {
            data_dir: d.join("hub"),
            quic: Some(format!("127.0.0.1:{udp}").parse().unwrap()),
            public_urls: vec![format!("idn://127.0.0.1:{udp}")],
            mqtt: None,
        }))
        .unwrap();
    Hub { rt, sm }
}

fn node(d: &Path, h: &Hub, name: &str) -> IndustrialNode {
    // Input recorded here (no uinput device to send it to on a test machine; without one
    // a real machine answers that input can't reach its screen, bug round F, F3-10).
    // SAFETY: set before the Node or its helper starts, no other thread reads it yet.
    unsafe { std::env::set_var("IDN_INPUT_RECORD", "1") };
    let n = IndustrialNode::start(NodeConfig {
        state_dir: d.join("node"),
        name: name.into(),
        version: "test".into(),
    })
    .unwrap();
    n.enrol(
        &h.sm
            .mint_token(None, BTreeMap::new(), Duration::from_secs(60))
            .unwrap()
            .0,
    )
    .unwrap();
    wait("online", 20, || n.status().connected);
    n
}

#[test]
fn users_have_roles() {
    let d = dir("users");
    let h = hub(&d);
    h.sm.set_user("sam", "password1", Role::Operator).unwrap();
    h.sm.set_user("viv", "password2", Role::Viewer).unwrap();
    assert_eq!(h.sm.verify_user("sam", "password1"), Some(Role::Operator));
    assert_eq!(h.sm.verify_user("viv", "password2"), Some(Role::Viewer));
    assert_eq!(h.sm.verify_user("sam", "wrong"), None);
    assert_eq!(h.sm.verify_user("nobody", "password1"), None);
    // A short password is refused.
    assert!(h.sm.set_user("tim", "short", Role::Viewer).is_err());
    let names: Vec<_> = h.sm.users().unwrap().into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&"sam".to_owned()) && names.contains(&"viv".to_owned()));
    assert!(h.sm.remove_user("sam").unwrap());
    assert_eq!(h.sm.verify_user("sam", "password1"), None);
    h.rt.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn a_screen_session_streams_input_flows_and_chat_shows() {
    let d = dir("screen");
    let h = hub(&d);
    let n = node(&d, &h, "Station1");
    // The supervisor handles remote.* commands (and keeps the chat queue).
    let sup = Supervisor::start(n.clone(), d.join("node"));
    let _cmds = idn_industrial::modules::dispatch(&n, sup.clone());
    let fp = h.sm.machines().unwrap()[0].fingerprint.clone();

    // Start a controlling, silent session.
    let (session, w, hgt) =
        h.rt.block_on(h.sm.start_screen(
            &fp,
            serde_json::json!({ "control": true, "notify": false, "fps": 10 }),
            "sam",
        ))
        .expect("start screen");
    assert_eq!((w, hgt), (1280, 720), "synthetic backend size");

    // Pictures reach the relay; the first a browser sees is a keyframe.
    let (mut rx, keyframe, control, got_fp) = h.sm.screen_watch(&session).expect("session");
    assert!(control && got_fp == fp);
    let rec = keyframe.or_else(|| {
        h.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .ok()
                .and_then(Result::ok)
        })
    });
    let rec = rec.expect("a picture reached the relay");
    assert_eq!(rec[0], 1, "the first picture is a keyframe");

    // Input: set then read the clipboard, round-tripped through the recording injector.
    let ans =
        h.rt.block_on(h.sm.send_input(
            &session,
            serde_json::json!([
                { "t": "move", "x": 100, "y": 200 },
                { "t": "key", "code": "KeyA", "down": true },
                { "t": "clipboardset", "text": "from the viewer" },
                { "t": "clipboardget" },
            ]),
        ))
        .expect("send input");
    assert_eq!(
        ans.get("clipboard").and_then(|c| c.as_str()),
        Some("from the viewer")
    );

    // Chat: shown on the station (queued for the helper by the time the command is
    // answered), and audited.
    h.rt.block_on(h.sm.send_chat(&fp, "sam", "are you there?"))
        .expect("chat");
    let pending = sup.pending_chat();
    assert!(
        pending.iter().any(|m| m.text == "are you there?"),
        "chat reached the station's queue: {pending:?}"
    );

    // The audit has the session start, the chat, and the machine's own records.
    let log = h.sm.audit_log(0, 100).unwrap();
    let kinds: Vec<String> = log
        .iter()
        .map(|(_, r)| {
            r.get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    assert!(kinds.iter().any(|k| k == "session.start"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "chat"), "{kinds:?}");

    // Watching, then leaving, ends the session on its own (§I.11.3): a screen is
    // captured only while someone watches. Two viewers: it ends when the last leaves.
    h.sm.viewer_joined(&session);
    h.sm.viewer_joined(&session);
    h.rt.block_on(h.sm.viewer_left(&session));
    assert!(!h.sm.screens().is_empty(), "one viewer still watching");
    h.rt.block_on(h.sm.viewer_left(&session));
    assert!(
        h.sm.screens().is_empty(),
        "the last viewer left, so it ended"
    );

    // Stopping an already-ended session is harmless.
    h.rt.block_on(h.sm.stop_screen(&session, "sam")).unwrap();

    n.stop();
    h.rt.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn screen_and_input_go_through_the_session_helper() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = dir("worker");
    let h = hub(&d);
    let n = node(&d, &h, "Station2");
    let sup = Supervisor::start(n.clone(), d.join("node"));
    let cmds = idn_industrial::modules::dispatch(&n, sup.clone());
    let fp = h.sm.machines().unwrap()[0].fingerprint.clone();

    // The Node's local API, which the helper connects to as the worker.
    let token = idn_industrial::local_api::token(&d.join("node")).unwrap();
    let port = free_tcp();
    let router =
        idn_industrial::local_api::router_with(n.clone(), token.clone(), cmds, Some(sup.clone()));
    h.rt.spawn(async move {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let _ = axum::serve(l, router).await;
    });

    // The helper's worker loop (synthetic capture, recording injector on Linux).
    let stop = Arc::new(AtomicBool::new(false));
    let (base, tok, st) = (
        format!("http://127.0.0.1:{port}"),
        token.clone(),
        stop.clone(),
    );
    h.rt.spawn(async move { idn_industrial::worker::run_worker(&base, &tok, st).await });
    wait("helper connected", 10, || sup.remote().has_worker());

    // A session now goes through the helper, not the Node directly.
    let (session, w, hgt) =
        h.rt.block_on(h.sm.start_screen(
            &fp,
            serde_json::json!({ "control": true, "notify": false, "fps": 10 }),
            "sam",
        ))
        .expect("start via helper");
    assert_eq!((w, hgt), (1280, 720));

    let (mut rx, keyframe, _c, _fp) = h.sm.screen_watch(&session).expect("session");
    let rec = keyframe.or_else(|| {
        h.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .ok()
                .and_then(Result::ok)
        })
    });
    assert_eq!(
        rec.expect("a picture via the helper")[0],
        1,
        "keyframe first"
    );

    // Input is injected by the helper (in-session); the clipboard round-trips.
    let ans =
        h.rt.block_on(h.sm.send_input(
            &session,
            serde_json::json!([
                { "t": "clipboardset", "text": "via helper" },
                { "t": "clipboardget" },
            ]),
        ))
        .expect("input via helper");
    assert_eq!(
        ans.get("clipboard").and_then(|c| c.as_str()),
        Some("via helper")
    );

    h.rt.block_on(h.sm.stop_screen(&session, "sam")).unwrap();
    stop.store(true, Ordering::Relaxed);
    n.stop();
    h.rt.shutdown_timeout(Duration::from_secs(2));
}
