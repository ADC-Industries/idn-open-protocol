//! The switcher's inputs: receiving, their pictures made the input's (filters, delay),
//! fitted, cameras followed and moved.

use super::*;

/// Whether a network input is to receive now (§S.3): in use within the last
/// [`SwitcherConfig::idle_after`] (the frame clock marks it), or kept active.
fn wanted_now(slot: &Slot, idle_after: Duration) -> bool {
    slot.keep_active.load(Ordering::Relaxed)
        || slot
            .used_at
            .lock_ok()
            .is_some_and(|t| t.elapsed() < idle_after)
}

impl Slot {
    /// Wakes its worker if it sleeps unused (after `stop` is set: taken through `used_at`'s
    /// lock, so the worker either sees `stop` or is already waiting).
    pub(super) fn wake_unused(&self) {
        drop(self.used_at.lock_ok());
        self.use_changed.notify_all();
    }
}

/// A network input's worker (§S.3): connected while it is in use (or kept active), its
/// pictures and sound received ([`receive`]); disconnected once it has been unused for
/// [`SwitcherConfig::idle_after`], its last picture dropped, and connected again when it
/// comes back into use.
pub(super) fn connection(slot: &Arc<Slot>, shared: &std::sync::Weak<Shared>) {
    let Some(base) = slot.input_cfg.clone() else {
        return;
    };
    let idle_after = match shared.upgrade() {
        Some(sh) => sh.cfg.idle_after,
        None => return,
    };
    let mut refused = idn_open_protocol::log::Throttle::new(Duration::from_secs(30));
    while !slot.stop.load(Ordering::Relaxed) {
        {
            // Unused: asleep until it comes into use (the frame clock), is kept active, or
            // stops, all of which notify `use_changed` (checked and waited on under
            // `used_at`'s lock, so none is missed; the timeout only a safety net).
            let used = slot.used_at.lock_ok();
            let wanted = slot.keep_active.load(Ordering::Relaxed)
                || used.is_some_and(|t| t.elapsed() < idle_after);
            if !wanted {
                if !slot.stop.load(Ordering::Relaxed) {
                    let _ = slot.use_changed.wait_timeout(used, Duration::from_secs(1));
                }
                continue;
            }
        }
        // The compositor's GPU as it is now (it may have failed since the input was added).
        #[allow(unused_mut)]
        let mut cfg = base.clone();
        #[cfg(feature = "gpu")]
        {
            cfg.gpu = shared.upgrade().and_then(|sh| {
                sh.gpu_hand
                    .lock_ok()
                    .clone()
                    .filter(|_| idn_common::switches::on("IDN_SWITCHER_GPU_INPUTS"))
            });
        }
        let input = match Input::open(cfg) {
            Ok(i) => Arc::new(i),
            Err(e) => {
                refused.log(&format!("switcher: input {}: {e}", slot.id), "errors");
                *slot.last_error.lock_ok() = Some((Instant::now(), e.to_string()));
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        *slot.told.lock_ok() = None;
        *slot.input.lock_ok() = Some(input.clone());
        slot.active.store(true, Ordering::Relaxed);
        if let Some(sh) = shared.upgrade() {
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        }
        receive(slot, &input, shared, idle_after);
        *slot.lost.lock_ok() = None;
        // Disconnected: nothing of it kept (a frozen picture shown when it comes back
        // into use), its sound lanes left to the frame clock to drop.
        *slot.input.lock_ok() = None;
        drop(input);
        slot.active.store(false, Ordering::Relaxed);
        slot.running.store(false, Ordering::Relaxed);
        slot.connected.store(false, Ordering::Relaxed);
        *slot.latest.lock_ok() = None;
        *slot.raw.lock_ok() = None;
        *slot.fitted.lock_ok() = None;
        *slot.tile.lock_ok() = None;
        *slot.tile_small.lock_ok() = None;
        slot.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(sh) = shared.upgrade() {
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        }
    }
    refused.flush();
}

/// An input's receiving while its connection is open (until it stops, or stays unused for
/// `idle_after`): the newest picture kept; sound to its lane while it has one.
/// Pictures are made the input's (converted, filtered) on a thread of their own, the
/// newest kept: a 4K file's picture with a filter took longer than its sound's blocks
/// come, and the sound waited behind it (gaps in the program's sound).
fn receive(
    slot: &Arc<Slot>,
    input: &Input,
    shared: &std::sync::Weak<Shared>,
    idle_after: Duration,
) {
    let mailbox: Arc<Mailbox> = Arc::default();
    let maker = {
        let (slot, mailbox) = (slot.clone(), mailbox.clone());
        std::thread::Builder::new()
            .name("idn-switcher-pi".into())
            .spawn(move || {
                let (m, cv) = &*mailbox;
                loop {
                    let next = {
                        let mut g = m.lock_ok();
                        while g.0.is_none() && !g.1 {
                            g = cv
                                .wait(g)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        match g.0.take() {
                            Some(p) => p,
                            None => return,
                        }
                    };
                    let f = Arc::new(Frame::from_owned(next.0));
                    if let Some(h) = next.2 {
                        crate::frame::attach_hdr16(&f, h);
                    }
                    store(&slot, f, Some(next.1));
                }
            })
    };
    // The level the last sound block ended at.
    let mut gain_now = 1f32;
    // Coefficients reconstructed on the GPU here (until that fails).
    #[cfg(feature = "gpu")]
    let mut recon: Option<idn_shq_gpu::Reconstructor> = None;
    #[cfg(feature = "gpu")]
    let mut gpu_recon_failed = false;
    #[cfg(feature = "gpu")]
    let mut too_large_said = false;
    let maker = match maker {
        Ok(t) => Some(t),
        Err(e) => {
            idn_open_protocol::log::log(&format!("switcher: no picture thread ({e})"));
            None
        }
    };
    let mut refused = idn_open_protocol::log::Throttle::new(Duration::from_secs(10));
    while !slot.stop.load(Ordering::Relaxed) && wanted_now(slot, idle_after) {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Video(v)) => {
                // (Taken apart on the GPU's way.)
                #[cfg(feature = "gpu")]
                let mut v = v;
                // The session's first picture: tell the source tally and size again, now
                // that it surely hears them (asked for as the session started, they could
                // be lost: a still on preview stayed small; and after a
                // reconnect the source has forgotten them).
                if !slot.running.swap(true, Ordering::Relaxed) {
                    *slot.told.lock_ok() = None;
                }
                // Kept on the GPU (§E.27): its sizes here, its planes there; nothing for the
                // picture thread to do.
                #[cfg(feature = "gpu")]
                if let Some(g) = v.gpu.take()
                    && let Some(hand) = shared
                        .upgrade()
                        .and_then(|sh| sh.gpu_hand.lock_ok().clone())
                {
                    slot.decode_path.store(2, Ordering::Relaxed);
                    let f = Arc::new(Frame::on_gpu(g, hand));
                    // (In the HDR program's transfer: copied up, not lifted as SDR.)
                    if let Some(m) = hdr_marker(&v, shared) {
                        crate::frame::attach_hdr16(&f, m);
                    }
                    store(slot, f, Some(v.header.timestamp));
                    continue;
                }
                // A camera as coefficients (no alpha): reconstructed on the GPU once, here,
                // and handed on as a picture there. The compositor and the scene drawer
                // (a mix's virtual shot of it) each packed and reconstructed it again every
                // frame, 13 MB of a 4K camera's words written twice.
                #[cfg(feature = "gpu")]
                if let Some(c) = v
                    .picture
                    .coefficients
                    .as_ref()
                    .filter(|c| c.planes.len() == 3)
                    && !gpu_recon_failed
                    && let Some(hand) = shared
                        .upgrade()
                        .and_then(|sh| sh.gpu_hand.lock_ok().clone())
                {
                    let r = recon.get_or_insert_with(|| {
                        idn_shq_gpu::Reconstructor::new(Arc::new(idn_shq_gpu::Gpu::adopt(
                            hand.device.clone(),
                            hand.queue.clone(),
                            "IDN_SWITCHER_IDCT",
                        )))
                    });
                    match r.reconstruct_gpu(c) {
                        Ok(g) => {
                            slot.decode_path.store(3, Ordering::Relaxed);
                            let f = Arc::new(Frame::on_gpu(g, hand));
                            if let Some(m) = hdr_marker(&v, shared) {
                                crate::frame::attach_hdr16(&f, m);
                            }
                            store(slot, f, Some(v.header.timestamp));
                            continue;
                        }
                        // One picture too large: that one by the compositor, the next
                        // tried on the GPU again (bug run I5-6: one 8K picture kept the
                        // input off the GPU for good). Said once.
                        Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                            if !too_large_said {
                                too_large_said = true;
                                idn_open_protocol::log::log(&format!(
                                    "switcher: input {}: pictures too large for the GPU's buffers reconstructed by the compositor ({e})",
                                    slot.id
                                ));
                            }
                        }
                        Err(e) => {
                            idn_open_protocol::log::log(&format!(
                                "switcher: input {}: its pictures reconstructed by the compositor from now on, the other inputs' on the GPU as before ({e})",
                                slot.id
                            ));
                            gpu_recon_failed = true;
                        }
                    }
                }
                slot.decode_path.store(1, Ordering::Relaxed);
                if maker.is_some() {
                    // (An HDR source's full-precision planes go with it: R.24 stage 2.)
                    let mut v = v;
                    let hdr16 = hdr16_of_video(&mut v, shared);
                    let (m, cv) = &*mailbox;
                    m.lock_ok().0 = Some((v.picture, v.header.timestamp, hdr16));
                    cv.notify_one();
                } else {
                    // An HDR source on an HDR program (R.24 stage 2): its full-precision
                    // planes kept beside the picture, for the compositor's HDR pass.
                    let mut v = v;
                    let hdr16 = hdr16_of_video(&mut v, shared);
                    let f = Arc::new(Frame::from_owned(v.picture));
                    if let Some(h) = hdr16 {
                        crate::frame::attach_hdr16(&f, h);
                    }
                    store(slot, f, Some(v.header.timestamp));
                }
            }
            Some(InputEvent::Audio(a)) => {
                slot.meter.lock_ok().add(&a.channels);
                // Into the monitor while it is heard there (§S.24), and the Preview
                // output while on preview (§S.25).
                for lane in [&slot.monitor_lane, &slot.preview_lane] {
                    if let Some((lane, _)) = lane.lock_ok().as_ref() {
                        let at = idn_open_protocol::audio::site_time(
                            a.header.timestamp,
                            a.header.metadata.as_deref(),
                        );
                        lane.push_timed(a.header.sample_rate, &a.channels, None, at);
                    }
                }
                // Its own sound for ISO recording, up to a second held.
                if !slot.iso.lock_ok().is_empty() {
                    let rate = a.header.sample_rate.max(1);
                    slot.iso_rate.store(rate, Ordering::Relaxed);
                    let mut buf = slot.iso_audio.lock_ok();
                    if buf.len() != a.channels.len() {
                        *buf = vec![Vec::new(); a.channels.len()];
                    }
                    for (b, c) in buf.iter_mut().zip(&a.channels) {
                        b.extend_from_slice(c);
                        if b.len() > rate as usize {
                            let extra = b.len() - rate as usize;
                            b.drain(..extra);
                        }
                    }
                }
                // Its sound into the sound-track recordings that take it.
                for (_, lane, _) in slot.track_lanes.lock_ok().iter() {
                    let at = idn_open_protocol::audio::site_time(
                        a.header.timestamp,
                        a.header.metadata.as_deref(),
                    );
                    lane.push_timed(a.header.sample_rate, &a.channels, None, at);
                }
                if let Some(lane) = slot.lane.lock_ok().as_ref() {
                    let mut tail = slot.audio_tail.lock_ok();
                    let at = idn_open_protocol::audio::site_time(
                        a.header.timestamp,
                        a.header.metadata.as_deref(),
                    );
                    // At its level in the stream, a change ramped over the block (no click).
                    let g = f32::from_bits(slot.gain.load(Ordering::Relaxed));
                    if g == 1.0 && gain_now == 1.0 {
                        lane.push_timed(a.header.sample_rate, &a.channels, None, at);
                    } else {
                        let scaled: Vec<Vec<f32>> = a
                            .channels
                            .iter()
                            .map(|c| {
                                let n = c.len().max(1) as f32;
                                c.iter()
                                    .enumerate()
                                    .map(|(i, v)| v * (gain_now + (g - gain_now) * i as f32 / n))
                                    .collect()
                            })
                            .collect();
                        lane.push_timed(a.header.sample_rate, &scaled, None, at);
                    }
                    gain_now = g;
                    let n = a.channels.first().map_or(0, Vec::len) as i64;
                    *tail = Some(
                        a.header.timestamp + n * 10_000_000 / a.header.sample_rate.max(1) as i64,
                    );
                }
            }
            // The source's capabilities: whether it takes PTZ commands.
            Some(InputEvent::Metadata { xml, .. }) if xml.contains("ndi_capabilities") => {
                slot.ptz_capable
                    .store(xml.contains("ntk_ptz=\"true\""), Ordering::Relaxed);
            }
            // The receiver's errors (a picture refused as too large, a decode error):
            // in the log at most once each 10 s, and in the input's state (lab 521-A:
            // an 8K camera showed nothing and said nothing).
            Some(InputEvent::Error(e)) => {
                refused.log(&format!("switcher: input {}: {e}", slot.id), "errors");
                *slot.last_error.lock_ok() = Some((Instant::now(), e));
            }
            Some(InputEvent::State(st)) => {
                let up = matches!(st, ConnectionState::Connected(_));
                // Why it was lost, while it is (§S.37's events).
                match &st {
                    ConnectionState::Lost(why) => *slot.lost.lock_ok() = Some(why.clone()),
                    ConnectionState::Connected(_) | ConnectionState::Routed { .. } => {
                        *slot.lost.lock_ok() = None
                    }
                    _ => {}
                }
                slot.running.store(false, Ordering::Relaxed);
                if slot.connected.swap(up, Ordering::Relaxed) != up
                    && let Some(sh) = shared.upgrade()
                {
                    let mut s = sh.state.lock_ok();
                    bump(&mut s, &sh.changed);
                }
            }
            _ => {}
        }
    }
    refused.flush();
    let (m, cv) = &*mailbox;
    m.lock_ok().1 = true;
    cv.notify_one();
    if let Some(t) = maker {
        let _ = t.join();
    }
}

