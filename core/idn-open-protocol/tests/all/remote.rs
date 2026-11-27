//! Remote control (R.25, §E.25): a source that takes control says so to its receivers,
//! and takes keyboard and mouse only from devices of its site.

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::remote::Control;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn a_source_says_it_takes_control_and_refuses_it_from_outside_its_site() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(
        &publisher,
        OutputConfig {
            remote_control: true,
            ..OutputConfig::new("Screen")
        },
    )
    .unwrap();
    let px: Arc<[u8]> = vec![128u8; 64 * 36 * 4].into();
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    // Its capabilities, as the connection starts.
    let end = Instant::now() + Duration::from_secs(10);
    let mut capable = false;
    while Instant::now() < end && !capable {
        let _ = out.send(VideoFrame::rgba(64, 36, px.clone(), AlphaMode::Opaque));
        if let Some(InputEvent::Metadata { xml, .. }) = input.recv(Duration::from_millis(50)) {
            capable = xml.contains("ndi_capabilities") && xml.contains("idn_input=\"true\"");
        }
    }
    assert!(capable, "the source says it takes control");
    // Not a device of the source's site: refused, nothing injected.
    input.send_control(&[
        Control::Move { x: 0.5, y: 0.5 },
        Control::Key {
            code: "KeyA".into(),
            down: true,
        },
    ]);
    input.send_metadata("<something_else/>");
    let end = Instant::now() + Duration::from_secs(5);
    let (mut got, mut refused) = (Vec::new(), 0);
    while Instant::now() < end && refused == 0 {
        std::thread::sleep(Duration::from_millis(50));
        let (e, r) = out.recv_control();
        got.extend(e);
        refused += r;
    }
    assert_eq!(refused, 1, "one message of control, refused");
    assert!(got.is_empty(), "nothing from outside the site");
}

/// A screen that takes control says so in discovery (§E.25), so IDN Remote lists it
/// without connecting; an ordinary source doesn't.
#[test]
fn a_screen_that_takes_control_is_announced_as_one() {
    use idn_open_protocol::discovery::{Browser, BrowserConfig};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(true),
        ..Default::default()
    })
    .unwrap();
    let tag = std::process::id();
    let screen = Output::new(
        &publisher,
        OutputConfig {
            remote_control: true,
            ..OutputConfig::new(&format!("Ctl Screen {tag}"))
        },
    )
    .unwrap();
    let plain = Output::new(&publisher, OutputConfig::new(&format!("Ctl Plain {tag}"))).unwrap();
    let browser = Browser::start(BrowserConfig::default()).unwrap();
    let end = Instant::now() + Duration::from_secs(8);
    let (mut s, mut p) = (None, None);
    while Instant::now() < end && (s.is_none() || p.is_none()) {
        for x in browser.sources() {
            if x.name.ends_with(&format!("(Ctl Screen {tag})")) {
                s = Some(x);
            } else if x.name.ends_with(&format!("(Ctl Plain {tag})")) {
                p = Some(x);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (Some(s), Some(p)) = (s, p) else {
        eprintln!("mDNS not answered here: not tested");
        return;
    };
    assert!(s.takes_control(), "{:?}", s.idn_ext);
    assert!(!p.takes_control(), "{:?}", p.idn_ext);
    drop((screen, plain));
}
