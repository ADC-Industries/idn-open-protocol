//! By site time and on the device's own clock.

use super::*;

/// R33-A (2026-11-16): a source whose sound is 6 ms old when it gets here (a
/// network-audio device's blocks, stamped with their time on its network), by site time
/// at a 2 ms target, 1 ms blocks. It never started: every block was "too old" and
/// dropped, with nothing counted. Now, after 200 ms of that, the target takes the age in
/// and the lane plays; adapting, it doesn't come back down under it.
fn an_old_source_by_time(adaptive: bool) -> (bool, LaneStats, u64) {
    let _turn = REAL_TIME.lock_ok();
    let map = Mapping {
        mono_ns: 0,
        site_ns: 0,
        rate: 1.0,
    };
    let (shared, mut inner, mut cfg, stats) = rig(2);
    if adaptive {
        cfg.adaptive = true;
        cfg.min_ms = 1;
    }
    let start = Instant::now();
    // Each block's time: 6 ms before it is pushed.
    let t0 = crate::ptp::mono_ns() as Ns - 6_000_000;
    let (mut pushed, mut next_push, mut next_pull) = (0u64, start, start);
    let (mut heard_late, mut underruns_at_3s) = (false, 0);
    let mut out = [0f32; 128];
    while start.elapsed() < Duration::from_secs(8) {
        let now = Instant::now();
        if now >= next_push {
            let ch = vec![0.25f32; 48];
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                Some((t0 + pushed as Ns * 1_000_000, map)),
            );
            pushed += 1;
            next_push += Duration::from_micros(1000);
        }
        if now >= next_pull {
            out.fill(0.0);
            shared.fill(&mut out);
            // Sound in the last three seconds: it plays, and went on playing.
            if start.elapsed() >= Duration::from_secs(5) && out.iter().any(|x| x.abs() > 0.2) {
                heard_late = true;
            }
            next_pull += Duration::from_micros(64 * 1_000_000 / 48_000);
        }
        if underruns_at_3s == 0 && start.elapsed() >= Duration::from_secs(3) {
            underruns_at_3s = shared.underruns.load(Ordering::Relaxed).max(1);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let late_underruns = shared.underruns.load(Ordering::Relaxed).max(1) - underruns_at_3s;
    let st = stats.lock_ok().clone();
    (heard_late, st, late_underruns)
}

#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn an_old_source_by_time_plays_at_a_set_latency() {
    let (heard, st, late_underruns) = an_old_source_by_time(false);
    assert!(st.by_time);
    assert!(
        heard,
        "silent: margin {} ms, late {} ms",
        st.margin_ms, st.late_ms
    );
    // The 2 ms set and at least the 4 ms more the source's age needs.
    assert!(st.margin_ms >= 4.0, "margin {} ms", st.margin_ms);
    assert!(late_underruns < 5, "{late_underruns} underruns after 3 s");
}

#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn an_old_source_by_time_plays_adapting() {
    let (heard, st, late_underruns) = an_old_source_by_time(true);
    assert!(st.by_time && st.adaptive);
    assert!(
        heard,
        "silent: target {} ms, late {} ms",
        st.target_ms, st.late_ms
    );
    // Not back under the source's age once settled.
    assert!(st.target_ms >= 6, "target {} ms", st.target_ms);
    assert!(late_underruns < 5, "{late_underruns} underruns after 3 s");
}

