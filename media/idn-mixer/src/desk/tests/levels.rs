//! Levels: faders and pans, automix and the ducker, audio follow video, sends, linked
//! pairs, out-of-range and NaN levels, an output's delay, fades in.

use super::*;

#[test]
fn unity_passes_and_faders_and_pan_do_what_they_say() {
    let mut d = Desk::new(4);
    // EQ flat at 0 dB gain bands, centre pan: −3 dB each side.
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    let l = to_db(peak(&out[0])) - to_db(0.5);
    assert!((l + 3.01).abs() < 0.1, "centre: {l}");
    assert!((peak(&out[0]) - peak(&out[1])).abs() < 1e-4);
    // Hard left at −6 dB.
    let mut s = d.settings().clone();
    s.channels[0].pan = -1.0;
    s.channels[0].fader = -6.0;
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    let l = to_db(peak(&out[0])) - to_db(0.5);
    assert!((l + 6.0).abs() < 0.1, "left: {l}");
    assert!(peak(&out[1]) < 1e-4);
    // Muted: nothing.
    let mut s = d.settings().clone();
    s.channels[0].mute = true;
    d.set(s, false);
    let out = run(&mut d, 5, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    assert!(peak(&out[0]) < 1e-6);
}

/// The automix (§M.12): the one talking gets the level, the quiet ones go down; all
/// quiet, an equal share (−3 dB each of two); weights and pre-fader sends.
#[test]
fn automix_shares_the_level() {
    let mut d = Desk::new(4);
    let mut s = d.settings().clone();
    s.automix = true;
    s.channels[0].automix = true;
    s.channels[1].automix = true;
    d.set(s, false);
    let auto = |d: &Desk, i: usize| d.meters().channel_auto[i];
    // All quiet from the start: an equal share each (two open mics: −3 dB).
    run(&mut d, 10, |_| vec![vec![0.0; BLOCK]; 2]);
    assert!((auto(&d, 0) + 3.01).abs() < 0.05 && (auto(&d, 1) + 3.01).abs() < 0.05);
    run(&mut d, 100, |b| {
        vec![sine(500.0, 0.3, b * BLOCK), vec![0.0; BLOCK]]
    });
    assert!(auto(&d, 0) > -0.1, "talking: {}", auto(&d, 0));
    assert!(auto(&d, 1) < -40.0, "quiet: {}", auto(&d, 1));
    assert_eq!(auto(&d, 2), 0.0, "not in the automix");
    // The talker stops: their mic stays open (the last one held), so the room doesn't
    // drop out between sentences.
    run(&mut d, 100, |_| vec![vec![0.0; BLOCK]; 2]);
    assert!(
        auto(&d, 0) > -0.1 && auto(&d, 1) < -20.0,
        "{} {}",
        auto(&d, 0),
        auto(&d, 1)
    );
    // Both talking alike: −3 dB each; a +6 dB weight takes most of it.
    let both = |b: usize| vec![sine(500.0, 0.3, b * BLOCK), sine(700.0, 0.3, b * BLOCK)];
    run(&mut d, 100, both);
    assert!((auto(&d, 0) + 3.01).abs() < 0.3, "{}", auto(&d, 0));
    let mut s = d.settings().clone();
    s.channels[0].automix_weight = 6.0;
    d.set(s, false);
    run(&mut d, 100, both);
    assert!(
        auto(&d, 0) > -1.2 && auto(&d, 1) < -6.0,
        "{} {}",
        auto(&d, 0),
        auto(&d, 1)
    );
    // Off: nothing done.
    let mut s = d.settings().clone();
    s.automix = false;
    d.set(s, false);
    run(&mut d, 5, both);
    assert_eq!((auto(&d, 0), auto(&d, 1)), (0.0, 0.0));
}

/// A ducker (§M.12): speech over the threshold brings the music down by its depth,
/// holds, then lets it back up.
#[test]
fn a_ducker_brings_the_music_down_while_someone_speaks() {
    let mut d = Desk::new(4);
    let mut s = d.settings().clone();
    let k = &mut s.duckers[0];
    k.on = true;
    k.keys[0] = true;
    k.targets[1] = true;
    (k.depth, k.attack, k.hold, k.release) = (-15.0, 20.0, 200.0, 300.0);
    d.set(s, false);
    let auto = |d: &Desk| d.meters().channel_auto[1];
    let music = |b: usize| sine(300.0, 0.2, b * BLOCK);
    run(&mut d, 50, |b| vec![vec![0.0; BLOCK], music(b)]);
    assert!(auto(&d) > -0.01, "no speech: {}", auto(&d));
    // Speech: down by 15 dB within its attack (a few time constants).
    run(&mut d, 100, |b| {
        vec![sine(1000.0, 0.2, b * BLOCK), music(b)]
    });
    assert!((auto(&d) + 15.0).abs() < 0.2, "speaking: {}", auto(&d));
    // Speech stops: still down through the hold (0.2 s = 100 blocks)…
    run(&mut d, 50, |b| vec![vec![0.0; BLOCK], music(b)]);
    assert!(auto(&d) < -14.0, "holding: {}", auto(&d));
    // …then back up over the release.
    run(&mut d, 1500, |b| vec![vec![0.0; BLOCK], music(b)]);
    assert!(auto(&d) > -0.1, "released: {}", auto(&d));
    // Quiet speech, under the threshold: nothing.
    run(&mut d, 200, |b| {
        vec![sine(1000.0, 0.001, b * BLOCK), music(b)]
    });
    assert!(auto(&d) > -0.1);
}

/// Audio follows video (§M.12): a following channel up while its input is on air,
/// off when not; nothing heard from the switcher: up.
#[test]
fn audio_follows_video() {
    let mut d = Desk::new(4);
    let mut s = d.settings().clone();
    s.afv = Afv {
        on: true,
        switcher: "HOST (Switcher)".into(),
        off: -144.0,
        fade: 100.0,
    };
    s.channels[0].follows = "Cam 1".into();
    s.channels[1].follows = "Cam 2".into();
    d.set(s, false);
    let auto = |d: &Desk, i: usize| d.meters().channel_auto[i];
    run(&mut d, 5, |_| vec![vec![0.0; BLOCK]; 2]);
    assert_eq!((auto(&d, 0), auto(&d, 1)), (0.0, 0.0), "no switcher: up");
    d.set_on_air(Some(["Cam 1".to_owned()].into()));
    run(&mut d, 100, |_| vec![vec![0.0; BLOCK]; 2]);
    assert_eq!(auto(&d, 0), 0.0);
    assert!(auto(&d, 1) <= -144.0, "off air: {}", auto(&d, 1));
    // Cut to Cam 2: it fades up over 100 ms (50 blocks), Cam 1 down.
    d.set_on_air(Some(["Cam 2".to_owned()].into()));
    run(&mut d, 25, |_| vec![vec![0.0; BLOCK]; 2]);
    assert!(
        auto(&d, 1) > -10.0 && auto(&d, 1) < -3.0,
        "halfway: {}",
        auto(&d, 1)
    );
    run(&mut d, 30, |_| vec![vec![0.0; BLOCK]; 2]);
    assert_eq!(auto(&d, 1), 0.0);
    assert!(auto(&d, 0) <= -144.0);
}

/// The built-in effects (§M.14): a new desk's FX 1 is a reverb; a channel sent to it
/// comes back on the main mix, and goes on ringing after the channel stops; a graphic
/// EQ on the main bus lifts its band.
#[test]
fn a_send_to_the_reverb_returns_with_a_tail() {
    let mut d = Desk::new(2);
    assert_eq!(
        d.settings().fx[0].effect.kind,
        crate::fx::EffectKind::Reverb
    );
    assert_eq!(d.settings().fx[0].name, "Reverb");
    let mut s = d.settings().clone();
    s.channels[0].main = false;
    s.channels[0].fx_sends[0] = SendTo {
        on: true,
        level: 0.0,
        ..Default::default()
    };
    d.set(s, false);
    let out = run(&mut d, 200, |b| vec![sine(440.0, 0.5, b * BLOCK)]);
    assert!(
        peak(&out[0]) > 0.01,
        "the reverb on the main mix: {}",
        peak(&out[0])
    );
    // The channel stops: half a second later, still ringing.
    let out = run(&mut d, 250, |_| vec![vec![0.0; BLOCK]]);
    assert!(peak(&out[0]) > 1e-4, "tail: {}", peak(&out[0]));
    // A graphic EQ on the main: 1 kHz up 6 dB.
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.main.geq.on = true;
    s.main.geq.gains[17] = 6.0;
    d.set(s, false);
    let out = run(&mut d, 300, |b| vec![sine(1000.0, 0.25, b * BLOCK)]);
    let l = to_db(peak(&out[0])) - to_db(0.25);
    assert!(
        (l - (6.0 - 3.01)).abs() < 0.3,
        "+6 dB less the pan law: {l}"
    );
}

/// An output's delay (§M.15): its signal that many samples later; the others not.
#[test]
fn an_outputs_delay() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.output_delay[0] = 10.0;
    d.set(s, false);
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for b in 0..10 {
        let mut x = vec![0.0; BLOCK];
        if b == 0 {
            x[0] = 1.0;
        }
        d.process(&[x], &mut out);
        l.extend_from_slice(&out[0]);
        r.extend_from_slice(&out[1]);
    }
    // 10 ms = 480 samples on the left; the right as it was.
    let first = |v: &[f32]| v.iter().position(|x| x.abs() > 1e-3).unwrap();
    assert_eq!(first(&l) - first(&r), 480);
}

