//! Inserts and plugins: lined up with the dry sound, before or after the processing, NaN,
//! on buses and groups; the round trip through a loop.

use super::*;

#[test]
fn inserts_line_up_and_pass_dry_when_late() {
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    for c in &mut s.channels {
        c.eq_on = false;
        c.pan = -1.0;
    }
    s.main.eq_on = false;
    d.set(s, false);
    // Channel 1 through a plugin of 10 samples' latency; channel 2 without, delayed
    // to match: both arrive together a block and 10 samples later, 2 × 0.5 + 0.5.
    let fake = Arc::new(Fake {
        latency: 10,
        ..Default::default()
    });
    d.set_plugins(Some(fake.clone()), BTreeMap::from([(channel_chain(0), 10)]));
    assert_eq!(d.plugin_latency(), BLOCK as u32 + 10);
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    d.process(&impulse_at(0, 3)(0), &mut out);
    assert!(peak(&out[0]) == 0.0, "nothing yet: a block of latency");
    d.process(&impulse_at(0, 3)(1), &mut out);
    assert!((out[0][13] - 1.5).abs() < 1e-6, "{:?}", &out[0][..20]);
    assert_eq!(out[0].iter().filter(|v| v.abs() > 1e-9).count(), 1);
    assert_eq!(d.meters().channel_late[0], 0);
    // The host behind: blocks go on dry (channel 1 at 0.5) and are counted.
    fake.drop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = run(&mut d, 3, |_| vec![vec![0.0; BLOCK]; 2]);
    let out = run(&mut d, 2, impulse_at(0, 3));
    assert!(
        (out[0][13] - 1.0).abs() < 1e-6,
        "dry, still in line: {:?}",
        &out[0][..20]
    );
    assert!(
        d.meters().channel_late[0] >= 2,
        "{:?}",
        d.meters().channel_late
    );
    assert_eq!(d.meters().channel_late[1], 0, "no plugins, never late");
}

/// Noise suppression on one channel delays the others by its frame, so a source on
/// another channel arrives exactly that much later and stays in line with it.
#[test]
fn denoise_lines_the_other_channels_up() {
    let first_out = |denoise_first: bool| {
        let mut d = Desk::new(2);
        let mut s = d.settings().clone();
        for c in &mut s.channels {
            c.eq_on = false;
            c.pan = -1.0;
        }
        s.main.eq_on = false;
        s.channels[0].denoise = denoise_first;
        d.set(s, false);
        let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
        let mut main_l = Vec::new();
        for b in 0..(2 * idn_dsp::denoise::LATENCY / BLOCK + 4) {
            // An impulse on channel 2 only.
            let mut v = vec![0.0; BLOCK];
            if b == 1 {
                v[7] = 0.5;
            }
            d.process(&[vec![0.0; BLOCK], v], &mut out);
            main_l.extend_from_slice(&out[0]);
        }
        main_l.iter().position(|x| x.abs() > 1e-6)
    };
    let dry = first_out(false).expect("the impulse comes out");
    let lined_up = first_out(true).expect("the impulse comes out");
    assert_eq!(lined_up - dry, idn_dsp::denoise::LATENCY);
}

#[test]
fn a_plugins_nan_is_silence_and_passes() {
    let mut d = Desk::new(1);
    let fake = Arc::new(Fake::default());
    d.set_plugins(Some(fake.clone()), BTreeMap::from([(MAIN_CHAIN, 0)]));
    let tone = |b: usize| vec![sine(1000.0, 0.5, b * BLOCK)];
    fake.nan.store(true, std::sync::atomic::Ordering::Relaxed);
    let out = run(&mut d, 20, tone);
    assert!(out.iter().flatten().all(|v| v.is_finite()));
    // The plugin well again: the main bus sounds (no NaN left in its filters).
    fake.nan.store(false, std::sync::atomic::Ordering::Relaxed);
    let out = run(&mut d, 50, tone);
    assert!(peak(&out[0]) > 0.1, "{}", peak(&out[0]));
}

