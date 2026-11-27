//! The processing: EQ bands and glides, filters, the strip, dynamics and the keyed ducker,
//! noise suppression.

use super::*;

/// Eight EQ bands (§M.15): a list of four or six (a patch giving its first bands) loads
/// with flat bands added after its own, which change nothing; one with more keeps its
/// first eight (a later build's file isn't refused whole).
#[test]
fn a_four_or_six_band_desk_loads_with_eight() {
    let old = serde_json::json!({"channels": [{"name": "Old", "eq": [
        {"on": true, "shape": "low_shelf", "freq": 100.0, "gain": 3.0, "q": 0.707},
        {"on": true, "shape": "peak", "freq": 500.0, "gain": -2.0, "q": 1.0},
        {"on": true, "shape": "peak", "freq": 2500.0, "gain": 0.0, "q": 0.707},
        {"on": true, "shape": "high_shelf", "freq": 8000.0, "gain": 1.0, "q": 0.707},
    ]}]});
    let s: Settings = serde_json::from_value(old).unwrap();
    let eq = &s.channels[0].eq;
    assert_eq!(eq.len(), EQ_BANDS);
    assert_eq!(
        (eq[0].gain, eq[1].freq, eq[3].shape),
        (3.0, 500.0, Shape::HighShelf)
    );
    assert!(
        eq[4..]
            .iter()
            .all(|b| b.gain == 0.0 && b.shape == Shape::Peak && !audible(b))
    );
    // Six bands: its own six, then two flat.
    let band = |g: f32| serde_json::json!({"on": true, "shape": "peak", "freq": 1000.0, "gain": g, "q": 1.0});
    let six = serde_json::json!({"channels": [{"eq": (1..=6).map(|g| band(g as f32)).collect::<Vec<_>>()}],
                                 "main": {"eq": (1..=6).map(|g| band(-(g as f32))).collect::<Vec<_>>()}});
    let s: Settings = serde_json::from_value(six).unwrap();
    for (eq, sign) in [(&s.channels[0].eq, 1.0), (&s.main.eq, -1.0)] {
        assert_eq!(
            eq[..6].iter().map(|b| b.gain).collect::<Vec<_>>(),
            (1..=6).map(|g| sign * g as f32).collect::<Vec<_>>()
        );
        assert!(eq[6..].iter().all(|b| !audible(b)));
    }
    // A fresh desk: eight bands, the main bus too.
    assert_eq!(Settings::new(2).main.eq.len(), 8);
    assert_eq!(Settings::new(2).channels[0].eq.len(), 8);
    // More than eight (a later build's): the first eight, the rest left out.
    let more = serde_json::json!({"channels": [{"eq": (1..=10).map(|g| band(g as f32)).collect::<Vec<_>>()}]});
    let s: Settings = serde_json::from_value(more).unwrap();
    assert_eq!(s.channels[0].eq[7].gain, 8.0);
    // No bands at all: eight flat ones.
    let none = serde_json::json!({"channels": [{"eq": []}]});
    let s: Settings = serde_json::from_value(none).unwrap();
    assert!(s.channels[0].eq.iter().all(|b| !audible(b)));
}

/// Band 8 of the EQ is heard: +6 dB at its frequency, on a channel and on the main bus.
#[test]
fn the_eighth_eq_band_is_heard() {
    for on_main in [false, true] {
        let mut d = Desk::new(2);
        let mut s = d.settings().clone();
        let eq = if on_main {
            &mut s.main.eq
        } else {
            &mut s.channels[0].eq
        };
        eq[7] = Band {
            on: true,
            shape: Shape::Peak,
            freq: 1000.0,
            gain: 6.0,
            q: 1.0,
        };
        d.set(s, false);
        let out = run(&mut d, 300, |b| vec![sine(1000.0, 0.25, b * BLOCK)]);
        let l = to_db(peak(&out[0])) - to_db(0.25);
        assert!(
            (l - (6.0 - 3.01)).abs() < 0.3,
            "+6 dB less the pan law (main {on_main}): {l}"
        );
    }
}