/// A new picture for an input: kept as it came, and shown through its filters (on the
/// thread that brings it: an input's own, in parallel with the others). With a timestamp,
/// it is kept with the picture (`shown`).
#[cfg(feature = "gpu")]
/// For a picture with no samples here (on the GPU): an empty [`crate::frame::Hdr16`]
/// when it is in the HDR program's transfer (copied up as it is, not lifted as SDR).
fn hdr_marker(
    v: &idn_open_protocol::input::VideoIn,
    shared: &std::sync::Weak<Shared>,
) -> Option<Arc<crate::frame::Hdr16>> {
    use idn_open_protocol::color::Transfer;
    let want = shared.upgrade()?.cfg.hdr;
    (want != Transfer::Sdr && Transfer::from_name(&v.color.transfer) == want).then(|| {
        Arc::new(crate::frame::Hdr16 {
            transfer: want,
            y: Vec::new(),
            cb: Vec::new(),
            cr: Vec::new(),
        })
    })
}

/// A picture's 16-bit planes when the program is HDR in the same transfer as the source
/// says it is, and the input decoded it precisely (4:2:2): `None` otherwise (lifted from
/// its 8-bit planes instead).
fn hdr16_of_video(
    v: &mut idn_open_protocol::input::VideoIn,
    shared: &std::sync::Weak<Shared>,
) -> Option<Arc<crate::frame::Hdr16>> {
    use idn_open_protocol::color::Transfer;
    let want = shared.upgrade()?.cfg.hdr;
    if want == Transfer::Sdr || Transfer::from_name(&v.color.transfer) != want {
        return None;
    }
    let (w, h) = (v.picture.width, v.picture.height);
    let cw = w.div_ceil(2);
    // Without 16-bit planes (decoded as coefficients for the GPU, an 8-bit codec/1
    // source): its 8-bit samples are already in the program's space, so they are copied
    // up, not lifted as SDR (an empty Hdr16 says so).
    let marker = || {
        Some(Arc::new(crate::frame::Hdr16 {
            transfer: want,
            y: Vec::new(),
            cb: Vec::new(),
            cr: Vec::new(),
        }))
    };
    let Some(p) = v.picture.precise.take() else {
        return marker();
    };
    if p.len() < 3 || p[0].data.len() != w * h {
        return marker();
    }
    let mut it = p.into_iter();
    let (y, cb, cr) = (it.next()?.data, it.next()?.data, it.next()?.data);
    // 4:2:0 (half the rows of chroma): each row twice, as the 8-bit planes have it.
    let rows = |c: Vec<u16>| -> Option<Vec<u16>> {
        if c.len() == cw * h {
            Some(c)
        } else if c.len() == cw * h.div_ceil(2) {
            Some(
                (0..h)
                    .flat_map(|r| c[(r / 2) * cw..(r / 2 + 1) * cw].iter().copied())
                    .collect(),
            )
        } else {
            None
        }
    };
    match (rows(cb), rows(cr)) {
        (Some(cb), Some(cr)) => Some(Arc::new(crate::frame::Hdr16 {
            transfer: want,
            y,
            cb,
            cr,
        })),
        _ => marker(),
    }
}

