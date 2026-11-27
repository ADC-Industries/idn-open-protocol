//! Routing: send taps, groups and matrices, mute groups, talkback, the patch, direct outs,
//! parked and hidden channels, FX returns, backups, DCAs, added buses, the stream bus.

use super::*;

#[test]
fn a_send_can_choose_its_own_tap_and_place() {
    let mut d = Desk::new(1);
    let mut s = plain(&d);
    s.channels[0].fader = -20.0;
    // Aux 1 is pre-fader, but this send is taken after the fader.
    s.channels[0].sends[0] = SendTo {
        on: true,
        level: 0.0,
        pre: Some(false),
        pan: None,
        ..Default::default()
    };
    // Aux 2 stereo: this send placed hard right whatever the channel's pan.
    s.aux[1].stereo = true;
    s.channels[0].sends[1] = SendTo {
        on: true,
        level: 0.0,
        pre: Some(true),
        pan: Some(1.0),
        ..Default::default()
    };
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(500.0, 0.5, b * BLOCK)]);
    assert!((level(&out[2], 0.5) + 20.0).abs() < 0.2, "post-fader");
    assert!(
        peak(&out[4]) < 1e-6 && level(&out[5], 0.5).abs() < 0.2,
        "hard right"
    );
}

#[test]
fn groups_and_matrix_mixes() {
    let mut d = Desk::new(2);
    let mut s = plain(&d);
    // Channel 1 to main directly; channel 2 only through group 1 at −6 dB.
    s.channels[1].main = false;
    s.channels[1].groups[0] = true;
    s.groups[0].fader = -6.0;
    // Matrix 1: group 1 at −3 dB.
    s.matrix[0].sources[1 + AUX] = SendTo {
        on: true,
        level: -3.0,
        ..Default::default()
    };
    d.set(s, false);
    let out = run(&mut d, 50, |b| {
        vec![vec![0.0; BLOCK], sine(500.0, 0.5, b * BLOCK)]
    });
    assert!(
        (level(&out[bus_out(group_out(0))], 0.5) + 6.0).abs() < 0.2,
        "the group's own output"
    );
    assert!(
        (level(&out[0], 0.5) + 6.0).abs() < 0.2,
        "the group into main"
    );
    assert!(
        (level(&out[bus_out(matrix_out(0))], 0.5) + 9.0).abs() < 0.2,
        "matrix: −6 −3"
    );
    let mut s = d.settings().clone();
    s.groups[0].to_main = false;
    d.set(s, false);
    let out = run(&mut d, 5, |b| {
        vec![vec![0.0; BLOCK], sine(500.0, 0.5, b * BLOCK)]
    });
    assert!(peak(&out[0]) < 1e-6, "not to main");
    assert_eq!(
        d.settings().output_labels()[bus_out(group_out(0))],
        "Group 1 L",
        "after the first 20"
    );
}

/// Eight mute groups (from 2026-11-18): a desk saved with six gets 7 and 8, off and with
/// no members; one with more (a later build's) keeps the first eight.
#[test]
fn a_six_mute_group_desk_loads_with_eight() {
    let old = serde_json::json!({
        "channels": [{"mute_groups": [true, false, false, false, false, true]}],
        "mute_groups": (1..=6).map(|i| serde_json::json!({"name": format!("Old {i}"), "on": i == 6})).collect::<Vec<_>>(),
    });
    let s = serde_json::from_value::<Settings>(old)
        .unwrap()
        .normalised(2, OUTPUTS);
    assert_eq!((s.mute_groups.len(), MUTE_GROUPS), (8, 8));
    assert_eq!(
        (s.mute_groups[5].name.as_str(), s.mute_groups[5].on),
        ("Old 6", true)
    );
    assert_eq!(
        (s.mute_groups[7].name.as_str(), s.mute_groups[7].on),
        ("Mute 8", false)
    );
    assert_eq!(
        s.channels[0].mute_groups,
        [true, false, false, false, false, true, false, false]
    );
    let more = serde_json::json!({
        "channels": [{"mute_groups": vec![true; 10]}],
        "mute_groups": (1..=10).map(|i| serde_json::json!({"name": format!("G {i}")})).collect::<Vec<_>>(),
    });
    let s = serde_json::from_value::<Settings>(more)
        .unwrap()
        .normalised(2, OUTPUTS);
    assert_eq!(
        (s.mute_groups.len(), s.channels[0].mute_groups.len()),
        (8, 8)
    );
    assert_eq!(s.mute_groups[7].name, "G 8");
}

