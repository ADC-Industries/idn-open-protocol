//! Slideshows.

use super::*;

/// A slideshow input's worker: shows the picture at its place, read ahead of time, cut or
/// cross-faded; moves on by itself while playing. A picture that can't be read leaves the
/// last one showing, its error in the state.
pub(super) fn slideshow_worker(
    slot: &Arc<Slot>,
    shared: &std::sync::Weak<Shared>,
    ctl: &SlideCtl,
    show: &Slideshow,
) {
    let Some((w, h, period)) = shared.upgrade().map(|sh| {
        let (num, den) = (sh.cfg.fps.0.max(1) as u64, sh.cfg.fps.1.max(1) as u64);
        (
            sh.cfg.width,
            sh.cfg.height,
            Duration::from_nanos(1_000_000_000 * den / num),
        )
    }) else {
        return;
    };
    let load = |place: usize, order: &[usize]| -> Result<Arc<Frame>, String> {
        let path = &show.pictures[order[place]];
        let loader = shared
            .upgrade()
            .and_then(|sh| sh.loader.lock_ok().clone())
            .ok_or("no picture loader")?;
        let f = loader(path).map_err(|e| format!("{path}: {e}"))?;
        Ok(Arc::new(f.fitted(w, h)))
    };
    let set = |f: Arc<Frame>| store(slot, f, None);
    let bump_state = || {
        if let Some(sh) = shared.upgrade() {
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        }
    };
    let every = Duration::from_secs_f32(show.every);
    let fade_frames = (show.fade as f64 / period.as_secs_f64()).round() as u32;
    let (mut shown, mut current): (Option<usize>, Option<Arc<Frame>>) = (None, None);
    let mut ahead: Option<(usize, Arc<Frame>)> = None;
    let mut since = Instant::now();
    while !slot.stop.load(Ordering::Relaxed) {
        let (place, order) = {
            let r = ctl.run.lock_ok();
            (r.index, r.order.clone())
        };
        if shown != Some(place) {
            let next = match ahead.take() {
                Some((p, f)) if p == place => Ok(f),
                _ => load(place, &order),
            };
            match next {
                Ok(next) => {
                    if let Some(cur) = current.as_ref().filter(|_| fade_frames > 1) {
                        for k in 1..fade_frames {
                            if slot.stop.load(Ordering::Relaxed) {
                                return;
                            }
                            let t0 = Instant::now();
                            set(Arc::new(mix_frames(cur, &next, k * 256 / fade_frames)));
                            std::thread::sleep(period.saturating_sub(t0.elapsed()));
                        }
                    }
                    set(next.clone());
                    current = Some(next);
                    ctl.run.lock_ok().error = None;
                    if !slot.connected.swap(true, Ordering::Relaxed) {
                        slot.running.store(true, Ordering::Relaxed);
                    }
                }
                Err(e) => ctl.run.lock_ok().error = Some(e),
            }
            shown = Some(place);
            since = Instant::now();
            bump_state();
            // The next picture read now, ready to show at once.
            if order.len() > 1 {
                let p = (place + 1) % order.len();
                ahead = load(p, &order).ok().map(|f| (p, f));
            }
            continue;
        }
        let r = ctl.run.lock_ok();
        let wait = if r.playing && show.every > 0.0 {
            every.saturating_sub(since.elapsed())
        } else {
            Duration::from_millis(250)
        }
        .min(Duration::from_millis(250));
        let (mut r, _) = ctl
            .wake
            .wait_timeout(r, wait)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if r.index != place || !r.playing || show.every <= 0.0 || since.elapsed() < every {
            continue;
        }
        if r.index + 1 < r.order.len() {
            r.index += 1;
        } else if show.looped {
            r.index = 0;
            if show.shuffle {
                r.order = slide_order(r.order.len(), true, slot.id as u64);
                ahead = None;
            }
        } else {
            // The end: stays on the last picture, and says so.
            r.playing = false;
            r.ended = true;
            drop(r);
            bump_state();
            if let Some(sh) = shared.upgrade() {
                run_event(&sh, &TriggerEvent::SlideshowEnd(slot.id));
            }
        }
    }
}

/// Moves slideshow `id` on (its worker shows the change).
pub(super) fn slide(sh: &Shared, id: u32, action: SlideAction) -> Result<(), String> {
    let ctl = sh
        .slides
        .lock_ok()
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("input {id} is not a slideshow"))?;
    let mut r = ctl.run.lock_ok();
    let n = r.order.len();
    match action {
        SlideAction::Next => r.index = (r.index + 1) % n.max(1),
        SlideAction::Previous => r.index = (r.index + n.max(1) - 1) % n.max(1),
        SlideAction::Goto(i) => {
            // By its place in the list, wherever a shuffle put it.
            r.index = r
                .order
                .iter()
                .position(|&p| p == i)
                .ok_or_else(|| format!("goto: 0 to {}", n.saturating_sub(1)))?
        }
        SlideAction::Play => r.playing = true,
        SlideAction::Pause => r.playing = false,
    }
    r.ended = false;
    drop(r);
    ctl.wake.notify_all();
    Ok(())
}

/// The order a slideshow's `n` pictures show in: as listed, or shuffled (from `seed` and
/// the time).
pub(super) fn slide_order(n: usize, shuffle: bool, seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if shuffle {
        let mut x = seed ^ idn_common::epoch::now().as_nanos() as u64 | 1;
        for i in (1..n).rev() {
            // xorshift64
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            order.swap(i, (x % (i as u64 + 1)) as usize);
        }
    }
    order
}
