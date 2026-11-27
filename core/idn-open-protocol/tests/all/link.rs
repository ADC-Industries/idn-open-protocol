//! The IDN link (`link/1`, spec/extensions/idn-extensions.md §E.11): one encrypted QUIC
//! connection between IDN peers, its modes, its fallback to the baseline, and its
//! policies (sticky secure, key memory).

use crate::common;
use common::free_port;
use idn_common::sync::LockOk;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::{KeyMemory, LinkConfig};
use idn_open_protocol::secure::{SecureConfig, StickyStore};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `link::SIMULATE_BLOCKED` is process-wide: the tests here run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock_ok()
}

struct Src {
    _out: Arc<Output>,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    _publisher: Publisher,
}

impl Drop for Src {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
    }
}

/// An output called "Linked" on `port`, sending frames, with its own link key.
fn source(port: u16, secure: Option<SecureConfig>) -> Src {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure,
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &publisher,
            OutputConfig {
                port,
                ..OutputConfig::new("Linked")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let _ = o.send(VideoFrame::rgba(
                160,
                90,
                vec![60u8; 160 * 90 * 4].into(),
                AlphaMode::Opaque,
            ));
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    Src {
        _out: out,
        stop,
        feeder: Some(feeder),
        _publisher: publisher,
    }
}

fn input(port: u16, secure: Option<SecureConfig>, link: LinkConfig) -> Input {
    Input::open(InputConfig {
        secure,
        link: Some(link),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap()
}

/// Video frames received within `secs`, and the error and log texts seen meanwhile.
/// Waits for `want` pictures, at most 10 s: with the whole suite running, a link took
/// longer than a fixed 3 s to start (the tests failed only then).
fn pictures(input: &Input, want: usize) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    let mut n = 0;
    while Instant::now() < end && n < want {
        if let Some(InputEvent::Video(_)) = input.recv(Duration::from_millis(50)) {
            n += 1;
        }
    }
    n >= want
}

fn watch(input: &Input, secs: u64) -> (usize, Vec<String>) {
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut n, mut texts) = (0, Vec::new());
    while Instant::now() < end {
        match input.recv(Duration::from_millis(50)) {
            Some(InputEvent::Video(_)) => n += 1,
            Some(InputEvent::Error(t) | InputEvent::Log(t)) => texts.push(t),
            Some(InputEvent::State(s)) => texts.push(format!("{s:?}")),
            _ => {}
        }
    }
    (n, texts)
}

#[test]
fn the_same_passphrase_gives_an_authenticated_link() {
    let _g = serial();
    let port = free_port();
    let _src = source(port, Some(SecureConfig::new("St Mark").unwrap()));
    let rx = input(
        port,
        Some(SecureConfig::new("St Mark").unwrap()),
        LinkConfig::ephemeral(),
    );
    assert!(pictures(&rx, 20));
    let st = rx.stats();
    assert_eq!(st.link_mode, Some("s1"));
    assert!(st.encrypted && !st.site_trust);
    assert!(
        st.extensions.iter().any(|e| e == "secure/1"),
        "{:?}",
        st.extensions
    );
    assert!(st.via.keys().any(|k| k == "video via link"), "{:?}", st.via);
}

#[test]
fn different_passphrases_give_an_unauthenticated_link() {
    let _g = serial();
    let port = free_port();
    let _src = source(port, Some(SecureConfig::new("St Mark").unwrap()));
    let rx = input(
        port,
        Some(SecureConfig::new("another site").unwrap()),
        LinkConfig::ephemeral(),
    );
    assert!(pictures(&rx, 20));
    let st = rx.stats();
    assert_eq!(st.link_mode, Some("open"));
    assert!(st.encrypted);
}

#[test]
fn blocked_udp_falls_back_to_the_baseline_and_is_remembered() {
    let _g = serial();
    let port = free_port();
    let _src = source(port, None);
    idn_open_protocol::link::SIMULATE_BLOCKED.store(true, Ordering::Relaxed);
    let rx = input(port, None, LinkConfig::ephemeral());
    let (n, texts) = watch(&rx, 4);
    idn_open_protocol::link::SIMULATE_BLOCKED.store(false, Ordering::Relaxed);
    assert!(n > 20, "{n} frames: {texts:?}");
    let st = rx.stats();
    // The plain baseline, not an encrypted fallback (§E.11.8).
    assert_eq!(st.link_mode, None);
    assert!(!st.encrypted);
    assert!(st.via.keys().any(|k| k == "video via quic"), "{:?}", st.via);
    assert!(
        texts.iter().any(|t| t.contains("looks blocked")),
        "the failure is remembered: {texts:?}"
    );
}

#[test]
fn a_sticky_source_is_not_accepted_on_an_unauthenticated_link() {
    let _g = serial();
    let port = free_port();
    let sticky = StickyStore::in_memory();
    let with_sticky = |pass: &str| {
        let mut c = SecureConfig::new(pass).unwrap();
        c.sticky = sticky.clone();
        c
    };
    // First an authenticated link with the source: remembered as encrypted.
    {
        let _src = source(port, Some(SecureConfig::new("St Mark").unwrap()));
        let rx = input(port, Some(with_sticky("St Mark")), LinkConfig::ephemeral());
        assert!(pictures(&rx, 20));
        assert_eq!(rx.stats().link_mode, Some("s1"));
    }
    // Then the same name without the passphrase: only mode open is possible, refused.
    std::thread::sleep(Duration::from_millis(500));
    let _src = source(port, None);
    let rx = input(port, Some(with_sticky("St Mark")), LinkConfig::ephemeral());
    let (n, texts) = watch(&rx, 3);
    assert_eq!(n, 0, "no frames from a downgraded source");
    assert!(
        texts.iter().any(|t| t.contains("used encryption before")),
        "{texts:?}"
    );
}

#[test]
fn a_changed_source_key_is_reported_and_the_session_continues() {
    let _g = serial();
    let port = free_port();
    // The receiver keeps its key memory across both connections.
    let link = LinkConfig {
        keys: KeyMemory::in_memory(),
        ..LinkConfig::ephemeral()
    };
    {
        let _src = source(port, None);
        let rx = input(port, None, link.clone());
        assert!(watch(&rx, 2).0 > 10);
        assert!(!rx.stats().key_changed);
    }
    std::thread::sleep(Duration::from_millis(500));
    // The same source name, now with another key (a reinstalled machine).
    let _src = source(port, None);
    let rx = input(port, None, link);
    let (n, texts) = watch(&rx, 3);
    assert!(n > 20, "the session continues");
    assert!(rx.stats().key_changed);
    assert!(texts.iter().any(|t| t.contains("link key")), "{texts:?}");
}

/// Resets a process-wide test switch when dropped, even if the test fails.
struct Reset(&'static std::sync::atomic::AtomicU64);

impl Drop for Reset {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

#[test]
fn a_session_that_started_on_tcp_moves_to_the_link() {
    let _g = serial();
    let port = free_port();
    let _src = source(port, None);
    // QUIC answers late (a lossy network): TCP wins the race, the session starts there.
    let slow = &idn_open_protocol::link::SIMULATE_SLOW_MS;
    slow.store(600, Ordering::Relaxed);
    let _reset = Reset(slow);
    let rx = input(port, None, LinkConfig::ephemeral());
    let (mut frames, mut texts) = (0, Vec::new());
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end && rx.stats().link_mode.is_none() {
        let (n, t) = watch(&rx, 1);
        frames += n;
        texts.extend(t);
    }
    let st = rx.stats();
    assert_eq!(st.link_mode, Some("open"), "{texts:?}");
    assert!(
        texts.iter().any(|t| t.contains("moved the session")),
        "{texts:?}"
    );
    assert_eq!(st.reconnects, 0, "moved, not reconnected");
    // Media keeps flowing, now over the link.
    let (n, _) = watch(&rx, 2);
    assert!(n > 40, "{n} frames after the move ({frames} before)");
    let st = rx.stats();
    assert!(st.via.keys().any(|k| k == "video via link"), "{:?}", st.via);
    assert!(
        st.via.keys().any(|k| k == "video via quic"),
        "it really started on TCP (QUIC media): {:?}",
        st.via
    );
}