/// An EQ band moved while a tone plays glides: no edge in the sound (
/// 0 → +6 dB at once was a click), and it gets there.
#[test]
fn eq_changes_glide_without_a_click() {
    let mut d = Desk::new(1);
    let mut s = d.settings().clone();
    s.channels[0].pan = -1.0;
    s.channels[0].eq[1].freq = 1000.0;
    s.channels[0].eq[1].q = 1.0;
    d.set(s, false);
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    let mut l = Vec::new();
    for b in 0..60 {
        if b == 30 {
            let mut s = d.settings().clone();
            s.channels[0].eq[1].gain = 6.0;
            d.set(s, false);
        }
        d.process(&[sine(1000.0, 0.25, b * BLOCK)], &mut out);
        l.extend_from_slice(&out[0]);
    }
    // The largest second difference: a steady 1 kHz sine's is 0.25 × (2π/48)² ≈ 0.004
    // of its level; a jump in level shows as far more.
    let edge = |r: std::ops::Range<usize>| {
        r.map(|i| (l[i + 1] - 2.0 * l[i] + l[i - 1]).abs())
            .fold(0f32, f32::max)
    };
    let after = edge(45 * BLOCK..60 * BLOCK - 1);
    let change = edge(30 * BLOCK..40 * BLOCK);
    assert!(
        change < after * 1.1,
        "after {after}, at the change {change}"
    );
    let tail = &l[50 * BLOCK..];
    let lv = to_db(peak(tail)) - to_db(peak(&l[10 * BLOCK..30 * BLOCK]));
    assert!((lv - 6.0).abs() < 0.2, "+6 dB at 1 kHz: {lv}");
}

#[test]
fn bus_gain_reduction_is_metered() {
    let mut d = Desk::new(1);
    let mut s = d.settings().clone();
    s.channels[0].pan = -1.0;
    s.main.comp = Comp {
        on: true,
        threshold: -20.0,
        ratio: 4.0,
        attack: 1.0,
        release: 50.0,
        knee: 0.0,
        makeup: 0.0,
        ..Default::default()
    };
    d.set(s, false);
    run(&mut d, 500, |_| vec![vec![db(-4.0); BLOCK]]);
    let gr = d.meters().bus_gr.clone();
    assert_eq!(gr.len(), BUS_STRIPS);
    assert!((gr[0].0 - 12.0).abs() < 0.3, "main {:?}", gr[0]);
    assert_eq!(gr[1], (0.0, 0.0));
}

#[test]
fn the_strip_processes() {
    let mut d = Desk::new(1);
    let mut s = d.settings().clone();
    s.channels[0].pan = -1.0; // all on the left: 0 dB
    s.channels[0].hpf = 100.0;
    d.set(s, false);
    let out = run(&mut d, 500, |b| vec![sine(30.0, 0.5, b * BLOCK)]);
    assert!(to_db(peak(&out[0])) - to_db(0.5) < -18.0, "the high-pass");
    let mut s = d.settings().clone();
    s.channels[0].hpf = 0.0;
    s.channels[0].comp = Comp {
        on: true,
        threshold: -20.0,
        ratio: 4.0,
        attack: 1.0,
        release: 50.0,
        knee: 0.0,
        makeup: 0.0,
        ..Default::default()
    };
    d.set(s, false);
    let out = run(&mut d, 500, |_| vec![vec![db(-4.0); BLOCK]]);
    let v = to_db(peak(&out[0]));
    assert!((v + 16.0).abs() < 0.3, "compressed to {v}");
    assert!((d.meters().channel_gr[0].1 - 12.0).abs() < 0.3);
    // Off: no reduction shown.
    let mut s = d.settings().clone();
    s.channels[0].comp.on = false;
    d.set(s, false);
    let _ = run(&mut d, 2, |_| vec![vec![db(-4.0); BLOCK]]);
    assert_eq!(d.meters().channel_gr[0], (0.0, 0.0, 0.0, 0.0));
}

