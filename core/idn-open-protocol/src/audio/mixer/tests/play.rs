//! Playing steadily: small blocks, gaps, restarts, underruns, rates, sums and drift.

use super::*;

/// The low-latency mode (idn-routing.md, R.14 stage 2): 2 ms blocks arriving with up
/// to 3 ms of jitter, a 128-frame device (2.7 ms), a 10 ms target. Once playing,
/// no underruns, and the level held near its target.
/// Real time on one thread standing in for the device: in an unoptimised build
/// beside the rest of the suite, scheduling alone makes underruns at a 10 ms buffer
/// (a real device callback runs at real-time priority).
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn small_blocks_and_a_small_buffer_play_steadily() {
    let (underruns, fill, hold, dropped) = play_steadily(10, 96, 2000, 30, 128);
    assert_eq!(
        underruns, 0,
        "underruns after 1 s, fill {fill:.1} ms of {hold:.1}"
    );
    assert!(
        (fill - hold).abs() < 6.0,
        "fill {fill:.1} ms, hold {hold:.1} ms"
    );
    assert!(dropped < 20.0, "dropped {dropped:.1} ms");
}

/// The ultra-low mode (idn-routing.md §A.12): 1 ms blocks arriving with up to 0.5 ms
/// of jitter (a wired network, real-time threads), a 64-frame device (1.3 ms), a 2 ms
/// target. Once playing, no underruns. Run on a quiet machine: real time, as above.
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn ultra_low_blocks_and_buffer_play_steadily() {
    let (underruns, fill, hold, dropped) = play_steadily(2, 48, 1000, 5, 64);
    assert_eq!(
        underruns, 0,
        "underruns after 1 s, fill {fill:.2} ms of {hold:.2}"
    );
    assert!(
        (fill - hold).abs() < 2.0,
        "fill {fill:.2} ms, hold {hold:.2} ms"
    );
    assert!(dropped < 10.0, "dropped {dropped:.1} ms");
}

#[test]
fn sound_after_a_gap_starts_whole() {
    // A source pausing (a media file restarting, a stalled sender) runs the lane dry;
    // the sound it resumes with must play from its first sample: the cut back to the
    // target on refilling took the front of it (a beep at a file's start lost or 10–23
    // ms late after a switcher's media restart).
    let (shared, mut inner, cfg, stats) = rig(10);
    let quiet = vec![0.001f32; 1024]; // AAC-sized blocks, 21.3 ms
    let (mut pushed, mut taken) = (0usize, 0usize);
    let mut out: Vec<f32> = Vec::new();
    let step = |inner: &mut Inner,
                block: Option<&Vec<f32>>,
                out: &mut Vec<f32>,
                pushed: &mut usize,
                taken: &mut usize| {
        if let Some(b) = block {
            push_inner(
                &shared,
                inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[b.clone(), b.clone()],
                None,
                None,
            );
            *pushed += b.len();
        }
        // The consumer's 33 ms frames as the source's time passes.
        while *taken + 1600 <= *pushed {
            let mut buf = [0f32; 1600 * 2];
            shared.fill(&mut buf);
            out.extend(buf.iter().step_by(2));
            *taken += 1600;
        }
    };
    for _ in 0..100 {
        step(&mut inner, Some(&quiet), &mut out, &mut pushed, &mut taken);
    }
    // 62 ms with nothing arriving: the consumer runs it dry.
    let before = shared.underruns.load(Ordering::Relaxed);
    for _ in 0..3 {
        let mut buf = [0f32; 1600 * 2];
        shared.fill(&mut buf);
    }
    assert!(shared.underruns.load(Ordering::Relaxed) > before);
    // The sound resumes with a 10 ms marker at its very start.
    let mut first = quiet.clone();
    first[..480].fill(0.5);
    out.clear();
    step(&mut inner, Some(&first), &mut out, &mut pushed, &mut taken);
    for _ in 0..30 {
        step(&mut inner, Some(&quiet), &mut out, &mut pushed, &mut taken);
    }
    let marker = out.iter().filter(|x| **x > 0.25).count();
    assert!(marker >= 470, "{marker} of the marker's 480 samples played");
}

