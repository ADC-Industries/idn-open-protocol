//! IDN extensions resume/1 and stats/1 (spec/extensions/idn-extensions.md §E.4, §E.5),
//! IDN ↔ IDN, and the compatibility rule: nothing extension-specific reaches a peer that
//! did not announce the extension (§E.1).

use crate::common;
use common::free_port;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::ext::Departure;
use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent};
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Feeds an output with frames until `stop`.
fn feed(out: Arc<Output>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut n = 0u8;
        while !stop.load(Ordering::Relaxed) {
            let px: Arc<[u8]> = vec![n; 160 * 90 * 4].into();
            let _ = out.send(VideoFrame::rgba(160, 90, px, AlphaMode::Opaque));
            n = n.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(40));
        }
    })
}

#[test]
fn stats_flow_both_ways_between_idn_peers() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let port = free_port();
    let out = Arc::new(
        Output::new(
            &publisher,
            OutputConfig {
                port,
                ..OutputConfig::new("Stats")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let f = feed(out.clone(), stop.clone());
    // Over the link: its statistics are what is tested (not §E.26's shared memory).
    let input = Input::open(InputConfig {
        machine: false,
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        input.recv(Duration::from_millis(100));
    }
    let st = input.stats();
    // Two IDN peers: the session runs over the IDN link (§E.11), encrypted.
    assert_eq!(
        st.extensions,
        [
            "audio-f32/1",
            "channels/1",
            "color/1",
            "link/1",
            "machine/1",
            "resume/1",
            "stats/1"
        ]
    );
    assert_eq!(st.link_mode, Some("open"));
    assert!(st.encrypted);
    let sr = st.sender_report.expect("sender stats arrived");
    assert!(sr.v_tx > 20 && sr.q == 92 && sr.mbps > 0.0, "{sr:?}");
    assert!(st.rtt_ms.is_some_and(|r| r < 50.0), "{:?}", st.rtt_ms);
    let reports = out.receiver_reports();
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0].1.v_rx > 20 && reports[0].1.transport == "link",
        "{reports:?}"
    );
    stop.store(true, Ordering::Relaxed);
    f.join().unwrap();
}

#[test]
fn planned_restart_reconnects_fast() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let port = free_port();
    let cfg = OutputConfig {
        port,
        ..OutputConfig::new("Resume")
    };
    let out = Arc::new(Output::new(&publisher, cfg.clone()).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let f = feed(out.clone(), stop.clone());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
    let wait = |pred: &dyn Fn(&InputEvent) -> bool, secs: u64| -> Option<Instant> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(e) = input.recv(Duration::from_millis(50))
                && pred(&e)
            {
                return Some(Instant::now());
            }
        }
        None
    };
    wait(&|e| matches!(e, InputEvent::Video(_)), 3).expect("first video");
    std::thread::sleep(Duration::from_millis(500)); // let ndi_version / extensions settle
    // Restart: announce, go away, come back on the same port after 400 ms.
    stop.store(true, Ordering::Relaxed);
    f.join().unwrap();
    Arc::try_unwrap(out)
        .ok()
        .unwrap()
        .close(Departure::Restart { retry_ms: 300 });
    wait(
        &|e| matches!(e, InputEvent::State(ConnectionState::Lost(_))),
        3,
    )
    .expect("lost");
    std::thread::sleep(Duration::from_millis(400));
    let back = Instant::now();
    let out2 = Arc::new(Output::new(&publisher, cfg).unwrap());
    let stop2 = Arc::new(AtomicBool::new(false));
    let f2 = feed(out2.clone(), stop2.clone());
    let t = wait(
        &|e| matches!(e, InputEvent::State(ConnectionState::Connected(_))),
        5,
    )
    .expect("reconnected");
    // The 250 ms direct retry beats the normal 1–2 s rediscovery.
    assert!(
        t.duration_since(back) < Duration::from_millis(700),
        "{:?}",
        t.duration_since(back)
    );
    wait(&|e| matches!(e, InputEvent::Video(_)), 3).expect("video after resume");
    assert_eq!(input.stats().reconnects, 1);
    // x_idn_stats counters restart with the new connection (§E.5).
    let end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < end {
        input.recv(Duration::from_millis(50));
    }
    let r = out2.receiver_reports();
    let total = input.stats().video as u64;
    assert!(
        r.len() == 1 && r[0].1.v_rx > 0 && r[0].1.v_rx + 10 < total,
        "{r:?} vs {total}"
    );
    stop2.store(true, Ordering::Relaxed);
    f2.join().unwrap();
}