#[test]
fn mute_groups_talkback_and_listen() {
    let mut d = Desk::new(2);
    let mut s = plain(&d);
    // Mute group 8 (from 2026-11-18; 6 before).
    s.channels[0].mute_groups[7] = true;
    s.mute_groups[7].on = true;
    // Talkback on channel 2 to aux 1 only (channel 2 itself muted in the mix).
    s.channels[1].mute = true;
    s.talkback = Talkback {
        channel: 2,
        on: true,
        level: -6.0,
        aux: (0..AUX).map(|k| k == 0).collect(),
        main: false,
        ..Default::default()
    };
    d.set(s, false);
    let input = |b: usize| vec![sine(500.0, 0.5, b * BLOCK), sine(700.0, 0.5, b * BLOCK)];
    let out = run(&mut d, 50, input);
    assert!(
        peak(&out[0]) < 1e-6,
        "channel 1 in an active mute group; channel 2 muted"
    );
    assert!((level(&out[2], 0.5) + 6.0).abs() < 0.2, "talkback in aux 1");
    assert!(peak(&out[4]) < 1e-6, "not in aux 2");
    // Listening to aux 1 puts it on the headphones instead of the main mix.
    let mut s = d.settings().clone();
    s.aux[0].listen = true;
    d.set(s, false);
    let out = run(&mut d, 5, input);
    assert!((level(&out[bus_out(MONITOR)], 0.5) + 6.0).abs() < 0.2);
}

#[test]
fn the_input_and_output_patch() {
    let mut d = Desk::with_io(2, 4, 6);
    let mut s = plain(&d);
    s.channels[0].source = Some(3);
    s.channels[1].source = Some(0);
    s.outputs = vec![
        "bus:0".into(),
        "pre:1".into(),
        "in:4".into(),
        "".into(),
        "post:1".into(),
        "bus:99".into(),
    ];
    s.channels[0].fader = -6.0;
    d.set(s, false);
    let mut out = vec![vec![0.0; BLOCK]; 6];
    let input = |b: usize| {
        vec![
            sine(300.0, 0.5, b * BLOCK),
            sine(400.0, 0.5, b * BLOCK),
            sine(500.0, 0.25, b * BLOCK),
            sine(600.0, 0.1, b * BLOCK),
        ]
    };
    for b in 0..50 {
        d.process(&input(b), &mut out);
    }
    assert!(
        (level(&out[0], 0.25) + 6.0).abs() < 0.2,
        "channel 1 takes input 3"
    );
    assert!(level(&out[1], 0.25).abs() < 0.2, "pre-fader direct out");
    assert!(
        level(&out[2], 0.1).abs() < 0.01,
        "an input straight through"
    );
    assert!(peak(&out[3]) == 0.0 && peak(&out[5]) == 0.0, "nothing");
    assert!(
        (level(&out[4], 0.25) + 6.0).abs() < 0.2,
        "post-fader direct out"
    );
    assert_eq!(d.settings().output_label(1), "Ch 1 (pre)");
    assert_eq!(d.settings().output_label(3), "Out 4");
}

#[test]
fn direct_outs_from_every_tap_point() {
    let mut d = Desk::with_io(1, 1, 3);
    let mut s = plain(&d);
    s.channels[0].trim = -3.0;
    s.channels[0].eq_on = true;
    s.channels[0].eq[1] = Band {
        on: true,
        shape: Shape::Peak,
        freq: 1000.0,
        gain: 6.0,
        q: 1.0,
    };
    for k in [0, 2, 3] {
        s.channels[0].eq[k].on = false;
    }
    s.outputs = vec!["input:1".into(), "preeq:1".into(), "posteq:1".into()];
    d.set(s, false);
    let mut out = vec![vec![0.0; BLOCK]; 3];
    for b in 0..300 {
        d.process(&[sine(1000.0, 0.25, b * BLOCK)], &mut out);
    }
    assert!(level(&out[0], 0.25).abs() < 0.05, "as it came");
    assert!(
        (level(&out[1], 0.25) + 3.0).abs() < 0.1,
        "trimmed, before the EQ"
    );
    assert!(
        (level(&out[2], 0.25) - 3.0).abs() < 0.3,
        "after the EQ: {}",
        level(&out[2], 0.25)
    );
    assert_eq!(d.settings().output_label(1), "Ch 1 (pre-EQ)");
}

