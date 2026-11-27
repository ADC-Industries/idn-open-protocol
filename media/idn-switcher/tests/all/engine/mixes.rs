//! Mixes and shots: independent and vertical mixes, virtual shots, PTZ presets.

use super::*;

/// Independent mixes (§S.15): a vertical mix with its own program, at its own size, its own
/// transition; a scene drawn at the mix's size; removing it.
#[test]
fn an_independent_vertical_mix() {
    let sw = Switcher::start(SwitcherConfig {
        name: "MixTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let ra = sw
        .add_input("A", Source::Colour(Ycc(60, 128, 128)))
        .unwrap();
    let rb = sw
        .add_input("B", Source::Colour(Ycc(200, 128, 128)))
        .unwrap();
    let (a, b) = (scene_of(&sw, ra), scene_of(&sw, rb));
    assert!(
        sw.add_mix("MixTest", 360, 640).is_err(),
        "not the program's name"
    );
    assert!(sw.add_mix("Odd", 361, 640).is_err());
    let v = sw.add_mix("Vertical", 360, 640).unwrap();
    let got = Arc::new(Mutex::new((0usize, 0usize, 0u8)));
    let g = got.clone();
    let _tap = sw
        .tap_mix(v, move |f| {
            if let Some(y) = f.planes.first() {
                *g.lock_ok() = (
                    f.width,
                    f.height,
                    y.data[(f.height / 2) * f.width + f.width / 2],
                );
            }
        })
        .unwrap();
    let wait = |want: (usize, usize, u8), what: &str| {
        wait_until(Duration::from_secs(3), || *got.lock_ok() == want);
        assert_eq!(*got.lock_ok(), want, "{what}");
    };
    // A new mix starts on program's input (A, the first).
    wait((360, 640, 60), "the new mix on program's A");
    // The main program shows A; the mix shows B, at 360 × 640 (a scene: an input refused).
    assert!(sw.mix_program(v, rb).is_err());
    sw.mix_program(v, b).unwrap();
    wait((360, 640, 200), "B on the vertical mix");
    assert_eq!(
        sw.state()["program"].as_u64(),
        Some(a as u64),
        "the program its own"
    );
    // Its own transition: a mix to A over 0.3 s.
    sw.mix_preview(v, a).unwrap();
    sw.mix_transition(v, Kind::Mix, 0.3).unwrap();
    assert!(sw.mix_transition(v, Kind::Stinger(1), 1.0).is_err());
    sw.mix_auto(v).unwrap();
    wait((360, 640, 60), "A after the mix's transition");
    // (The picture is fully A a frame before the transition completes.)
    wait_until(Duration::from_secs(2), || {
        sw.state()["mixes"][0]["transition"]["running"] != true
    });
    let m = sw.state()["mixes"][0].clone();
    assert_eq!(
        (m["program"].as_u64(), m["preview"].as_u64()),
        (Some(a as u64), Some(b as u64))
    );
    // A scene on the mix is drawn at the mix's size: an item on its top half only.
    let sc = sw.add_input("Vertical scene", Source::Scene).unwrap();
    sw.scene_items(
        sc,
        vec![Item {
            source: ItemSource::Input(rb),
            rect: (0.0, 0.5, 1.0, 0.5),
            ..Default::default()
        }],
    )
    .unwrap();
    sw.mix_program(v, sc).unwrap();
    wait((360, 640, 200), "the scene's item, at the mix's centre");
    sw.remove_mix(v).unwrap();
    assert!(sw.state()["mixes"].as_array().unwrap().is_empty());
    assert!(sw.tap_mix(v, |_| {}).is_err());
}

/// A virtual shot (§S.21): a zoomed part of another input, moved smoothly.
#[test]
fn a_virtual_shot_crops_and_moves() {
    use idn_switcher::View;
    let sw = Switcher::start(SwitcherConfig {
        name: "VirtualTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    // The wide camera: dark on the left half, bright on the right.
    let cam = sw.add_input("Wide", Source::Feed).unwrap();
    let mut f = Frame::solid(W, H, Ycc(40, 128, 128));
    for y in 0..H {
        for x in W / 2..W {
            f.y[y * W + x] = 200;
        }
    }
    sw.feed(cam, Arc::new(f)).unwrap();
    let shot = sw
        .add_input(
            "Left",
            Source::Virtual {
                input: cam,
                view: View {
                    x: 0.25,
                    y: 0.5,
                    zoom: 2.0,
                },
            },
        )
        .unwrap();
    let seen = Arc::new(Mutex::new((0u8, 0u8)));
    let g = seen.clone();
    let _tap = sw.program_output().tap(move |f| {
        if let Some(y) = f.planes.first() {
            *g.lock_ok() = (y.data[(H / 2) * W + 4], y.data[(H / 2) * W + W - 4]);
        }
    });
    sw.program(scene_of(&sw, shot)).unwrap();
    let wait = |want: (u8, u8), what: &str| {
        wait_until(Duration::from_secs(4), || *seen.lock_ok() == want);
        assert_eq!(*seen.lock_ok(), want, "{what}");
    };
    wait((40, 40), "the left quarter, zoomed: dark across");
    sw.virtual_view(
        shot,
        View {
            x: 0.75,
            y: 0.5,
            zoom: 2.0,
        },
        0.5,
    )
    .unwrap();
    let st = sw.state();
    let me = st["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == shot)
        .unwrap()
        .clone();
    assert_eq!(me["source"]["virtual"]["moving"], true);
    wait((200, 200), "moved to the right quarter");
    // The whole picture: dark then bright.
    sw.virtual_view(shot, View::WHOLE, 0.0).unwrap();
    wait((40, 200), "whole");
    // Over the edge: kept inside the picture.
    let c = View {
        x: 0.0,
        y: 0.0,
        zoom: 4.0,
    }
    .crop(1920, 1080, 1920, 1080);
    assert_eq!(c, (0.0, 0.0, 0.25, 0.25));
    assert!(sw.virtual_view(cam, View::WHOLE, 0.0).is_err());
}

/// PTZ presets as inputs (§S.21) over VISCA: a preset input on preview moves the camera
/// there; one put on preview while the camera is on air through another doesn't; cut to
/// program, it does.
#[test]
fn ptz_presets_move_the_camera_from_preview() {
    use idn_switcher::ptz::{Control, Move, Visca};
    let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(1500)))
        .unwrap();
    let sw = Switcher::start(SwitcherConfig {
        name: "PtzTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let cam = sw.add_input("Cam", Source::Feed).unwrap();
    sw.feed(cam, Arc::new(Frame::solid(W, H, Ycc(90, 128, 128))))
        .unwrap();
    let other = sw.add_input("Other", Source::Colour(Ycc::BLACK)).unwrap();
    sw.set_camera(
        cam,
        Some(Control::Visca {
            how: Visca::Udp,
            address: rx.local_addr().unwrap().to_string(),
        }),
    )
    .unwrap();
    let p3 = sw
        .add_input(
            "Cam · P3",
            Source::Ptz {
                camera: cam,
                preset: 3,
            },
        )
        .unwrap();
    let p4 = sw
        .add_input(
            "Cam · P4",
            Source::Ptz {
                camera: cam,
                preset: 4,
            },
        )
        .unwrap();
    assert!(
        sw.add_input(
            "Bad",
            Source::Ptz {
                camera: cam,
                preset: 0
            }
        )
        .is_err()
    );
    let recalled = || -> Option<u8> {
        let mut b = [0u8; 32];
        let n = rx.recv(&mut b).ok()?;
        (b[..4] == [0x81, 0x01, 0x04, 0x3F] && b[4] == 0x02 && n == 7).then_some(b[5] + 1)
    };
    // Each in a scene (only scenes go on the buses): the preset moves from its scene.
    sw.program(scene_of(&sw, other)).unwrap();
    sw.preview(scene_of(&sw, p3)).unwrap();
    assert_eq!(recalled(), Some(3), "preset 3 on preview: the camera moves");
    // The preset input shows the camera's picture.
    wait_until(Duration::from_secs(2), || {
        sw.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == p3)
            .unwrap()["width"]
            == W
    });
    sw.cut().unwrap();
    // P3 on air: P4 on preview must not move the camera.
    sw.preview(scene_of(&sw, p4)).unwrap();
    assert_eq!(recalled(), None, "the camera is on air: no move");
    sw.cut().unwrap();
    assert_eq!(recalled(), Some(4), "cut to P4: the camera moves");
    // The state says it once the move is through (it was read at once and was 3 now and
    // then under a loaded test run).
    let camera_preset = || {
        sw.state()["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == cam)
            .unwrap()["camera"]["preset"]
            .clone()
    };
    wait_until(Duration::from_secs(2), || camera_preset() == 4);
    assert_eq!(camera_preset(), 4);
    // By hand: a store through a preset input reaches the camera.
    sw.ptz(p3, Move::Store { preset: 5 }).unwrap();
    let mut b = [0u8; 32];
    let n = rx.recv(&mut b).unwrap();
    assert_eq!(&b[..n], [0x81, 0x01, 0x04, 0x3F, 0x01, 0x04, 0xFF]);
    assert!(sw.ptz(other, Move::Home).is_err(), "no control set");
}

/// A virtual shot on a mix (§S.15, §S.21): framed as on the program, fitted to the mix;
/// on the GPU its camera cropped there (its worker then makes no whole pictures).
#[test]
fn a_virtual_shot_on_a_mix() {
    virtual_shot_on_a_mix(false);
}

#[test]
fn a_virtual_shot_on_a_mix_on_the_gpu() {
    virtual_shot_on_a_mix(true);
}

fn virtual_shot_on_a_mix(gpu: bool) {
    use idn_switcher::View;
    let sw = Switcher::start(SwitcherConfig {
        name: "VirtualMix".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu,
        ..Default::default()
    })
    .unwrap();
    // The wide camera: dark on the left half, bright on the right.
    let cam = sw.add_input("Wide", Source::Feed).unwrap();
    let mut f = Frame::solid(W, H, Ycc(40, 128, 128));
    for y in 0..H {
        for x in W / 2..W {
            f.y[y * W + x] = 200;
        }
    }
    let f = Arc::new(f);
    let shot = sw
        .add_input(
            "Left",
            Source::Virtual {
                input: cam,
                view: View {
                    x: 0.25,
                    y: 0.5,
                    zoom: 2.0,
                },
            },
        )
        .unwrap();
    let v = sw.add_mix("Vertical", 360, 640).unwrap();
    sw.mix_program(v, scene_of(&sw, shot)).unwrap();
    // The middle of the mix, and a row above the fitted 16:9 shot (black).
    let seen = Arc::new(Mutex::new((0u8, 0u8)));
    let g = seen.clone();
    let _tap = sw
        .tap_mix(v, move |f| {
            if let Some(y) = f.planes.first() {
                *g.lock_ok() = (
                    y.data[(f.height / 2) * f.width + f.width / 2],
                    y.data[10 * f.width + f.width / 2],
                );
            }
        })
        .unwrap();
    let wait = |want: (u8, u8), what: &str| {
        wait_until(Duration::from_secs(4), || {
            if *seen.lock_ok() == want {
                return true;
            }
            // A camera sends pictures on: fed again, as a camera's would arrive.
            sw.feed(cam, f.clone()).unwrap();
            false
        });
        assert_eq!(*seen.lock_ok(), want, "{what} (gpu {gpu})");
    };
    wait((40, 16), "the left quarter, letterboxed");
    sw.virtual_view(
        shot,
        View {
            x: 0.75,
            y: 0.5,
            zoom: 2.0,
        },
        0.0,
    )
    .unwrap();
    wait((200, 16), "the right quarter");
}

/// A mix following the program ("also make a vertical version"): the program's
/// scene in its own shape at once and after a cut; its own buses refused; kept as
/// following in the state.
#[test]
fn a_vertical_version_follows_the_program() {
    let sw = Switcher::start(SwitcherConfig {
        name: "FollowTest".into(),
        width: W,
        height: H,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })
    .unwrap();
    let ra = sw
        .add_input("A", Source::Colour(Ycc(60, 128, 128)))
        .unwrap();
    let rb = sw
        .add_input("B", Source::Colour(Ycc(200, 128, 128)))
        .unwrap();
    let (a, b) = (scene_of(&sw, ra), scene_of(&sw, rb));
    let v = sw.add_following_mix("Vertical", 360, 640).unwrap();
    let got = Arc::new(Mutex::new((0usize, 0usize, 0u8)));
    let g = got.clone();
    let _tap = sw
        .tap_mix(v, move |f| {
            if let Some(y) = f.planes.first() {
                *g.lock_ok() = (
                    f.width,
                    f.height,
                    y.data[(f.height / 2) * f.width + f.width / 2],
                );
            }
        })
        .unwrap();
    let wait = |want: (usize, usize, u8), what: &str| {
        wait_until(Duration::from_secs(3), || *got.lock_ok() == want);
        assert_eq!(*got.lock_ok(), want, "{what}");
    };
    wait((360, 640, 60), "the program's A, vertical");
    assert!(
        sw.mix_program(v, b)
            .unwrap_err()
            .contains("follows the program")
    );
    sw.program(b).unwrap();
    wait((360, 640, 200), "the program's B after the cut");
    let m = sw.state()["mixes"][0].clone();
    assert_eq!(m["follows_program"], true);
    assert_eq!(m["program"].as_u64(), Some(b as u64));
    // A scene's thumbnail, drawn though it is on no bus (A is on preview only now), and an
    // input that is no scene refused.
    sw.preview(b).unwrap();
    let j = sw.scene_jpeg(a, 320).unwrap();
    assert_eq!(&j[..2], &[0xFF, 0xD8]);
    assert!(sw.scene_jpeg(ra, 320).is_err());
}
