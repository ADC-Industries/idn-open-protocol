//! `channels/1` (§E.21.4): a receiver asking for some of a source's channels gets only
//! those, in its order, each block saying which; the list changes mid-session; a receiver
//! that doesn't ask gets every channel.

use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::input::{AudioIn, Input, InputConfig, InputEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Channel c holds the constant (c + 1) / 10, bit-exact through audio-f32/1.
fn level(c: usize) -> f32 {
    (c as f32 + 1.0) / 10.0
}

/// The next sound block within 3 s that satisfies `pred`.
fn block(input: &Input, pred: impl Fn(&AudioIn) -> bool) -> AudioIn {
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(50))
            && pred(&a)
        {
            return a;
        }
    }
    panic!("no matching audio block in 3 s");
}

fn levels(a: &AudioIn) -> Vec<f32> {
    a.channels.iter().map(|c| c[0]).collect()
}

#[test]
fn receivers_get_the_channels_they_ask_for() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Desk")).unwrap());
    let addr = format!("127.0.0.1:{}", out.port());
    let stop = Arc::new(AtomicBool::new(false));
    // Sound only, as idn-audio sends it: four channels, 10 ms blocks.
    let feeder = {
        let (out, stop) = (out.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let channels = (0..4).map(|c| vec![level(c); 480]).collect();
                let _ = out.send_audio(
                    AudioBlock {
                        sample_rate: 48000,
                        channels,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    // 1. Channels 2 and 0, in that order.
    let input = Input::open(InputConfig {
        lossless_audio: true,
        receive_video: false,
        audio_channels: Some(vec![2, 0]),
        ..InputConfig::new(&addr)
    })
    .unwrap();
    let a = block(&input, |_| true);
    assert_eq!(&a.header.fourcc, b"IDF3");
    assert_eq!(a.source_channels(), Some(vec![2, 0]));
    assert_eq!(levels(&a), vec![level(2), level(0)]);
    assert_eq!(a.channels[0].len(), 480);

    // 2. A new list mid-session, including a channel the source lacks: switches at a
    // block boundary, each block saying what it holds.
    input.set_audio_channels(Some(vec![3, 9, 1]));
    let a = block(&input, |a| a.source_channels() != Some(vec![2, 0]));
    assert_eq!(a.source_channels(), Some(vec![3, 1]));
    assert_eq!(levels(&a), vec![level(3), level(1)]);

    // 3. Every channel again.
    input.set_audio_channels(None);
    let a = block(&input, |a| a.source_channels().is_none());
    assert_eq!(levels(&a), (0..4).map(level).collect::<Vec<_>>());
    drop(input);

    // 4. A receiver that asks for neither channels nor lossless sound (the baseline, as an
    // NDI receiver): whole blocks, no channel metadata, `fowt`.
    let plain = Input::open(InputConfig {
        receive_video: false,
        lossless_audio: false,
        ..InputConfig::new(&addr)
    })
    .unwrap();
    let a = block(&plain, |_| true);
    assert_eq!(a.channels.len(), 4);
    assert_eq!(a.source_channels(), None);
    assert_eq!(&a.header.fourcc, b"fowt");

    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}