#[test]
fn parked_and_hidden_channels_send_nothing() {
    let mut d = Desk::new(2);
    let s = plain(&d);
    d.set(s, false);
    d.set_routed(vec![true, false]);
    let input = |b: usize| vec![sine(500.0, 0.5, b * BLOCK), sine(500.0, 0.5, b * BLOCK)];
    let _ = run(&mut d, 20, input);
    assert!(d.active(0) && !d.active(1));
    assert!(
        d.meters().channel_in[1].peak == 0.0,
        "parked: not even metered"
    );
    let out = run(&mut d, 20, input);
    assert!(level(&out[0], 0.5).abs() < 0.2, "channel 1 alone");
    // Hidden while routed: nothing of it anywhere.
    let mut s = d.settings().clone();
    s.channels[0].shown = Some(false);
    d.set(s, false);
    let out = run(&mut d, 5, input);
    assert!(!d.active(0) && peak(&out[0]) < 1e-6);
    // Shown by hand: routed again, it plays.
    let mut s = d.settings().clone();
    s.channels[0].shown = Some(true);
    d.set(s, false);
    d.set_routed(vec![true, true]);
    let out = run(&mut d, 20, input);
    assert!(
        (level(&out[0], 0.5) - 6.02).abs() < 0.2,
        "both: {}",
        level(&out[0], 0.5)
    );
}

/// Lab 477(c): a soloed channel hidden from the desk leaves the headphones on the main
/// mix, not on silence; lab 477(a): a pan out of range is held to the edge in the
/// setting too.
#[test]
fn a_hidden_solo_leaves_the_monitor_and_a_pan_is_held() {
    let mut d = Desk::new(2);
    let mut s = plain(&d);
    s.channels[0].solo = true;
    d.set(s, false);
    d.set_routed(vec![true, true]);
    let input = |b: usize| vec![sine(500.0, 0.5, b * BLOCK), sine(700.0, 0.25, b * BLOCK)];
    let out = run(&mut d, 20, input);
    assert!(
        peak(&out[bus_out(MONITOR)]) > 0.1,
        "the solo on the headphones"
    );
    let mut s = d.settings().clone();
    s.channels[0].shown = Some(false);
    d.set(s, false);
    let out = run(&mut d, 40, input);
    assert!(!d.active(0));
    // Channel 2 alone in the main mix, and that on the headphones.
    assert!(
        peak(&out[0]) > 0.05,
        "the main mix plays: {}",
        peak(&out[0])
    );
    assert!(
        (peak(&out[bus_out(MONITOR)]) - peak(&out[0])).abs() < 0.02,
        "the headphones carry the main mix: {} against {}",
        peak(&out[bus_out(MONITOR)]),
        peak(&out[0])
    );

    let mut s = d.settings().clone();
    s.channels[1].pan = 50.0;
    assert_eq!(s.clone().normalised(2, OUTPUTS).channels[1].pan, 1.0);
    s.channels[1].pan = f32::NAN;
    assert_eq!(s.normalised(2, OUTPUTS).channels[1].pan, 0.0);
}

#[test]
fn a_matrix_takes_an_fx_return() {
    // The channel only to FX 1; matrix 1 only from FX 1 (it read past its buffers
    // and stopped the desk's sound).
    let mut d = Desk::new(1);
    let mut s = plain(&d);
    s.channels[0].main = false;
    s.channels[0].fx_sends[0] = SendTo {
        on: true,
        level: 0.0,
        ..Default::default()
    };
    s.matrix[0].sources[1 + AUX + GROUPS] = SendTo {
        on: true,
        level: 0.0,
        ..Default::default()
    };
    d.set(s, false);
    let input = |b: usize| vec![sine(500.0, 0.5, b * BLOCK)];
    let out = run(&mut d, 50, input);
    let (fx, matrix) = (
        level(&out[bus_out(fx_out(0))], 0.5),
        level(&out[bus_out(matrix_out(0))], 0.5),
    );
    assert!(fx > -20.0, "FX 1 has the channel: {fx}");
    assert!(
        (matrix - fx).abs() < 0.2,
        "matrix 1 = FX 1: {matrix} / {fx}"
    );
}

