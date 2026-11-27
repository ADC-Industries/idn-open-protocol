//! A lane's timing: the re-level and the once-a-second servo (margins, the age of its sound, drift).

use super::*;

/// What the re-level looks with: the block's time, device frames per ms, the ring's frame
/// width, a crossfade's and the quiet search's margin in frames, the frames just added,
/// and whether the lane plays by site time.
pub(crate) struct Look {
    pub(crate) now: Instant,
    pub(crate) per_ms: f64,
    pub(crate) rw: usize,
    pub(crate) fade: usize,
    pub(crate) margin: usize,
    pub(crate) fresh_frames: usize,
    pub(crate) by_time: bool,
}

/// A fresh start's level set right: cut or silence added where the sound is quiet,
/// so it isn't heard; after 5 s without a quiet moment, at the front, crossfaded.
/// By time only while passing through: the drift left is set right this way, as
/// there's no ratio to trim. Gives the ms cut.
pub(crate) fn relevel_step(
    shared: &Shared,
    inner: &mut Inner,
    ring: &mut Ring,
    stats: &Mutex<LaneStats>,
    look: Look,
) -> f64 {
    let Look {
        now,
        per_ms,
        rw,
        fade,
        margin,
        fresh_frames,
        by_time,
    } = look;
    let Inner {
        rate,
        servo,
        servo_at,
        prev_late,
        relevel,
        quiet_looked,
        fade_left,
        ..
    } = inner;
    let (_, _, rate) = rate.as_mut().expect("set above");
    let mut dropped = 0.0;
    let looking =
        relevel.filter(|_| shared.ready.load(Ordering::Relaxed) && (!by_time || rate.is_through()));
    let looked_before = std::mem::take(quiet_looked);
    if let Some((off, since)) = looking {
        let n = (off.abs() * per_ms) as usize;
        let recent = (looked_before == Some(since)).then_some(fresh_frames);
        let mut done = if off > 0.0 {
            cut_quiet(ring, n, rw, margin, recent)
        } else {
            pad_quiet(ring, n, rw, margin, recent)
        };
        if !done {
            *quiet_looked = Some(since);
        }
        if !done && now.duration_since(since) >= Duration::from_secs(5) {
            if off > 0.0 {
                let cut = (n * rw).min(ring.len());
                let cut = cut_front(ring, cut, rw, fade);
                ring.cut += cut as u64;
            } else {
                fade_in_front(ring, rw, fade, fade_left);
                for _ in 0..n * rw {
                    ring.push_front(0.0);
                }
                ring.padded += n as u64;
            }
            done = true;
        } else if done {
            // (A quiet stretch cut or padded: frames moved.)
            ring.moved += 1;
        }
        if done {
            if off > 0.0 {
                dropped += off;
            }
            if rate.is_through() {
                stats.lock_ok().through_fixed_ms += off.abs();
            }
            *relevel = None;
            // The step in the lateness isn't drift.
            *prev_late = None;
            shared.fill_sum.store(0, Ordering::Relaxed);
            shared.fill_n.store(0, Ordering::Relaxed);
            *servo_at = Some(now);
            servo.resync();
        }
    }
    dropped
}

/// What the pushing side measured with this block, for the servo and the lane's stats.
pub(crate) struct Measured {
    pub(crate) now: Instant,
    /// Device frames per ms.
    pub(crate) per_ms: f64,
    /// The ring's frame width (its columns).
    pub(crate) rw: usize,
    /// A crossfade's length, frames.
    pub(crate) fade: usize,
    /// The level the servo holds, the ring's level, and the block's length, ms.
    pub(crate) hold: f64,
    pub(crate) in_ring_ms: f64,
    pub(crate) in_block_ms: f64,
    /// Sound cut from the ring this block, ms.
    pub(crate) dropped: f64,
    /// Recovering from a gap (the backlog still arriving).
    pub(crate) recovering: bool,
}

