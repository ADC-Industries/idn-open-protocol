//! A block pushed into a lane: its channels placed, the ring kept to its target.

use super::*;

/// What a push reads of the lane's configuration: copied out with each block (the whole
/// configuration was cloned, its device name with it: an allocation a block).
#[derive(Clone, Copy)]
pub(crate) struct PushCfg {
    pub(crate) latency_ms: u32,
    pub(crate) max_correction_ppm: f64,
    pub(crate) adaptive: bool,
    pub(crate) min_ms: u32,
    pub(crate) keep_start_level: bool,
}

impl PushCfg {
    pub(crate) fn of(c: &LaneConfig) -> Self {
        PushCfg {
            latency_ms: c.latency_ms,
            max_correction_ppm: c.max_correction_ppm,
            adaptive: c.adaptive,
            min_ms: c.min_ms,
            keep_start_level: c.keep_start_level,
        }
    }
}

/// The pushing side, apart from the device (see [`Playback::push`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_inner(
    shared: &Shared,
    inner: &mut Inner,
    cfg: &PushCfg,
    stats: &Mutex<LaneStats>,
    sample_rate: u32,
    channels: &[Vec<f32>],
    map: Option<&[(usize, u16)]>,
    at: Option<(Ns, Mapping)>,
) {
    {
        let Some(frames) = channels.iter().map(Vec::len).max().filter(|&n| n > 0) else {
            return;
        };
        // Whatever pushes it: a rate no sound has would size the resampler absurdly.
        if !crate::media::AUDIO_RATES.contains(&sample_rate) {
            shared.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let dev_ch = shared.dev_ch;
        // A routed lane's blocks change their channels with the routes; the resampler
        // works on the device's channels, so only a new rate starts it over.
        let ch = if map.is_some() {
            0
        } else {
            channels.len() as u16
        };
        if inner
            .rate
            .as_ref()
            .is_none_or(|(r, c, _)| (*r, *c) != (sample_rate, ch))
        {
            // A quarter of the target, 1–10 ms: sound waits in the resampler until a whole
            // chunk is there, which a 10 ms chunk made longer than a 10 ms target (by
            // site time it was always late).
            // (Adapting: of the least it may come down to, not where it starts: a lane
            // that started at a Wi-Fi guess kept 10 ms chunks at a 3 ms target.)
            let target = if cfg.adaptive {
                cfg.min_ms.max(1)
            } else {
                cfg.latency_ms
            };
            let chunk_ms = (target as usize / 4).clamp(1, 10);
            let chunk = (sample_rate as usize * chunk_ms / 1000).max(32);
            match Rate::new(
                sample_rate,
                shared.dev_rate,
                dev_ch,
                chunk,
                0.002 + cfg.max_correction_ppm * 1e-6,
            ) {
                Ok(mut r) => {
                    r.allow_through(shared.through);
                    inner.rate = Some((sample_rate, ch, r));
                }
                Err(_) => {
                    shared.errors.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            inner.servo.reset();
            let mut st = stats.lock_ok();
            st.source_rate = sample_rate;
            st.streams += 1;
        }
        if inner.channels_seen != channels.len() {
            // A routed lane's channel count follows its routes without a new stream.
            inner.channels_seen = channels.len();
            stats.lock_ok().source_channels = channels.len() as u16;
        }
        let mapped = match map {
            Some(m) => route_channels(channels, frames, dev_ch, m),
            None => map_channels(channels, frames, dev_ch),
        };
        let now = Instant::now();
        let Inner {
            rate,
            servo,
            last,
            hold_until,
            underruns_seen,
            servo_at,
            last_trim,
            growth,
            margin_ms,
            margin_at,
            margin_floor,
            timed,
            too_old,
            age_floor,
            late_sum,
            late_n,
            prev_late,
            starting,
            relevel,
            fresh_buf,
            cut_asked,
            watch,
            fade_left,
            heard_ms,
            ever_ready,
            heard_from,
            heard_n,
            restarted,
            jitter,
            auto_ms,
            auto_at,
            underrun_at,
            underran_ms,
            resumed_at,
            measured,
            settled_from,
            published,
            group_ms,
            ..
        } = &mut *inner;
        // Nothing for 300 ms: the source paused (stopped, restarted, a file's end), not
        // the network's jitter. What it sends next is a fresh start.
        let paused = last.is_none_or(|l| now.duration_since(l) > Duration::from_millis(300));
        if paused && !shared.ready.load(Ordering::Relaxed) {
            *starting = true;
            *relevel = None;
            if *ever_ready {
                *restarted = true;
            }
        }
        let (_, _, rate) = rate.as_mut().expect("set above");
        if paused && rate.is_through() {
            // Back after a pause: perhaps another sender, on another clock.
            watch.leave(rate, "pause");
        }
        if paused {
            rate.forget();
        }
        // A routed lane resamples only the device channels its routes feed; one dropped
        // from its routes fades out over what the ring still holds of it.
        let mut gone = Vec::new();
        if let Some(m) = map {
            let mut used = vec![false; dev_ch];
            for &(_, d) in m {
                if let Some(u) = used.get_mut(d as usize) {
                    *u = true;
                }
            }
            gone = rate.set_used(&used);
        }
        rate.push(&mapped);
        // Resampled before the ring is locked: the device's callback takes from the ring,
        // and must never wait for a resampling pass (64 channels of it on a mixer input).
        // Interleaved in a plain buffer, then copied in at once (a sample at a time into
        // the ring was a quarter of a mixer lane's work).
        let mut fresh = std::mem::take(fresh_buf);
        fresh.clear();
        // The ring's columns: the channels with sound now added (a routed lane's), its
        // others silent; copied in by column, each its channel's (only those with sound:
        // the others, a channel being flushed among them, stay the zeros a block starts
        // as).
        let live: Vec<usize> = (0..dev_ch).filter(|&c| rate.is_used(c)).collect();
        let cols = {
            let mut r = shared.ring.lock_ok();
            if map.is_some() {
                r.add_cols(&live);
            } else {
                r.add_cols(&(0..dev_ch).collect::<Vec<_>>());
            }
            r.cols.clone()
        };
        let rw = cols.len().max(1);
        let copy: Vec<(usize, usize)> = cols
            .iter()
            .enumerate()
            .filter(|&(_, c)| live.contains(c))
            .map(|(k, &c)| (k, c))
            .collect();
        let _ = rate.pull(|planar, n| {
            let at = fresh.len();
            fresh.resize(at + n * rw, 0.0);
            let frames = &mut fresh[at..];
            for &(k, c) in copy.iter().filter(|&&(_, c)| c < planar.len()) {
                for (i, &v) in planar[c][..n].iter().enumerate() {
                    frames[i * rw + k] = v;
                }
            }
        });
        let mut ring = shared.ring.lock_ok();
        let gone_cols: Vec<usize> = gone
            .iter()
            .filter_map(|g| cols.iter().position(|c| c == g))
            .collect();
        fade_tail(&mut ring, rw, &gone_cols, (shared.dev_rate / 200) as usize);
        fade_in_rest(&mut fresh, rw, fade_left);
        ring.extend(fresh.iter().copied());
        let fresh_frames = fresh.len() / rw;
        *fresh_buf = fresh;
        let per_ms = shared.dev_rate as f64 / 1000.0; // frames per ms
        // Each underrun while playing steadily (not in the 2 s after a start or a refill,
        // when one or two are expected) adds 2 ms to the target, up to the target again
        // (at least 20 ms), so a path with more jitter than the target covers settles at
        // what it needs instead of breaking up. Ten clean minutes give 1 ms back, never
        // down to the margin the last underrun happened at (1 ms above it at least):
        // giving back a minute at a time walked it down to the level that underran,
        // again and again.
        let underruns = shared.underruns.load(Ordering::Relaxed);
        // By time an underrun right after a start counts too: the start was at the
        // sample's time, so running dry again means the latency can't be met.
        let steady = *timed || hold_until.is_none_or(|h| now >= h);
        // (A source that paused ran it dry, not the path's jitter: else the target grows
        // 2 ms with each restart of a sender.)
        // Adapting (R.30): what the path needs, from the arrivals' lateness (not counted
        // while the source pauses or the lane recovers: those aren't the path).
        let in_ms = frames as f64 * 1000.0 / sample_rate as f64;
        if cfg.adaptive {
            // The arrivals are measured but for a start's (or a resume's) first seconds,
            // whatever the ring does: an underrun or a refill changes nothing about the
            // path, and the late block behind an underrun is the one to measure.
            if paused {
                jitter.reset();
                *resumed_at = Some(now);
            }
            let rush = resumed_at.is_some_and(|t| now.duration_since(t) < Duration::from_secs(2));
            if !paused && !rush {
                jitter.arrive(now, in_ms);
            }
            let floor = (cfg.min_ms as f64).max(0.5).max(*age_floor);
            let grow = underruns != *underruns_seen
                && !paused
                && (steady || !rush)
                && underrun_at.is_none_or(|t| now.duration_since(t) >= ADAPT_UNDERRUN_STEP);
            if grow {
                *underran_ms = *auto_ms;
                let grown = (*auto_ms + (*auto_ms / 2.0).clamp(0.5, 2.0)).min(500.0);
                *auto_ms = match settled_from.take() {
                    Some((guess, at)) if now.duration_since(at) < Duration::from_secs(30) => {
                        // (A cut the settle asked for, not made yet, called off: it would
                        // take the restored level away again.)
                        if relevel.is_some_and(|(off, _)| off > 0.0) {
                            *relevel = None;
                        }
                        *starting = false;
                        grown.max(guess)
                    }
                    _ => grown,
                };
                *auto_at = Some(now);
                *underrun_at = Some(now);
                *measured = true;
            } else if let Some(need) = jitter.need_ms(now, ADAPT_RISE_S) {
                // (A rise is taken from the last seconds' arrivals; a fall only from the whole
                // window.)
                let want = (need * 1.2 + 0.5).max(floor);
                if want > *auto_ms {
                    *auto_ms = want.min(500.0);
                    *auto_at = Some(now);
                    *measured = true;
                } else if !*measured && underrun_at.is_none() {
                    // The first settling (the smallest buffer that doesn't
                    // click): once the path has been measured, straight down to what it
                    // needs rather than 1 ms every 5 s from the start guess (10 ms wired:
                    // over a minute to reach 2 ms on a calm path). From then on as below:
                    // grown at once, brought down slowly. The sound held for the guess
                    // is set right once as after a fresh start (`starting`): the next look
                    // measures the level against the target actually played (the sync
                    // group's, not this lane's guess) and cuts at a quiet moment — not on
                    // a lane playing by time (its lateness check does that; a cut asked
                    // there was never made, and blocked the rest, bug round 2026-11-09),
                    // nor with `keep_start_level`.
                    *settled_from = Some((*auto_ms, now));
                    *auto_ms = want;
                    *auto_at = Some(now);
                    *measured = true;
                    if relevel.is_none() {
                        *starting = true;
                    }
                } else if let Some(long) = jitter.need_ms(now, ADAPT_WINDOW_S) {
                    let want = (long * 1.2 + 0.5).max(floor);
                    let calm = |t: &Option<Instant>, d: Duration| {
                        t.is_none_or(|t| now.duration_since(t) >= d)
                    };
                    // (Not below where it last underran, for a while.)
                    let want = if calm(underrun_at, ADAPT_MEMORY) {
                        want
                    } else {
                        want.max(*underran_ms + 0.5)
                    };
                    if want + 1.0 <= *auto_ms
                        && calm(auto_at, ADAPT_STEP)
                        && calm(underrun_at, ADAPT_CALM)
                    {
                        *auto_ms = (*auto_ms - 1.0).max(want);
                        *auto_at = Some(now);
                    }
                    if *auto_ms <= want + 1.0 {
                        *measured = true;
                    }
                }
            }
            // In line with the mixer's other sources: the largest need among them. (A
            // start guess, not measured yet, is this lane's alone: a laptop's Wi-Fi
            // start of 40 ms raised every lane of the group and the site's with it.)
            // Until then it says what it has measured so far, at most its guess.
            let publish = if *measured {
                *auto_ms
            } else {
                jitter
                    .need_ms(now, ADAPT_RISE_S)
                    .map_or(floor, |n| (n * 1.2 + 0.5).max(floor))
                    .min(*auto_ms)
            };
            // (At most every 20 ms unless it changed: with 32 sources a mixer's group was
            // locked 16,000 times a second, a tenth of the pushing's cost.)
            if publish != published.1
                || published
                    .0
                    .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(20))
            {
                *published = (Some(now), publish);
                *group_ms = shared.sync.publish(shared.sync_id, publish, now);
            }
            *group_ms = group_ms.max(*auto_ms);
            let mut st = stats.lock_ok();
            st.adaptive = true;
            st.target_ms = group_ms.round() as u32;
            st.jitter_ms = jitter.need_ms(now, 1).map_or(st.jitter_ms, |_| {
                jitter
                    .need_ms(now, ADAPT_WINDOW_S)
                    .unwrap_or_else(|| jitter.need_ms(now, 1).unwrap_or(0.0))
            });
        }
        if cfg.adaptive {
            // (Adapting: underruns are taken in the target above.)
        } else if underruns != *underruns_seen && steady && !paused {
            // Steps of half the target, 0.5 to 2 ms: at an ultra-low 2 ms, 2 ms steps
            // tripled it after a few start-up underruns.
            // Not below what the source's age already needed (bug run K6-5: an underrun
            // cut a margin `too_old` raised, and the lane cut and underran again).
            let cap = (cfg.latency_ms as f64)
                .max(20.0)
                .max(*margin_floor)
                .min(500.0);
            let step = (cfg.latency_ms as f64 / 2.0).clamp(0.5, 2.0);
            *margin_floor = (*margin_ms + step / 2.0).min(cap);
            *margin_ms = (*margin_ms + step).min(cap);
            *margin_at = Some(now);
        } else if *margin_ms > *margin_floor
            && margin_at.is_some_and(|t| now.duration_since(t) >= Duration::from_secs(600))
        {
            *margin_ms = (*margin_ms - 1.0).max(*margin_floor);
            *margin_at = Some(now);
        }
        let target = if cfg.adaptive {
            group_ms.max(*auto_ms)
        } else {
            cfg.latency_ms as f64 + *margin_ms
        };
        // Levels, all in ms of device sound. The ring steps up by a source block as each
        // arrives and down by a device block as each is taken. Between arrivals it must
        // never fall below the target (at least a device block, so a callback is always
        // served), so right after an arrival it holds target + a source block: that is
        // `prime`, what the ring is filled to before sound starts and trimmed back to
        // after a gap. What the callbacks find after taking their block then cycles from
        // prime − device block down to the target, so its mean, which the servo holds,
        // is target + half a source block − half a device block (`hold`). Priming
        // exactly there leaves the servo nothing to drain: it only tracks drift.
        let block_ms = shared.block.load(Ordering::Relaxed) as f64 / per_ms;
        let in_block_ms = frames as f64 * 1000.0 / sample_rate as f64;
        let target = target.max(block_ms);
        let prime = target + in_block_ms;
        shared
            .start_ns
            .store((prime * 1e6) as i64, Ordering::Relaxed);
        shared
            .pushed_mono
            .store(crate::ptp::mono_ns(), Ordering::Relaxed);
        let hold = target + in_block_ms / 2.0 - block_ms / 2.0;
        let in_ring_ms = (ring.len() / rw) as f64 / per_ms;
        let mut dropped = 0.0;
        // By time (idn-clock.md §C.4): how late the ring's front sample will play against
        // its site time + the target. Its time: the block's, plus the block, less what
        // the resampler still holds and its filter's delay, less the ring. When it plays:
        // after the block the device took last, and the device's own delay.
        let late_ns = at.and_then(|(t0, map)| {
            let ns_src = 1e9 / sample_rate as f64;
            let ns_dev = 1e9 / shared.dev_rate as f64;
            let tail = frames as f64 * ns_src
                - rate.pending() as f64 * ns_src
                - rate.output_delay() as f64 * ns_dev;
            let front = tail - (ring.len() / rw) as f64 * ns_dev;
            let cb = Some(shared.cb_mono.load(Ordering::Relaxed))
                .filter(|&c| c != 0)
                .unwrap_or_else(crate::ptp::mono_ns);
            let block_ns = shared.block.load(Ordering::Relaxed) as f64 * ns_dev;
            let due = (map.site(cb) - t0) as f64
                + block_ns
                + shared.out_delay_ns.load(Ordering::Relaxed) as f64;
            // Seconds off: the sender's clock is wrong (or lies), not the network. Played
            // by the buffer level instead (the ring grew without
            // bound waiting for the time, or an hour of silence went in at once).
            Some(due - (front + target * 1e6)).filter(|l| l.abs() < 2e9)
        });
        if late_ns.is_none() && *timed {
            // Back to the buffer level: no shared clock any more (or not one to trust).
            *timed = false;
            servo.reset();
        }
        if late_ns.is_some() && !*timed {
            *timed = true;
            *late_sum = 0.0;
            *late_n = 0;
            *prev_late = None;
        }
        let was_ready = shared.ready.load(Ordering::Relaxed);
        if underruns != *underruns_seen {
            let n = underruns - *underruns_seen;
            if paused {
                &shared.pauses
            } else {
                &shared.jitter_underruns
            }
            .fetch_add(n, Ordering::Relaxed);
            *underruns_seen = underruns;
            *hold_until = Some(now + Duration::from_secs(2));
            servo.resync();
        }
        let mut recovering = hold_until.is_some_and(|h| now < h);
        // Too much sound. Far past the prime level at any time (a clock step): cut to
        // prime. While recovering from a gap, the backlog arrives in a rush, and its tail
        // would leave a surplus the servo takes a minute per 30 ms to bleed off; cutting
        // while it still arrives would cost a cut per block, so the fill is
        // watched and cut once, when it has stopped growing (under 5 ms in 100 ms). Every
        // cut is crossfaded over 5 ms. The recovery lasts while the ring is over.
        let fade = (5.0 * per_ms) as usize;
        // (A source block over prime is how readiness leaves it: not a burst.)
        let over = in_ring_ms > prime + in_block_ms + 10.0;
        // (Not for the small surplus a fresh start leaves: the level is measured once the
        // start has settled, and a fill hovering about `over` held that off for ever.)
        let small_start = *starting && !*cut_asked && in_ring_ms < prime + 100.0;
        // (By time the lateness check deals with a surplus: held off while it lasted, a
        // lower latency slewed at 1000 ppm instead of being cut.)
        if recovering && over && relevel.is_none() && !small_start && late_ns.is_none() {
            *hold_until = Some(now + Duration::from_secs(2));
            growth.push((now, in_ring_ms));
            growth.retain(|(t, _)| now.duration_since(*t) <= Duration::from_millis(300));
        } else if !over {
            growth.clear();
        }
        let settled = growth
            .iter()
            .find(|(t, _)| now.duration_since(*t) >= Duration::from_millis(100))
            .is_some_and(|(_, then)| in_ring_ms - then < 5.0);
        let spaced = last_trim.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(500));
        // Where it is cut: in a quiet stretch if the ring has one; else at the front for a
        // backlog (over 100 ms: stale sound, after the network held it up) or a lower
        // latency asked for; a smaller surplus, the burst a source starts with, waits up
        // to 5 s for a quiet moment instead (`relevel`): cut at the front, it was the
        // start of a sound (its first 44 ms lost).
        let margin = (per_ms as usize).max(1);
        if late_ns.is_none()
            && (in_ring_ms > prime + 250.0 || (recovering && over && settled))
            && spaced
        {
            let keep = (prime * per_ms) as usize * rw;
            let n = (ring.len() - keep) / rw;
            let quiet = cut_quiet(&mut ring, n, rw, margin, None);
            if quiet || *cut_asked || n as f64 / per_ms > 100.0 {
                dropped = if quiet {
                    ring.moved += 1;
                    n as f64 / per_ms
                } else {
                    let cut = cut_front(&mut ring, n * rw, rw, fade);
                    ring.cut += cut as u64;
                    cut as f64 / per_ms
                };
                *hold_until = Some(now + Duration::from_secs(2));
                *last_trim = Some(now);
                growth.clear();
                recovering = true;
                *relevel = None;
                *cut_asked = false;
                servo.resync();
            } else if relevel.is_none() {
                *relevel = Some((n as f64 / per_ms, now));
                growth.clear();
            }
        }
        // Before it first plays: its blocks coming evenly (the sound heard so far is the
        // time since the first came, and that block: a source pushing in real time) need
        // no waiting for.
        let even = !*ever_ready && {
            let (since, first) = *heard_from.get_or_insert((now, in_block_ms));
            *heard_n += 1;
            let ahead =
                *heard_ms + in_block_ms - first - now.duration_since(since).as_secs_f64() * 1e3;
            *heard_n >= 3 && ahead.abs() <= (in_block_ms / 2.0).max(2.0)
        };
        let young = !*ever_ready && *heard_ms < NEW_LANE_MS && !even;
        if !*ever_ready {
            *heard_ms += in_block_ms;
        }
        if young {
            // Not heard yet: no more than a start needs is kept (the next block brings it
            // to the prime level).
            let keep = ((prime - in_block_ms).max(0.0) * per_ms) as usize * rw;
            if late_ns.is_none() && ring.len() > keep {
                let extra = ring.len() - keep;
                ring.drain(..extra / rw * rw);
                ring.cut += (extra / rw) as u64;
            }
        }
        if let Some(late) = late_ns {
            let frames_of = |ns: f64| (ns.abs() * per_ms / 1e6) as usize * rw;
            if !was_ready && young {
                // A new lane, still hearing its source: nothing plays yet.
            } else if !was_ready {
                // Starts when the front sample is due: what is older is cut (nothing has
                // been heard yet); younger, it waits for its time.
                if late >= 0.0 && ring.len() > frames_of(late) {
                    ring.drain(..frames_of(late));
                    ring.cut += (frames_of(late) / rw) as u64;
                    fade_in_front(&mut ring, rw, fade, fade_left);
                    shared.ready.store(true, Ordering::Relaxed);
                    *ever_ready = true;
                    shared.fill_sum.store(0, Ordering::Relaxed);
                    shared.fill_n.store(0, Ordering::Relaxed);
                    *hold_until = Some(now + Duration::from_secs(2));
                    *servo_at = None;
                    *late_sum = 0.0;
                    *late_n = 0;
                    *too_old = None;
                } else if late >= 0.0 {
                    ring.moved += 1;
                    ring.clear(); // all of it too old
                    // Every block for 200 ms too old to start with: it isn't a stale
                    // burst, the source's sound is that old when it gets here (a
                    // network-audio device's, stamped with its time on its network: 5 ms
                    // before; the lane aimed at 2 ms, never started, and said nothing:
                    // R33-A). The target then has to cover that age: raised by the least
                    // of what was seen and a little, as after an underrun (adapting: its
                    // own target; else the margin on the latency set).
                    let late_ms = late / 1e6;
                    // The 200 ms are of blocks in a row: a gap in the source (a
                    // reconnect) starts them again (bug run I4-8: one stale block after
                    // it raised the target at once).
                    if too_old.is_some_and(|(_, _, last)| {
                        now.duration_since(last) > Duration::from_millis(100)
                    }) {
                        *too_old = None;
                    }
                    let (since, least, last) = too_old.get_or_insert((now, late_ms, now));
                    *least = least.min(late_ms);
                    *last = now;
                    if now.duration_since(*since) >= Duration::from_millis(200) {
                        let more = *least + 1.0;
                        if cfg.adaptive {
                            *auto_ms = (*auto_ms + more).min(500.0);
                            *age_floor = *auto_ms;
                            *auto_at = Some(now);
                            *measured = true;
                        } else {
                            // Capped as adapting is (I4-8: it had no cap).
                            *margin_ms = (*margin_ms + more).min(500.0);
                            *margin_floor = *margin_ms;
                            *margin_at = Some(now);
                        }
                        stats.lock_ok().late_ms = late_ms;
                        *too_old = None;
                    }
                } else {
                    // Younger than its time: it waits, and will start.
                    *too_old = None;
                }
            } else {
                *late_sum += late;
                *late_n += 1;
            }
        } else if !was_ready
            && !young
            && (ring.len() / rw) as f64 / per_ms >= prime - in_block_ms - 0.01
        {
            // Readiness comes right after an arrival that brings the ring within a source
            // block of the prime level: silence put before the sound makes up the rest, so
            // it starts at the level held, and every sample's latency is the same from the
            // first on (started past it, the surplus was cut at a quiet moment ~2 s later: a
            // step in the lip-sync). Over it (a burst), it is kept, played a little later,
            // and set right once settled. (Cut, it was the front of what the source resumed
            // with: after a pause the first 10-21 ms of its sound were lost, a short sound
            // at a file's start entirely.) The sound faded in, so a resume mid-wave doesn't
            // click.
            fade_in_front(&mut ring, rw, fade, fade_left);
            let short = (prime * per_ms) as usize * rw;
            if ring.len() < short {
                let n = (short - ring.len()) / rw * rw;
                for _ in 0..n {
                    ring.push_front(0.0);
                }
                ring.moved += 1;
            }
            shared.ready.store(true, Ordering::Relaxed);
            *ever_ready = true;
            shared.fill_sum.store(0, Ordering::Relaxed);
            shared.fill_n.store(0, Ordering::Relaxed);
            *hold_until = Some(now + Duration::from_secs(2));
            *servo_at = None;
        }
        dropped += relevel_step(
            shared,
            inner,
            &mut ring,
            stats,
            Look {
                now,
                per_ms,
                rw,
                fade,
                margin,
                fresh_frames,
                by_time: late_ns.is_some(),
            },
        );
        drop(ring);
        servo_second(
            shared,
            inner,
            cfg,
            stats,
            Measured {
                now,
                per_ms,
                rw,
                fade,
                hold,
                in_ring_ms,
                in_block_ms,
                dropped,
                recovering,
            },
        );
    }
}
