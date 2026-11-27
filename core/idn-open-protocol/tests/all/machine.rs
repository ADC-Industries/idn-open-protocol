//! Pictures and sound between programs on one machine (`machine/1`, §E.26): an output's
//! pictures and sound go to its receivers on this machine through rings of shared memory,
//! not encoded, encrypted or sent; the connection carries only setup and control.

use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn receivers_on_this_machine_take_pictures_from_the_ring() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Ring")).unwrap();
    let addr = format!("127.0.0.1:{}", out.port());
    let inputs: Vec<Input> = (0..2)
        .map(|i| {
            Input::open(InputConfig {
                receiver_name: format!("R{i}"),
                ..InputConfig::new(&addr)
            })
            .unwrap()
        })
        .collect();
    // Mid grey, 320 x 180, and sound with each picture.
    let px: Arc<[u8]> = vec![128u8; 320 * 180 * 4].into();
    let mut via = vec![Vec::<&'static str>::new(); 2];
    let mut sound = [0usize; 2];
    let mut luma = [0f64; 2];
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end
        && !(via
            .iter()
            .all(|v| v.iter().filter(|x| **x == "machine").count() >= 20)
            && sound.iter().all(|&n| n > 5))
    {
        let _ = out.send(VideoFrame::rgba(320, 180, px.clone(), AlphaMode::Opaque));
        let _ = out.send_audio(
            AudioBlock {
                sample_rate: 48000,
                channels: vec![vec![0.25; 1600]; 2],
            },
            None,
        );
        for (i, input) in inputs.iter().enumerate() {
            while let Some(ev) = input.recv(Duration::from_millis(1)) {
                match ev {
                    InputEvent::Video(v) => {
                        via[i].push(v.via);
                        if v.via == "machine" {
                            assert_eq!((v.picture.width, v.picture.height), (320, 180));
                            let y = &v.picture.planes[0].data;
                            luma[i] = y.iter().map(|&b| b as f64).sum::<f64>() / y.len() as f64;
                        }
                    }
                    InputEvent::Audio(a) if a.via == "machine" => {
                        sound[i] += 1;
                        assert_eq!(a.channels.len(), 2);
                        assert!((a.channels[1][0] - 0.25).abs() < 1e-6, "float, as sent");
                    }
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    // The source: both on the ring, and nothing encoded since the ring was taken (a
    // picture or two over the connection before).
    let st = out.stats();
    assert_eq!(st.machine_readers, 2, "{st:?}");
    let encodes = st.shq_encodes;
    for _ in 0..10 {
        let _ = out.send(VideoFrame::rgba(320, 180, px.clone(), AlphaMode::Opaque));
        std::thread::sleep(Duration::from_millis(33));
    }
    assert_eq!(
        out.stats().shq_encodes,
        encodes,
        "no SpeedHQ encode for receivers here"
    );
    assert!(encodes <= 4, "{encodes} encodes before the ring was taken");
    // The ring goes with its last reader.
    drop(inputs);
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..5 {
        let _ = out.send(VideoFrame::rgba(320, 180, px.clone(), AlphaMode::Opaque));
        std::thread::sleep(Duration::from_millis(33));
    }
    assert_eq!(out.stats().machine_readers, 0);
    for i in 0..2 {
        let n = via[i].iter().filter(|x| **x == "machine").count();
        assert!(
            n >= 20,
            "receiver {i}: {n} pictures through the ring of {}",
            via[i].len()
        );
        assert!(sound[i] > 5, "receiver {i}: sound through the ring");
        // Mid grey: Y' about 126 (limited range), as the encoder would have had it.
        assert!(
            (luma[i] - 126.0).abs() < 4.0,
            "receiver {i}: luma {}",
            luma[i]
        );
        // The first pictures came over the connection, until the ring was taken.
        eprintln!(
            "receiver {i}: {} over the connection first, then {n} through the ring",
            via[i].iter().take_while(|x| **x != "machine").count()
        );
    }
}

#[test]
fn a_desk_taking_only_sound_gets_it_from_the_ring_in_order() {
    // As the desk takes a source: no pictures asked for, short blocks, many channels.
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Sound ring")).unwrap();
    let input = Input::open(InputConfig {
        receiver_name: "Desk".into(),
        receive_video: false,
        ..InputConfig::new(&format!("127.0.0.1:{}", out.port()))
    })
    .unwrap();
    // 2 ms blocks of 8 channels; each sample is its running number (channel c adds c).
    let (block, channels) = (96usize, 8usize);
    let mut sent = 0usize;
    let mut next: Option<usize> = None;
    let (mut via_ring, mut gaps) = (0usize, 0usize);
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && via_ring < 400 {
        let ch: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                (0..block)
                    .map(|i| ((sent + i) % 100_000 + c) as f32)
                    .collect()
            })
            .collect();
        sent += block;
        let _ = out.send_audio(
            AudioBlock {
                sample_rate: 48000,
                channels: ch,
            },
            None,
        );
        while let Some(ev) = input.recv(Duration::ZERO) {
            if let InputEvent::Audio(a) = ev {
                if a.via == "machine" {
                    via_ring += 1;
                } else {
                    assert_eq!(via_ring, 0, "over the connection after the ring took over");
                }
                assert_eq!(a.channels.len(), channels);
                let first = a.channels[0][0].round() as usize;
                assert_eq!(
                    a.channels[5][0].round() as usize,
                    first + 5,
                    "channels in place"
                );
                // Every block once, in order, over the connection and then the ring: none
                // twice or lost where the ring takes over.
                if next.is_some_and(|n| n != first) {
                    gaps += 1;
                    eprintln!(
                        "GAP want {next:?} got {first} len {} via {} ring# {via_ring}",
                        a.channels[0].len(),
                        a.via
                    );
                }
                next = Some((first + a.channels[0].len()) % 100_000);
            }
        }
        // In bursts of four (8 ms of sound at once), so blocks are on their way while
        // the ring is being taken.
        if sent.is_multiple_of(block * 4) {
            std::thread::sleep(Duration::from_millis(8));
        }
    }
    assert!(via_ring >= 400, "{via_ring} blocks through the ring");
    assert_eq!(gaps, 0, "blocks in order, none lost or twice");
    assert_eq!(out.stats().machine_readers, 1);
}