/// A sender stopped and started again: its pause doesn't widen the
/// target as jitter would, and the level each start leaves is set right within a few
/// seconds, cut where the sound is quiet (a beep a second, as the test clips).
/// Real time, a 21 ms-block source into a 15 ms device at 60 ms.
#[test]
#[cfg_attr(debug_assertions, ignore = "real time: cargo test --release")]
fn a_restarted_source_keeps_its_target_and_level() {
    let (shared, mut inner, cfg, stats) = rig(60);
    let (block, dev_block) = (1024usize, 720usize);
    let start = Instant::now();
    let (mut next_push, mut next_pull) = (start, start);
    let mut t = 0usize;
    // Playing 0-3 s and again from 4 s; the restart comes with a burst of 4 blocks.
    let mut burst = 0;
    while start.elapsed() < Duration::from_secs(10) {
        let now = Instant::now();
        let e = start.elapsed().as_secs_f64();
        if now >= next_push {
            let gap = (3.0..4.0).contains(&e);
            if !gap {
                let n = if burst == 0 && e >= 4.0 { 5 } else { 1 };
                burst += (e >= 4.0) as usize;
                for _ in 0..n {
                    // A 100 ms beep at the start of each second, silence between.
                    let ch: Vec<f32> = (0..block)
                        .map(|i| {
                            if (t + i) % 48000 < 4800 {
                                0.25 * ((t + i) as f32 * 0.13).sin()
                            } else {
                                0.0
                            }
                        })
                        .collect();
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
                    t += block;
                }
            } else {
                t += block;
            }
            next_push += Duration::from_micros(block as u64 * 1_000_000 / 48_000);
        }
        if now >= next_pull {
            let mut buf = vec![0f32; dev_block * 2];
            shared.fill(&mut buf);
            next_pull += Duration::from_micros(dev_block as u64 * 1_000_000 / 48_000);
        }
        std::thread::sleep(Duration::from_micros(300));
    }
    let st = stats.lock_ok().clone();
    assert!(
        shared.underruns.load(Ordering::Relaxed) >= 1,
        "the pause ran it dry"
    );
    assert_eq!(st.margin_ms, 0.0, "a pause isn't jitter");
    assert!(
        (st.fill_ms - st.hold_ms).abs() < 4.0,
        "fill {:.1} ms, hold {:.1} ms",
        st.fill_ms,
        st.hold_ms
    );
}
#[test]
fn an_underrun_widens_the_target() {
    let (shared, mut inner, cfg, stats) = rig(10);
    let block = vec![0.1f32; 96]; // 2 ms
    let push = |inner: &mut Inner| {
        push_inner(
            &shared,
            inner,
            &PushCfg::of(&cfg),
            &stats,
            48000,
            &[block.clone(), block.clone()],
            None,
            None,
        )
    };
    // Plays for `wait` (block in, block out), then is taken from 2 ms at a time with
    // nothing arriving until it runs dry: the source late, not paused (a pause over
    // 300 ms isn't jitter).
    let play_then_dry = |inner: &mut Inner, wait: Duration| {
        // (60 ms: a new lane hears 40 ms of its source before it first plays.)
        for _ in 0..30 {
            push(inner);
        }
        assert!(shared.ready.load(Ordering::Relaxed));
        let until = Instant::now() + wait;
        while Instant::now() < until {
            push(inner);
            shared.fill(&mut [0f32; 96 * 2]);
            std::thread::sleep(Duration::from_millis(2));
        }
        let before = shared.underruns.load(Ordering::Relaxed);
        for _ in 0..1000 {
            shared.fill(&mut [0f32; 96 * 2]);
            if shared.underruns.load(Ordering::Relaxed) > before {
                break;
            }
        }
        push(inner);
    };
    // Within 2 s of starting to play: expected, not counted.
    play_then_dry(&mut inner, Duration::ZERO);
    assert_eq!(stats.lock_ok().margin_ms, 0.0);
    play_then_dry(&mut inner, Duration::from_millis(2100));
    assert_eq!(stats.lock_ok().margin_ms, 2.0);
    play_then_dry(&mut inner, Duration::from_millis(2100));
    assert_eq!(stats.lock_ok().margin_ms, 4.0);
    assert_eq!(shared.underruns.load(Ordering::Relaxed), 3);
}
/// A source's absurd sample rate (its header says 1 Hz) is refused, not resampled: a
/// 48,000-times resampler wanted hundreds of MB on a 64-channel device.
#[test]
fn an_absurd_sample_rate_is_refused() {
    let mixer = Mixer::new(48000, 64, "test");
    let lane = mixer.lane(&LaneConfig {
        latency_ms: 60,
        max_correction_ppm: 1000.0,
        ..Default::default()
    });
    let block = vec![vec![0.1f32; 1024]; 64];
    for rate in [1, 7_999, 384_001, u32::MAX] {
        lane.push_all(rate, &block);
    }
    let st = lane.stats();
    assert_eq!((st.errors, st.source_rate), (4, 0));
    lane.push_all(48_000, &block);
    assert_eq!(lane.stats().source_rate, 48_000);
}
/// Two sources on one 4-channel device, each in its own lane with its own map: the
/// device plays their sum, each on its channels (idn-routing.md §A.9, 4).
#[test]
fn lanes_sum_on_their_own_channels() {
    let cfg = LaneConfig {
        latency_ms: 20,
        max_correction_ppm: 1000.0,
        ..Default::default()
    };
    let device = Arc::new(Mixer {
        lanes: Mutex::new(Vec::new()),
        lanes_gen: AtomicU64::new(0),
        grave: Mutex::new(Vec::new()),
        cb_lanes: Mutex::new((u64::MAX, Vec::new())),
        sync: Arc::default(),
        block: Arc::new(AtomicU32::new(0)),
        block_window: AtomicU32::new(0),
        window_at: AtomicU64::new(0),
        frames_out: AtomicU64::new(0),
        errors: Arc::new(AtomicU64::new(0)),
        dev_ch: 4,
        dev_rate: 48000,
        name: "test".into(),
        cb_mono: Arc::new(AtomicI64::new(0)),
        out_delay_ns: Arc::new(AtomicI64::new(0)),
        through: false,
    });
    let a = Lane::new(&device, &cfg, "test".into());
    let b = Lane::new(&device, &cfg, "test".into());
    // A's only channel on device channels 0 and 1; B's second channel on 3.
    for _ in 0..20 {
        a.push(48000, &[vec![0.25; 480]], &[(0, 0), (0, 1)]);
        b.push(48000, &[vec![0.9; 480], vec![-0.5; 480]], &[(1, 3)]);
        let mut out = vec![0f32; 480 * 4];
        device.fill(&mut out);
    }
    let mut out = vec![0f32; 480 * 4];
    a.push(48000, &[vec![0.25; 480]], &[(0, 0), (0, 1)]);
    b.push(48000, &[vec![0.9; 480], vec![-0.5; 480]], &[(1, 3)]);
    device.fill(&mut out);
    let frame = &out[200 * 4..201 * 4];
    assert!((frame[0] - 0.25).abs() < 1e-3, "{frame:?}");
    assert!((frame[1] - 0.25).abs() < 1e-3, "{frame:?}");
    assert!(
        frame[2].abs() < 1e-6,
        "nothing routed to channel 2: {frame:?}"
    );
    assert!((frame[3] + 0.5).abs() < 1e-3, "{frame:?}");
    // Each lane keeps only its routes' channels.
    assert_eq!(a.shared.ring.lock_ok().cols, [0, 1]);
    assert_eq!(b.shared.ring.lock_ok().cols, [3]);
    // A route added while playing: a column of its own, its sound there soon after.
    for _ in 0..20 {
        a.push(48000, &[vec![0.25; 480]], &[(0, 0), (0, 1)]);
        b.push(48000, &[vec![0.9; 480], vec![-0.5; 480]], &[(1, 3), (0, 2)]);
        let mut out = vec![0f32; 480 * 4];
        device.fill(&mut out);
    }
    assert_eq!(b.shared.ring.lock_ok().cols, [3, 2]);
    let mut out = vec![0f32; 480 * 4];
    a.push(48000, &[vec![0.25; 480]], &[(0, 0), (0, 1)]);
    b.push(48000, &[vec![0.9; 480], vec![-0.5; 480]], &[(1, 3), (0, 2)]);
    device.fill(&mut out);
    let frame = &out[200 * 4..201 * 4];
    assert!((frame[2] - 0.9).abs() < 1e-3, "{frame:?}");
    assert!((frame[3] + 0.5).abs() < 1e-3, "{frame:?}");
    // A lane dropped plays out what it held, faded (5 ms at most), then leaves the device.
    drop(b);
    assert_eq!(device.lanes.lock_ok().len(), 2);
    let mut out = vec![0f32; 480 * 4];
    device.fill(&mut out);
    let ch3: Vec<f32> = out.iter().skip(3).step_by(4).copied().collect();
    assert!(
        ch3[0] < -0.4 && ch3[240..].iter().all(|v| v.abs() < 1e-6),
        "{:?}",
        &ch3[..8]
    );
    assert!(
        ch3[..240]
            .windows(2)
            .all(|w| w[1].abs() <= w[0].abs() + 1e-6),
        "faded out"
    );
    assert_eq!(device.lanes.lock_ok().len(), 1);
}
/// A steady stretch's drift is the slope of the offset less the corrections made: exact
/// while the correction changes every second (a servo pulling the level in), and the
/// fill's jitter at the stretch's ends no longer decides it.
#[test]
fn the_drift_is_fitted_to_every_second() {
    let t0 = Instant::now();
    let drift = 7.0;
    let mut c = Calm::new(t0);
    let mut off = 0.4;
    for i in 0..31u64 {
        let corr = 20.0 - i as f64 * 0.5;
        c.add(t0 + Duration::from_secs(i), off, corr);
        // The next second: the correction raises the offset, the drift lowers it.
        off += (corr - drift) / 1000.0;
    }
    assert!((c.drift() - drift).abs() < 1e-6, "{}", c.drift());
    // Jitter of ±0.3 ms on the fill, worst at the two ends: within a few ppm.
    let mut c = Calm::new(t0);
    for i in 0..31u64 {
        let jitter = match i {
            0 => 0.3,
            30 => -0.3,
            _ if i % 2 == 0 => 0.15,
            _ => -0.15,
        };
        c.add(
            t0 + Duration::from_secs(i),
            jitter - drift * i as f64 / 1000.0,
            0.0,
        );
    }
    assert!((c.drift() - drift).abs() < 6.0, "{}", c.drift());
}