#[test]
fn low_pass_delay_and_limiter() {
    let mut d = Desk::new(1);
    let mut s = plain(&d);
    s.channels[0].lpf = 2000.0;
    d.set(s, false);
    let out = run(&mut d, 300, |b| vec![sine(12000.0, 0.5, b * BLOCK)]);
    assert!(level(&out[0], 0.5) < -18.0, "the low-pass");
    let mut s = plain(&d);
    s.channels[0].lpf = 0.0;
    s.channels[0].delay = 1.0; // 48 samples
    d.set(s, false);
    let out = run(&mut d, 3, impulse_at(2, 10));
    assert!((out[0][58] - 0.5).abs() < 1e-6, "{:?}", &out[0][50..64]);
    let mut s = plain(&d);
    s.channels[0].delay = 0.0;
    s.channels[0].limiter.on = true;
    s.channels[0].limiter.ceiling = -10.0;
    d.set(s, false);
    let out = run(&mut d, 300, |_| vec![vec![db(-2.0); BLOCK]]);
    assert!(
        (to_db(peak(&out[0])) + 10.0).abs() < 0.2,
        "{}",
        to_db(peak(&out[0]))
    );
    assert!((d.meters().channel_gr[0].2 - 8.0).abs() < 0.3);
}

/// Lab 498-A: a channel's noise suppression switched on and off under a steady tone
/// doesn't click: the sharpest bend in the main mix stays near a steady tone's (it was
/// 12 to 68 times that).
#[test]
fn switching_noise_suppression_doesnt_click() {
    let mut d = Desk::new(1);
    let mut s = plain(&d);
    s.channels[0].pan = 0.0;
    d.set(s, false);
    d.set_routed(vec![true]);
    let input = |b: usize| vec![sine(997.0, 0.5, b * BLOCK)];
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    // The sharpest bend (second difference) of main L over `blocks` blocks from `from`.
    let mut bend = |d: &mut Desk, from: usize, blocks: usize| {
        let (mut a, mut b, mut worst) = (0.0f32, 0.0f32, 0.0f32);
        for k in from..from + blocks {
            d.process(&input(k), &mut out);
            for (i, &x) in out[0].iter().enumerate() {
                if k > from || i >= 2 {
                    worst = worst.max((x - 2.0 * a + b).abs());
                }
                (b, a) = (a, x);
            }
        }
        worst
    };
    let per_second = (rate() as usize) / BLOCK;
    let _settle = bend(&mut d, 0, per_second);
    let steady = bend(&mut d, per_second, per_second);
    assert!(steady > 0.0);
    let mut worst = 0.0f32;
    let mut k = 2 * per_second;
    for n in 0..8 {
        let mut s = d.settings().clone();
        s.channels[0].denoise = n % 2 == 0;
        d.set(s, false);
        // Half a second after each switch (past the fade and the suppressor's start).
        worst = worst.max(bend(&mut d, k, per_second / 2));
        k += per_second / 2;
    }
    assert!(
        worst < 2.0 * steady,
        "a click: {:.1} times a steady tone's bend",
        worst / steady
    );
}