/// The servo once a second, on the mean fill the callbacks saw (by time: on the
/// lateness), with passing through's watch; then the lane's stats.
pub(crate) fn servo_second(
    shared: &Shared,
    inner: &mut Inner,
    cfg: &PushCfg,
    stats: &Mutex<LaneStats>,
    m: Measured,
) {
    let Measured {
        now,
        per_ms,
        rw,
        fade,
        hold,
        in_ring_ms,
        in_block_ms,
        dropped,
        recovering,
    } = m;
    let Inner {
        rate,
        servo,
        last,
        hold_until,
        servo_at,
        margin_ms,
        timed,
        late_sum,
        late_n,
        drift_ppm,
        applied_ppm,
        prev_late,
        starting,
        restarted,
        relevel,
        watch,
        fade_left,
        ..
    } = inner;
    let (_, _, rate) = rate.as_mut().expect("set above");
    // The servo once a second, on the mean fill the callbacks saw.
    let ready = shared.ready.load(Ordering::Relaxed);
    let mut fill_ms = in_ring_ms;
    if ready {
        let n = shared.fill_n.load(Ordering::Relaxed);
        if n > 0 {
            fill_ms = shared.fill_sum.load(Ordering::Relaxed) as f64 / n as f64 / per_ms;
        }
        let due = servo_at.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
        if due && n > 0 {
            shared.fill_sum.store(0, Ordering::Relaxed);
            shared.fill_n.store(0, Ordering::Relaxed);
            // By time: the lateness is the alignment itself, measured cleanly once a
            // second (from the callbacks' times, not the stepping fill). Its slope is
            // the drift left over (1 ms a second is 1000 ppm), learned at once instead
            // of integrated over a minute; a proportional part pulls the offset in
            // (a 10 s time constant). Else the fill's servo holds its level.
            if *timed {
                if *late_n > 0 {
                    let late = *late_sum / *late_n as f64 / 1e6;
                    stats.lock_ok().late_ms = late;
                    if late.abs() > 5.0 && !recovering {
                        // A jump (the grandmaster changed, a step): one crossfaded cut,
                        // or silence put in before what plays next.
                        {
                            let mut st = stats.lock_ok();
                            st.jumps += 1;
                            st.last_jump_ms = late;
                        }
                        let mut ring = shared.ring.lock_ok();
                        let n = (late.abs() * per_ms) as usize * rw;
                        if late > 0.0 {
                            let cut = n.min(ring.len());
                            let cut = cut_front(&mut ring, cut, rw, fade);
                            ring.cut += cut as u64;
                            let gone = cut as f64 / per_ms;
                            stats.lock_ok().dropped_ms += gone;
                        } else {
                            fade_in_front(&mut ring, rw, fade, fade_left);
                            for _ in 0..n {
                                ring.push_front(0.0);
                            }
                            ring.padded += (n / rw.max(1)) as u64;
                        }
                        *hold_until = Some(now + Duration::from_secs(2));
                        *prev_late = None;
                        // The clock may be another now: filtered until it's steady.
                        watch.leave(rate, "jump");
                    } else if let Some((t, prev)) = *prev_late {
                        let dt = now.duration_since(t).as_secs_f64().max(0.1);
                        let residual = -(late - prev) / dt * 1000.0;
                        *drift_ppm += 0.6 * (*applied_ppm + residual - *drift_ppm);
                    }
                    *prev_late = Some((now, late));
                    let limit = cfg.max_correction_ppm;
                    *drift_ppm = drift_ppm.clamp(-limit, limit);
                    if rate.is_through() {
                        watch.hold(rate, late, now, relevel);
                    }
                    if rate.is_through() {
                        *applied_ppm = 0.0;
                    } else {
                        *applied_ppm = (*drift_ppm - 100.0 * late).clamp(-limit, limit);
                        rate.set_relative(1.0 + *applied_ppm * 1e-6);
                        let steady = shared.through && !recovering && relevel.is_none();
                        if let Some(d) = watch.watch(rate, steady, *applied_ppm, late, now, true) {
                            stats.lock_ok().calm_drift_ppm = d;
                        }
                    }
                }
                *late_sum = 0.0;
                *late_n = 0;
            }
            if rate.is_through() && watch.timed != *timed {
                // Decided by the other measure (site time came or went): filtered until
                // this one has been steady.
                watch.leave(rate, "measure");
            }
            let lead = (!*timed).then_some(fill_ms - hold);
            if let Some(lead) = lead
                && !recovering
                && hold_until.is_none_or(|h| now >= h)
            {
                if *starting {
                    // Settled after a fresh start: how far off the level is, set right
                    // once (a burst at the start, or sound ahead of real time, left it
                    // anywhere; bled off by the servo it took a minute or more). Not for
                    // less than a source block and a device block: that is the start's
                    // timing (blocks arriving a little bunched, a device's callbacks not
                    // the size the level assumes), the latency `heard_at` says is already
                    // the true one, and setting it right was a step ~2 s in (seen in
                    // lip-sync tests: 2 to 7 ms, 3 runs in 10). The servo takes the rest slowly.
                    // (A source that paused and came back: set right as before, at 2 ms;
                    // its sound stopped anyway.)
                    *starting = false;
                    let block_ms = shared.block.load(Ordering::Relaxed) as f64 / per_ms;
                    let noise = if *restarted {
                        2.0
                    } else {
                        (in_block_ms + block_ms).max(2.0)
                    };
                    if lead.abs() > noise && relevel.is_none() && !cfg.keep_start_level {
                        *relevel = Some((lead, now));
                    }
                } else if rate.is_through() {
                    watch.hold(rate, lead, now, relevel);
                    if !rate.is_through() {
                        servo.resync();
                    }
                } else if relevel.is_none() {
                    let dt = servo_at.map_or(1.0, |t| now.duration_since(t).as_secs_f64());
                    let rel = servo.update(lead, dt);
                    rate.set_relative(rel);
                    let steady = shared.through && !*starting;
                    if let Some(d) = watch.watch(rate, steady, servo.ppm(), lead, now, false) {
                        stats.lock_ok().calm_drift_ppm = d;
                    }
                } else {
                    watch.calm = None;
                }
            } else if lead.is_some() {
                watch.calm = None;
            }
            *servo_at = Some(now);
        }
    }
    *last = Some(now);
    let mut st = stats.lock_ok();
    st.blocks_in += 1;
    st.fill_ms = fill_ms;
    st.hold_ms = hold;
    st.margin_ms = *margin_ms;
    st.by_time = *timed;
    st.source_block_ms = in_block_ms;
    st.dropped_ms += dropped;
    st.correction_ppm = match (*timed, rate.is_through()) {
        (_, true) => 0.0,
        (true, false) => *applied_ppm,
        (false, false) => servo.ppm(),
    };
    st.through = rate.is_through();
    st.through_ended = watch.ended;
    st.device_block = shared.block.load(Ordering::Relaxed);
}
