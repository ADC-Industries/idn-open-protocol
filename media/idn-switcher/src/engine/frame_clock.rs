//! The switcher's frame clock: each frame's program composed (on the GPU, or the CPU),
//! sent, recorded, its sound mixed, and the monitor's picture made.

use super::*;

pub(super) fn frame_clock(sh: &Arc<Shared>) {
    let (num, den) = (sh.cfg.fps.0.max(1) as u64, sh.cfg.fps.1.max(1) as u64);
    let period = Duration::from_nanos(1_000_000_000 * den / num);
    let dt = period.as_secs_f32();
    let (w, h) = (sh.cfg.width, sh.cfg.height);
    let mut k: u64 = 0;
    // (Read once: a lookup in the environment every block or frame otherwise.)
    let crash_test = idn_common::switches::on("IDN_CLOCK_CRASH_TEST");
    // Samples sent so far, so each frame carries its exact share of the sound.
    let mut samples_sent: u64 = 0;
    // The program's sound (the monitor's too, §S.24).
    let mut sound = ProgramSound::default();
    // Lanes an earlier run of this clock made (it restarted after a panic) dropped: this
    // run knows only its own, and an input left with one went on being mixed into the
    // program (and the monitor) for good (bug round 2026-11-09).
    for slot in sh.inputs.lock_ok().values() {
        *slot.lane.lock_ok() = None;
        *slot.monitor_lane.lock_ok() = None;
    }
    // The GPU compositor, if there is one (pipelined: each frame comes out a frame later,
    // its sound held back with it).
    #[cfg(feature = "gpu")]
    let mut gpu = if sh.cfg.gpu {
        let made = sh.gpu_made.lock_ok().take().unwrap_or_else(|| make_gpu(sh));
        match made {
            Ok(mut g) => {
                // An HDR program composed in its space on the GPU too (R.24, §S.34).
                if sh.program_hdr.is_some() {
                    g.set_hdr(sh.cfg.hdr);
                }
                // The program's pictures shared GPU to GPU with receivers on this machine
                // on the same GPU (the monitor, display outputs: §E.27).
                // (Not an HDR program: it is made HDR from its samples, in memory.)
                if sh.program_hdr.is_none() {
                    sh.program_out.use_gpu(Some(g.hand_in()));
                }
                *sh.compositor.lock_ok() = if gpu_pictures() {
                    format!("GPU: {} (SpeedHQ in {:?})", g.name, g.precision())
                } else {
                    format!("GPU: {} (SpeedHQ decoded on the CPU)", g.name)
                };
                Some(g)
            }
            Err(e) => {
                idn_open_protocol::log::log(&format!("switcher: compositing on the CPU ({e})"));
                *sh.compositor.lock_ok() = format!("CPU ({e})");
                *sh.gpu_hand.lock_ok() = None;
                None
            }
        }
    } else {
        None
    };
    // Scenes drawn on the compositor's GPU when there is one (§S.12 stage 1e): those in
    // use by their own thread, a mix's at its size here (each drawer's buffers grow only
    // as it draws).
    #[allow(unused_mut)]
    let (mut scene_drawer, mut worker_drawer) = (SceneDrawer::default(), SceneDrawer::default());
    #[cfg(feature = "gpu")]
    if let Some(g) = gpu.as_ref()
        && !idn_common::switches::on("IDN_SWITCHER_SCENES_CPU")
    {
        scene_drawer.gpu = Some(g.scene_drawer());
        worker_drawer.gpu = Some(g.scene_drawer());
        sh.compositor.lock_ok().push_str(" · scenes on the GPU");
    }
    // This run of the clock (one more after each restart): the scene worker of an earlier
    // run stops when it sees a later one (else each restart left one more drawing).
    let run = sh.clock_runs.fetch_add(1, Ordering::Relaxed) + 1;
    {
        let shared = Arc::downgrade(sh);
        if let Err(e) = std::thread::Builder::new()
            .name("idn-switcher-sc".into())
            .spawn(move || scene_worker(&shared, worker_drawer, run))
        {
            idn_open_protocol::log::log(&format!("switcher: no thread for scenes ({e})"));
        }
    }
    // The multiview drawn on a thread of its own: its tiles (each input's newest picture
    // scaled to its tile) took 30-140 ms of a 2160p frame on this one, the program's frames
    // late meanwhile (lab 381). One waiting at most: one that comes while it draws goes.
    let mv_jobs = {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MultiviewJob>(1);
        let shared = Arc::downgrade(sh);
        let _ = std::thread::Builder::new()
            .name("idn-switcher-mv".into())
            .spawn(move || multiview_worker(&shared, rx));
        tx
    };
    // Inputs' table filters on the same device (§S.23; `IDN_SWITCHER_FILTERS_CPU=1`: on
    // the CPU as before).
    #[cfg(feature = "gpu")]
    if let Some(g) = gpu.as_ref() {
        crate::gpu_filter::install(g.filter_gpu());
    }
    // Shown for an empty bus (made once: 12 MB a frame at 2160p before, on the CPU path).
    let black = Arc::new(Frame::black(w, h));
    // The compositor's picture regions made now for the largest picture it takes (the
    // program's size, with alpha), by composing one: made when a full-size picture first
    // came (a scene, a camera going from its tile to full size), about 300 MB at 2160p
    // held that frame for 110 ms.
    #[cfg(feature = "gpu")]
    if let Some(g) = gpu.as_mut() {
        let mut keyed = Frame::black(w, h);
        keyed.a = Some(vec![255; w * h]);
        let layer = crate::gpu::Layer::fitted(Arc::new(keyed), w, h);
        let _ = g.compose(&crate::gpu::Scene {
            width: w,
            height: h,
            program: layer,
            transition: None,
            ftb: 0.0,
            pattern: None,
            stinger: None,
        });
    }
    // Pipelined: the last frame's sound, its stamps (timecode, timestamp) and its sound's
    // offset from them, held for it.
    let mut held_audio: Option<(AudioBlock, i64, i64, i64)> = None;
    // On the GPU, the program's own picture sent as it is (no compositing): held a frame,
    // as the compositor's frames come out a frame late.
    #[cfg(feature = "gpu")]
    let mut held_direct: Option<Composed> = None;
    let (mut fps_at, mut fps_seq) = (Instant::now(), BTreeMap::<u32, u64>::new());
    // The program's planes as sent, used again once the library is done with them.
    let mut pool: Vec<Arc<Vec<Plane>>> = Vec::new();
    // The planes made from the program's last shared picture (reused while it is shown).
    let mut shared_planes: Option<(std::sync::Weak<Frame>, Arc<Vec<Plane>>)> = None;
    // The mixes (§S.15).
    let mut mixes = Mixes::default();
    // The preview output (§S.25).
    let mut preview = PreviewOut::default();
    // The clock starts once the GPU is ready (its making counted as frames late).
    let start = Instant::now();
    *sh.frame_clock.lock_ok() = Some((start, period));
    while !sh.stop.load(Ordering::Relaxed) {
        // In 64 bits (`period * k as u32` wraps after 2³² frames).
        let due = start + Duration::from_nanos(period.as_nanos() as u64 * k);
        let now = Instant::now();
        if let Some(d) = due.checked_duration_since(now) {
            std::thread::sleep(d);
        } else if now - due > period * 3 {
            // Far behind (a stall): skip ahead rather than rush.
            let skip = ((now - due).as_nanos() / period.as_nanos()) as u64;
            sh.late.fetch_add(skip, Ordering::Relaxed);
            k += skip;
            // The sound isn't skipped: the next frame carries what these frames would
            // have (up to a second). It was, and every stall left a gap in the program's
            // sound (85 gaps of 40–146 ms in 30 s were seen).
            samples_sent =
                samples_sent.max((rate() as u64 * k * den / num).saturating_sub(rate() as u64));
            continue;
        }
        let t0 = Instant::now();
        // This frame's stamps: the tick's own time, not the moment it is sent, so that a
        // late frame clock never shifts pictures against their sound.
        let lag = (t0.saturating_duration_since(due).as_nanos() / 100) as i64;
        let stamps = (
            sh.program_out.timecode_now() - lag,
            idn_open_protocol::api::timestamp_now() - lag,
        );
        let s = {
            let mut s = sh.state.lock_ok();
            let mut moved = false;
            // A stinger runs for its clip's length.
            let stinger = match s.kind {
                Kind::Stinger(n) => stinger_of(&s, n),
                _ => None,
            };
            let duration = stinger.map_or(s.duration, |st| st.duration);
            if s.running {
                s.position += dt / duration.max(0.01);
                moved = true;
            }
            // A stinger's cut point (run or by T-bar): program and preview swap beneath it.
            if let Some(st) = stinger
                && !s.cut_done
                && s.position > 0.0
                && s.position * st.duration >= st.cut
            {
                let st = &mut *s;
                std::mem::swap(&mut st.program, &mut st.preview);
                st.cut_done = true;
                moved = true;
            }
            if s.running && s.position >= 1.0 {
                complete(&mut s);
            }
            for m in s.mixes.values_mut() {
                if m.running {
                    m.position += dt / m.duration.max(0.01);
                    if m.position >= 1.0 {
                        complete_mix(m);
                    }
                    moved = true;
                }
            }
            // Scene layers fading in or out (§S.12): the state moves with them.
            if !sh.layer_fades.lock_ok().is_empty() {
                moved = true;
            }
            let ftb_target = if s.ftb { 1.0 } else { 0.0 };
            if s.ftb_level != ftb_target {
                let step = dt / 1.0;
                s.ftb_level = if ftb_target > s.ftb_level {
                    (s.ftb_level + step).min(1.0)
                } else {
                    (s.ftb_level - step).max(0.0)
                };
                moved = true;
            }
            if moved {
                bump(&mut s, &sh.changed);
            }
            s.clone()
        };
        let inputs = sh.inputs.lock_ok().clone();
        // (Scenes in use are drawn by their own thread: `scene_worker`; a mix's here.)
        let scene_defs = effective_scenes(sh);
        // The buses as composed: a scene that passes one input through is that input
        // (§S.12); the state `s` keeps the scenes (tally, sound, triggers).
        let rs = {
            let mut r = s.clone();
            r.program = resolve_bus(r.program, &scene_defs, &inputs);
            r.preview = resolve_bus(r.preview, &scene_defs, &inputs);
            for m in r.mixes.values_mut() {
                if m.follow {
                    // The program's buses and transition, as they are this frame.
                    (m.program, m.preview) = (r.program, r.preview);
                    (m.kind, m.duration, m.position, m.running) =
                        (r.kind, r.duration, r.position, r.running);
                    continue;
                }
                m.program = resolve_bus(m.program, &scene_defs, &inputs);
                m.preview = resolve_bus(m.preview, &scene_defs, &inputs);
            }
            r
        };
        let pic = |id: Option<u32>| -> Arc<Frame> {
            id.and_then(|i| inputs.get(&i))
                .and_then(|slot| fitted(slot, w, h))
                .unwrap_or_else(|| black.clone())
        };
        // On the GPU: this frame starts, the one before comes out.
        #[allow(unused_mut)]
        let mut composed: Option<Option<Composed>> = None;
        #[cfg(feature = "gpu")]
        if let Some(g) = gpu.as_mut() {
            sh.views_on_gpu.store(true, Ordering::Relaxed);
            // On a GPU of its own memory, the program is kept for receivers on this machine
            // only while they take it there (§E.27).
            g.share_program = sh.program_out.wants_gpu_pictures();
            // Not read back while nothing here or elsewhere wants its samples in memory
            // (no receiver over the network or reading memory, no recording or stream, no
            // multiview): §E.27.
            let multiview_idle = sh.multiview_out.lock_ok().is_none()
                && Instant::now() >= *sh.multiview_wanted.lock_ok();
            // (An HDR program always: it is made HDR from its samples.)
            let wanted = sh.program_out.samples_wanted() || sh.program_hdr.is_some();
            // (Not only while it is shared there: with nothing taking it at all, a 2160p
            // program was read back each frame for nobody, 6-12 ms of the frame, lab 350.)
            // The multiview takes its program tile shrunk on the GPU (`program_small`):
            // it needs none of it here either.
            g.gpu_only = !wanted;
            // Wanted only by receivers reading this machine's shared memory: read back
            // straight into it, not into this memory first (§E.26).
            // `IDN_SWITCHER_RING_READ=0`: into this memory as before (to compare).
            // Not while a recorder takes the program on the GPU: read into the ring on one
            // memory, the program is neither there nor in this memory for it (lab 348-A).
            let ring_only = wanted
                && sh.program_hdr.is_none()
                && multiview_idle
                && !sh.program_out.samples_wanted_off_ring()
                && !sh.program_out.gpu_taps_wanting()
                && idn_common::switches::on("IDN_SWITCHER_RING_READ");
            if ring_only != g.ring_only {
                idn_open_protocol::log::log(if ring_only {
                    "switcher: the program read back straight into shared memory (only receivers on this machine want it in memory)"
                } else {
                    "switcher: the program read back into this memory again"
                });
            }
            g.ring_only = ring_only;
            // The program's own picture, whole at its size and without alpha (a file, an
            // opaque scene), with nothing on it: sent as it is, a frame late as the
            // compositor's are, the compositor's last frame read out first. Composed, a
            // 2160p scene went up and the program came back: 15 ms of the frame and
            // 50 MB of copies.
            // (Not for an HDR program: it is composed, to be composed in HDR too.)
            let direct = (bare(&rs) && sh.program_hdr.is_none())
                .then(|| rs.program.and_then(|i| inputs.get(&i)))
                .flatten()
                .and_then(|sl| sl.latest.lock_ok().clone())
                .filter(|p| (p.width, p.height) == (w, h) && p.a.is_none() && !p.deferred());
            let mut f = recycled(&mut pool);
            let made = match direct {
                Some(p) => g.take_last(&mut f).map(|got| {
                    let last = if got {
                        Some(Composed::Owned(f))
                    } else {
                        held_direct.take()
                    };
                    held_direct = Some(Composed::Shared(p));
                    sh.gpu_parts
                        .iter()
                        .for_each(|t| t.store(0, Ordering::Relaxed));
                    last
                }),
                None => {
                    let views = sh.views.lock_ok().clone();
                    let scene = gpu_scene(&rs, &inputs, w, h, &black, &views, due);
                    g.compose_next(&scene, &mut f).map(|ready| {
                        for (slot, ms) in sh
                            .gpu_parts
                            .iter()
                            .zip([g.timing.0, g.timing.1, g.timing.2])
                        {
                            slot.store((ms * 1000.0) as u64, Ordering::Relaxed);
                        }
                        // The first composed after direct ones: the last of those.
                        let last = held_direct.take();
                        if ready {
                            Some(Composed::Owned(f))
                        } else {
                            last
                        }
                    })
                }
            };
            match made {
                Ok(out) => composed = Some(out),
                // One frame's pictures too large for the GPU's buffers together: that frame
                // on the CPU, the GPU kept (bug run L4-4: off it for good), said once.
                Err(e) if e.starts_with(crate::TOO_LARGE) => {
                    static SAID: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !SAID.swap(true, Ordering::Relaxed) {
                        idn_open_protocol::log::log(&format!(
                            "switcher: {e}; such frames composed on the CPU"
                        ));
                    }
                    // In the pipeline's order, as a direct picture is (bug run M7-2: the
                    // GPU's last frame came out after this one): its last frame read out
                    // now, this one composed on the CPU and held for the next.
                    let mut last = recycled(&mut pool);
                    let got = g.take_last(&mut last).unwrap_or(false);
                    let prev = if got {
                        Some(Composed::Owned(last))
                    } else {
                        held_direct.take()
                    };
                    held_direct = Some(cpu_compose(&rs, &inputs, w, h, &pic));
                    composed = Some(prev);
                }
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "switcher: the GPU failed ({e}); compositing on the CPU"
                    ));
                    *sh.compositor.lock_ok() = format!("CPU (the GPU failed: {e})");
                    sh.program_out.use_gpu(None);
                    for o in sh.mix_outs.lock_ok().values() {
                        o.use_gpu(None);
                    }
                    mixes.gpu.clear();
                    // Inputs on that GPU too, as at a failed start: inputs opened from now
                    // take none, and those open go on in memory (else after a lost device
                    // every network input is black).
                    *sh.gpu_hand.lock_ok() = None;
                    *sh.gpu_seed.lock_ok() = Some(Err(format!("the GPU failed earlier: {e}")));
                    for slot in sh.inputs.lock_ok().values() {
                        if let Some(i) = slot.input.lock_ok().as_ref() {
                            i.stop_using_gpu();
                        }
                    }
                    gpu = None;
                    held_audio = None;
                    // Virtual shots' workers make whole pictures again (they stayed small,
                    // a frame in four, on the CPU's program).
                    sh.views_on_gpu.store(false, Ordering::Relaxed);
                }
            }
        }
        let pipelined = composed.is_some();
        let t_compose = Instant::now();
        let out = match composed {
            Some(f) => f,
            None => Some(cpu_compose(&rs, &inputs, w, h, &pic)),
        };
        let n = (rate() as u64 * (k + 1) * den / num - samples_sent) as usize;
        samples_sent += n as u64;
        let Sound {
            channels,
            offset,
            mix_sound,
        } = sound.frame(sh, &s, &inputs, &scene_defs, n, stamps);
        let audio = AudioBlock {
            sample_rate: rate(),
            channels,
        };
        // Pipelined, the frame that came out is the last one: so is its sound.
        let (audio, stamps, offset) = if pipelined {
            match held_audio.replace((audio, stamps.0, stamps.1, offset)) {
                Some((a, tc, ts, off)) => (Some(a), (tc, ts), off),
                None => (None, stamps, 0),
            }
        } else {
            (Some(audio), stamps, offset)
        };
        if let Some(po) = &sh.preview_out {
            // Its sound: the preview scene's loudest input's (§S.25).
            let sound = s
                .preview
                .and_then(|p| sound_of(p, &scene_defs, &inputs).first().map(|h| h.0));
            preview.frame(po, sh, &rs, &inputs, &pic, n, sound);
        }
        let t_mv = Instant::now();
        // The multiview, about 15 times a second, while it's sent or looked at.
        let every = (num / den / 15).max(1);
        let wanted = multiview_wanted(sh);
        if let Some(out) = out.as_ref()
            && wanted
            && k.is_multiple_of(every)
        {
            // The program at its tile's size: on the GPU, shrunk there and read back (the
            // whole 2160p program was read back every frame for it, lab 381); else scaled
            // here (the multiview's thread gets no 4K picture to copy). (Its largest
            // program tile's; none: a small one, for the API's look at it.)
            let layout = layout_of(sh, &inputs);
            let (tw, th) = layout
                .iter()
                .filter(|t| t.kind == crate::multiview::TileKind::Program)
                .map(|t| crate::multiview::picture_size(t.pixels()))
                .max_by_key(|(a, b)| a * b)
                .unwrap_or((w / 4, h / 4));
            let (tw, th) = (tw.max(2) & !1, th.max(1));
            #[allow(unused_mut)]
            let mut small = None;
            // (Read back on the multiview's thread: waited for here, it lengthened the
            // frame.)
            #[cfg(feature = "gpu")]
            if out.y.is_empty()
                && let Some(g) = gpu.as_mut()
                && let Some((pic, hand)) = g.program_small_on_gpu(tw, th)
            {
                small = Some(MultiviewProgram::OnGpu(pic, hand, (tw, th)));
            }
            if small.is_none() && !out.y.is_empty() {
                small = Some(MultiviewProgram::Here(out.fitted(tw, th)));
            }
            if let Some(program) = small {
                let _ = mv_jobs.try_send(MultiviewJob {
                    state: s.clone(),
                    inputs: inputs.clone(),
                    scenes: scene_defs.clone(),
                    layout,
                    program,
                    size: (w, h),
                });
            }
        }
        let t_send = Instant::now();
        // The program, its planes moved (not copied) into a set the pool keeps for again.
        // The sound on its own, stamped with its own time (above).
        if let Some(a) = audio {
            // The mixes carry the stream's sound too, or their own feed (§S.26).
            let outs: Vec<(u32, Arc<Output>)> = sh
                .mix_outs
                .lock_ok()
                .iter()
                .map(|(id, o)| (*id, o.clone()))
                .collect();
            for (id, o) in &outs {
                let block = mix_sound.get(id).cloned().unwrap_or_else(|| a.clone());
                let _ = o.send_audio_at(block, Some(stamps.0 + offset), stamps.1 + offset);
            }
            let _ = sh
                .program_out
                .send_audio_at(a, Some(stamps.0 + offset), stamps.1 + offset);
        }
        if let Some(out) = out {
            // An HDR program composed on the GPU (P216), taken before the program's 8-bit
            // planes borrow the GPU below.
            #[cfg(feature = "gpu")]
            let hdr_from_gpu = if sh.program_hdr.is_some() {
                gpu.as_mut().and_then(|g| g.take_hdr())
            } else {
                None
            };
            #[cfg(not(feature = "gpu"))]
            let hdr_from_gpu: Option<Vec<u16>> = None;
            // Composed on the GPU and still there: shared from there too (§E.27). Passed
            // through as it came: uploaded once, while a receiver here takes it on the GPU.
            // Left for the shared memory to read straight from the GPU's mapped memory.
            #[cfg(feature = "gpu")]
            let (on_gpu, ring) = match (&out, gpu.as_mut()) {
                (Composed::Owned(_), Some(g)) => {
                    let g: &crate::gpu::Gpu = g;
                    (g.program_on_gpu(), g.read_pending().then_some(g))
                }
                (Composed::Shared(p), Some(g)) if sh.program_out.wants_gpu_pictures() => {
                    (g.upload_program(p), None)
                }
                _ => (None, None),
            };
            let planes = match out {
                Composed::Owned(f) => to_planes(f, &mut pool),
                // The same picture as last frame: the same planes.
                Composed::Shared(f) => match &shared_planes {
                    Some((last, p)) if last.upgrade().is_some_and(|l| Arc::ptr_eq(&l, &f)) => {
                        p.clone()
                    }
                    _ => {
                        let p = Arc::new((*f).clone().into_planes());
                        shared_planes = Some((Arc::downgrade(&f), p.clone()));
                        p
                    }
                },
            };
            let mut f = planar(planes, w, h);
            (f.timecode, f.timestamp) = (Some(stamps.0), Some(stamps.1));
            // Closed captions with this picture (§S.29), for the streams' encoders.
            {
                let mut c = sh.captions.lock_ok();
                if c.on {
                    let after = c.clear_after;
                    let cc = c.captioner.next_frame(num as f64 / den as f64, after);
                    sh.program_out.caption_next(cc);
                }
            }
            // An HDR program: made HDR and sent on its own thread (the frame clock never
            // waits for the conversion).
            if let Some(hdr) = &sh.program_hdr {
                // Composed in HDR on the GPU: sent as it came back (P216).
                if let Some(p216) = hdr_from_gpu {
                    sh.hdr_path.store(1, Ordering::Relaxed);
                    hdr.send_p216(p216, (w, h), (f.timecode, f.timestamp));
                } else {
                    sh.hdr_path.store(2, Ordering::Relaxed);
                    #[cfg(feature = "gpu")]
                    let f = match ring {
                        Some(g) if f.planes.first().is_some_and(|p| p.data.is_empty()) => {
                            let mut f = f;
                            let mut planes: Vec<Plane> = (*f.planes).clone();
                            for p in &mut planes {
                                p.data = vec![0; p.width * p.height];
                            }
                            let mut bufs: Vec<&mut [u8]> =
                                planes.iter_mut().map(|p| &mut p.data[..]).collect();
                            g.read_into(&mut bufs);
                            f.planes = Arc::new(planes);
                            f
                        }
                        _ => f,
                    };
                    #[cfg(feature = "gpu")]
                    let _ = on_gpu;
                    hdr.send(f);
                }
            } else {
                // (A frame left on the GPU has planes with sizes only: sent so, or not at all.)
                #[cfg(feature = "gpu")]
                let _ = match (on_gpu, ring) {
                    (Some((buffer, from)), Some(g)) => {
                        sh.program_out
                            .send_planes_gpu_filled(f, buffer, &from, &mut |p| g.read_into(p))
                    }
                    (None, Some(g)) => sh
                        .program_out
                        .send_planes_filled(f, &mut |p| g.read_into(p)),
                    (on_gpu, None) => match on_gpu {
                        Some((buffer, from))
                            if f.planes.first().is_some_and(|p| p.data.is_empty()) =>
                        {
                            sh.program_out.send_planes_gpu_sizes(f, buffer, &from)
                        }
                        _ if f.planes.first().is_some_and(|p| p.data.is_empty()) => Ok(()),
                        Some((buffer, from)) => sh.program_out.send_planes_gpu(f, buffer, &from),
                        None => sh.program_out.send_planes(f),
                    },
                };
                #[cfg(not(feature = "gpu"))]
                let _ = sh.program_out.send_planes(f);
            }
        }
        mixes.frame(
            sh,
            &rs,
            &inputs,
            &scene_defs,
            &mut scene_drawer,
            (due, stamps),
        );
        iso_frame(&inputs, stamps);
        tally(sh, &s, &inputs, &scene_defs);
        let took = t0.elapsed().as_micros() as u64;
        sh.frame_us.store(took, Ordering::Relaxed);
        let us = |a: Instant, b: Instant| b.saturating_duration_since(a).as_micros() as u64;
        // Composing until the sound; the multiview; the sends (the preview's, the
        // program's) and the rest.
        sh.parts[0].store(us(t0, t_compose), Ordering::Relaxed);
        sh.parts[1].store(us(t_mv, t_send), Ordering::Relaxed);
        sh.parts[2].store(
            took.saturating_sub(us(t0, t_compose) + us(t_mv, t_send)),
            Ordering::Relaxed,
        );
        // The inputs' picture rates, once a second.
        if fps_at.elapsed() >= Duration::from_secs(1) {
            let secs = fps_at.elapsed().as_secs_f64();
            for (id, slot) in &inputs {
                let seq = slot.seq.load(Ordering::Relaxed);
                let before = fps_seq.insert(*id, seq).unwrap_or(seq);
                slot.fps.store(
                    ((seq - before) as f64 / secs * 100.0).round() as u32,
                    Ordering::Relaxed,
                );
            }
            fps_at = Instant::now();
        }
        sh.worst
            .lock_ok()
            .note(took, t0.saturating_duration_since(due).as_micros() as u64);
        sh.frame_beat.beat();
        // For trying the restart (lab): one panic about 5 s in, with `IDN_CLOCK_CRASH_TEST`.
        if crash_test && k >= 5 * num / den && sh.frame_beat.restarts() == 0 {
            panic!("IDN_CLOCK_CRASH_TEST: the frame clock stops, as asked");
        }
        k += 1;
    }
}

