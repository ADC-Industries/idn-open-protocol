//! secure/1 (spec/extensions/idn-extensions.md §E.8): encryption between IDN peers that
//! share a site passphrase, optional by default, enforceable by "require", sticky once
//! used, and invisible to peers that do not offer it.
//!
//! These run with the IDN link off on both sides: they cover the TCP paths, which are
//! also what peers fall back to when UDP is blocked. The link is tested in `link.rs`.

use crate::common;
use common::free_port;
use idn_common::sync::LockOk;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
use idn_open_protocol::input::{Input, InputConfig, InputEvent, Transport};
use idn_open_protocol::secure::SecureConfig;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `squic::simulate_udp_blocked` is process-wide: tests that use secure QUIC hold this.
static SQUIC: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Src {
    out: Arc<Output>,
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

fn source(port: u16, secure: Option<SecureConfig>) -> Src {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure,
        link: None,
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &publisher,
            OutputConfig {
                port,
                ..OutputConfig::new("Secure")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let mut n = 0u8;
        while !s.load(Ordering::Relaxed) {
            let px: Arc<[u8]> = vec![n; 160 * 90 * 4].into();
            let _ = o.send(VideoFrame::rgba(160, 90, px, AlphaMode::Opaque));
            n = n.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    Src {
        out,
        stop,
        feeder: Some(feeder),
        _publisher: publisher,
    }
}

fn input(addr: &str, secure: Option<SecureConfig>) -> Input {
    Input::open(InputConfig {
        // Over the connection: the transport is what is tested (§E.26's ring otherwise).
        machine: false,
        secure,
        transports: vec![Transport::Quic],
        ..InputConfig::new(addr)
    })
    .unwrap()
}

/// Number of video frames received within `secs`.
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

#[test]
fn same_passphrase_encrypts_with_media_over_secure_quic() {
    let _g = SQUIC.lock_ok();
    let port = free_port();
    let src = source(port, Some(SecureConfig::new("St Mark's AV").unwrap()));
    let rx = input(
        &format!("127.0.0.1:{port}"),
        Some(SecureConfig::new("St Mark's AV").unwrap()),
    );
    assert!(videos(&rx, 4) > 20);
    let st = rx.stats();
    assert!(st.encrypted);
    assert!(
        st.extensions.contains(&"secure/1".to_string()),
        "{:?}",
        st.extensions
    );
    assert!(!st.secure_fallback);
    // Media over QUIC with real TLS (§E.8.4), not on the control connection.
    assert!(
        st.via.keys().any(|k| k == "video via squic"),
        "{:?}",
        st.via
    );
    assert!(
        !st.via.keys().any(|k| k == "video via control"),
        "{:?}",
        st.via
    );
    let reports = src.out.receiver_reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].1.transport, "squic");
}

/// UDP blocked: the sender falls back to media inside the encrypted control connection
/// after 2 s, and the receiver reports the fallback (§E.8.4). Never plaintext.
#[test]
fn blocked_udp_falls_back_to_encrypted_inline_media() {
    let _g = SQUIC.lock_ok();
    idn_open_protocol::squic::simulate_udp_blocked(true);
    let port = free_port();
    let src = source(port, Some(SecureConfig::new("site").unwrap()));
    let rx = input(
        &format!("127.0.0.1:{port}"),
        Some(SecureConfig::new("site").unwrap()),
    );
    let n = videos(&rx, 5);
    idn_open_protocol::squic::simulate_udp_blocked(false);
    assert!(n > 20, "{n} frames");
    let st = rx.stats();
    assert!(st.encrypted && st.secure_fallback);
    assert!(
        st.via.keys().any(|k| k == "video via control")
            && !st.via.keys().any(|k| k == "video via squic"),
        "{:?}",
        st.via
    );
    assert_eq!(src.out.receiver_reports()[0].1.transport, "inline");
    // The 2 s QUIC attempt is not "skipped frames" (lab X-q3), and no backlog burst.
    let sr = st.sender_report.expect("sender stats");
    assert_eq!(sr.v_skipped, 0, "{sr:?}");
}

#[test]
fn different_passphrases_fall_back_to_plain() {
    let port = free_port();
    let _src = source(port, Some(SecureConfig::new("site A").unwrap()));
    let rx = input(
        &format!("127.0.0.1:{port}"),
        Some(SecureConfig::new("site B").unwrap()),
    );
    assert!(videos(&rx, 4) > 20);
    assert!(!rx.stats().encrypted);
}

#[test]
fn optional_encryption_serves_plain_receivers() {
    let port = free_port();
    let _src = source(port, Some(SecureConfig::new("site").unwrap()));
    let rx = input(&format!("127.0.0.1:{port}"), None);
    assert!(videos(&rx, 4) > 20);
    assert!(!rx.stats().encrypted);
}

#[test]
fn require_on_the_sender_refuses_plain_receivers() {
    let port = free_port();
    let mut sc = SecureConfig::new("site").unwrap();
    sc.require = true;
    let _src = source(port, Some(sc.clone()));
    let plain = input(&format!("127.0.0.1:{port}"), None);
    let other = input(
        &format!("127.0.0.1:{port}"),
        Some(SecureConfig::new("other").unwrap()),
    );
    let good = input(
        &format!("127.0.0.1:{port}"),
        Some(SecureConfig::new("site").unwrap()),
    );
    assert_eq!(videos(&plain, 3), 0);
    assert_eq!(videos(&other, 1), 0);
    assert!(videos(&good, 2) > 10);
}

#[test]
fn require_on_the_receiver_refuses_plain_sources() {
    let port = free_port();
    let _src = source(port, None);
    let mut sc = SecureConfig::new("site").unwrap();
    sc.require = true;
    let rx = input(&format!("127.0.0.1:{port}"), Some(sc));
    assert_eq!(videos(&rx, 3), 0);
}

#[test]
fn sticky_secure_refuses_a_later_downgrade() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let sc = SecureConfig::new("site").unwrap();
    {
        let _src = source(port, Some(SecureConfig::new("site").unwrap()));
        let rx = input(&addr, Some(sc.clone()));
        assert!(videos(&rx, 3) > 10);
        assert!(rx.stats().encrypted);
    }
    // The same source comes back without encryption (or an attacker strips it).
    std::thread::sleep(Duration::from_millis(500)); // the old listener closes
    let _src = source(port, None);
    let rx = input(&addr, Some(sc.clone()));
    assert_eq!(videos(&rx, 3), 0, "sticky record must refuse plain text");
    // The operator's reset accepts it again.
    sc.sticky.forget_all();
    let rx = input(&addr, Some(sc));
    assert!(videos(&rx, 3) > 10);
}

