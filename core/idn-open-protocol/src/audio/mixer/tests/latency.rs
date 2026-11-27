//! Latency: adapting lanes, latency changes, and a lane's latency as heard.

use super::*;

/// Adapting (R.30): 2 ms blocks into a 128-frame device, starting at 20 ms. First a
/// rough path (up to 8 ms late): the target rises past it; then a calm one (up to 0.3 ms):
/// it comes back down, to a few ms; no underruns once grown. Real time, as above.
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn an_adapting_lane_follows_its_path_both_ways() {
    let (shared, mut inner, mut cfg, stats) = rig(20);
    cfg.adaptive = true;
    inner.auto_ms = 20.0;
    let start = Instant::now();
    let (mut next_push, mut next_pull) = (start, start);
    let (mut pushed, mut t) = (0u64, 0usize);
    let (mut rough_target, mut rough_underruns) = (0u32, 0u64);
    let mut calm_from = None;
    // (n × 7919) mod: repeatable lateness, never early.
    let late = |n: u64, max_100us: u64| Duration::from_micros((n * 7919 % (max_100us + 1)) * 100);
    while start.elapsed() < Duration::from_secs(12) {
        let now = Instant::now();
        let rough = start.elapsed() < Duration::from_secs(4);
        if now >= next_push + late(pushed, if rough { 80 } else { 3 }) {
            let ch: Vec<f32> = (0..96)
                .map(|i| {
                    0.25 * ((t + i) as f32 * 2.0 * std::f32::consts::PI * 997.0 / 48000.0).sin()
                })
                .collect();
            t += 96;
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                None,
            );
            pushed += 1;
            next_push += Duration::from_micros(2000);
        }
        if now >= next_pull {
            let mut buf = vec![0f32; 128 * 2];
            shared.fill(&mut buf);
            next_pull += Duration::from_micros(128 * 1_000_000 / 48_000);
        }
        if rough && start.elapsed() >= Duration::from_millis(3900) {
            rough_target = stats.lock_ok().target_ms;
            rough_underruns = shared.underruns.load(Ordering::Relaxed);
        }
        if !rough && calm_from.is_none() {
            calm_from = Some(shared.underruns.load(Ordering::Relaxed));
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let st = stats.lock_ok();
    assert!(st.adaptive);
    assert!(rough_target >= 9, "rough path: target {rough_target} ms");
    assert!(
        st.target_ms <= 4,
        "calm path: target {} ms (jitter {:.2} ms)",
        st.target_ms,
        st.jitter_ms
    );
    let calm_underruns = shared.underruns.load(Ordering::Relaxed) - calm_from.unwrap();
    assert_eq!(
        calm_underruns, 0,
        "underruns while calm (rough: {rough_underruns})"
    );
}

/// Adapting lanes of one mixer hold the largest need among them; one that stops saying
/// is forgotten.
#[test]
fn a_mixers_adapting_lanes_hold_one_target() {
    let g = SyncGroup::default();
    let t0 = Instant::now();
    assert_eq!(g.publish(1, 3.0, t0), 3.0);
    assert_eq!(g.publish(2, 9.0, t0), 9.0);
    assert_eq!(g.publish(1, 3.0, t0), 9.0, "lane 1 holds lane 2's need");
    assert_eq!(g.publish(2, 4.0, t0), 4.0, "lane 2 better: both come down");
    let later = t0 + SYNC_FORGET + Duration::from_millis(1);
    assert_eq!(g.publish(1, 2.0, later), 2.0, "lane 2 gone: forgotten");
}
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_lower_latency_cuts_the_surplus_once() {
    // An OBS source's low-latency box: 60 ms to 10 ms while playing.
    let mixer = Mixer::new(48000, 2, "test");
    let lane = mixer.lane(&LaneConfig {
        latency_ms: 60,
        max_correction_ppm: 1000.0,
        ..Default::default()
    });
    let start = Instant::now();
    let (mut next_push, mut next_pull, mut switched) = (start, start, false);
    let block = vec![0.1f32; 96];
    while start.elapsed() < Duration::from_secs(4) {
        let now = Instant::now();
        if now >= next_push {
            lane.push_all(48000, &[block.clone(), block.clone()]);
            next_push += Duration::from_millis(2);
        }
        if now >= next_pull {
            mixer.fill(&mut [0f32; 256]);
            next_pull += Duration::from_micros(128 * 1_000_000 / 48_000);
        }
        if !switched && start.elapsed() >= Duration::from_secs(1) {
            assert!(lane.stats().fill_ms > 50.0, "{:?}", lane.stats());
            lane.set_latency(10);
            switched = true;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let st = lane.stats();
    assert!(st.fill_ms < 20.0, "still {:.1} ms", st.fill_ms);
    assert!(
        (40.0..60.0).contains(&st.dropped_ms),
        "dropped {:.1} ms",
        st.dropped_ms
    );
    assert_eq!(st.target_ms, 10);
    assert_eq!(st.underruns, 0);
}
/// A latency raised while playing (a desk's target for a source on this machine grows
/// with its blocks) plays on without a break: the buffer grows where the sound is quiet,
/// not by stopping it to refill (lab 374: cut dead mid-wave 0.7 s after a route's return).
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_latency_raised_while_playing_plays_on() {
    let _turn = REAL_TIME.lock_ok();
    let mixer = Mixer::new(48000, 2, "test");
    let lane = mixer.lane(&LaneConfig {
        latency_ms: 5,
        max_correction_ppm: 1000.0,
        ..Default::default()
    });
    let start = Instant::now();
    let (mut block, mut pulled) = (0u64, 0u64);
    let mut heard: Vec<f32> = Vec::new();
    let mut raised = None;
    while start.elapsed() < Duration::from_secs(3) {
        let now = Instant::now();
        if now >= start + Duration::from_micros(block * 2000) {
            // A tone with a quiet moment every half second (a desk's sound has some).
            let ch: Vec<f32> = (0..96u64)
                .map(|i| {
                    let n = block * 96 + i;
                    if n % 24_000 < 1200 {
                        0.0
                    } else {
                        0.5 * (n as f32 * std::f32::consts::TAU / 48.0).sin()
                    }
                })
                .collect();
            lane.push(48000, &[ch.clone(), ch], &[(0, 0), (1, 1)]);
            block += 1;
        }
        if now >= start + Duration::from_micros(pulled * 2000) {
            let mut buf = [0f32; 96 * 2];
            mixer.fill(&mut buf);
            heard.extend(buf.iter().step_by(2));
            pulled += 1;
        }
        if raised.is_none() && start.elapsed() >= Duration::from_millis(1200) {
            raised = Some(heard.len());
            lane.set_latency(8);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    // After the raise: no sample of the tone dropped to silence outside its quiet
    // moments (a run of silence where the tone was playing before and after).
    // (From 10 ms before it: the block it fell in included.)
    let from = raised.unwrap().saturating_sub(480);
    let peaks: Vec<f32> = heard[from..]
        .chunks(48)
        .map(|c| c.iter().fold(0f32, |m, v| m.max(v.abs())) / 0.5)
        .collect();
    let cuts: Vec<usize> = (1..peaks.len().saturating_sub(1))
        .filter(|&k| peaks[k - 1] > 0.9 && peaks[k] < 0.5)
        .collect();
    // Into a quiet moment the tone ends whole (its last ms near full), so a cut is told
    // by a fall that the quiet moments' 25 ms of silence don't explain: the ms after a
    // fall is silent for less than 20 ms.
    let short: Vec<usize> = cuts
        .into_iter()
        .filter(|&k| peaks[k..(k + 20).min(peaks.len())].iter().any(|&p| p > 0.5))
        .collect();
    assert!(
        short.is_empty(),
        "a break in the sound at ms {short:?} after the raise"
    );
}
/// Adapting: a lane's start guess (40 ms for Wi-Fi) is its own until measured: its sync
/// group (and the site) aren't raised by it.
#[test]
fn a_start_guess_is_not_the_groups_need() {
    let (shared, mut inner, mut cfg, stats) = rig(40);
    cfg.adaptive = true;
    inner.auto_ms = 40.0;
    let ch = vec![0.1f32; 96];
    push_inner(
        &shared,
        &mut inner,
        &PushCfg::of(&cfg),
        &stats,
        48000,
        &[ch.clone(), ch],
        None,
        None,
    );
    let need = shared.sync.need(Instant::now()).expect("an adapting lane");
    assert!(
        need <= f64::from(cfg.min_ms).max(0.5),
        "published {need} ms (the floor: {})",
        cfg.min_ms
    );
    assert!(
        inner.group_ms >= 40.0,
        "the lane itself holds its guess: {}",
        inner.group_ms
    );
}

/// Adapting: once the rise window has arrivals, the lane publishes what it measured, not
/// its start guess, and settles there itself (no longer the guess held
/// for the 30 s before a fall is allowed, then 1 ms every 5 s).
#[test]
fn a_measured_need_not_the_guess_is_published() {
    let (shared, mut inner, mut cfg, stats) = rig(40);
    cfg.adaptive = true;
    inner.auto_ms = 40.0;
    let ch = vec![0.1f32; 96];
    let start = Instant::now();
    // Blocks every 2 ms on time (by a schedule: sleeps overshoot), for a little over the
    // rise window and the start's rush.
    let mut next = start;
    while start.elapsed() < Duration::from_millis(3500) {
        push_inner(
            &shared,
            &mut inner,
            &PushCfg::of(&cfg),
            &stats,
            48000,
            &[ch.clone(), ch.clone()],
            None,
            None,
        );
        next += Duration::from_millis(2);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    let need = shared.sync.need(Instant::now()).expect("an adapting lane");
    // (Loose: a loaded test machine delays some pushes by several ms.)
    assert!(need < 30.0, "published {need} ms, the guess 40");
    assert!(
        inner.auto_ms < 30.0,
        "the lane settled to what it measured: {}",
        inner.auto_ms
    );
}
/// When a sample pushed now is heard (lip-sync): what the lane holds and the
/// device's output delay, as its callbacks say.
#[test]
fn a_lanes_latency_counts_the_devices_delay() {
    let mixer = Mixer::new(48000, 2, "test");
    assert_eq!(mixer.output_delay(), Duration::ZERO);
    let lane = mixer.lane(&LaneConfig::default());
    // 20 ms waiting in the lane.
    let block = vec![0.1f32; 960];
    lane.push_all(48000, &[block.clone(), block]);
    // (Not playing yet: the level it will start at, less the time since the block came,
    // so a moment later a little less: the time it is heard at stays put.)
    let held = lane.queued();
    let ms = Duration::from_millis;
    assert!(held >= ms(19), "{held:?}");
    assert!(lane.latency() + ms(1) >= held);
    // The device says its blocks are heard 12 ms after its callback.
    mixer.set_output_delay(ms(12));
    assert_eq!(mixer.output_delay(), ms(12));
    let l = lane.latency();
    assert!(l + ms(1) >= held + ms(12), "{l:?}");
    assert!(l < held + Duration::from_millis(20), "{l:?}");
    let before = Instant::now();
    let at = lane.heard_at();
    assert!(at >= before + Duration::from_millis(12) + held - Duration::from_millis(1));
}

/// A source pushing in real time from its first sample (a lip-sync check: 10 ms
/// blocks, a 20 ms latency, a 5 ms device block): the lane starts at the level it holds
/// and keeps it, so a sample's latency is the same from the first block on; nothing is cut
/// (before it plays or after), every sample pushed is heard. A new lane started a block
/// over its level, after cutting what it heard first, and cut ~8 ms at 2 s.
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_real_time_source_starts_at_its_level() {
    let (shared, mut inner, cfg, stats) = rig(20);
    let (block, dev_block) = (480usize, 240usize);
    let start = Instant::now();
    let (mut next_push, mut next_pull) = (start, start);
    let mut t = 0usize;
    let (mut heard, mut first_heard) = (0usize, false);
    let (mut early, mut late) = (Vec::new(), Vec::new());
    while start.elapsed() < Duration::from_secs(4) {
        let now = Instant::now();
        if now >= next_push {
            // Never silent (a sine with an offset): the first sample heard is the first
            // pushed.
            let ch: Vec<f32> = (0..block)
                .map(|i| {
                    0.1 + 0.25
                        * ((t + i) as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin()
                })
                .collect();
            t += block;
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                None,
            );
            next_push += Duration::from_micros(10_000);
        }
        if now >= next_pull {
            let mut buf = vec![0f32; dev_block * 2];
            shared.fill(&mut buf);
            for f in buf.chunks(2) {
                first_heard |= f[0] != 0.0;
                heard += usize::from(first_heard);
            }
            if first_heard {
                let level = {
                    let r = shared.ring.lock_ok();
                    (r.len() / r.width().max(1)) as f64 / 48.0
                };
                let s = start.elapsed();
                if s < Duration::from_millis(600) {
                    early.push(level);
                } else if s > Duration::from_secs(3) {
                    late.push(level);
                }
            }
            next_pull += Duration::from_micros(dev_block as u64 * 1_000_000 / 48_000);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let ring = {
        let r = shared.ring.lock_ok();
        r.len() / r.width().max(1)
    };
    // (What the resampler still holds is on its way too.)
    let held = inner
        .rate
        .as_ref()
        .map_or(0, |(_, _, r)| r.pending() + r.output_delay());
    let lost = t as i64 - heard as i64 - ring as i64 - held as i64;
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let st = stats.lock_ok();
    assert_eq!(st.dropped_ms, 0.0, "dropped");
    assert!(lost.abs() < 48, "{lost} frames pushed never heard");
    assert!(
        (mean(&early) - mean(&late)).abs() < 1.0,
        "level {:.1} ms at the start, {:.1} ms later (hold {:.1})",
        mean(&early),
        mean(&late),
        st.hold_ms
    );
    assert!(
        (mean(&late) - st.hold_ms).abs() < 1.5,
        "{:.1} of {:.1}",
        mean(&late),
        st.hold_ms
    );
    assert_eq!(shared.underruns.load(Ordering::Relaxed), 0);
}
/// Beeps (quiet moments to cut in) into a device that misses one take just after the lane
/// starts: the lane runs a device block over its level, and keeps it (`heard_at` says it)
/// rather than cutting it at a quiet moment ~2 s in: a step in the lip-sync (seen in
/// real-card runs: 2 to 7 ms, 3 runs in 10).
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_device_late_at_the_start_is_kept_not_stepped() {
    let (shared, mut inner, cfg, stats) = rig(20);
    let (block, dev_block) = (480usize, 240usize);
    let start = Instant::now();
    let (mut next_push, mut next_pull) = (start, start);
    let mut t = 0usize;
    let (mut heard, mut first_heard, mut skipped) = (0usize, false, false);
    let (mut early, mut late) = (Vec::new(), Vec::new());
    while start.elapsed() < Duration::from_secs(4) {
        let now = Instant::now();
        if now >= next_push {
            // Never silent (a sine with an offset): the first sample heard is the first
            // pushed.
            let ch: Vec<f32> = (0..block)
                .map(|i| {
                    let n = t + i;
                    // 100 ms on, 100 ms off (never exactly silent while on).
                    if (n / 4800).is_multiple_of(2) {
                        0.1 + 0.25
                            * (n as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin()
                    } else {
                        0.0
                    }
                })
                .collect();
            t += block;
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                None,
            );
            next_push += Duration::from_micros(10_000);
        }
        if now >= next_pull {
            let mut buf = vec![0f32; dev_block * 2];
            // The device misses one take soon after the lane starts playing.
            if first_heard && !skipped {
                skipped = true;
                next_pull += Duration::from_micros(dev_block as u64 * 1_000_000 / 48_000);
                continue;
            }
            shared.fill(&mut buf);
            for f in buf.chunks(2) {
                first_heard |= f[0] != 0.0;
                heard += usize::from(first_heard);
            }
            if first_heard {
                let level = {
                    let r = shared.ring.lock_ok();
                    (r.len() / r.width().max(1)) as f64 / 48.0
                };
                let s = start.elapsed();
                if s < Duration::from_millis(600) {
                    early.push(level);
                } else if s > Duration::from_secs(3) {
                    late.push(level);
                }
            }
            next_pull += Duration::from_micros(dev_block as u64 * 1_000_000 / 48_000);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let ring = {
        let r = shared.ring.lock_ok();
        r.len() / r.width().max(1)
    };
    // (What the resampler still holds is on its way too.)
    let held = inner
        .rate
        .as_ref()
        .map_or(0, |(_, _, r)| r.pending() + r.output_delay());
    let lost = t as i64 - heard as i64 - ring as i64 - held as i64;
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let st = stats.lock_ok();
    assert_eq!(st.dropped_ms, 0.0, "dropped");
    assert!(lost.abs() < 48, "{lost} frames pushed never heard");
    assert!(
        (mean(&early) - mean(&late)).abs() < 1.0,
        "level {:.1} ms at the start, {:.1} ms later (hold {:.1})",
        mean(&early),
        mean(&late),
        st.hold_ms
    );
    assert!(
        (mean(&late) - st.hold_ms).abs() < 1.5 + 5.0,
        "{:.1} of {:.1}",
        mean(&late),
        st.hold_ms
    );
    assert_eq!(shared.underruns.load(Ordering::Relaxed), 0);
}