/// The artboard's dynamics (§M.28): a ducker keyed from another channel (music under a
/// voice), an expander gentler than a gate, the hold keeping a gate open, the
/// compressor's mix (0 %: dry) and RMS detection.
#[test]
fn keyed_ducker_expander_hold_and_parallel_compression() {
    let music = |b: usize| sine(300.0, 0.3, b * BLOCK);
    let voice = |b: usize, on: bool| sine(1000.0, if on { 0.3 } else { 0.0 }, b * BLOCK);
    let mut d = Desk::new(2);
    let mut s = d.settings().clone();
    // Only the music in the main mix; its gate a ducker keyed from channel 2.
    s.channels[1].main = false;
    s.channels[1].stream.on = false;
    s.channels[0].gate = Gate {
        on: true,
        threshold: -30.0,
        range: 12.0,
        attack: 1.0,
        release: 50.0,
        mode: GateMode::Ducker,
        key: 2,
        ..Default::default()
    };
    d.set(s, false);
    let quiet = peak(&run(&mut d, 60, |b| vec![music(b), voice(b, false)])[0]);
    let ducked = peak(&run(&mut d, 60, |b| vec![music(b), voice(b, true)])[0]);
    let down = to_db(quiet) - to_db(ducked);
    assert!((down - 12.0).abs() < 1.0, "ducked by its range: {down}");
    let back = peak(&run(&mut d, 100, |b| vec![music(b), voice(b, false)])[0]);
    assert!((to_db(back) - to_db(quiet)).abs() < 0.5, "back up after");
    // The voice's channel parked while it talks: its key is silence, not its last block
    // heard again and again (bug run P).
    run(&mut d, 60, |b| vec![music(b), voice(b, true)]);
    let mut s = d.settings().clone();
    s.channels[1].shown = Some(false);
    d.set(s, false);
    let parked = peak(&run(&mut d, 150, |b| vec![music(b), voice(b, true)])[0]);
    assert!(
        (to_db(parked) - to_db(quiet)).abs() < 0.5,
        "a parked key ducks nothing: {} vs {}",
        to_db(parked),
        to_db(quiet)
    );
    // An expander takes 10 dB under the threshold down by (ratio − 1) × 10 = 10 dB at 2:1;
    // a gate by its whole range.
    let low = |b: usize| vec![sine(500.0, db(-40.0), b * BLOCK)];
    let level_with = |mode: GateMode| {
        let mut d = Desk::new(1);
        let mut s = d.settings().clone();
        s.channels[0].gate = Gate {
            on: true,
            threshold: -30.0,
            range: 40.0,
            mode,
            ratio: 2.0,
            ..Default::default()
        };
        d.set(s, false);
        to_db(peak(&run(&mut d, 200, low)[0]))
    };
    let (gate, exp) = (level_with(GateMode::Gate), level_with(GateMode::Expander));
    let open = -40.0 - 3.01;
    assert!(
        (open - gate - 40.0).abs() < 1.5,
        "gate closed by its range: {gate}"
    );
    assert!(
        (open - exp - 10.0).abs() < 2.0,
        "expander 10 dB down: {exp}"
    );
    // Hold: a burst then silence; with 500 ms hold the gate is still open 100 ms later.
    let after_burst = |hold: f32| {
        let mut d = Desk::new(1);
        let mut s = d.settings().clone();
        s.channels[0].gate = Gate {
            on: true,
            threshold: -30.0,
            range: 60.0,
            release: 20.0,
            hold,
            ..Default::default()
        };
        d.set(s, false);
        let blocks = (0.1 * rate()) as usize / BLOCK;
        run(&mut d, 50, |b| vec![sine(500.0, 0.5, b * BLOCK)]);
        // Under the threshold (−40 dB), 100 ms on.
        let out = run(&mut d, blocks, |b| vec![sine(500.0, db(-40.0), b * BLOCK)]);
        to_db(peak(&out[0]))
    };
    let (held, not) = (after_burst(500.0), after_burst(0.0));
    assert!(held > -45.0, "held open: {held}");
    assert!(not < -80.0, "closed without hold: {not}");
    // The compressor's mix: 0 % is the dry sound, 100 % compressed, RMS hears less of a
    // sine's peak (3 dB under), so takes less off.
    let comp_out = |mix: f32, detect: Detect| {
        let mut d = Desk::new(1);
        let mut s = d.settings().clone();
        s.channels[0].comp = Comp {
            on: true,
            threshold: -30.0,
            ratio: 10.0,
            knee: 0.0,
            mix,
            detect,
            ..Default::default()
        };
        d.set(s, false);
        to_db(peak(
            &run(&mut d, 200, |b| vec![sine(500.0, 0.5, b * BLOCK)])[0],
        ))
    };
    let dry = comp_out(0.0, Detect::Peak);
    let wet = comp_out(100.0, Detect::Peak);
    let rms = comp_out(100.0, Detect::Rms);
    assert!((dry - (to_db(0.5) - 3.01)).abs() < 0.2, "0 %: dry {dry}");
    assert!(wet < dry - 15.0, "compressed: {wet}");
    assert!(rms > wet + 1.0, "RMS takes less off a sine: {rms} vs {wet}");
}