#[test]
fn fx_returns_dim_and_mono() {
    let mut d = Desk::new(1);
    let mut s = plain(&d);
    // The channel only to FX 1 (after its −6 dB fader), FX 1 back into main.
    s.channels[0].main = false;
    s.channels[0].fader = -6.0;
    s.channels[0].fx_sends[0] = SendTo {
        on: true,
        level: 0.0,
        ..Default::default()
    };
    d.set(s, false);
    let input = |b: usize| vec![sine(500.0, 0.5, b * BLOCK)];
    let out = run(&mut d, 50, input);
    assert!(
        (level(&out[bus_out(fx_out(0))], 0.5) + 6.0).abs() < 0.2,
        "FX 1's own output"
    );
    assert!(
        (level(&out[0], 0.5) + 6.0).abs() < 0.2,
        "returned into main"
    );
    assert!(peak(&out[1]) < 1e-6, "hard left");
    // Mono: both sides the same, half each; dim: 20 dB less.
    let mut s = d.settings().clone();
    s.main.mono = true;
    d.set(s, false);
    let out = run(&mut d, 5, input);
    assert!(
        (level(&out[1], 0.5) + 12.0).abs() < 0.2 && (peak(&out[0]) - peak(&out[1])).abs() < 1e-6
    );
    let mut s = d.settings().clone();
    (s.main.mono, s.main.dim) = (false, true);
    d.set(s, false);
    let out = run(&mut d, 5, input);
    assert!((level(&out[0], 0.5) + 26.0).abs() < 0.2, "dimmed");
    assert_eq!(d.settings().output_labels()[bus_out(fx_out(0))], "Reverb L");
}

#[test]
fn a_backup_input_takes_over_and_the_analyzer_listens() {
    let mut d = Desk::with_io(1, 2, OUTPUTS);
    let mut s = plain(&d);
    s.channels[0].input_b = 2;
    s.channels[0].failover = true;
    d.set(s, false);
    let input = |b: usize| vec![sine(500.0, 0.5, b * BLOCK), sine(500.0, 0.25, b * BLOCK)];
    let out = run(&mut d, 30, input);
    assert!(
        level(&out[0], 0.5).abs() < 0.2,
        "input A while it has sound"
    );
    assert!(!d.uses_b(0));
    // A's sound stops arriving: B takes over.
    d.set_live(vec![false, true]);
    let out = run(&mut d, 30, input);
    assert!(level(&out[0], 0.25).abs() < 0.2, "input B");
    assert!(d.uses_b(0));
    // By hand, B whatever A does.
    d.set_live(vec![true, true]);
    let mut s = d.settings().clone();
    s.channels[0].use_b = true;
    d.set(s, false);
    let _ = run(&mut d, 5, input);
    assert!(d.uses_b(0));
    // The analyzer: channel 1's sound, a 500 Hz peak.
    d.set_analyze(Some(0));
    let _ = run(&mut d, SCOPE / BLOCK + 1, input);
    let spec = crate::dsp::spectrum(&d.scope(), rate(), 120);
    let k = (0..120)
        .max_by(|&a, &b| spec[a].total_cmp(&spec[b]))
        .unwrap();
    let f = 20.0 * 1000f32.powf((k as f32 + 0.5) / 120.0);
    assert!((f - 500.0).abs() < 40.0, "{f}");
}

#[test]
fn dca_groups_move_and_mute_their_members() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    for c in &mut s.channels {
        c.pan = -1.0;
    }
    s.channels[0].dca[2] = true;
    s.dca[2].fader = -6.0;
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    let l = to_db(peak(&out[0])) - to_db(0.5);
    assert!((l + 6.0).abs() < 0.1, "DCA at -6: {l}");
    let mut s = d.settings().clone();
    s.dca[2].mute = true;
    d.set(s, false);
    let out = run(&mut d, 5, |b| {
        vec![sine(1000.0, 0.5, b * BLOCK), sine(1000.0, 0.5, b * BLOCK)]
    });
    let l = to_db(peak(&out[0])) - to_db(0.5);
    assert!(l.abs() < 0.1, "channel 2 (not in it) alone: {l}");
}

