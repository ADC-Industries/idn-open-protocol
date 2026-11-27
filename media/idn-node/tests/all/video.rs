//! The machine's video settings (spec/extensions/idn-node.md §N.13), end to end: set at
//! the `idn-node` program in user mode, heard by a program connected to it. A file of its
//! own: the settings a program heard are per process.

use crate::common;
use common::start;

use idn_common::testing::TempDir;
use idn_open_protocol::node::Node;
use idn_open_protocol::node::video::{self, Decoding, Encoding, VideoSettings};
use idn_open_protocol::xml::Element;
use std::time::Duration;

/// A socket path short enough for `sun_path`.
fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/ivideo-{}-{name}.sock", std::process::id())
}

fn wait_for(secs: u64, what: &str, pred: impl Fn() -> bool) {
    idn_common::testing::wait_for(what, Duration::from_secs(secs), pred);
}

#[test]
fn machine_video_settings_reach_programs_and_their_own_settings_win() {
    // SAFETY: before any thread of this test reads the environment.
    unsafe { std::env::remove_var("IDN_WORKER_THREADS") };
    let (endpoint, state) = (sock("v"), TempDir::new("state"));
    let child = start(&endpoint, &state);
    let program = Node::connect_to(&endpoint, "video test").unwrap();
    assert_eq!(video::machine(), VideoSettings::default());
    let automatic = idn_open_protocol::default_worker_threads();
    assert_eq!(idn_open_protocol::worker_threads(), automatic);

    // A change at the Node reaches the running program at once; the pool follows.
    let set = |attrs: &[(&str, &str)]| {
        let mut e = Element::new("set");
        for (k, v) in attrs {
            e = e.with_attr(k, v);
        }
        program.request(e)
    };
    let threads = if automatic == 3 { 2 } else { 3 };
    set(&[
        ("video.threads", &threads.to_string()),
        ("video.parallel_decode", "false"),
        ("video.encoding", "cpu"),
        ("video.decoding", "gpu"),
    ])
    .unwrap();
    wait_for(5, "the settings", || {
        video::machine().threads == Some(threads)
    });
    let heard = video::machine();
    assert_eq!(heard.parallel_decode, Some(false));
    assert_eq!(heard.encoding, Some(Encoding::Cpu));
    assert_eq!(heard.decoding, Some(Decoding::Gpu));
    assert_eq!(idn_open_protocol::worker_threads(), threads);

    // The program's own number wins over the machine's, and 0 hands it back.
    idn_open_protocol::set_worker_threads(5);
    set(&[("video.threads", "4")]).unwrap();
    wait_for(5, "threads 4", || video::machine().threads == Some(4));
    assert_eq!(idn_open_protocol::worker_threads(), 5);
    idn_open_protocol::set_worker_threads(0);
    assert_eq!(idn_open_protocol::worker_threads(), 4);

    // Bad values are refused and change nothing.
    assert!(set(&[("video.threads", "0")]).is_err());
    assert!(set(&[("video.encoding", "fast"), ("video.threads", "6")]).is_err());
    assert_eq!(video::machine().threads, Some(4));

    // An empty value goes back to the default.
    set(&[("video.decoding", "")]).unwrap();
    wait_for(5, "decoding unset", || video::machine().decoding.is_none());

    // The settings outlive the Node: a restart gives them to a program connecting anew,
    // and the status shows them.
    drop(child);
    drop(program);
    let child = start(&endpoint, &state);
    let again = Node::connect_to(&endpoint, "video test again").unwrap();
    let status = again.status().unwrap();
    let shown = VideoSettings::from_element(status.child("video").unwrap());
    assert_eq!(
        shown,
        VideoSettings {
            threads: Some(4),
            parallel_decode: Some(false),
            encoding: Some(Encoding::Cpu),
            decoding: None,
            ndi: None,
            precision: None,
            processor: None,
            format: None,
        }
    );
    assert_eq!(video::machine(), shown);
    drop(child);
}