#[test]
fn bus_inserts_and_the_buses_stay_together() {
    let mut d = Desk::new(1);
    let mut s = d.settings().clone();
    s.channels[0].eq_on = false;
    s.channels[0].pan = -1.0;
    s.channels[0].sends[0] = SendTo {
        on: true,
        level: 0.0,
        ..Default::default()
    };
    s.main.eq_on = false;
    s.aux[0].eq_on = false;
    d.set(s, false);
    let fake = Arc::new(Fake::default());
    d.set_plugins(Some(fake), BTreeMap::from([(MAIN_CHAIN, 0)]));
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    // (Two blocks first, for the gains to come up from silence.)
    let input = |b: usize| impulse_at(2, 5)(b)[..1].to_vec();
    for b in 0..3 {
        d.process(&input(b), &mut out);
    }
    assert_eq!(
        (peak(&out[0]), peak(&out[2])),
        (0.0, 0.0),
        "main and aux 1 both a block late"
    );
    d.process(&input(3), &mut out);
    assert!(
        (out[0][5] - 1.0).abs() < 1e-6,
        "main through the plugin: {:?}",
        &out[0][..8]
    );
    assert!(
        (out[2][5] - 0.5).abs() < 1e-6,
        "aux 1 without: {:?}",
        &out[2][..8]
    );
}

#[test]
fn plugins_on_a_group_line_up_with_the_direct_sound() {
    let mut d = Desk::new(2);
    let mut s = plain(&d);
    s.channels[1].main = false;
    s.channels[1].groups[0] = true;
    d.set(s, false);
    let fake = Arc::new(Fake {
        latency: 7,
        ..Default::default()
    });
    d.set_plugins(Some(fake), BTreeMap::from([(GROUP_CHAIN, 7)]));
    assert_eq!(d.plugin_latency(), BLOCK as u32 + 7);
    // Both channels' impulse at sample 5 of block 3: one sum on main, a block and 7
    // samples later: 0.5 directly + 2 × 0.5 through the group's plugin.
    let out = run(&mut d, 5, impulse_at(3, 5));
    assert!((out[0][12] - 1.5).abs() < 1e-6, "{:?}", &out[0][..16]);
    assert_eq!(out[0].iter().filter(|v| v.abs() > 1e-9).count(), 1);
}

/// Inserts first (§M.28): a plugin doubling the level, then a limiter at −12 dBFS, comes
/// out at the ceiling; after it (as always), 6 dB over it.
#[test]
fn inserts_before_or_after_the_processing() {
    let level = |point: InsertPoint| {
        let mut d = Desk::new(1);
        let mut s = d.settings().clone();
        let c = &mut s.channels[0];
        (c.eq_on, c.pan, c.insert_point) = (false, -1.0, point);
        c.limiter.on = true;
        c.limiter.ceiling = -12.0;
        s.main.eq_on = false;
        d.set(s, false);
        d.set_plugins(
            Some(Arc::new(Fake::default())),
            BTreeMap::from([(channel_chain(0), 0)]),
        );
        to_db(peak(
            &run(&mut d, 200, |b| vec![sine(500.0, 0.5, b * BLOCK)])[0],
        ))
    };
    let (post, pre) = (level(InsertPoint::Post), level(InsertPoint::Pre));
    assert!((post - (-12.0 + 6.02)).abs() < 1.0, "after: {post}");
    assert!((pre - -12.0).abs() < 1.0, "before: {pre}");
}

/// The round trip (§M.28.2): output 1 looped back to input 2 through 500 samples of
/// delay (and a block, as the loop runs) measures that; with nothing coming back, it gives up; a noisy input, too.
#[test]
fn the_round_trip_is_measured_through_a_loop() {
    let run_loop = |delay: Option<usize>, noise: bool| {
        let mut d = Desk::new(2);
        d.start_round_trip(1, 2).unwrap();
        let mut line: std::collections::VecDeque<f32> = vec![0.0; delay.unwrap_or(0)].into();
        let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
        let mut back = vec![0.0; BLOCK];
        for b in 0..(3.0 * rate()) as usize / BLOCK {
            let input = vec![
                vec![0.0; BLOCK],
                if noise {
                    sine(300.0, 0.5, b * BLOCK)
                } else {
                    back.clone()
                },
            ];
            d.process(&input, &mut out);
            for (k, v) in out[0].iter().enumerate() {
                back[k] = match delay {
                    Some(_) => {
                        line.push_back(*v);
                        line.pop_front().unwrap()
                    }
                    None => 0.0,
                };
            }
            if !d.round_trip_active() {
                break;
            }
        }
        d.round_trip()
    };
    match run_loop(Some(500), false) {
        // (Plus the loop's own block: each block's output is the next block's input.)
        RoundTrip::Done { samples, .. } => assert_eq!(samples, 500 + BLOCK),
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(run_loop(None, false), RoundTrip::Failed(e) if e.contains("didn't come back"))
    );
    assert!(matches!(run_loop(Some(500), true), RoundTrip::Failed(e) if e.contains("quiet")));
    let mut d = Desk::new(2);
    assert!(d.start_round_trip(0, 1).is_err());
    assert!(d.start_round_trip(1, 999).is_err());
}