/// A peer that does not announce extensions (as NDI does) never receives x_idn_ messages,
/// even with stats and resume compiled in.
#[test]
fn no_extension_messages_to_plain_peers() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let input = Input::open(InputConfig {
        transports: Vec::new(),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    let (mut s, _) = l.accept().unwrap();
    s.write_all(
        &Frame::text(
            kind::CONTROL,
            0,
            r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.2.0" platform="LINUX"/>"#,
        )
        .encode(),
    )
    .unwrap();
    let mut rx = s.try_clone().unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut seen = Vec::new();
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        input.try_recv();
        if let Ok(Some(f)) = read_frame(&mut rx, &Limits::default()) {
            seen.push(f.text_payload().unwrap().to_owned());
        }
    }
    assert!(
        seen.iter()
            .any(|x| x.starts_with("<ndi_version") && x.contains("x_idn_ext=")),
        "{seen:?}"
    );
    assert!(!seen.iter().any(|x| x.starts_with("<x_idn_")), "{seen:?}");
}

/// audio-f32/1: a receiver that asks gets bit-exact float audio (IDF3); one that does not
/// gets baseline fowt.
#[test]
fn lossless_audio_on_request_only() {
    use idn_open_protocol::api::AudioBlock;
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let port = free_port();
    let out = Arc::new(
        Output::new(
            &publisher,
            OutputConfig {
                port,
                ..OutputConfig::new("Audio")
            },
        )
        .unwrap(),
    );
    let samples: Vec<f32> = (0..1600)
        .map(|i| match i % 5 {
            0 => -0.0,
            1 => f32::NAN,
            2 => 1e-30,
            3 => 0.123_456_79,
            _ => -3.75,
        })
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (out, stop, samples) = (out.clone(), stop.clone(), samples.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut f =
                    VideoFrame::rgba(160, 90, vec![9u8; 160 * 90 * 4].into(), AlphaMode::Opaque);
                f.audio = Some(AudioBlock {
                    sample_rate: 48000,
                    channels: vec![samples.clone(), vec![0.0; 1600]],
                });
                let _ = out.send(f);
                std::thread::sleep(Duration::from_millis(40));
            }
        })
    };
    for lossless in [true, false] {
        let input = Input::open(InputConfig {
            lossless_audio: lossless,
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        let mut got = None;
        while Instant::now() < end && got.is_none() {
            if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(100)) {
                got = Some(a);
            }
        }
        let a = got.expect("audio");
        if lossless {
            assert_eq!(&a.header.fourcc, b"IDF3");
            assert!(
                a.channels[0]
                    .iter()
                    .zip(&samples)
                    .all(|(x, y)| x.to_bits() == y.to_bits()),
                "not bit-exact"
            );
            assert!(a.channels[1].iter().all(|x| *x == 0.0));
        } else {
            assert_eq!(&a.header.fourcc, b"fowt");
        }
    }
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

/// color/1: the source's colour description arrives with its pictures.
#[test]
fn color_description_reaches_receiver() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let port = free_port();
    let mut cfg = OutputConfig {
        port,
        ..OutputConfig::new("Colour")
    };
    cfg.color.limited = false;
    cfg.color.matrix = idn_open_protocol::color::Matrix::Bt601;
    let out = Arc::new(Output::new(&publisher, cfg).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let f = feed(out.clone(), stop.clone());
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    let mut col = None;
    while Instant::now() < end && col.is_none() {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(100))
            && v.color.range == "full"
        {
            col = Some(v);
        }
    }
    let v = col.expect("colour description");
    let c = v.color;
    assert_eq!(
        (
            c.matrix.as_str(),
            c.range.as_str(),
            c.alpha.as_str(),
            c.bits
        ),
        ("bt_601", "full", "straight", 8)
    );
    // The colour space travels in NDI's own per-frame element (§3.10.4), not x_idn_color.
    assert_eq!(
        v.header.metadata.as_deref(),
        Some(r#"<ndi_color_info primaries="bt_709" transfer="bt_709" matrix="bt_601"/>"#)
    );
    stop.store(true, Ordering::Relaxed);
    f.join().unwrap();
}

/// The lab's X-c3 case: found by name over mDNS, the sender restarts WITHOUT announcing
/// again. The 250 ms direct retry must run in parallel with the (now failing) mDNS lookup,
/// and the reconnect must carry x_idn_resume with the previous instance (§E.4).
#[test]
fn fast_retry_runs_during_discovery_and_resume_is_sent() {
    use idn_open_protocol::discovery::responder::{HostInfo, Published, Responder};
    let host = HostInfo::local(None).unwrap();
    let port = free_port();
    let name = format!(
        "{} (FASTRETRY{})",
        host.host.to_uppercase(),
        std::process::id()
    );
    let hello = |inst: &str| {
        Frame::text(kind::CONTROL, 0, &format!(
        r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.2.0" platform="OPENSRC" x_idn_ext="resume/1 stats/1" x_idn_instance="{inst}"/>"#)).encode()
    };
    // Serves one session: greets, then returns every text message received for `secs`.
    let serve = |l: TcpListener, inst: &'static str, secs: u64| {
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&hello(inst)).unwrap();
            let mut r = s.try_clone().unwrap();
            r.set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut got = Vec::new();
            let end = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < end {
                if let Ok(Some(f)) = read_frame(&mut r, &Limits::default()) {
                    got.push(f.text_payload().unwrap_or("").to_owned());
                }
            }
            got
        })
    };
    let resp = Responder::start(
        host.clone(),
        vec![Published {
            name: name.clone(),
            port,
            groups: vec!["public".into()],
            routing: false,
            idn_ext: None,
        }],
        false,
    )
    .unwrap();
    let s1 = serve(
        TcpListener::bind(("0.0.0.0", port)).unwrap(),
        "aaaaaaaaaaaaaaaa",
        2,
    );
    let input = Input::open(InputConfig {
        transports: Vec::new(),
        ..InputConfig::new(&name)
    })
    .unwrap();
    let wait = |pred: &dyn Fn(&InputEvent) -> bool, secs: u64| -> Option<Instant> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(e) = input.recv(Duration::from_millis(20))
                && pred(&e)
            {
                return Some(Instant::now());
            }
        }
        None
    };
    wait(
        &|e| matches!(e, InputEvent::State(ConnectionState::Connected(_))),
        8,
    )
    .expect("connected via mDNS");
    drop(resp); // goodbye: mDNS will no longer find it
    s1.join().unwrap(); // first session ends (connection closed)
    wait(
        &|e| matches!(e, InputEvent::State(ConnectionState::Lost(_))),
        3,
    )
    .expect("lost");
    std::thread::sleep(Duration::from_millis(300));
    let back = Instant::now();
    let s2 = serve(
        TcpListener::bind(("0.0.0.0", port)).unwrap(),
        "bbbbbbbbbbbbbbbb",
        2,
    );
    let t = wait(
        &|e| matches!(e, InputEvent::State(ConnectionState::Connected(_))),
        8,
    )
    .expect("reconnected");
    assert!(
        t.duration_since(back) < Duration::from_millis(600),
        "took {:?}",
        t.duration_since(back)
    );
    let got = s2.join().unwrap();
    assert!(
        got.iter()
            .any(|m| m == r#"<x_idn_resume instance="aaaaaaaaaaaaaaaa"/>"#),
        "{got:?}"
    );
}