/// A man in the middle that alters the sender's plain-text ndi_version (drops stats/1)
/// but leaves secure/1 alone: the in-TLS transcript check must catch it.
#[test]
fn tampered_offer_fails_the_transcript_check() {
    let port = free_port();
    let _src = source(port, Some(SecureConfig::new("site").unwrap()));
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let paddr = proxy.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in proxy.incoming() {
            let Ok(mut c) = c else { break };
            let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
                continue;
            };
            let (mut c2, mut s2) = (c.try_clone().unwrap(), s.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut c2, &mut s2);
                let _ = s2.shutdown(std::net::Shutdown::Write);
            });
            std::thread::spawn(move || {
                if let Ok(Some(f)) = read_frame(&mut s, &Limits::default()) {
                    let x = f.text_payload().unwrap().replace(" stats/1", "");
                    let _ = c.write_all(&Frame::text(kind::CONTROL, 0, &x).encode());
                }
                let mut buf = [0u8; 65536];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 || c.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = c.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    let rx = input(&paddr.to_string(), Some(SecureConfig::new("site").unwrap()));
    let end = Instant::now() + Duration::from_secs(3);
    let mut seen = String::new();
    while Instant::now() < end {
        match rx.recv(Duration::from_millis(50)) {
            Some(InputEvent::Video(_)) => panic!("video through a tampered session"),
            Some(e) => seen += &format!("{e:?}\n"),
            None => {}
        }
    }
    assert!(seen.contains("transcript check failed"), "{seen}");
}

/// An NDI-style receiver (no x_idn_ext) talking to a secure-preferred sender sees no
/// extension traffic at all and gets video.
#[test]
fn plain_ndi_peer_sees_nothing_new() {
    let port = free_port();
    let _src = source(port, Some(SecureConfig::new("site").unwrap()));
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    for x in [
        r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.0" platform="LINUX"/>"#.to_string(),
        r#"<ndi_identify name="NDI (Plain)"/>"#.into(),
        r#"<ndi_video quality="high"/>"#.into(),
        r#"<ndi_enabled_streams video="true" audio="true" text="true" shq_skip_block="true" shq_short_dc="true" shq_4_2_2="true"/>"#.into(),
    ] {
        s.write_all(&Frame::text(kind::CONTROL, 0, &x).encode()).unwrap();
    }
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    let mut video = 0;
    while Instant::now() < end && video < 5 {
        let f = read_frame(&mut s, &Limits::default()).unwrap().unwrap();
        if f.kind == kind::VIDEO {
            video += 1;
        } else if let Ok(t) = f.text_payload().map(|t| t.to_owned()) {
            if t.starts_with("<ndi_version") {
                assert!(!t.contains("x_idn_keyid=\"\""), "{t}"); // the announcement itself is allowed (§E.2)
                continue;
            }
            assert!(!t.contains("x_idn"), "extension leak: {t}");
        }
    }
    assert!(video >= 5);
}

/// Lab X-s3: NDI receivers retry a refused source at once (~50/s). The sender holds a
/// refused connection about 2 s, so a reconnect-on-close client manages only a couple.
#[test]
fn refused_connections_are_held_to_slow_retry_storms() {
    let port = free_port();
    let mut sc = SecureConfig::new("site").unwrap();
    sc.require = true;
    let _src = source(port, Some(sc));
    let src_out = _src.out.clone();
    let end = Instant::now() + Duration::from_secs(3);
    let mut attempts = 0;
    while Instant::now() < end {
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
            continue;
        };
        attempts += 1;
        let v = r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.0" platform="LINUX"/>"#;
        let _ = s.write_all(&Frame::text(kind::CONTROL, 0, v).encode());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            src_out.receivers(),
            0,
            "a held, refused connection is not a receiver"
        );
        let mut buf = [0u8; 4096];
        while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
    }
    assert!(attempts <= 2, "{attempts} attempts in 3 s");
}