#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_small_target_by_time_settles() {
    // Lab item 129: by site time at a 10 ms target, 2 ms blocks up to 1 ms late, a
    // device taking 128 frames: about 100 underruns a second before. Now a few at
    // the start at most, then none.
    let map = Mapping {
        mono_ns: 0,
        site_ns: 0,
        rate: 1.0,
    };
    let (shared, mut inner, cfg, stats) = rig(10);
    let start = Instant::now();
    let t0 = crate::ptp::mono_ns() as Ns;
    let (mut pushed, mut next_push, mut next_pull) = (0u64, start, start);
    let jitter = |n: u64| Duration::from_micros((n * 7919 % 11) * 100);
    let mut at_3s = None;
    while start.elapsed() < Duration::from_secs(6) {
        let now = Instant::now();
        if now >= next_push + jitter(pushed) {
            let ch = vec![0.1f32; 96];
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                Some((t0 + pushed as Ns * 2_000_000, map)),
            );
            pushed += 1;
            next_push += Duration::from_micros(2000);
        }
        if now >= next_pull {
            shared.fill(&mut [0f32; 256]);
            next_pull += Duration::from_micros(128 * 1_000_000 / 48_000);
        }
        if at_3s.is_none() && start.elapsed() >= Duration::from_secs(3) {
            at_3s = Some(shared.underruns.load(Ordering::Relaxed));
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let (total, late) = (
        shared.underruns.load(Ordering::Relaxed),
        shared.underruns.load(Ordering::Relaxed) - at_3s.unwrap(),
    );
    let st = stats.lock_ok();
    assert!(st.by_time);
    assert_eq!(
        late, 0,
        "underruns after 3 s ({total} in all), margin {} ms",
        st.margin_ms
    );
    assert!(total < 10, "{total} underruns at the start");
}
/// A lane on the device's own clock (exactly 48 kHz both sides) for `secs`, by site time
/// if `timed` (else held by its fill, as a program and the desk on one machine are),
/// samples passing through from 2 s on (settled) if `force`, else when it decides to: when
/// it started passing through, the blocks after that were exactly the input, the
/// underruns after 2 s, and its stats at the end.
fn on_the_device_clock(
    force: bool,
    timed: bool,
    secs: u64,
) -> (Option<Instant>, u32, u64, LaneStats) {
    let _turn = REAL_TIME.lock_ok();
    let map = Mapping {
        mono_ns: 0,
        site_ns: 0,
        rate: 1.0,
    };
    let (mut shared, mut inner, cfg, stats) = rig(20);
    shared.through = true;
    let start = Instant::now();
    let t0 = crate::ptp::mono_ns() as Ns;
    let (mut pushed, mut pulled, mut next_push, mut next_pull) = (0u64, 0u64, start, start);
    let mut at_2s = None;
    let mut through_at = None;
    let mut out = [0f32; 256];
    let mut exact = 0;
    while start.elapsed() < Duration::from_secs(secs) {
        let now = Instant::now();
        if now >= next_push {
            let ch = vec![0.25f32; 96];
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                timed.then_some((t0 + pushed as Ns * 2_000_000, map)),
            );
            if force && pushed == 1000 {
                inner.rate.as_mut().unwrap().2.set_through(true);
                inner.watch.timed = timed;
            }
            pushed += 1;
            next_push += Duration::from_micros(2000);
        }
        if now >= next_pull {
            shared.fill(&mut out);
            // Exactly 48 kHz (2666 µs a block would be 250 ppm fast).
            pulled += 1;
            next_pull = start + Duration::from_nanos(pulled * 128 * 1_000_000_000 / 48_000);
            if through_at.is_some_and(|t: Instant| now.duration_since(t).as_millis() > 100) {
                // The filter's ripple is gone: the samples themselves.
                exact += out.iter().all(|&v| v == 0.25) as u32;
            }
        }
        if at_2s.is_none() && start.elapsed() >= Duration::from_secs(2) {
            at_2s = Some(shared.underruns.load(Ordering::Relaxed));
        }
        if through_at.is_none() && stats.lock_ok().through {
            through_at = Some(now);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let late = shared.underruns.load(Ordering::Relaxed) - at_2s.unwrap();
    let st = stats.lock_ok().clone();
    (through_at, exact, late, st)
}

/// Passing through on the device's clock: the samples themselves, played on without
/// a gap, and still passing through at the end (what offset there was set right by a
/// cut or a pad, not by leaving).
#[test]
fn passing_through_plays_the_samples_themselves() {
    for timed in [true, false] {
        let (through_at, exact, late, st) = on_the_device_clock(true, timed, 9);
        assert!(through_at.is_some());
        assert!(
            st.through,
            "timed {timed}: left passing through ({}), late {} ms, fill {} against {} ms",
            st.through_ended, st.late_ms, st.fill_ms, st.hold_ms
        );
        assert!(
            exact > 1000,
            "timed {timed}: {exact} blocks exactly the input"
        );
        assert_eq!(late, 0, "timed {timed}: underruns after 2 s");
    }
}

/// The same, deciding by itself: steady for the calm time with no drift measured, the
/// samples pass through. Real time: a 3 s window (30 s outside tests) under a loaded
/// test run measures up to tens of ppm.
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_lane_on_the_device_clock_passes_through() {
    for timed in [true, false] {
        let (through_at, exact, late, st) = on_the_device_clock(false, timed, 20);
        assert!(
            through_at.is_some(),
            "timed {timed}: never passed through: drift {} ppm, late {} ms, fill {} \
                 against {} ms, correction {} ppm",
            st.calm_drift_ppm,
            st.late_ms,
            st.fill_ms,
            st.hold_ms,
            st.correction_ppm
        );
        assert!(st.through, "timed {timed}: left passing through");
        assert!(
            exact > 100,
            "timed {timed}: {exact} blocks exactly the input"
        );
        assert_eq!(late, 0, "timed {timed}: underruns after 2 s");
    }
}
/// A lower latency by site time (idn-routing.md §A.12): the surplus is
/// cut within a second or two, not slewed off at 1000 ppm (60 ms to 2 took a minute),
/// and the margin underruns added to the old latency is gone.
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_lower_latency_by_time_is_cut_at_once() {
    let mixer = Mixer::new(48000, 2, "test");
    let lane = mixer.lane(&LaneConfig {
        latency_ms: 60,
        max_correction_ppm: 1000.0,
        ..Default::default()
    });
    lane.inner.lock_ok().margin_ms = 6.0;
    let map = Mapping {
        mono_ns: 0,
        site_ns: 0,
        rate: 1.0,
    };
    let start = Instant::now();
    let t0 = crate::ptp::mono_ns() as Ns;
    let (mut pushed, mut next_push, mut next_pull) = (0u64, start, start);
    let mut changed: Option<(Instant, f64)> = None;
    let mut settled_after = None;
    while start.elapsed() < Duration::from_secs(9) {
        let now = Instant::now();
        if now >= next_push {
            let ch = vec![0.1f32; 96];
            let at = t0 + pushed as Ns * 2_000_000;
            let cfg = lane.cfg.lock_ok().clone();
            push_inner(
                &lane.shared,
                &mut lane.inner.lock_ok(),
                &PushCfg::of(&cfg),
                &lane.stats,
                48000,
                &[ch.clone(), ch],
                None,
                Some((at, map)),
            );
            pushed += 1;
            next_push += Duration::from_micros(2000);
        }
        if now >= next_pull {
            let mut buf = vec![0f32; 128 * 2];
            mixer.fill(&mut buf);
            next_pull += Duration::from_micros(128 * 1_000_000 / 48_000);
        }
        if changed.is_none() && start.elapsed() >= Duration::from_secs(4) {
            let held = lane.stats().fill_ms;
            lane.set_latency(2);
            assert_eq!(
                lane.inner.lock_ok().margin_ms,
                0.0,
                "the old latency's margin gone"
            );
            changed = Some((now, held));
        }
        if let Some((t, _)) = changed
            && settled_after.is_none()
            && now.duration_since(t) > Duration::from_millis(200)
            && lane.stats().fill_ms < 12.0
        {
            settled_after = Some(now.duration_since(t));
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let st = lane.stats();
    let (_, held_before) = changed.unwrap();
    assert!(held_before > 50.0, "60 ms held before: {held_before:.1}");
    let took = settled_after.expect("never down to the new latency");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(
        st.dropped_ms > 40.0,
        "the surplus cut: {:.1} ms",
        st.dropped_ms
    );
    // (Underruns at 2 ms in a test thread may grow it again, a step of 1 ms each.)
    eprintln!(
        "took {took:?}, dropped {:.1} ms, margin now {:.1} ms",
        st.dropped_ms, st.margin_ms
    );
}

#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn two_lanes_by_time_play_the_same_sample() {
    // idn-clock.md §C.6 test 3: the same 2 ms blocks, site-timed, 0-3 ms late, into
    // two devices whose clocks run 200 ppm fast and 300 ppm slow. Played by time with
    // the same latency, they play the same source sample within 1 ms. The signal is
    // the source sample's index, so what plays says which sample it is.
    let map = Mapping {
        mono_ns: 0,
        site_ns: 0,
        rate: 1.0,
    };
    let mut rigs = [rig(20), rig(20)];
    let drift = [200e-6, -300e-6];
    let start = Instant::now();
    let t0 = crate::ptp::mono_ns() as Ns;
    let (block, dev_block) = (96usize, 128usize);
    let mut next_pull = [start, start];
    let (mut pushed, mut next_push) = (0u64, start);
    let jitter = |n: u64| Duration::from_micros((n * 7919 % 31) * 100);
    // (when, index playing) per device, after 10 s (drifts this large take a few seconds
    // to learn; real devices are within about 50 ppm).
    let mut seen: [Vec<(i64, f32)>; 2] = [Vec::new(), Vec::new()];
    while start.elapsed() < Duration::from_secs(16) {
        let now = Instant::now();
        if now >= next_push + jitter(pushed) {
            let first = pushed as usize * block;
            let ch: Vec<f32> = (0..block).map(|i| (first + i) as f32).collect();
            let at = t0 + pushed as Ns * 2_000_000;
            for (shared, inner, cfg, stats) in rigs.iter_mut() {
                push_inner(
                    shared,
                    inner,
                    &PushCfg::of(cfg),
                    stats,
                    48000,
                    &[ch.clone(), ch.clone()],
                    None,
                    Some((at, map)),
                );
            }
            pushed += 1;
            next_push += Duration::from_micros(2000);
        }
        for d in 0..2 {
            if now >= next_pull[d] {
                let mut buf = vec![0f32; dev_block * 2];
                rigs[d].0.fill(&mut buf);
                // The device's own clock says when its block plays: the pull's scheduled
                // time (a late-woken test thread isn't a late device).
                if start.elapsed() > Duration::from_secs(10)
                    && rigs[d].0.ready.load(Ordering::Relaxed)
                {
                    let at = t0 as i64 + next_pull[d].duration_since(start).as_nanos() as i64;
                    seen[d].push((at, buf[0]));
                }
                let period = dev_block as f64 / (48_000.0 * (1.0 + drift[d]));
                next_pull[d] += Duration::from_secs_f64(period);
            }
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    // Where device 1 was at device 0's moments, by interpolating its line.
    let mut worst = 0f64;
    let mut diffs = Vec::new();
    for &(t, a) in &seen[0] {
        let i = seen[1].partition_point(|&(u, _)| u < t);
        if i == 0 || i >= seen[1].len() {
            continue;
        }
        let ((t1, b1), (t2, b2)) = (seen[1][i - 1], seen[1][i]);
        let b = b1 + (b2 - b1) * ((t - t1) as f32 / (t2 - t1).max(1) as f32);
        let d = ((a - b) / 48.0) as f64;
        diffs.push(((t - t0 as i64) as f64 / 1e9, d));
        worst = worst.max(d.abs());
    }
    for (t, d) in diffs.iter().step_by(100) {
        eprintln!("{t:.2} s: {d:+.3} ms");
    }
    if let Some((t, d)) = diffs
        .iter()
        .max_by(|x, y| x.1.abs().partial_cmp(&y.1.abs()).unwrap())
    {
        eprintln!("worst at {t:.3} s: {d:+.3} ms");
    }
    for r in &rigs {
        let st = r.3.lock_ok();
        eprintln!(
            "late {:.3} ms, ppm {:.0}, dropped {:.1}, underruns {}",
            st.late_ms,
            st.correction_ppm,
            st.dropped_ms,
            r.0.underruns.load(Ordering::Relaxed)
        );
    }
    assert!(rigs.iter().all(|r| r.3.lock_ok().by_time));
    assert!(seen[0].len() > 1000 && seen[1].len() > 1000);
    assert!(worst < 1.0, "the devices were up to {worst:.2} ms apart");
}
/// By site time, as a desk plays a lane (10 ms, 2 ms blocks both sides): a tone that
/// starts, pauses and starts again rises each time with one fade and no step (lab 372:
/// the fade-in was cut short by what the ring held at the start, then a jump to full).
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_timed_start_fades_in_whole() {
    let _turn = REAL_TIME.lock_ok();
    let map = Mapping {
        mono_ns: 0,
        site_ns: 0,
        rate: 1.0,
    };
    let (shared, mut inner, cfg, stats) = rig(10);
    let start = Instant::now();
    let t0 = crate::ptp::mono_ns() as Ns;
    let (mut block, mut next_push, mut pulled) = (0u64, start, 0u64);
    let mut heard: Vec<f32> = Vec::new();
    while start.elapsed() < Duration::from_secs(4) {
        let now = Instant::now();
        let s = start.elapsed().as_secs_f64();
        if now >= next_push {
            // Silent from 1.5 s to 2.5 s (the source paused).
            if !(1.5..2.5).contains(&s) {
                let ch: Vec<f32> = (0..96u64)
                    .map(|i| 0.5 * ((block * 96 + i) as f32 * std::f32::consts::TAU / 48.0).sin())
                    .collect();
                push_inner(
                    &shared,
                    &mut inner,
                    &PushCfg::of(&cfg),
                    &stats,
                    48000,
                    &[ch.clone(), ch],
                    None,
                    // Taken 7 ms before it arrives (the network, the sender's
                    // block): 3 ms of the 10 left when it starts.
                    Some((t0 + block as Ns * 2_000_000 - 7_000_000, map)),
                );
            }
            block += 1;
            next_push += Duration::from_micros(2000);
        }
        if now >= start + Duration::from_micros(pulled * 2000) {
            let mut buf = [0f32; 96 * 2];
            shared.fill(&mut buf);
            heard.extend(buf.iter().step_by(2));
            pulled += 1;
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let peaks: Vec<f32> = heard
        .chunks(48)
        .map(|c| c.iter().fold(0f32, |m, v| m.max(v.abs())) / 0.5)
        .collect();
    let mut starts = Vec::new();
    for k in 1..peaks.len() {
        if peaks[k - 1] < 0.02 && peaks[k] >= 0.02 {
            starts.push(k);
        }
    }
    assert!(starts.len() >= 2, "starts at {starts:?}");
    for &k in &starts {
        let rise = &peaks[k..(k + 12).min(peaks.len())];
        assert!(
            rise.windows(2).all(|w| w[1] - w[0] <= 0.3),
            "a step in the start at {k} ms: {rise:?}"
        );
    }
}
