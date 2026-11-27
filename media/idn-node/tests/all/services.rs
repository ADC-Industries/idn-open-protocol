//! The services list (spec/extensions/idn-node.md §N.14.4, §N.14.5), end to end: an AES67
//! stream run by the Node service itself and a screen run by the session, switched off
//! and on again with their settings kept; the session's runner reports how its services
//! do, and they wait for a log-in once it is gone.

use crate::common;
use common::start;

use idn_common::testing::TempDir;
use idn_open_protocol::node::Node;
use idn_open_protocol::xml::Element;
use std::process::Command;
use std::time::Duration;

/// A socket path short enough for `sun_path`.
fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/isvc-{}-{name}.sock", std::process::id())
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

fn service(n: &Node, id: &str) -> Element {
    let s = n.status().unwrap();
    s.children_named("service")
        .find(|x| x.attr("id") == Some(id))
        .cloned()
        .unwrap_or_else(|| panic!("no service {id} in {}", s.to_xml()))
}

/// Waits up to 5 s for the service's state to be one of `want`.
fn wait_state(n: &Node, id: &str, want: &[&str]) -> Element {
    idn_common::testing::wait_until(Duration::from_secs(5), || {
        want.contains(&service(n, id).attr("state").unwrap_or(""))
    });
    service(n, id)
}

#[test]
fn services_switch_off_and_on_and_report_their_state() {
    let endpoint = sock("s");
    let state = TempDir::new("state");
    let _node = start(&endpoint, &state);
    let n = Node::connect_to(&endpoint, "services test").unwrap();

    // The microphone is listed while off, and switched by its own setting.
    assert_eq!(service(&n, "mic").attr("state"), Some("off"));
    cli_set(&endpoint, "service-on=mic");
    assert_eq!(service(&n, "mic").attr("on"), Some("true"));
    cli_set(&endpoint, "service-off=mic");

    // An AES67 stream: run by the Node service (here `idn` may not be built next to it:
    // then it is restarting, with the reason).
    cli_set(&endpoint, "aes67-sends=NOWHERE (Nothing)=Test stream|ch=2");
    let x = wait_state(&n, "aes67-send:Test stream", &["running", "restarting"]);
    assert_eq!(x.attr("runs"), Some("node"));
    assert!(
        matches!(x.attr("state"), Some("running" | "restarting")),
        "{}",
        x.to_xml()
    );
    cli_set(&endpoint, "service-off=aes67-send:Test stream");
    assert_eq!(
        wait_state(&n, "aes67-send:Test stream", &["off"]).attr("state"),
        Some("off")
    );
    let saved = std::fs::read_to_string(state.join("node.xml")).unwrap();
    assert!(
        saved.contains("Test stream"),
        "the setting is kept while off"
    );

    // A screen: the session's, waiting for a log-in until a runner reports it.
    cli_set(&endpoint, "capture-screens=1=Slides");
    assert_eq!(service(&n, "screen:1").attr("state"), Some("no_session"));
    let runner = Node::connect_to(&endpoint, "idn session").unwrap();
    runner
        .request(
            Element::new("services_state").with_child(
                Element::new("service")
                    .with_attr("id", "screen:1")
                    .with_attr("up", "true")
                    .with_attr("restarts", "2")
                    .with_attr("for_s", "7")
                    .with_attr("line", "30.0 fps"),
            ),
        )
        .unwrap();
    let x = service(&n, "screen:1");
    assert_eq!(
        (x.attr("state"), x.attr("restarts"), x.attr("line")),
        (Some("running"), Some("2"), Some("30.0 fps"))
    );
    drop(runner);
    assert_eq!(
        wait_state(&n, "screen:1", &["no_session"]).attr("state"),
        Some("no_session")
    );

    // Off and on again, and gone with its setting (forgotten switched off too).
    cli_set(&endpoint, "service-off=screen:1");
    assert_eq!(service(&n, "screen:1").attr("state"), Some("off"));
    cli_set(&endpoint, "service-on=screen:1");
    assert_eq!(service(&n, "screen:1").attr("state"), Some("no_session"));
    cli_set(&endpoint, "service-off=screen:1");
    cli_set(&endpoint, "capture-screens=");
    cli_set(&endpoint, "capture-screens=1=Slides");
    assert_eq!(service(&n, "screen:1").attr("on"), Some("true"));

    // An unknown service is refused.
    let out = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_NODE", &endpoint)
        .args(["set", "service-off=screen:9"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no service"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// "Programs' sound" (2026-11-05): the session's runner tells the Node the programs with
/// sound open; the status carries them for the panel; `service_run` / `service_stop` switch
/// a service for now only, its start by itself as it was.
#[test]
fn the_session_tells_the_programs_with_sound_and_services_switch_for_now() {
    let endpoint = sock("apps");
    let state = TempDir::new("apps");
    let _node = start(&endpoint, &state);
    let n = Node::connect_to(&endpoint, "idn-session").unwrap();
    n.request(
        Element::new("audio_apps")
            .with_child(
                Element::new("app")
                    .with_attr("exe", "spotify")
                    .with_attr("name", "Spotify")
                    .with_attr("playing", "true"),
            )
            .with_child(
                Element::new("app")
                    .with_attr("exe", "chrome")
                    .with_attr("name", "Google Chrome")
                    .with_attr("playing", "false"),
            ),
    )
    .unwrap();
    let s = n.status().unwrap();
    let a = s.child("audio_apps").expect("the list in the status");
    assert!(a.attr("at").is_some());
    let apps: Vec<_> = a
        .children_named("app")
        .map(|x| {
            (
                x.attr("exe").unwrap().to_owned(),
                x.attr("playing") == Some("true"),
            )
        })
        .collect();
    assert_eq!(
        apps,
        [("spotify".to_owned(), true), ("chrome".to_owned(), false)]
    );
    // The mixer on for now: on, not by itself; off again.
    let mixer = |n: &Node| {
        let s = n.status().unwrap();
        let m = s
            .children_named("service")
            .find(|x| x.attr("id") == Some("mixer"))
            .cloned()
            .unwrap();
        (m.attr("on") == Some("true"), m.attr("auto") == Some("true"))
    };
    assert_eq!(mixer(&n), (false, false));
    n.request(Element::new("set").with_attr("service_run", "mixer"))
        .unwrap();
    assert_eq!(mixer(&n), (true, false));
    n.request(Element::new("set").with_attr("service_stop", "mixer"))
        .unwrap();
    assert_eq!(mixer(&n), (false, false));
    // service_on (the command line's, the Site Manager's) sets both.
    n.request(Element::new("set").with_attr("service_on", "mixer"))
        .unwrap();
    assert_eq!(mixer(&n), (true, true));
    // The session runner's bridges' lines go to the Node's log (2026-11-06).
    n.request(Element::new("log").with_attr(
        "text",
        "audio send --app spotify.exe: no such program\nplaying",
    ))
    .unwrap();
    let _ = std::fs::remove_file(&endpoint);
}
