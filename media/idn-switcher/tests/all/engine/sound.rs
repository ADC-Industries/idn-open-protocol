//! Sound: noise suppression, a scene's inputs' sound, sync offsets, the monitor, and
//! closed captions from a captions source.

use super::*;

/// Noise suppression on the stream's sound turns on and off, and the frame clock runs the
/// sound through it without trouble.
#[test]
fn stream_sound_noise_suppression() {
    let sw = Switcher::start(SwitcherConfig {
        name: "DenoiseTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    sw.add_input("A", Source::Colour(Ycc::from_rgb(90, 90, 90)))
        .unwrap();
    sw.audio_denoise(true).unwrap();
    assert_eq!(sw.state()["audio"]["denoise"].as_bool(), Some(true));
    std::thread::sleep(Duration::from_millis(300));
    sw.audio_denoise(false).unwrap();
    assert_eq!(sw.state()["audio"]["denoise"].as_bool(), Some(false));
}

/// A scene carries its inputs' sound, each at its item's level (§S.12): following program
/// with a scene on it hears every input in it, a muted or hidden item's not, a nested
/// scene's inputs at both levels, an input twice at its loudest.
#[test]
fn a_scene_carries_its_inputs_sound() {
    use idn_switcher::scene::{Item, ItemSource};
    let sw = Switcher::start(SwitcherConfig {
        name: "SceneSound".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let grey = Source::Colour(Ycc::from_rgb(90, 90, 90));
    let a = sw.add_input("A", grey.clone()).unwrap();
    let b = sw.add_input("B", grey.clone()).unwrap();
    let c = sw.add_input("C", grey).unwrap();
    let inner = sw.add_input("Inner", Source::Scene).unwrap();
    let sc = sw.add_input("Scene", Source::Scene).unwrap();
    let item = |id: u32, volume_db: f32| Item {
        source: ItemSource::Input(id),
        volume_db,
        ..Default::default()
    };
    sw.scene_items(inner, vec![item(c, -6.0)]).unwrap();
    sw.scene_items(
        sc,
        vec![
            item(a, 0.0),
            item(b, -10.0),
            item(a, -20.0),
            item(inner, -6.0),
        ],
    )
    .unwrap();
    sw.program(sc).unwrap();
    let heard = || {
        let mut v = Vec::new();
        wait_until(Duration::from_secs(2), || {
            // (A moment first: a change just made reaches the state.)
            std::thread::sleep(Duration::from_millis(60));
            v = sw.state()["audio"]["heard"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    (
                        h["id"].as_u64().unwrap() as u32,
                        h["gain_db"].as_f64().unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            !v.is_empty()
        });
        v
    };
    // Loudest first; A at its loudest item; C at −6 inside −6.
    assert_eq!(heard(), vec![(a, 0.0), (b, -10.0), (c, -12.0)]);
    // Balanced and muted live, by the layer's place or its id.
    use idn_switcher::scene::LayerRef;
    sw.scene_item_sound(sc, LayerRef::Index(1), Some(-3.0), None)
        .unwrap();
    let fourth = sw.scene(sc).unwrap()[3].id;
    sw.scene_item_sound(sc, LayerRef::Id(fourth), None, Some(true))
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let h = heard();
    assert_eq!(h[..2], [(a, 0.0), (b, -3.0)]);
    assert_eq!(h[2].0, c);
    assert!(h[2].1 <= -100.0, "muted: {h:?}");
    assert!(
        sw.scene_item_sound(sc, LayerRef::Index(9), Some(0.0), None)
            .is_err()
    );
    assert!(
        sw.scene_item_sound(sc, LayerRef::Id(999), Some(0.0), None)
            .is_err()
    );
    assert!(
        sw.scene_item_sound(a, LayerRef::Index(0), Some(0.0), None)
            .is_err(),
        "not a scene"
    );
    // An input alone in its scene on program: itself alone.
    sw.program(scene_of(&sw, b)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(heard(), vec![(b, 0.0)]);
}

/// An input's sound sync offset is kept per input, capped at 500 ms, and shown in the state.
#[test]
fn per_input_sound_sync_offset() {
    let sw = Switcher::start(SwitcherConfig {
        name: "OffsetTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let a = sw
        .add_input("A", Source::Colour(Ycc::from_rgb(90, 90, 90)))
        .unwrap();
    let b = sw
        .add_input("B", Source::Colour(Ycc::from_rgb(20, 20, 20)))
        .unwrap();
    let off = |id: u32| {
        sw.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"].as_u64() == Some(id as u64))
            .unwrap()["audio_offset_ms"]
            .as_u64()
    };
    assert_eq!(off(a), Some(0));
    sw.input_audio_offset(a, 80).unwrap();
    sw.input_audio_offset(b, 9_000).unwrap();
    assert_eq!(off(a), Some(80));
    assert_eq!(off(b), Some(500), "capped at 500 ms");
    assert!(sw.input_audio_offset(999, 10).is_err());
    // Following program with offsets set: the frame clock re-makes the lane, no trouble.
    sw.program(scene_of(&sw, b)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
}

/// An output sending a grey picture and a 1 kHz tone at `amp` 30 times a second.
fn tone(publisher: &Publisher, name: &str, amp: f32) -> (Arc<Output>, Arc<AtomicBool>) {
    let out = Arc::new(Output::new(publisher, OutputConfig::new(name)).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut k = 0usize;
        let start = Instant::now();
        let mut n = 0u32;
        while !s.load(Ordering::Relaxed) {
            let wave: Vec<f32> = (0..1600)
                .map(|i| amp * ((k + i) as f32 * 2.0 * std::f32::consts::PI / 48.0).sin())
                .collect();
            k += 1600;
            let f = Frame::solid(W, H, Ycc::from_rgb(90, 90, 90));
            let _ = o.send_planes(PlanarFrame {
                layout: Fourcc::Shq2,
                width: W,
                height: H,
                planes: Arc::new(f.into_planes()),
                timestamp: None,
                timecode: None,
                audio: Some(idn_open_protocol::api::AudioBlock {
                    sample_rate: 48_000,
                    channels: vec![wave.clone(), wave],
                }),
                coded: None,
            });
            n += 1;
            // On a steady 30 a second, not 33 ms after each send.
            let next = start + Duration::from_micros(33_333 * n as u64);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    (out, stop)
}

/// The monitor (§S.24): the stream's sound as it goes out, preview's, a solo, volume and
/// mute, through Listen; the inputs' and the stream's meters.
#[test]
fn the_monitor_hears_as_streamed_preview_and_solo() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let (a, stop_a) = tone(&publisher, "Loud", 0.5);
    let (b, stop_b) = tone(&publisher, "Quiet", 0.1);
    let sw = Switcher::start(SwitcherConfig {
        name: "MonitorTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let ia = sw
        .add_input("Loud", Source::Network(format!("127.0.0.1:{}", a.port())))
        .unwrap();
    let ib = sw
        .add_input("Quiet", Source::Network(format!("127.0.0.1:{}", b.port())))
        .unwrap();
    // Loud's scene on program, Quiet's on preview (both received: in use).
    let (_, _) = (scene_of(&sw, ia), scene_of(&sw, ib));
    // The peak heard over the next 0.6 s (after 0.4 s to settle).
    let mut from = 0u64;
    let mut heard = |sw: &Switcher| {
        let end = Instant::now() + Duration::from_secs(1);
        let mut peak = 0f32;
        let settle = Instant::now() + Duration::from_millis(400);
        while Instant::now() < end {
            let (first, pcm) = sw.monitor_listen(from, Duration::from_millis(200));
            from = first + pcm.len() as u64 / 2;
            if Instant::now() > settle {
                peak = pcm
                    .iter()
                    .fold(peak, |m, &v| m.max(v.abs() as f32 / 32767.0));
            }
        }
        peak
    };
    let near = |v: f32, want: f32| (v - want).abs() < want * 0.15 + 0.01;
    // The stream's sound follows program (Loud).
    let p = heard(&sw);
    assert!(near(p, 0.5), "the stream: {p}");
    let m = sw.meters();
    let db = |v: &serde_json::Value| v[0].as_f64().unwrap() as f32;
    assert!((db(&m["inputs"][ia.to_string()]) + 6.0).abs() < 1.5, "{m}");
    assert!((db(&m["inputs"][ib.to_string()]) + 20.0).abs() < 1.5, "{m}");
    assert!((db(&m["stream"]) + 6.0).abs() < 1.5, "{m}");
    // Preview's sound (Quiet) instead.
    sw.monitor(Some(true), None, None, None, None).unwrap();
    let p = heard(&sw);
    assert!(near(p, 0.1), "preview: {p}");
    // A solo wins over preview: Loud alone.
    sw.monitor_solo(ia, true).unwrap();
    let p = heard(&sw);
    assert!(near(p, 0.5), "solo: {p}");
    assert_eq!(sw.state()["monitor"]["solo"], serde_json::json!([ia]));
    // Back to the stream, 6 dB down; then muted. The stream itself is untouched.
    sw.monitor_solo(ia, false).unwrap();
    sw.monitor(Some(false), None, Some(-6.0), None, None)
        .unwrap();
    let p = heard(&sw);
    assert!(near(p, 0.25), "the stream at -6 dB: {p}");
    sw.monitor(None, None, None, Some(true), None).unwrap();
    let p = heard(&sw);
    assert!(p < 0.001, "muted: {p}");
    assert!((db(&sw.meters()["stream"]) + 6.0).abs() < 1.5);
    // A removed input leaves the solo.
    sw.monitor_solo(ib, true).unwrap();
    sw.remove_input(ib).unwrap();
    assert_eq!(sw.state()["monitor"]["solo"], serde_json::json!([]));
    assert!(sw.monitor_solo(999, true).is_err());
    stop_a.store(true, Ordering::Relaxed);
    stop_b.store(true, Ordering::Relaxed);
}

/// Closed captions from a captions source (G2, idn-captions.md): its final lines become
/// the program's captions, each once; drafts and repeats don't.
#[test]
fn closed_captions_come_from_a_captions_source() {
    use idn_open_protocol::text::{CaptionLine, TextSource};
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let src = TextSource::new(&publisher, "Captions").unwrap();
    let sw = Switcher::start(SwitcherConfig {
        width: 320,
        height: 180,
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    sw.captions_from(Some(format!("127.0.0.1:{}", src.output().port())));
    std::thread::sleep(Duration::from_millis(800));
    let say = |line: u64, text: &str, is_final: bool| {
        src.send_caption(&CaptionLine {
            line,
            text: text.into(),
            is_final,
            lang: "en".into(),
            ..Default::default()
        })
    };
    say(1, "Welcome", false);
    say(1, "Welcome, everyone.", true);
    say(2, "Please stand", false);
    wait_until(Duration::from_secs(10), || {
        sw.state()["captions"]["recent"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    });
    assert_eq!(
        sw.state()["captions"]["recent"],
        serde_json::json!(["Welcome, everyone."])
    );
    // The repeat (every 5 s) adds nothing; the next final line does.
    say(2, "Please stand for the reading.", true);
    wait_until(Duration::from_secs(10), || {
        sw.state()["captions"]["recent"]
            .as_array()
            .map_or(0, |a| a.len())
            >= 2
    });
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(
        sw.state()["captions"]["recent"],
        serde_json::json!(["Welcome, everyone.", "Please stand for the reading."])
    );
    assert_eq!(
        sw.state()["captions"]["from"],
        format!("127.0.0.1:{}", src.output().port())
    );
}