#[test]
fn levels_out_of_range_are_held_to_the_desks() {
    let mut d = Desk::new(2);
    let fresh = d.settings().clone();
    // The defaults are within the ranges: unchanged.
    assert_eq!(fresh.clone().normalised(2, OUTPUTS), fresh);
    let mut s = fresh;
    s.channels[0].fader = 300.0;
    s.channels[0].trim = 1e30;
    s.channels[0].eq[1].gain = 400.0;
    s.channels[0].comp.makeup = 1000.0;
    s.channels[0].sends[0].level = 99.0;
    s.main.fader = 1e9;
    s.dca[0].fader = 50.0;
    d.set(s, false);
    let s = d.settings();
    assert_eq!((s.channels[0].fader, s.channels[0].trim), (10.0, 40.0));
    assert_eq!(s.channels[0].eq[1].gain, 15.0);
    assert_eq!(s.channels[0].comp.makeup, 24.0);
    assert_eq!(s.channels[0].sends[0].level, 10.0);
    assert_eq!((s.main.fader, s.dca[0].fader), (10.0, 10.0));
    let out = run(&mut d, 20, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    assert!(out.iter().flatten().all(|v| v.is_finite()));
}

/// A source's NaN, infinity or absurd level (IDF3 float carries them as they are; a
/// fowt scale of 1e38 is finite) must not stay in a channel's filters: one bad block,
/// then the channel carries on.
#[test]
fn a_sources_nan_or_absurd_level_passes() {
    for bad in [f32::NAN, f32::INFINITY, 1e38] {
        let mut d = Desk::new(2);
        let mut s = d.settings().clone();
        s.channels[0].eq_on = true;
        s.channels[0].eq[1].gain = 12.0;
        s.channels[0].comp.on = true;
        d.set(s, false);
        run(&mut d, 5, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
        run(&mut d, 1, |_| vec![vec![bad; BLOCK]]);
        let out = run(&mut d, 200, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
        assert!(
            out.iter().flatten().all(|v| v.is_finite()),
            "{bad}: still not finite"
        );
        assert!(peak(&out[0]) > 0.05, "{bad}: the channel went silent");
    }
}

#[test]
fn sends_pre_and_post_and_solo() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.channels[0].sends[0] = SendTo {
        on: true,
        level: 0.0,
        ..Default::default()
    }; // aux 1 is pre-fader
    s.channels[0].sends[1] = SendTo {
        on: true,
        level: -6.0,
        ..Default::default()
    };
    s.aux[1].pre = false; // aux 2 post-fader
    s.channels[0].fader = -20.0;
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(500.0, 0.5, b * BLOCK)]);
    // Aux 1 (outputs 2 and 3): the full signal, whatever the fader.
    assert!((peak(&out[2]) - 0.5).abs() < 0.01, "{}", peak(&out[2]));
    assert_eq!(peak(&out[2]), peak(&out[3]));
    // Aux 2: −6 dB and the fader's −20.
    let a2 = to_db(peak(&out[4])) - to_db(0.5);
    assert!((a2 + 26.0).abs() < 0.2, "{a2}");
    // Monitor follows main without solo; with channel 2 soloed (silent), silence.
    assert_eq!(peak(&out[bus_out(MONITOR)]), peak(&out[0]));
    let mut s = d.settings().clone();
    s.channels[1].solo = true;
    d.set(s, false);
    let out = run(&mut d, 5, |b| vec![sine(500.0, 0.5, b * BLOCK)]);
    assert!(peak(&out[bus_out(MONITOR)]) < 1e-6);
    assert!(peak(&out[0]) > 0.01, "solo never changes the main mix");
}