/// 16 aux, 8 matrix, 8 FX: the outside numbering keeps the 52 channels
/// of 8 aux, 4 matrix and 4 FX where they were and puts the added buses after them; every
/// channel has one number.
#[test]
fn added_buses_keep_the_old_numbers() {
    let mut seen = [false; OUTPUTS];
    for i in 0..OUTPUTS {
        let j = bus_out(i);
        assert!(!seen[j], "{j} twice");
        seen[j] = true;
        assert_eq!(bus_in(j), Some(i));
    }
    // As before: main 0, aux 1 at 2, aux 8 at 16, headphones 18, group 1 at 20, matrix 1 at
    // 36, FX 1 at 44.
    assert_eq!(bus_out(0), 0);
    assert_eq!(bus_out(2), 2);
    assert_eq!(bus_out(2 + 2 * 7), 16);
    assert_eq!(bus_out(MONITOR), 18);
    assert_eq!(bus_out(group_out(0)), 20);
    assert_eq!(bus_out(matrix_out(0)), 36);
    assert_eq!(bus_out(fx_out(0)), 44);
    assert_eq!(bus_out(fx_out(3) + 1), 51);
    // Added: aux 9 at 52, matrix 5 at 68, FX 5 at 76; the last at 83.
    assert_eq!(bus_out(2 + 2 * 8), 52);
    assert_eq!(bus_out(matrix_out(4)), 68);
    assert_eq!(bus_out(fx_out(4)), 76);
    // The stream bus (2026-11-18): the last two, 84 and 85.
    assert_eq!(bus_out(STREAM), 84);
    assert_eq!(bus_out(STREAM + 1), 85);
    assert_eq!(OUTPUTS, 86);
    assert_eq!(bus_in(OUTPUTS), None);
    // The labels by those numbers.
    let s = Settings::new(4);
    let l = s.output_labels();
    assert_eq!(l[36], format!("{} L", s.matrix[0].name));
    assert!(l[52].starts_with(&s.aux[8].name), "{}", l[52]);
    assert_eq!(l[84], "Stream L");
}

/// A matrix mix saved with its sources' names in another layout (a desk of fewer aux) has
/// each level put back on the source of that name (bug run 5, 3-7b).
#[test]
fn matrix_sources_come_back_by_name() {
    let mut s = Desk::new(2).settings().clone();
    let old_names: Vec<String> = std::iter::once("main".to_owned())
        .chain((1..=8).map(|k| format!("aux {k}")))
        .chain((1..=GROUPS).map(|k| format!("group {k}")))
        .collect();
    let mut sources = vec![SendTo::default(); old_names.len()];
    sources[9].on = true; // group 1 in the old layout
    sources[9].level = -6.0;
    s.matrix[0].sources = sources;
    s.matrix[0].source_names = old_names;
    let s = s.normalised(2, OUTPUTS);
    let names = settings_of::matrix_source_names();
    let g1 = names.iter().position(|n| n == "group 1").unwrap();
    assert_eq!(g1, 1 + AUX);
    assert!(s.matrix[0].sources[g1].on);
    assert_eq!(s.matrix[0].sources[g1].level, -6.0);
    assert!(!s.matrix[0].sources[9].on, "aux 9 not taken for group 1");
    assert_eq!(s.matrix[0].source_names, names);
}

/// The stream bus: on a new desk a copy of the main mix (each channel's
/// stream send on at 0 dB after its fader), `bus:84`/`bus:85`; its own send level, the
/// send off, and its own fader.
#[test]
fn the_stream_bus_is_its_own_mix() {
    let mut d = Desk::new(4);
    let (sl, sr) = (bus_out(STREAM), bus_out(STREAM + 1));
    // (A new desk patches the first 52 outputs; these two by hand.)
    let mut s = d.settings().clone();
    s.outputs[sl] = format!("bus:{sl}");
    s.outputs[sr] = format!("bus:{sr}");
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    assert!((peak(&out[sl]) - peak(&out[0])).abs() < 1e-3, "as main");
    assert!((peak(&out[sr]) - peak(&out[1])).abs() < 1e-3);
    // Its loudness, measured on its own: as main's while it carries the same.
    let m = d.meters();
    assert!(m.stream_lufs.0 > -60.0 && (m.stream_lufs.0 - m.lufs_m).abs() < 0.5);
    // After the fader: −6 dB on the channel is −6 dB on the stream too.
    let mut s = d.settings().clone();
    s.channels[0].fader = -6.0;
    s.channels[0].stream.level = -4.0;
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    let st = to_db(peak(&out[sl])) - to_db(peak(&out[0]));
    assert!((st + 4.0).abs() < 0.1, "its own send level: {st}");
    // Its own fader, and the send off: silence on the stream, main as it was.
    let mut s = d.settings().clone();
    s.stream.fader = -10.0;
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    let st = to_db(peak(&out[sl])) - to_db(peak(&out[0]));
    assert!((st + 14.0).abs() < 0.1, "its fader: {st}");
    let mut s = d.settings().clone();
    s.channels[0].stream.on = false;
    d.set(s, false);
    let out = run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    assert!(peak(&out[sl]) < 1e-5 && peak(&out[0]) > 0.1);
}