pub(super) fn store(slot: &Slot, f: Arc<Frame>, ts: Option<i64>) {
    *slot.raw.lock_ok() = Some((f.clone(), Instant::now()));
    let chain = slot.filters.lock_ok().clone();
    let f = match chain {
        Some(c) if !c.is_empty() => Arc::new(filtered(&c, &f)),
        _ => f,
    };
    let mut latest = slot.latest.lock_ok();
    if let Some(ts) = ts {
        slot.picture_ts.store(ts, Ordering::Relaxed);
    }
    *latest = Some(f);
    drop(latest);
    slot.seq.fetch_add(1, Ordering::Relaxed);
}

/// A delay input's worker: keep the source input's recent pictures and hand this input the
/// one from `by_ms` ago, so it shows the source late. Video only. Memory is the delay in
/// frames times a picture, so a long delay on a big picture costs real RAM.
pub(super) fn delay_worker(slot: &Arc<Slot>, shared: &std::sync::Weak<Shared>) {
    let (src_id, by) = match &slot.source {
        Source::Delay { input, by_ms } => (*input, Duration::from_millis(*by_ms as u64)),
        _ => return,
    };
    let period = match shared.upgrade() {
        Some(sh) => {
            let (num, den) = (sh.cfg.fps.0.max(1) as u64, sh.cfg.fps.1.max(1) as u64);
            Duration::from_nanos(1_000_000_000 * den / num)
        }
        None => return,
    };
    // The source's recent pictures, oldest first, each with when it was taken here.
    let mut ring: std::collections::VecDeque<(Instant, Arc<Frame>)> =
        std::collections::VecDeque::new();
    let cap = (by.as_secs_f64() / period.as_secs_f64()).ceil() as usize + 8;
    let mut last_seq = u64::MAX;
    while !slot.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let Some(sh) = shared.upgrade() else {
            return;
        };
        match sh.inputs.lock_ok().get(&src_id).cloned() {
            Some(src) => {
                // Take the source's newest picture when it changes.
                let seq = src.seq.load(Ordering::Relaxed);
                if seq != last_seq {
                    last_seq = seq;
                    if let Some(f) = src.latest.lock_ok().clone() {
                        ring.push_back((now, f));
                        while ring.len() > cap {
                            ring.pop_front();
                        }
                    }
                }
                // Show the newest picture at least `by` old; drop the ones it passes.
                let mut out = None;
                while let Some((t, _)) = ring.front() {
                    if now.saturating_duration_since(*t) >= by {
                        out = ring.pop_front().map(|(_, f)| f);
                    } else {
                        break;
                    }
                }
                if let Some(f) = out {
                    store(slot, f, None);
                    if !slot.connected.swap(true, Ordering::Relaxed) {
                        let mut s = sh.state.lock_ok();
                        bump(&mut s, &sh.changed);
                    }
                }
            }
            // The source went away: stop following it.
            None => slot.connected.store(false, Ordering::Relaxed),
        }
        drop(sh);
        let elapsed = now.elapsed();
        if elapsed < period {
            std::thread::sleep(period - elapsed);
        }
    }
}

