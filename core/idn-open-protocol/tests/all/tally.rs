//! The tally echo (§1.4, measured against NDI 6.3): a source echoes
//! its combined tally, the OR of every receiver's, to every receiver when it changes, one
//! that sets none included, and to a receiver that connects while something is on.

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The latest echo `input` heard within `secs` that `want` accepts.
fn echo_until(
    input: &Input,
    out: &Output,
    secs: u64,
    want: (bool, bool),
) -> Result<(), Option<(bool, bool)>> {
    let px: Arc<[u8]> = vec![128u8; 64 * 36 * 4].into();
    let end = Instant::now() + Duration::from_secs(secs);
    let mut last = None;
    while Instant::now() < end {
        let _ = out.send(VideoFrame::rgba(64, 36, px.clone(), AlphaMode::Opaque));
        if let Some(InputEvent::Metadata { xml, .. }) = input.recv(Duration::from_millis(20))
            && let Some(e) =
                idn_open_protocol::xml::parse(&xml).filter(|e| e.name == "ndi_tally_echo")
        {
            let t = (
                e.attr("on_program") == Some("true"),
                e.attr("on_preview") == Some("true"),
            );
            last = Some(t);
            if t == want {
                return Ok(());
            }
        }
    }
    Err(last)
}

#[test]
fn every_receiver_hears_the_combined_tally() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("TallyQ")).unwrap();
    let addr = format!("127.0.0.1:{}", out.port());
    let open = |name: &str| {
        Input::open(InputConfig {
            receiver_name: name.into(),
            ..InputConfig::new(&addr)
        })
        .unwrap()
    };
    // B sets no tally of its own; A puts the source on program.
    let b = open("B");
    let a = open("A");
    a.set_tally(true, false);
    assert_eq!(
        echo_until(&b, &out, 10, (true, false)),
        Ok(()),
        "B hears A's program"
    );
    assert_eq!(echo_until(&a, &out, 10, (true, false)), Ok(()), "A too");
    // A newcomer hears it at once.
    let late = open("Late");
    assert_eq!(
        echo_until(&late, &out, 10, (true, false)),
        Ok(()),
        "a newcomer hears it"
    );
    drop(late);
    // A leaves: off again for B.
    drop(a);
    assert_eq!(echo_until(&b, &out, 10, (false, false)), Ok(()), "A gone");
    // C puts it on preview.
    let c = open("C");
    c.set_tally(false, true);
    assert_eq!(
        echo_until(&b, &out, 10, (false, true)),
        Ok(()),
        "B hears C's preview"
    );
}
