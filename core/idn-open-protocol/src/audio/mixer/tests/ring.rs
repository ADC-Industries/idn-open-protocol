//! The ring: fades, backlogs, added columns, and the callback never held up.

use super::*;

/// A route taken away fades out over the ring's last frames, the other channels
/// untouched.
#[test]
fn a_removed_channel_fades_out_of_the_ring() {
    let mut ring: VecDeque<f32> = std::iter::repeat_n([1.0f32, 1.0], 100).flatten().collect();
    fade_tail(&mut ring, 2, &[1], 10);
    let ch1: Vec<f32> = ring.iter().skip(1).step_by(2).copied().collect();
    assert!(ch1[..90].iter().all(|&v| v == 1.0));
    assert!(
        ch1[90..].windows(2).all(|w| w[1] < w[0]),
        "{:?}",
        &ch1[90..]
    );
    assert_eq!(ch1[99], 0.0);
    assert!(
        ring.iter().step_by(2).all(|&v| v == 1.0),
        "channel 0 untouched"
    );
}
/// The lab's pacing: the backlog trickles in at two blocks per interval
/// instead of arriving at once. The output must be as clean.
#[test]
fn a_trickling_backlog_is_clean_too() {
    let _turn = REAL_TIME.lock_ok();
    let (shared, mut inner, cfg, stats) = rig(60);
    let (block, dev_block) = (1600usize, 480usize);
    let mut t = 0usize;
    let mut out: Vec<f32> = Vec::new();
    let mut push = |inner: &mut Inner, n_blocks: usize| {
        for _ in 0..n_blocks {
            let ch: Vec<f32> = (0..block)
                .map(|i| {
                    0.25 * ((t + i) as f32 * 2.0 * std::f32::consts::PI * 997.0 / 48000.0).sin()
                })
                .collect();
            t += block;
            push_inner(
                &shared,
                inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                None,
            );
        }
    };
    let consume = |out: &mut Vec<f32>, n_blocks: usize| {
        for _ in 0..n_blocks {
            let mut buf = vec![0f32; dev_block * 2];
            shared.fill(&mut buf);
            out.extend(buf.chunks(2).map(|f| f[0]));
        }
    };
    for i in 0..60 {
        push(&mut inner, 1);
        consume(&mut out, if i % 3 == 2 { 4 } else { 3 });
        std::thread::sleep(Duration::from_millis(10));
    }
    let steady_end = out.len();
    consume(&mut out, 30);
    std::thread::sleep(Duration::from_millis(2100));
    // 90 blocks of backlog at two per 33 ms: 1.5 s of trickle.
    for i in 0..45 {
        push(&mut inner, 2);
        consume(&mut out, if i % 3 == 2 { 4 } else { 3 });
        std::thread::sleep(Duration::from_millis(33));
    }
    // Steady again until the level is back: the cut that brings it back waits for a quiet
    // moment or 5 s of the clock, which other tests running alongside stretch.
    for i in 0..240 {
        push(&mut inner, 1);
        consume(&mut out, if i % 3 == 2 { 4 } else { 3 });
        std::thread::sleep(Duration::from_millis(33));
        if i >= 60 && stats.lock_ok().fill_ms < 110.0 {
            break;
        }
    }
    let after: Vec<(usize, f32)> = edges(&out, steady_end);
    let mut events = 0;
    let mut last = 0usize;
    for (i, _) in &after {
        if *i > last + 20 {
            events += 1;
        }
        last = *i;
    }
    let st = stats.lock_ok();
    assert!(
        events <= 1,
        "{events} discontinuities after the gap: {:?} (dropped {:.0} ms, fill {:.0})",
        after.iter().take(20).collect::<Vec<_>>(),
        st.dropped_ms,
        st.fill_ms
    );
    assert!(
        st.fill_ms < 110.0,
        "fill after recovery {:.0} ms",
        st.fill_ms
    );
}