/// color/1 high precision and HDR (§E.7, §3.9.5, §3.10.4): a P216 source with PQ
/// signalling. Flat stripes 100 + k/8 arrive exactly; the colour space arrives as NDI's
/// ndi_color_info, the bit depth as x_idn_color.
#[test]
fn p216_hdr_source_keeps_precision_and_signalling() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let port = free_port();
    let info = idn_open_protocol::ext::ColorInfo {
        primaries: "bt_2020".into(),
        transfer: "bt_2100_pq".into(),
        matrix: "bt_2020".into(),
        bits: 10,
        ..Default::default()
    };
    let out = Output::new(
        &publisher,
        OutputConfig {
            port,
            color_info: Some(info),
            ..OutputConfig::new("HDR")
        },
    )
    .unwrap();
    let (w, h) = (128usize, 32usize);
    let mut d = vec![0u16; 2 * w * h];
    for (i, v) in d[..w * h].iter_mut().enumerate() {
        *v = ((800 + (i % w) / 16) as u16) << 5; // Y' = 100 + k/8
    }
    d[w * h..].fill(128 << 8);
    let d: Arc<[u16]> = d.into();
    let input = Input::open(InputConfig {
        precise: true,
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    let end = Instant::now() + Duration::from_secs(4);
    let mut got = None;
    while Instant::now() < end && got.is_none() {
        let _ = out.send(VideoFrame::p216(w, h, d.clone()));
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(40))
            && v.color.bits == 10
        {
            got = Some(v);
        }
    }
    let v = got.expect("precise video with color/1");
    assert_eq!(
        (
            v.color.primaries.as_str(),
            v.color.transfer.as_str(),
            v.color.matrix.as_str()
        ),
        ("bt_2020", "bt_2100_pq", "bt_2020")
    );
    assert!(v.picture.high_precision);
    let y = &v.picture.precise.as_ref().expect("precise planes")[0];
    for x in 0..w {
        assert_eq!(y.data[x], ((800 + x / 16) as u16) << 5, "x={x}");
    }
}

