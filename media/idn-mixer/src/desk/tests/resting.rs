//! Resting and timing: silent desks and banks resting and waking, mutes at rest, every
//! block size, mute ramps, delay changes without a gap.

use super::*;

/// A desk silent for long enough rests (its chains not run); a sample or a setting wakes
/// it at once, and what it outputs then is what it would have.
#[test]
fn a_silent_desk_rests_and_wakes_at_once() {
    let mut d = Desk::new(4);
    let silence = |_| vec![vec![0.0; BLOCK]; 4];
    // A tone, then silence: the tail dies away before the count starts.
    run(&mut d, 50, |b| vec![sine(1000.0, 0.5, b * BLOCK)]);
    run(&mut d, REST_AFTER as usize + 2000, silence);
    assert!(d.quiet_blocks >= REST_AFTER, "{}", d.quiet_blocks);
    // At rest: silence out, the count held.
    let out = run(&mut d, 10, silence);
    assert!(out.iter().all(|o| o.iter().all(|&v| v == 0.0)));
    // A sample wakes it in the same block.
    let out = run(&mut d, 1, |b| vec![sine(1000.0, 0.5, b * BLOCK + 7)]);
    assert!(peak(&out[0]) > 0.01, "{}", peak(&out[0]));
    assert_eq!(d.quiet_blocks, 0);
    // A setting changed during a rest wakes it too.
    run(&mut d, REST_AFTER as usize + 2000, silence);
    assert!(d.quiet_blocks >= REST_AFTER);
    let mut s = d.settings().clone();
    s.channels[0].fader = -10.0;
    d.set(s, false);
    run(&mut d, 1, silence);
    assert_eq!(d.quiet_blocks, 0);
}