/// Filters that move (a scroll) on an input whose pictures don't (a still, a title, a
/// paused file): its last picture filtered again each frame.
pub(super) fn filters_tick(sh: &Arc<Shared>) {
    let (num, den) = (sh.cfg.fps.0.max(1) as u64, sh.cfg.fps.1.max(1) as u64);
    let period = Duration::from_nanos(1_000_000_000 * den / num);
    // No input with moving filters at the last look: looked at again a few times a second
    // rather than each frame (a scroll added starts within a quarter second).
    let mut idle = false;
    while !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(if idle {
            period.max(Duration::from_millis(250))
        } else {
            period
        });
        idle = true;
        let slots: Vec<Arc<Slot>> = sh.inputs.lock_ok().values().cloned().collect();
        for slot in slots {
            let Some(chain) = slot.filters.lock_ok().clone().filter(|c| c.animated()) else {
                continue;
            };
            idle = false;
            let raw = slot.raw.lock_ok().clone();
            if let Some((f, at)) = raw.filter(|(_, at)| at.elapsed() >= period) {
                let out = Arc::new(chain.apply(&f, filter_time()));
                // Unless a new picture came meanwhile.
                if slot.raw.lock_ok().as_ref().is_some_and(|(_, a)| *a == at) {
                    *slot.latest.lock_ok() = Some(out);
                    slot.seq.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// The program size version of an input's newest picture (cached until a new one), its
/// timestamp noted as shown.
pub(super) fn fitted(slot: &Slot, w: usize, h: usize) -> Option<Arc<Frame>> {
    let seq = slot.seq.load(Ordering::Relaxed);
    let mut cache = slot.fitted.lock_ok();
    if let Some((s, f, ts)) = cache.as_ref()
        && *s == seq
    {
        slot.shown_ts.store(*ts, Ordering::Relaxed);
        return Some(f.clone());
    }
    let raw = shown(slot)?;
    let ts = slot.shown_ts.load(Ordering::Relaxed);
    let f = if (raw.width, raw.height) == (w, h) {
        // Coefficients (asked for while there was to be a GPU) as samples: the CPU's
        // compositing found no planes in them, so mixes, keys and fade to black did
        // nothing, and a mix from an empty bus panicked.
        if raw.deferred() {
            Arc::new(raw.pixels())
        } else {
            raw
        }
    } else {
        Arc::new(raw.fitted(w, h))
    };
    *cache = Some((seq, f.clone(), ts));
    Some(f)
}

/// The inputs `id` shows besides itself: a scene's members (nested scenes too), and the
/// input a preset input, a virtual shot or a delay shows (§S.21), with what that shows.
pub(super) fn shown_through(
    id: Option<u32>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
) -> std::collections::BTreeSet<u32> {
    let follows = |id: u32| match inputs.get(&id).map(|s| &s.source) {
        Some(Source::Ptz { camera: src, .. })
        | Some(Source::Virtual { input: src, .. })
        | Some(Source::Delay { input: src, .. }) => Some(*src),
        _ => None,
    };
    let mut m = std::collections::BTreeSet::new();
    if let Some(id) = id {
        crate::scene::members(id, scenes, &mut m);
        let mut more: Vec<u32> = m.iter().copied().chain([id]).collect();
        while let Some(i) = more.pop() {
            if let Some(src) = follows(i)
                && m.insert(src)
            {
                crate::scene::members(src, scenes, &mut m);
                more.push(src);
            }
        }
    }
    m
}

/// A preset input's or a virtual shot's worker (§S.21): the camera's pictures as they are,
/// or the part of them the shot looks at (cut and scaled again when the picture or the
/// view changes).
pub(super) fn follow_worker(slot: &Arc<Slot>, shared: &std::sync::Weak<Shared>) {
    let src_id = match &slot.source {
        Source::Ptz { camera, .. } => *camera,
        Source::Virtual { input, .. } => *input,
        _ => return,
    };
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
    let (mut last_seq, mut last_view) = (u64::MAX, None::<(f32, f32, f32)>);
    // How long a picture takes to make (smoothed): a 4K camera's, cut and scaled on the
    // CPU, took longer than half a period.
    let mut making = Duration::ZERO;
    while !slot.stop.load(Ordering::Relaxed) {
        let Some(sh) = shared.upgrade() else {
            return;
        };
        // Made before each of the compositor's frames, for that frame's time, by as long
        // as making takes (at least half a period): on a clock of its own the worker
        // drifted against the frames, which then took a picture twice and skipped the
        // next (a move stood still a frame, then jumped twice as far).
        let clock = *sh.frame_clock.lock_ok();
        let lead = (making + making / 4 + Duration::from_millis(2)).clamp(period / 2, period * 2);
        let (due, wake) = next_frame(clock, period, lead, Instant::now());
        drop(sh);
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
        let Some(sh) = shared.upgrade() else {
            return;
        };
        let src = sh.inputs.lock_ok().get(&src_id).cloned();
        match src {
            Some(src) => {
                let seq = src.seq.load(Ordering::Relaxed);
                let run = sh.views.lock_ok().get(&slot.id).copied();
                let latest = src.latest.lock_ok().clone();
                let view = run
                    .zip(latest.as_ref())
                    .map(|(r, f)| r.now_on(f.width, f.height, w, h, due));
                let key = view.map(|v| (v.x, v.y, v.zoom));
                // On a bus the GPU crops the camera itself (gpu_scene); a picture here is
                // needed whole only where the CPU takes it, else small and a frame in four
                // for the multiview's tile, or once a second with no multiview shown (the
                // API's thumbnail: rebuilding the 4K camera's pixels a frame in four was a
                // sixth of a core for nothing).
                let small = view.is_some()
                    && sh.views_on_gpu.load(Ordering::Relaxed)
                    && !wanted_whole(&sh, slot);
                let multiview = multiview_wanted(&sh);
                let skip = small
                    && clock.is_some_and(|(start, p)| {
                        let every = if multiview {
                            4
                        } else {
                            (1_000_000_000 / p.as_nanos().max(1)).max(1)
                        };
                        (due.saturating_duration_since(start).as_nanos() / p.as_nanos().max(1))
                            % every
                            != 0
                    });
                let (w, h) = if small { ((w / 2) & !1, h / 2) } else { (w, h) };
                if (seq != last_seq || key != last_view)
                    && !skip
                    && let Some(f) = latest
                {
                    last_seq = seq;
                    last_view = key;
                    let t_make = Instant::now();
                    let out = match view {
                        // The whole picture at the program's size: as it is.
                        Some(v)
                            if (f.width, f.height) == (w, h)
                                && v.crop(f.width, f.height, w, h) == (0.0, 0.0, 1.0, 1.0) =>
                        {
                            f
                        }
                        Some(v) => Arc::new(f.scaled(v.crop(f.width, f.height, w, h), w, h)),
                        None => f,
                    };
                    store(slot, out, None);
                    making = (making * 3 + t_make.elapsed()) / 4;
                }
                let up = src.connected.load(Ordering::Relaxed);
                if slot.connected.swap(up, Ordering::Relaxed) != up {
                    let mut s = sh.state.lock_ok();
                    bump(&mut s, &sh.changed);
                }
            }
            None => slot.connected.store(false, Ordering::Relaxed),
        }
    }
}

/// Keeps the inputs feeding `mixer` (each through the lane `lane_of` gives, at its
/// latency) as `want`: lanes made for those that came, dropped for those that went.
pub(super) fn follow(
    mixer: &Arc<Mixer>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    lane_of: fn(&Slot) -> &InputLane,
    want: Vec<(u32, u32)>,
    from: &mut Vec<(u32, u32)>,
) {
    if want == *from {
        return;
    }
    for (id, _) in from.iter() {
        if let Some(slot) = inputs.get(id) {
            *lane_of(slot).lock_ok() = None;
        }
    }
    for &(id, lat) in &want {
        let lane = mixer.lane(&LaneConfig {
            latency_ms: lat,
            max_correction_ppm: 1000.0,
            // (As the program's lanes: no step a few seconds in, lab 467-B.)
            keep_start_level: true,
            ..Default::default()
        });
        *lane_of(&inputs[&id]).lock_ok() = Some((lane, lat));
    }
    *from = want;
}

/// Sends a move to a camera by its control (§S.21).
pub(super) fn camera_move(sh: &Shared, camera: u32, m: crate::ptz::Move) -> Result<(), String> {
    let slot = sh
        .inputs
        .lock_ok()
        .get(&camera)
        .cloned()
        .ok_or_else(|| format!("no input {camera}"))?;
    let (control, visca) = {
        let cams = sh.cameras.lock_ok();
        let c = cams
            .get(&camera)
            .ok_or_else(|| format!("input {camera} has no camera control set"))?;
        (c.control.clone(), c.visca.clone())
    };
    let r = match control {
        crate::ptz::Control::Network => match slot.input.lock_ok().clone() {
            Some(input) => {
                for xml in m.ndi() {
                    input.send_metadata(&xml);
                }
                Ok(())
            }
            None if slot.input_cfg.is_some() => Err("not connected yet".to_owned()),
            None => Err("not a network input".to_owned()),
        },
        crate::ptz::Control::Visca { .. } => visca
            .ok_or_else(|| "no VISCA link".to_owned())?
            .send(m)
            .map_err(|e| format!("VISCA: {e}")),
    };
    if let Some(c) = sh.cameras.lock_ok().get_mut(&camera) {
        c.error = r.as_ref().err().cloned();
        if let (Ok(()), crate::ptz::Move::Recall { preset, .. }) = (&r, m) {
            c.last_preset = Some(preset);
        }
    }
    r
}

/// A preset input `id` shown on a bus (in its scene): its camera moved to its preset — for
/// preview only when the camera isn't on air through another input (`on_air`: what the
/// program shows); not again when it is there already.
pub(super) fn recall_for(sh: &Arc<Shared>, id: u32, on_air: &[u32], to_program: bool) {
    let inputs = sh.inputs.lock_ok().clone();
    let Some(Source::Ptz { camera, preset }) = inputs.get(&id).map(|s| s.source.clone()) else {
        return;
    };
    let camera_of = |i: u32| match inputs.get(&i).map(|s| &s.source) {
        Some(Source::Ptz { camera, .. }) => Some(*camera),
        Some(_) => Some(i),
        None => None,
    };
    let on_air = on_air
        .iter()
        .any(|&p| p != id && camera_of(p) == Some(camera));
    if !to_program && on_air {
        return;
    }
    let already = sh
        .cameras
        .lock_ok()
        .get(&camera)
        .is_some_and(|c| c.last_preset == Some(preset));
    if !already {
        // On a thread of its own: a camera that doesn't answer (DNS, a 2 s connect, twice)
        // held up tally and the other triggers. One a camera at a time, the latest preset
        // wanted taken by it (bug run 2026-11-25: cutting back and forth stacked a thread
        // per recall behind an unreachable camera).
        static RECALLS: std::sync::Mutex<std::collections::BTreeMap<u32, (bool, Option<u8>)>> =
            std::sync::Mutex::new(std::collections::BTreeMap::new());
        let key = camera;
        {
            let mut r = RECALLS.lock_ok();
            let e = r.entry(key).or_insert((false, None));
            e.1 = Some(preset);
            if e.0 {
                return; // the camera's thread takes it next
            }
            e.0 = true;
        }
        let sh = sh.clone();
        let spawned = std::thread::Builder::new()
            .name("idn-ptz-recall".into())
            .spawn(move || {
                loop {
                    let next = {
                        let mut r = RECALLS.lock_ok();
                        let e = r.entry(key).or_insert((true, None));
                        match e.1.take() {
                            Some(p) => p,
                            None => {
                                e.0 = false;
                                return;
                            }
                        }
                    };
                    let _ = camera_move(
                        &sh,
                        camera,
                        crate::ptz::Move::Recall {
                            preset: next,
                            speed: 1.0,
                        },
                    );
                }
            });
        // (Not started: the camera free again, else its recalls stopped for good.)
        if spawned.is_err() {
            let mut r = RECALLS.lock_ok();
            if let Some(e) = r.get_mut(&key) {
                e.0 = false;
            }
        }
    }
}