#[test]
fn receiver_converts_with_the_senders_announced_colour() {
    use idn_open_protocol::color::{ColorConfig, Matrix};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let px = [200u8, 30, 90, 128];
    // (sender colour config, alpha output, RGBA asked of the input, expected pixel)
    let cases = [
        (
            ColorConfig {
                matrix: Matrix::Bt601,
                limited: false,
                ..Default::default()
            },
            false,
            AlphaMode::Straight,
            [200, 30, 90, 255],
        ),
        (
            ColorConfig {
                matrix: Matrix::Bt2020,
                ..Default::default()
            },
            false,
            AlphaMode::Straight,
            [200, 30, 90, 255],
        ),
        (
            ColorConfig {
                wire_alpha: AlphaMode::Premultiplied,
                ..Default::default()
            },
            true,
            AlphaMode::Straight,
            [200, 30, 90, 128],
        ),
        (
            ColorConfig {
                wire_alpha: AlphaMode::Premultiplied,
                ..Default::default()
            },
            true,
            AlphaMode::Premultiplied,
            [100, 15, 45, 128],
        ),
    ];
    for (i, (color, alpha, out_alpha, want)) in cases.into_iter().enumerate() {
        // The output takes a free port itself: one picked beforehand could be taken by
        // another test before the output opened it (it failed so with the whole suite).
        let out = Output::new(
            &publisher,
            OutputConfig {
                color,
                alpha,
                ..OutputConfig::new(&format!("Announced {i}"))
            },
        )
        .unwrap();
        let port = out.port();
        let input = Input::open(InputConfig {
            rgba: Some(out_alpha),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap();
        let frame: Arc<[u8]> = px.repeat(64 * 32).into();
        let end = Instant::now() + Duration::from_secs(10);
        let mut got = None;
        while Instant::now() < end && got.is_none() {
            let _ = out.send(VideoFrame::rgba(64, 32, frame.clone(), AlphaMode::Straight));
            // The colour description arrives with the session; skip frames decoded before.
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(40))
                && (i < 2 || v.color.alpha == "premultiplied")
                && (i != 0 || v.color.range == "full")
            {
                got = v.rgba.map(|r| [r[0], r[1], r[2], r[3]]);
            }
        }
        let got = got.expect("an RGBA frame");
        // Un-premultiplying 8-bit colour at alpha 128 doubles the rounding error.
        let tol = if i == 2 { 4 } else { 2 };
        assert!(
            got.iter()
                .zip(&want)
                .all(|(g, w)| (*g as i32 - *w).abs() <= tol),
            "case {i}: got {got:?}, want {want:?}"
        );
    }
}