/// A 3 s gap in a stream the link survives: the backlog then arrives in a rush. The
/// output may show the gap's start and its end, and the one cut that brings the
/// buffer back to its level; nothing else (not a tick per block).
#[test]
fn a_backlog_burst_costs_one_cut() {
    let _turn = REAL_TIME.lock_ok();
    let (shared, mut inner, cfg, stats) = rig(60);
    let (block, dev_block) = (1600usize, 480usize); // 33 ms source blocks, 10 ms device
    let mut t = 0usize;
    let mut out: Vec<f32> = Vec::new();
    let mut push = |inner: &mut Inner, n_blocks: usize| {
        for _ in 0..n_blocks {
            let ch: Vec<f32> = (0..block)
                .map(|i| {
                    0.25 * ((t + i) as f32 * 2.0 * std::f32::consts::PI * 997.0 / 48000.0).sin()
                })
                .collect();
            t += block;
            push_inner(
                &shared,
                inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                None,
            );
        }
    };
    let consume = |out: &mut Vec<f32>, n_blocks: usize| {
        for _ in 0..n_blocks {
            let mut buf = vec![0f32; dev_block * 2];
            shared.fill(&mut buf);
            out.extend(buf.chunks(2).map(|f| f[0]));
        }
    };
    // Steady state for 2 s: a block in, then 33 ms of device callbacks (3.33 × 10 ms).
    for i in 0..60 {
        push(&mut inner, 1);
        consume(&mut out, if i % 3 == 2 { 4 } else { 3 });
    }
    let steady_end = out.len();
    // The gap: the device keeps taking, nothing arrives, the buffer runs dry.
    consume(&mut out, 30);
    // Wait out the hold so the recovery logic is in its steady path.
    std::thread::sleep(Duration::from_millis(2100));
    // The backlog of 90 blocks arrives in 0.3 s (9 blocks per 33 ms of device time),
    // then real time again.
    for _ in 0..10 {
        push(&mut inner, 9);
        consume(&mut out, 3);
        std::thread::sleep(Duration::from_millis(30));
    }
    // Steady again until the level is back: the cut that brings it back waits for a quiet
    // moment or 5 s of the clock, which other tests running alongside stretch.
    for i in 0..240 {
        push(&mut inner, 1);
        consume(&mut out, if i % 3 == 2 { 4 } else { 3 });
        std::thread::sleep(Duration::from_millis(33));
        if i >= 60 && stats.lock_ok().fill_ms < 110.0 {
            break;
        }
    }
    // Steady state: no edge after the start of sound (silence to tone at the prime).
    let steady = edges(&out, 6000);
    assert!(
        steady.iter().all(|(i, _)| *i >= steady_end),
        "edges in steady state: {:?}",
        &steady[..5.min(steady.len())]
    );
    // Count edges after the gap started, merging those within 20 samples (one event).
    let after: Vec<(usize, f32)> = edges(&out, steady_end);
    let mut events = 0;
    let mut last = 0usize;
    for (i, _) in &after {
        if *i > last + 20 {
            events += 1;
        }
        last = *i;
    }
    let st = stats.lock_ok();
    assert!(
        st.dropped_ms > 500.0,
        "backlog dropped {:.0} ms",
        st.dropped_ms
    );
    // The gap's start and end are faded and the one cut is crossfaded: nothing
    // should exceed what the edge detector lets through.
    assert!(
        events <= 1,
        "{events} discontinuities after the gap: {:?}",
        after.iter().take(20).collect::<Vec<_>>()
    );
    assert!(
        st.fill_ms < 110.0,
        "fill after recovery {:.0} ms",
        st.fill_ms
    );
}
/// Columns added to a ring keep what waits in it, frame for frame: the new columns silent,
/// and a ring with none yet (a routed lane before its first route) keeps its frames of
/// silence, so the lane's level doesn't drop when the route comes.
#[test]
fn a_ring_keeps_its_frames_when_it_gets_columns() {
    let mut r = Ring::new(16);
    r.extend(std::iter::repeat_n(0.0, 10));
    r.add_cols(&[1, 3]);
    assert_eq!((r.cols.clone(), r.len() / r.width()), (vec![1, 3], 10));
    for (i, s) in r.iter_mut().enumerate() {
        *s = i as f32;
    }
    r.add_cols(&[0, 3]);
    assert_eq!(r.cols, vec![1, 3, 0]);
    assert_eq!(r.len() / r.width(), 10);
    assert_eq!(
        &r.data.range(0..6).copied().collect::<Vec<_>>(),
        &[0.0, 1.0, 0.0, 2.0, 3.0, 0.0]
    );
}