#[test]
fn a_linked_pair_goes_left_and_right() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.channels[0].link = true;
    s.channels[0].fader = -6.0;
    s.channels[1].fader = 10.0; // ignored: the pair follows channel 1
    d.set(s, false);
    let out = run(&mut d, 50, |b| {
        vec![sine(500.0, 0.5, b * BLOCK), sine(700.0, 0.25, b * BLOCK)]
    });
    let l = to_db(peak(&out[0])) - to_db(0.5);
    let r = to_db(peak(&out[1])) - to_db(0.25);
    assert!((l + 6.0).abs() < 0.2 && (r + 6.0).abs() < 0.2, "{l} {r}");
}

/// A linked pair's compressor takes the same off both sides, driven by the louder:
/// the quiet side comes down with the loud one.
#[test]
fn a_linked_pair_compresses_both_sides_alike() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.channels[0].link = true;
    s.channels[0].eq_on = false;
    s.channels[0].comp = Comp {
        on: true,
        threshold: -30.0,
        ratio: 4.0,
        attack: 1.0,
        release: 50.0,
        knee: 0.0,
        makeup: 0.0,
        ..Default::default()
    };
    d.set(s, false);
    let out = run(&mut d, 100, |b| {
        vec![sine(500.0, 0.5, b * BLOCK), sine(700.0, 0.01, b * BLOCK)]
    });
    let l = to_db(peak(&out[0])) - to_db(0.5);
    let r = to_db(peak(&out[1])) - to_db(0.01);
    assert!(l < -6.0, "the loud side compressed: {l}");
    assert!((l - r).abs() < 0.5, "the quiet side as much: {l} / {r}");
}

/// A channel coming on (its input routed, or put on the desk) fades in: no step at
/// its start on its direct out or in the mix.
#[test]
fn a_channel_coming_on_fades_in() {
    let mut d = Desk::new(1);
    let mut s = d.settings().clone();
    s.channels[0].eq_on = false;
    d.set(s, false);
    d.set_routed(vec![false]);
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    let dc = |_| vec![vec![0.5; BLOCK]];
    for b in 0..3 {
        d.process(&dc(b), &mut out);
    }
    assert!(peak(&out[0]) == 0.0, "off: silent");
    d.set_routed(vec![true]);
    let mut l = Vec::new();
    for b in 0..8 {
        d.process(&dc(b), &mut out);
        l.extend_from_slice(&out[0]);
    }
    let step = l
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(l[0].abs(), f32::max);
    assert!(step < 0.01, "largest step {step}");
    assert!(l[l.len() - 1] > 0.3, "then on");
}