/// Send taps (§M.28): an aux taken before the EQ doesn't hear its boost, one after it
/// does; a dim of −10 dB, and talkback turning the main down while it talks.
#[test]
fn send_taps_dim_amount_and_talkback_dim() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.channels[0].eq_on = true;
    s.channels[0].eq[1] = Band {
        on: true,
        shape: crate::dsp::Shape::Peak,
        freq: 1000.0,
        gain: 12.0,
        q: 1.0,
    };
    s.channels[0].sends[0] = SendTo {
        on: true,
        tap: Some(SendTap::PreEq),
        ..Default::default()
    };
    s.channels[0].sends[1] = SendTo {
        on: true,
        tap: Some(SendTap::PostEq),
        ..Default::default()
    };
    d.set(s, false);
    let out = run(&mut d, 100, |b| vec![sine(1000.0, 0.1, b * BLOCK)]);
    let (pre_eq, post_eq) = (to_db(peak(&out[2])), to_db(peak(&out[4])));
    assert!((pre_eq - to_db(0.1)).abs() < 0.3, "before the EQ: {pre_eq}");
    assert!((post_eq - pre_eq - 12.0).abs() < 0.5, "after it: {post_eq}");
    // Dim −10 dB on the main.
    let main = to_db(peak(&out[0]));
    let mut s = d.settings().clone();
    (s.main.dim, s.main.dim_db) = (true, -10.0);
    d.set(s, false);
    let dimmed = to_db(peak(
        &run(&mut d, 100, |b| vec![sine(1000.0, 0.1, b * BLOCK)])[0],
    ));
    assert!(
        (main - dimmed - 10.0).abs() < 0.3,
        "dim −10: {main} {dimmed}"
    );
    // Talkback on channel 2 with −6 dB on the main while it talks.
    let mut s = d.settings().clone();
    s.main.dim = false;
    s.talkback = Talkback {
        channel: 2,
        on: true,
        dim_main: -6.0,
        ..Default::default()
    };
    s.channels[1].main = false;
    d.set(s, false);
    let talking = to_db(peak(
        &run(&mut d, 100, |b| vec![sine(1000.0, 0.1, b * BLOCK)])[0],
    ));
    assert!(
        (main - talking - 6.0).abs() < 0.3,
        "talkback dims: {main} {talking}"
    );
}

/// The engineer's talkback in the IEM flow (§M.28): its microphone a lane of the flow,
/// silent there until talkback is on.
#[test]
fn talkback_goes_to_the_phones_while_on() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    s.channels[0].iem = true;
    s.talkback = Talkback {
        channel: 2,
        to_iem: true,
        ..Default::default()
    };
    d.set(s.clone(), false);
    let lanes = |d: &mut Desk| {
        run(d, 20, |b| {
            vec![sine(500.0, 0.3, b * BLOCK), sine(900.0, 0.3, b * BLOCK)]
        });
        let mut out = Vec::new();
        let ids = d.iem_tap(&mut out);
        (ids.to_vec(), peak(&out[0]), peak(&out[1]))
    };
    let (ids, band, tb) = lanes(&mut d);
    assert_eq!(ids, [0, 1]);
    assert!(
        band > 0.2 && tb == 0.0,
        "talkback off: silent ({band}, {tb})"
    );
    s.talkback.on = true;
    d.set(s, false);
    let (_, _, tb) = lanes(&mut d);
    assert!(tb > 0.2, "talkback on: heard ({tb})");
}