/// A route taken away and put back on a playing tone: its sound comes back with one
/// fade-in, never falling back once it has started (lab 356-A: the fade-in started,
/// 2 ms of silence, then started again: a click). 2 ms blocks, as the desk takes them;
/// the route gone either with the sender still sending (no channel of it routed) or with
/// nothing arriving for a while.
#[test]
fn a_route_put_back_fades_in_once() {
    let _rt = REAL_TIME.lock_ok();
    for sender_stops in [false, true] {
        let (shared, mut inner, cfg, stats) = rig(10);
        let routed: [(usize, u16); 2] = [(0, 0), (1, 1)];
        let mut phase = 0usize;
        let mut tone = |n: usize, amp: f32| -> Vec<f32> {
            (0..n)
                .map(|_| {
                    phase += 1;
                    amp * (phase as f32 * 2.0 * std::f32::consts::PI / 48.0).sin()
                })
                .collect()
        };
        let mut out: Vec<f32> = Vec::new();
        // A block (with its routes) or none, then the device's 2 ms.
        type Block<'a> = Option<(Vec<f32>, Option<&'a [(usize, u16)]>)>;
        let step = |inner: &mut Inner, block: Block, out: &mut Vec<f32>| {
            if let Some((b, map)) = block {
                push_inner(
                    &shared,
                    inner,
                    &PushCfg::of(&cfg),
                    &stats,
                    48000,
                    &[b.clone(), b],
                    map,
                    None,
                );
            }
            let mut buf = [0f32; 96 * 2];
            shared.fill(&mut buf);
            out.extend(buf.iter().step_by(2));
            std::thread::sleep(Duration::from_micros(2000));
        };
        // 1 s routed.
        for _ in 0..500 {
            let b = tone(96, 0.5);
            step(&mut inner, Some((b, Some(&routed[..]))), &mut out);
        }
        // The route gone for 0.6 s.
        for _ in 0..300 {
            if sender_stops {
                step(&mut inner, None, &mut out);
            } else {
                let b = tone(96, 0.5);
                step(&mut inner, Some((b, Some(&[][..]))), &mut out);
            }
        }
        // Back: a round trip of zeros first (the sender asked again), then the tone.
        out.clear();
        for i in 0..400 {
            let b = if i < 10 { vec![0.0; 96] } else { tone(96, 0.5) };
            step(&mut inner, Some((b, Some(&routed[..]))), &mut out);
        }
        let peaks: Vec<f32> = out
            .chunks(48)
            .map(|c| c.iter().fold(0f32, |m, v| m.max(v.abs())))
            .collect();
        let start = peaks
            .iter()
            .position(|&p| p > 0.01)
            .expect("the tone came back");
        let full = peaks[start..]
            .iter()
            .position(|&p| p > 0.45)
            .map(|k| start + k)
            .expect("to its full level");
        let fell = peaks[start..full].windows(2).find(|w| w[1] < w[0] - 0.02);
        assert!(
            fell.is_none(),
            "sender_stops {sender_stops}: the fade-in fell back: {:?}",
            &peaks[start..(full + 2).min(peaks.len())]
        );
        assert!(
            peaks[full..].iter().take(100).all(|&p| p > 0.4),
            "sender_stops {sender_stops}: a gap after the fade-in: {:?}",
            &peaks[start..(full + 30).min(peaks.len())]
        );
    }
}
/// The callback never waits for the pushing side: with the ring held elsewhere it plays
/// the block it copied, and that comes off the ring's front afterwards (no block twice,
/// none lost); a cut made meanwhile is kept.
#[test]
fn a_held_ring_never_holds_the_callback_up() {
    let (shared, _inner, _cfg, _stats) = rig(10);
    let shared = Arc::new(shared);
    {
        let mut r = shared.ring.lock_ok();
        r.add_cols(&[0, 1]);
        // Frame i: (i, -i).
        r.extend((0..1000).flat_map(|i| [i as f32, -(i as f32)]));
    }
    shared.ready.store(true, Ordering::Relaxed);
    let block = |s: &Shared| {
        let mut out = vec![0f32; 64 * 2];
        let t = Instant::now();
        s.fill(&mut out);
        (out, t.elapsed())
    };
    let firsts = |out: &[f32]| (out[0], out[out.len() - 2]);
    let (out, _) = block(&shared);
    assert_eq!(firsts(&out), (0.0, 63.0));
    // Held by another thread (the pushing side mid-push), `cut` frames cut from the front
    // or `pad` frames of silence put there, counted as the pushing side does.
    let held_with = |cut: usize, pad: usize| {
        let (got, go) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
        let s = shared.clone();
        let t = std::thread::spawn(move || {
            let mut r = s.ring.lock_ok();
            r.drain(..cut * 2);
            r.cut += cut as u64;
            for _ in 0..pad * 2 {
                r.push_front(0.0);
            }
            r.padded += pad as u64;
            got.0.send(()).unwrap();
            let _ = go.1.recv();
        });
        got.1.recv().unwrap();
        (go.0, t)
    };
    let held = |cut: usize| held_with(cut, 0);
    let (go, t) = held(0);
    let (out, took) = block(&shared);
    assert!(took < Duration::from_millis(50), "not waited for: {took:?}");
    assert_eq!(firsts(&out), (64.0, 127.0), "the copied block");
    go.send(()).unwrap();
    t.join().unwrap();
    let (out, _) = block(&shared);
    assert_eq!(firsts(&out), (128.0, 191.0), "on from there, nothing twice");
    // Cut while held (a trim of 100 frames at the front): the copy plays, and the
    // ring's new front after it, nothing repeated.
    let (go, t) = held(100);
    let (out, took) = block(&shared);
    assert!(took < Duration::from_millis(50), "{took:?}");
    assert_eq!(firsts(&out), (192.0, 255.0));
    go.send(()).unwrap();
    t.join().unwrap();
    let (out, _) = block(&shared);
    assert_eq!(firsts(&out), (292.0, 355.0), "the cut kept");
    assert_eq!(out[1], -292.0);
    // A cut smaller than the copy played meanwhile (20 of its 64 frames): the rest of
    // what was heard comes off, nothing twice.
    let (go, t) = held(20);
    let (out, _) = block(&shared);
    assert_eq!(firsts(&out), (356.0, 419.0));
    go.send(()).unwrap();
    t.join().unwrap();
    let (out, _) = block(&shared);
    assert_eq!(firsts(&out), (420.0, 483.0), "after a small cut");
    // Silence put in front meanwhile (10 frames): what was heard comes out from behind
    // it, and the silence plays next, then the sound on.
    let (go, t) = held_with(0, 10);
    let (out, _) = block(&shared);
    assert_eq!(firsts(&out), (484.0, 547.0));
    go.send(()).unwrap();
    t.join().unwrap();
    let (out, _) = block(&shared);
    assert_eq!(&out[..20], &[0.0; 20], "the silence");
    assert_eq!(
        (out[20], out[out.len() - 2]),
        (548.0, 601.0),
        "then on, nothing twice"
    );
}