/// Banks of four (§M.24): channels 5–8 silent rest while 1–4 play; a sample into one of
/// them wakes its bank in the same block, at full level (it rested silent: nothing to fade).
#[test]
fn a_silent_bank_rests_and_wakes_at_once() {
    let mut d = Desk::new(8);
    let tone_on = |chans: &'static [usize]| {
        move |b: usize| {
            (0..8)
                .map(|c| {
                    if chans.contains(&c) {
                        sine(1000.0, 0.5, b * BLOCK)
                    } else {
                        vec![0.0; BLOCK]
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    run(&mut d, REST_AFTER as usize + 50, tone_on(&[0]));
    assert_eq!(d.resting_banks(), 1, "channels 5-8 rest, 1-4 don't");
    assert!((4..8).all(|i| d.resting[i]) && (0..4).all(|i| !d.resting[i]));
    // Channel 6 speaks: its bank wakes in that block, its sound out at once.
    let before = run(&mut d, 1, tone_on(&[0]));
    let out = run(&mut d, 1, tone_on(&[0, 5]));
    assert!((4..8).all(|i| !d.resting[i]), "woken");
    assert!(
        peak(&out[0]) > peak(&before[0]) * 1.2,
        "channel 6 heard in the waking block: {} vs {}",
        peak(&out[0]),
        peak(&before[0])
    );
}

/// A muted bank rests after half a second though its inputs have sound: its input meters
/// still read; unmuted, it plays again (from a fresh strip, faded in).
#[test]
fn a_muted_bank_rests_meters_and_comes_back() {
    let mut d = Desk::new(8);
    let all = |b: usize| {
        (0..8)
            .map(|_| sine(1000.0, 0.5, b * BLOCK))
            .collect::<Vec<_>>()
    };
    let mut s = d.settings().clone();
    for c in &mut s.channels[4..8] {
        c.mute = true;
    }
    d.set(s, false);
    let with_mute = run(&mut d, MUTE_REST as usize + 20, all);
    assert_eq!(d.resting_banks(), 1, "channels 5-8 muted rest");
    assert!(
        d.meters.channel_in[5].peak > 0.1,
        "input still metered: {}",
        d.meters.channel_in[5].peak
    );
    let muted_level = peak(&with_mute[0]);
    let mut s = d.settings().clone();
    for c in &mut s.channels[4..8] {
        c.mute = false;
    }
    d.set(s, false);
    let out = run(&mut d, 50, all);
    assert_eq!(d.resting_banks(), 0);
    assert!(
        peak(&out[0]) > muted_level * 1.5,
        "unmuted heard: {} vs {muted_level}",
        peak(&out[0])
    );
}

/// A channel muted while its bank rests silent: its sound coming soon after doesn't slip
/// through on waking (gains frozen at rest), and its input is metered (review 2026-11-08).
#[test]
fn a_channel_muted_at_rest_stays_muted() {
    let mut d = Desk::new(8);
    let tone_on = |chans: &'static [usize]| {
        move |b: usize| {
            (0..8)
                .map(|c| {
                    if chans.contains(&c) {
                        sine(1000.0, 0.5, b * BLOCK)
                    } else {
                        vec![0.0; BLOCK]
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    run(&mut d, REST_AFTER as usize + 50, tone_on(&[]));
    assert_eq!(d.resting_banks(), 2, "a silent desk rests");
    let mut s = d.settings().clone();
    s.channels[5].mute = true;
    d.set(s, false);
    run(&mut d, 2, tone_on(&[]));
    // Its sound comes 4 ms after the mute (well within MUTE_REST): nothing of it out.
    let out = run(&mut d, 3, tone_on(&[5]));
    assert!(
        peak(&out[0]) < 1e-4,
        "muted channel heard: {}",
        peak(&out[0])
    );
    // Again at rest, silent; muted (channel 7) and kept silent past MUTE_REST, so the
    // bank never wakes; then its sound: still at rest, its input metered.
    let mut s = d.settings().clone();
    s.channels[5].mute = false;
    d.set(s, false);
    run(&mut d, REST_AFTER as usize + 50, tone_on(&[]));
    assert_eq!(d.resting_banks(), 2);
    let mut s = d.settings().clone();
    s.channels[6].mute = true;
    d.set(s, false);
    run(&mut d, MUTE_REST as usize + 20, tone_on(&[]));
    run(&mut d, 20, tone_on(&[6]));
    assert_eq!(
        d.resting_banks(),
        2,
        "a muted channel's sound doesn't wake its bank"
    );
    assert!(
        d.meters.channel_in[6].peak > 0.1,
        "input metered: {}",
        d.meters.channel_in[6].peak
    );
}

/// Lab 445 (c): a 32-channel desk, tones on 1–4, 5–8 silent and resting; channel 6 muted;
/// then sound into channel 6: nothing of it on main.
#[test]
fn lab_445c_muted_channel_into_a_resting_bank() {
    let mut d = Desk::new(32);
    let feed = |with6: bool| {
        move |b: usize| {
            (0..32)
                .map(|c| {
                    if c < 4 {
                        sine(300.0 + 50.0 * c as f32, 0.25, b * BLOCK)
                    } else if c == 5 && with6 {
                        sine(500.0, 0.5, b * BLOCK)
                    } else {
                        vec![0.0; BLOCK]
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    run(&mut d, REST_AFTER as usize + 50, feed(false));
    assert!(d.resting[5], "channels 5–8 rest");
    let before = run(&mut d, 20, feed(false));
    let mut s = d.settings().clone();
    s.channels[5].mute = true;
    d.set(s, false);
    run(&mut d, MUTE_REST as usize + 20, feed(false));
    let out = run(&mut d, 200, feed(true));
    let (a, b) = (peak(&before[0]), peak(&out[0]));
    assert!(
        (b - a).abs() < 0.02,
        "main grew from {a} to {b}: the muted channel is heard"
    );
}

/// 64-sample blocks: the same levels as 96, its buffers and rest
/// counters by its own block; an unknown size is taken as the default.
#[test]
fn a_desk_runs_every_block_size() {
    for bs in BLOCKS {
        let mut d = Desk::with_block(4, 4, OUTPUTS, bs);
        assert_eq!(d.block(), bs);
        assert_eq!(d.rest_after, (10.0 * rate()) as u32 / bs as u32);
        let sine = |b: usize| {
            (0..bs)
                .map(|i| {
                    0.5 * (2.0 * std::f32::consts::PI * 1000.0 * (b * bs + i) as f32 / rate()).sin()
                })
                .collect::<Vec<f32>>()
        };
        let mut out = vec![vec![0.0; bs]; OUTPUTS];
        // 100 ms of tone, whatever the block.
        for b in 0..4800 / bs {
            d.process(&[sine(b)], &mut out);
        }
        assert_eq!(out[0].len(), bs);
        // Centre pan: −3 dB each side, at every block size.
        let l = to_db(peak(&out[0])) - to_db(0.5);
        assert!((l + 3.0).abs() < 0.3, "{bs}: {l}");
    }
    // Not one of them (96 was the default before 2026-11-08): this machine's default.
    assert_eq!(Desk::with_block(2, 2, OUTPUTS, 96).block(), default_block());
    assert_eq!(
        Desk::with_block(2, 2, OUTPUTS, 100).block(),
        default_block()
    );
}

/// At 32 samples a block, a mute takes about 2 ms, not one block's 0.67 ms (bug run 5,
/// 3-3b): the first block after it still carries most of the sound, and by about the
/// fourth it is gone.
#[test]
fn a_mute_at_block_32_ramps_over_about_two_ms() {
    let mut d = Desk::with_block(1, 1, OUTPUTS, 32);
    let dc = |_: usize| vec![vec![0.5f32; 32]];
    let mut out = vec![vec![0.0; 32]; OUTPUTS];
    for _ in 0..200 {
        d.process(&dc(0), &mut out);
    }
    let before = out[0][31].abs();
    assert!(before > 0.1, "{before}");
    let mut s = d.settings().clone();
    s.channels[0].mute = true;
    d.set(s, false);
    d.process(&dc(0), &mut out);
    let after_one = out[0][31].abs();
    assert!(
        after_one > before * 0.5,
        "one block in: {after_one} of {before}"
    );
    for _ in 0..6 {
        d.process(&dc(0), &mut out);
    }
    assert!(out[0][31].abs() < before * 0.05, "{}", out[0][31]);
}

/// A delay's new length crossfades from the old: a steady signal stays steady when a
/// channel's line-up grows by noise suppression's 20 ms or shrinks back (bug round F,
/// F7-1: a 20 ms gap of silence, then a skip, on every channel).
#[test]
fn a_delay_change_has_no_gap_or_jump() {
    let mut d = super::super::Delay::default();
    let mut block = [1.0f32; 64];
    for _ in 0..40 {
        block.fill(1.0);
        d.run(&mut block, 0);
    }
    for len in [idn_dsp::denoise::LATENCY, 0, 480, idn_dsp::denoise::LATENCY] {
        for _ in 0..4 {
            block.fill(1.0);
            d.run(&mut block, len);
            assert!(
                block.iter().all(|v| (v - 1.0).abs() < 1e-6),
                "len {len}: {block:?}"
            );
        }
    }
    // An impulse comes out the new length late once the fade is over.
    let mut d = super::super::Delay::default();
    let mut x = vec![0.0f32; 4096];
    d.run(&mut x[..256], 100);
    x.fill(0.0);
    x[0] = 1.0;
    d.run(&mut x, 100);
    assert_eq!(x.iter().position(|v| *v == 1.0), Some(100));
}