/// ISO recording: each tapped input's newest picture, stamped as this program frame
/// (`stamps`: its files line up with the program's), with its own sound since the last
/// frame.
pub(super) fn iso_frame(inputs: &BTreeMap<u32, Arc<Slot>>, stamps: (i64, i64)) {
    for slot in inputs.values() {
        let mut iso = slot.iso.lock_ok();
        if iso.is_empty() {
            continue;
        }
        let Some(pic) = slot.latest.lock_ok().clone() else {
            continue;
        };
        let audio = std::mem::take(&mut *slot.iso_audio.lock_ok());
        let rate = match slot.iso_rate.load(Ordering::Relaxed) {
            0 => rate(),
            r => r,
        };
        // A tap that is behind skips the picture but not the sound: kept, it goes with
        // the next one (it went with the picture: gaps in the ISO's sound and drift),
        // up to 2 s.
        iso.retain_mut(|(_, tx, carry)| {
            let mut sound = std::mem::take(carry);
            if sound.len() < audio.len() {
                sound.resize(audio.len(), Vec::new());
            }
            for (c, a) in sound.iter_mut().zip(&audio) {
                c.extend_from_slice(a);
            }
            match tx.try_send((pic.clone(), sound, rate, stamps.0, stamps.1)) {
                Ok(()) => true,
                Err(std::sync::mpsc::TrySendError::Full((_, mut sound, ..))) => {
                    let most = rate as usize * 2;
                    for c in &mut sound {
                        let over = c.len().saturating_sub(most);
                        c.drain(..over);
                    }
                    *carry = sound;
                    true
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
            }
        });
    }
}

/// The monitor's share of a frame (§S.24): `stream` is the stream's sound as it goes out
/// (`n` frames); `from` the inputs whose lanes feed the monitor now.
#[allow(clippy::too_many_arguments)]
pub(super) fn monitor_frame(
    sh: &Shared,
    s: &State,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    stream: &[Vec<f32>],
    n: usize,
    latency: u32,
    from: &mut Vec<(u32, u32)>,
) {
    let m = &s.monitor;
    let mut out = sh.monitor_out.lock_ok();
    out.stream.add(stream);
    #[cfg(feature = "card")]
    let card = sh.monitor_card.lock_ok().as_ref().map(|c| c.lane());
    #[cfg(not(feature = "card"))]
    let card: Option<Arc<Lane>> = None;
    let listening = out.listened.is_some_and(|t| t.elapsed() < LISTEN_KEEP);
    let received = sh.monitor_net.receivers() > 0;
    let active = card.is_some()
        || listening
        || received
        || out.metered.is_some_and(|t| t.elapsed() < LISTEN_KEEP);
    // The inputs heard instead of the stream (nobody listening or looking: none, no lanes).
    let want: Vec<(u32, u32)> = if !active {
        Vec::new()
    } else if !m.solo.is_empty() {
        m.solo.clone()
    } else if m.preview {
        // The inputs the preview's scene carries the sound of (§S.12).
        s.preview
            .map(|p| {
                sound_of(p, scenes, inputs)
                    .into_iter()
                    .map(|h| h.0)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    }
    .into_iter()
    .filter_map(|id| {
        let slot = inputs.get(&id)?;
        Some((id, latency + slot.audio_offset_ms.load(Ordering::Relaxed)))
    })
    .collect();
    follow(&sh.monitor_mixer, inputs, |sl| &sl.monitor_lane, want, from);
    if !active {
        out.monitor.add(&[]);
        return;
    }
    let solo_or_preview = !m.solo.is_empty() || m.preview;
    let mut ch: Vec<Vec<f32>> = if solo_or_preview {
        let mut buf = vec![0f32; n * 2];
        sh.monitor_mixer.fill(&mut buf);
        (0..2)
            .map(|c| buf.iter().skip(c).step_by(2).copied().collect())
            .collect()
    } else {
        stream.to_vec()
    };
    let g = if m.mute {
        0.0
    } else {
        10f32.powf(m.volume_db / 20.0)
    };
    for c in &mut ch {
        for v in c.iter_mut() {
            *v *= g;
        }
    }
    out.monitor.add(&ch);
    if let Some(lane) = card {
        lane.push(rate(), &ch, &[(0, 0), (1, 1)]);
    }
    if received {
        let _ = sh.monitor_net.send_audio(
            AudioBlock {
                sample_rate: rate(),
                channels: ch.clone(),
            },
            None,
        );
    }
    if listening {
        for i in 0..n {
            for c in &ch {
                out.ring.push_back((c[i].clamp(-1.0, 1.0) * 32767.0) as i16);
            }
        }
        let extra = out.ring.len().saturating_sub(listen_ring() * 2);
        out.ring.drain(..extra);
        out.end += n as u64;
        sh.monitor_ready.notify_all();
    }
}

/// The inputs whose sound `id` carries, each at its level, loudest first (§S.12): itself;
/// a virtual shot's or preset's camera; a scene's inputs (nested scenes' too), each at its
/// item's volume (none while muted or hidden), times the scene's own level inside
/// another. An input in a scene more than once is heard once, at its loudest.
pub(super) fn sound_of(
    id: u32,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
) -> Vec<(u32, f32)> {
    fn walk(
        id: u32,
        gain: f32,
        depth: usize,
        scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
        inputs: &BTreeMap<u32, Arc<Slot>>,
        out: &mut BTreeMap<u32, f32>,
    ) {
        if depth > crate::scene::MAX_DEPTH + 2 {
            return;
        }
        if let Some(items) = scenes.get(&id) {
            for it in items {
                if let crate::scene::ItemSource::Input(src) = it.source
                    && src != id
                {
                    walk(src, gain * it.sound_gain(), depth + 1, scenes, inputs, out);
                }
            }
            return;
        }
        match inputs.get(&id).map(|s| &s.source) {
            Some(Source::Ptz { camera, .. }) | Some(Source::Virtual { input: camera, .. }) => {
                walk(*camera, gain, depth + 1, scenes, inputs, out)
            }
            Some(_) => {
                let e = out.entry(id).or_insert(0.0);
                *e = e.max(gain);
            }
            None => {}
        }
    }
    let mut out = BTreeMap::new();
    walk(id, 1.0, 0, scenes, inputs, &mut out);
    let mut v: Vec<(u32, f32)> = out.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

/// A frame whose planes' memory is a set the library has finished with (none: new).
// (Used by the GPU path only.)
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub(super) fn recycled(pool: &mut Vec<Arc<Vec<Plane>>>) -> Frame {
    let mut f = Frame {
        width: 0,
        height: 0,
        y: Vec::new(),
        cb: Vec::new(),
        cr: Vec::new(),
        a: None,
        coefs: None,
        gpu: None,
    };
    if let Some(i) = pool.iter().position(|p| Arc::strong_count(p) == 1)
        && let Ok(planes) = Arc::try_unwrap(pool.swap_remove(i))
    {
        let mut it = planes.into_iter().map(|p| p.data);
        (f.y, f.cb, f.cr) = (
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
        );
    }
    f
}

pub(super) fn planar(planes: Arc<Vec<Plane>>, w: usize, h: usize) -> PlanarFrame {
    PlanarFrame {
        layout: Fourcc::Shq2,
        width: w,
        height: h,
        planes,
        timestamp: None,
        timecode: None,
        audio: None,
        coded: None,
    }
}

pub(super) fn cpu_compose(
    s: &State,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    w: usize,
    h: usize,
    pic: &dyn Fn(Option<u32>) -> Arc<Frame>,
) -> Composed {
    let program = pic(s.program);
    // A stinger's clip over the bus while it runs (§S.14).
    let stinger = match s.kind {
        Kind::Stinger(n) if s.running || s.position > 0.0 => stinger_of(s, n)
            .and_then(|st| inputs.get(&st.input))
            .and_then(|sl| fitted(sl, w, h)),
        _ => None,
    };
    let mixing = s.position > 0.0 && s.kind != Kind::Cut && !matches!(s.kind, Kind::Stinger(_));
    // Nothing on it: the program's picture as it is (a cut copied the whole picture every
    // frame: 8 ms at 2160p).
    if !mixing && stinger.is_none() && s.ftb_level <= 0.0 {
        return Composed::Shared(program);
    }
    let mut out = if mixing {
        // A luma wipe by its pattern's picture (a mix while it has none).
        let pattern = match s.kind {
            Kind::Luma {
                pattern,
                soft,
                invert,
            } => inputs
                .get(&pattern)
                .and_then(|sl| fitted(sl, w, h))
                .map(|p| (p, soft, invert)),
            _ => None,
        };
        match pattern {
            Some((p, soft, invert)) => {
                compose::luma(&program, &pic(s.preview), &p, soft, invert, s.position)
            }
            None => compose::transition(&program, &pic(s.preview), s.kind, s.position),
        }
    } else {
        (*program).clone()
    };
    if let Some(clip) = &stinger {
        compose::key(&mut out, clip, 1.0);
    }
    compose::to_black(&mut out, s.ftb_level);
    Composed::Owned(out)
}

/// The frame for the GPU (§S.9 stage 3): the inputs' pictures as they arrived, placed.
#[cfg(feature = "gpu")]
pub(super) fn gpu_scene(
    s: &State,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    w: usize,
    h: usize,
    black: &Arc<Frame>,
    views: &BTreeMap<u32, ViewRun>,
    at: Instant,
) -> crate::gpu::Scene {
    use crate::gpu::{Layer, Scene};
    // As it arrived, unless it is too large for the GPU: then fitted to the program here.
    let raw = |id: Option<u32>| {
        let slot = inputs.get(&id?)?;
        let pic = shown(slot)?;
        if crate::gpu::Gpu::fits(&pic) {
            Some(pic)
        } else {
            fitted(slot, w, h)
        }
    };
    // A virtual shot on a bus: its camera's picture as it arrived, cropped to the view at
    // this frame's time by the GPU (its worker cut and scaled a whole picture on the CPU
    // each frame: about three cores at 2160p).
    let shot = |id: Option<u32>| -> Option<Layer> {
        let slot = inputs.get(&id?)?;
        let Source::Virtual { input: cam, .. } = slot.source else {
            return None;
        };
        let pic = shown(inputs.get(&cam)?)?;
        if !crate::gpu::Gpu::fits(&pic) {
            return None;
        }
        let c = views
            .get(&slot.id)?
            .now_on(pic.width, pic.height, w, h, at)
            .crop(pic.width, pic.height, w, h);
        Some(Layer {
            pic,
            rect: (0, 0, w, h),
            crop: (c.0 as f32, c.1 as f32, c.2 as f32, c.3 as f32),
        })
    };
    let bus = |id: Option<u32>| {
        shot(id).unwrap_or_else(|| Layer::fitted(raw(id).unwrap_or_else(|| black.clone()), w, h))
    };
    let transition =
        (s.position > 0.0 && s.kind != Kind::Cut && !matches!(s.kind, Kind::Stinger(_)))
            .then(|| (bus(s.preview), s.kind, s.position));
    // A luma wipe's pattern; a stinger's clip while it runs (§S.14).
    let pattern = match s.kind {
        Kind::Luma { pattern, .. } => raw(Some(pattern)).map(|p| Layer::fitted(p, w, h)),
        _ => None,
    };
    let stinger = match s.kind {
        Kind::Stinger(n) if s.running || s.position > 0.0 => stinger_of(s, n)
            .and_then(|st| raw(Some(st.input)))
            .map(|p| Layer::fitted(p, w, h)),
        _ => None,
    };
    Scene {
        width: w,
        height: h,
        program: bus(s.program),
        transition,
        ftb: s.ftb_level,
        pattern,
        stinger,
    }
}

/// Nothing on the program but its picture: no transition under way, nothing keyed over
/// it, no fade to black.
#[cfg(feature = "gpu")]
pub(super) fn bare(s: &State) -> bool {
    let moving = s.position > 0.0 && s.kind != Kind::Cut;
    let stinger = matches!(s.kind, Kind::Stinger(_)) && (s.running || s.position > 0.0);
    !moving && !stinger && s.ftb_level <= 0.0
}

/// The compositor's next frame whose making (`lead` before it) is still ahead of `now`:
/// (when the frame is due, when to make its picture). Without the clock yet, a period on.
pub(super) fn next_frame(
    clock: Option<(Instant, Duration)>,
    period: Duration,
    lead: Duration,
    now: Instant,
) -> (Instant, Instant) {
    let Some((start, p)) = clock.filter(|(_, p)| !p.is_zero()) else {
        return (now + period, now + period);
    };
    let since = (now + lead).saturating_duration_since(start).as_nanos() as u64;
    let k = since / p.as_nanos() as u64 + 1;
    let due = start + Duration::from_nanos(p.as_nanos() as u64 * k);
    (due, due - lead)
}

/// Whether a virtual shot's own picture is needed whole, every frame: where the CPU takes
/// it (a drawn scene, a stinger or pattern, a mix, an ISO, a delay, the preview output),
/// rather than only the GPU's bus (which crops the camera itself, through a scene that
/// passes it through) and the multiview.
pub(super) fn wanted_whole(sh: &Shared, slot: &Slot) -> bool {
    let id = slot.id;
    if !slot.iso.lock_ok().is_empty() {
        return true;
    }
    // One lock at a time (the inputs', then the state's).
    let delayed = sh
        .inputs
        .lock_ok()
        .values()
        .any(|i| matches!(i.source, Source::Delay { input, .. } if input == id));
    if delayed {
        return true;
    }
    let (scenes, inputs) = (effective_scenes(sh), sh.inputs.lock_ok().clone());
    // In a scene on a bus that is drawn (not passed through): drawn there from it.
    let drawn = {
        let s = sh.state.lock_ok();
        let mut buses = vec![s.program, s.preview];
        buses.extend(s.mixes.values().flat_map(|m| [m.program, m.preview]));
        buses.into_iter().any(|b| {
            resolve_bus(b, &scenes, &inputs).is_some_and(|r| {
                r != id
                    && scenes.contains_key(&r)
                    && shown_through(Some(r), &inputs, &scenes).contains(&id)
            })
        })
    };
    if drawn {
        return true;
    }
    let s = sh.state.lock_ok();
    let used = |x: Option<u32>| resolve_bus(x, &scenes, &inputs) == Some(id);
    s.stingers
        .iter()
        .any(|st| st.as_ref().is_some_and(|st| st.input == id))
        || matches!(s.kind, Kind::Luma { pattern, .. } if pattern == id)
        || !sh.mixes_on_gpu.load(Ordering::Relaxed)
            && s.mixes.values().any(|m| used(m.program) || used(m.preview))
        || sh.preview_out.is_some() && used(s.preview)
}

/// What the multiview's thread draws from: the state, the inputs, the scenes, the layout
/// and the program at its tile's size, as the frame clock had them.
pub(super) struct MultiviewJob {
    pub(super) state: State,
    pub(super) inputs: BTreeMap<u32, Arc<Slot>>,
    pub(super) scenes: BTreeMap<u32, Vec<crate::scene::Item>>,
    pub(super) layout: Vec<crate::multiview::TileSpec>,
    program: MultiviewProgram,
    size: (usize, usize),
}

/// The multiview's program picture: here, or shrunk on the GPU and still to be read back
/// (its size).
enum MultiviewProgram {
    Here(Frame),
    #[cfg(feature = "gpu")]
    OnGpu(
        idn_open_protocol::gpu::GpuPicture,
        idn_open_protocol::gpu::GpuHandIn,
        (usize, usize),
    ),
}

/// Draws the multiview from each job (see `frame_clock`), until the switcher goes.
fn multiview_worker(
    shared: &std::sync::Weak<Shared>,
    jobs: std::sync::mpsc::Receiver<MultiviewJob>,
) {
    let mut mv_preview: Option<PreviewTile> = None;
    while let Ok(job) = jobs.recv() {
        let Some(sh) = shared.upgrade() else { return };
        let (w, h) = job.size;
        let black = Arc::new(Frame::black(w, h));
        let pic = |id: Option<u32>| -> Arc<Frame> {
            id.and_then(|i| job.inputs.get(&i))
                .and_then(|slot| fitted(slot, w, h))
                .unwrap_or_else(|| black.clone())
        };
        let program = match &job.program {
            MultiviewProgram::Here(f) => f.clone(),
            #[cfg(feature = "gpu")]
            MultiviewProgram::OnGpu(small, hand, (tw, th)) => {
                match crate::gpu::read_small(small, hand, *tw, *th) {
                    Some(f) => f,
                    None => continue,
                }
            }
        };
        multiview_frame(&sh, &job, &program, &pic, &mut mv_preview);
    }
}