/// Filter slopes (§M.28): an octave under a 1 kHz high-pass, a slope of 6·N dB an octave
/// takes about 10·log10(1 + 4^N) dB off (Butterworth); 12 dB stays as it always was.
#[test]
fn high_pass_slopes() {
    let under = |slope: u8| {
        let mut d = Desk::new(1);
        let mut s = d.settings().clone();
        (s.channels[0].hpf, s.channels[0].hpf_slope) = (1000.0, slope);
        d.set(s, false);
        let out = run(&mut d, 200, |b| vec![sine(500.0, 0.5, b * BLOCK)]);
        to_db(0.5) - 3.01 - to_db(peak(&out[0]))
    };
    for (slope, n) in [(6u8, 1), (12, 2), (18, 3), (24, 4), (48, 8)] {
        let want = 10.0 * (1.0 + 4f32.powi(n)).log10();
        let got = under(slope);
        assert!((got - want).abs() < 1.0, "{slope} dB/oct: {got} vs {want}");
    }
    // An unknown slope is the nearest one.
    let mut s = Desk::new(1).settings().clone();
    s.channels[0].hpf_slope = 30;
    let s = s.normalised(1, OUTPUTS);
    assert!(s.channels[0].hpf_slope == 24 || s.channels[0].hpf_slope == 36);
}

/// The vintage bell (§M.28): a small boost wider than a peak band's at the same Q, a
/// large one narrower (an octave above, more and less of it).
#[test]
fn a_vintage_bell_widens_and_narrows_with_its_gain() {
    use crate::dsp::{Filter, Shape};
    let octave_up = |shape: Shape, gain: f32| {
        let mut f = Filter::new(shape, 48_000.0, 1000.0, gain, 1.0);
        let mut top = 0f32;
        for k in 0..48_000 {
            let y = f.run((2.0 * std::f32::consts::PI * 2000.0 * k as f32 / 48_000.0).sin());
            if k > 24_000 {
                top = top.max(y.abs());
            }
        }
        to_db(top)
    };
    assert!(octave_up(Shape::Veq, 3.0) > octave_up(Shape::Peak, 3.0) + 0.3);
    assert!(octave_up(Shape::Veq, 15.0) < octave_up(Shape::Peak, 15.0) - 1.0);
    // At ±6 dB the same.
    assert!((octave_up(Shape::Veq, 6.0) - octave_up(Shape::Peak, 6.0)).abs() < 0.05);
}

/// The noise suppression's amount (§M.28): 0 % is the sound as it came, delayed as the
/// suppression delays it; 100 % takes steady noise down.
#[test]
fn the_noise_suppressions_amount_blends_the_dry_sound() {
    let rms_with = |on: bool, amount: f32| {
        let mut d = Desk::new(1);
        let mut s = d.settings().clone();
        let c = &mut s.channels[0];
        (c.eq_on, c.pan, c.denoise, c.denoise_amount) = (false, -1.0, on, amount);
        s.main.eq_on = false;
        d.set(s, false);
        let mut seed = 12345u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut sum = 0f64;
        let mut n = 0usize;
        let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
        for b in 0..(48_000 * 2 / BLOCK) {
            let x: Vec<f32> = (0..BLOCK).map(|_| noise() * 0.1).collect();
            d.process(&[x], &mut out);
            if b > 48_000 / BLOCK {
                sum += out[0]
                    .iter()
                    .map(|v| f64::from(*v) * f64::from(*v))
                    .sum::<f64>();
                n += BLOCK;
            }
        }
        10.0 * (sum / n as f64).log10()
    };
    let off = rms_with(false, 100.0);
    let dry = rms_with(true, 0.0);
    let full = rms_with(true, 100.0);
    assert!((dry - off).abs() < 0.5, "0 %: as it came ({dry} vs {off})");
    assert!(full < off - 3.0, "100 %: noise down ({full} vs {off})");
    let half = rms_with(true, 50.0);
    assert!(
        half < dry - 0.5 && half > full + 0.5,
        "50 % between: {half}"
    );
}