/// Lab X-t7: a plain-text receiver refused by the sender's sticky record (same name as a
/// receiver that encrypted before) is held 2 s, and not counted as a receiver meanwhile.
#[test]
fn sticky_refusal_is_held_and_not_counted() {
    let _g = SQUIC.lock_ok();
    let port = free_port();
    let src = source(port, Some(SecureConfig::new("site").unwrap()));
    let rx = input(
        &format!("127.0.0.1:{port}"),
        Some(SecureConfig::new("site").unwrap()),
    );
    assert!(videos(&rx, 2) > 5);
    drop(rx);
    std::thread::sleep(Duration::from_millis(300));
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_uppercase();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    for x in [
        r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.0" platform="LINUX"/>"#.to_string(),
        format!(r#"<ndi_identify name="{host} (receiver)"/>"#),
    ] {
        s.write_all(&Frame::text(kind::CONTROL, 0, &x).encode())
            .unwrap();
    }
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        src.out.receivers(),
        0,
        "a held, refused receiver is not counted"
    );
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 4096];
    while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
    assert!(
        t0.elapsed() >= Duration::from_millis(1500),
        "held about 2 s, closed after {:?}",
        t0.elapsed()
    );
}

/// Lab item 16: a source that refuses after connecting (sticky record; the refused
/// connection is held about 2 s, §E.8.5) makes the receiver back off instead of
/// reconnecting at once each time the hold ends.
#[test]
fn a_refusing_source_makes_the_receiver_back_off() {
    let _g = SQUIC.lock_ok();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let src = source(port, Some(SecureConfig::new("site").unwrap()));
    let rx = input(&addr, Some(SecureConfig::new("site").unwrap()));
    assert!(videos(&rx, 2) > 5);
    drop(rx);
    std::thread::sleep(Duration::from_millis(300));
    // The same receiver name comes back in plain text.
    let rx = input(&addr, None);
    let end = Instant::now() + Duration::from_secs(20);
    let (mut connects, mut texts) = (0, Vec::new());
    while Instant::now() < end {
        match rx.recv(Duration::from_millis(50)) {
            Some(InputEvent::Video(_)) => panic!("served in plain text"),
            Some(InputEvent::State(s)) => {
                if format!("{s:?}").starts_with("Connected") {
                    connects += 1;
                }
            }
            Some(InputEvent::Error(t) | InputEvent::Log(t)) => texts.push(t),
            _ => {}
        }
    }
    assert_eq!(src.out.receivers(), 0);
    assert!(
        texts.iter().any(|t| t.contains("retrying in")),
        "no back-off: {texts:?}"
    );
    assert!(connects <= 6, "{connects} attempts in 20 s: {texts:?}");
}
