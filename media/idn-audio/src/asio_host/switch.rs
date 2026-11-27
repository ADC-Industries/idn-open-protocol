//! The driver's buffers and its buffer switch (on the driver's own thread): the streams'
//! samples in and out, nothing that locks or allocates.

use super::*;

/// One channel's buffers as the switch sees them.
pub(super) struct Ch {
    pub(super) halves: [*mut u8; 2],
    pub(super) kind: SampleType,
    pub(super) size: usize,
}

/// The switch's state: the buffers, the streams' data callbacks (moved in while running,
/// taken back when stopped), scratch for the interleaved samples.
pub(super) struct Rt {
    pub(super) frames: usize,
    pub(super) rate: u32,
    pub(super) ins: Vec<Ch>,
    pub(super) outs: Vec<Ch>,
    pub(super) input: Option<Slot>,
    pub(super) output: Option<Slot>,
    pub(super) scratch_in: Vec<f32>,
    pub(super) scratch_out: Vec<f32>,
    pub(super) output_ready: bool,
    pub(super) drv: *mut Driver,
    pub(super) start: Instant,
}

/// The switch's state while running (one driver in the program), and its run's gate:
/// the switches in progress (stopping waits for them before the state goes) and one
/// switch at a time. A gate per run, never freed (a few bytes a run): a switch stuck past
/// a stop keeps its own, and the next runs aren't held by it (bug run 2026-11-25).
pub(super) static RT: AtomicPtr<Rt> = AtomicPtr::new(std::ptr::null_mut());
pub(super) static GATE: AtomicPtr<Gate> = AtomicPtr::new(std::ptr::null_mut());

#[derive(Default)]
pub(super) struct Gate {
    pub(super) in_switch: AtomicUsize,
    pub(super) busy: AtomicBool,
}

/// Stops the driver, waits out a switch in progress, and takes the streams back.
pub(super) fn stop(a: &mut Active) {
    if a.rt.is_null() {
        return;
    }
    let _ = a.loaded.stop();
    RT.store(std::ptr::null_mut(), Ordering::SeqCst);
    // A driver whose stop doesn't wait for its thread (or failed): its switch finishes
    // first. (Nothing in a switch waits on this thread: streams dropped there close
    // without waiting; window messages go on being handled meanwhile.)
    // SAFETY: a gate is never freed.
    let gate = unsafe { &*a.gate };
    let since = Instant::now();
    while gate.in_switch.load(Ordering::SeqCst) != 0 {
        if since.elapsed() > SWITCH_WAIT {
            // A callback stuck that long: its state let go of (not freed: it may still
            // be in it), and the driver set aside (never released or its buffers let go:
            // the switch may still write them); the streams told.
            idn_open_protocol::log::log(
                "idn-audio: an ASIO buffer switch didn't end; its driver is set aside",
            );
            a.rt = std::ptr::null_mut();
            a.poisoned = true;
            let ids: Vec<(usize, u64)> = a
                .ids
                .iter()
                .enumerate()
                .filter_map(|(k, i)| i.map(|i| (k, i)))
                .collect();
            a.tell(
                ErrorKind::StreamInvalidated,
                "the ASIO driver's buffer switch didn't end",
                None,
            );
            for (k, id) in ids {
                a.ids[k] = None;
                a.errors.retain(|(i, _)| *i != id);
            }
            return;
        }
        pump();
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = a.loaded.dispose_buffers();
    // SAFETY: made by `Box::into_raw` in `run`; no switch can reach it now.
    let rt = unsafe { Box::from_raw(a.rt) };
    a.rt = std::ptr::null_mut();
    let rt = *rt;
    if a.input.is_none() {
        a.input = rt.input;
    }
    if a.output.is_none() {
        a.output = rt.output;
    }
}

/// The host's callbacks (one set: one driver at a time).
pub(super) static CALLBACKS: Callbacks = Callbacks {
    buffer_switch: Some(buffer_switch),
    sample_rate_did_change: Some(rate_changed),
    asio_message: Some(message),
    buffer_switch_time_info: Some(switch_time_info),
};

/// Sets the buffers up for the open streams and starts the driver.
pub(super) fn run(a: &mut Active, start: Instant) -> Result<(), Error> {
    let n_in = a.input.as_ref().map_or(0, |s| s.channels);
    let n_out = a.output.as_ref().map_or(0, |s| s.channels);
    let mut infos: Vec<BufferInfo> = (0..n_in)
        .map(|c| (true, c))
        .chain((0..n_out).map(|c| (false, c)))
        .map(|(input, c)| BufferInfo {
            is_input: if input { ASIO_TRUE } else { ASIO_FALSE },
            channel_num: c as c_long,
            buffers: [std::ptr::null_mut(); 2],
        })
        .collect();
    let kind = |input: bool, c: usize| -> Result<SampleType, Error> {
        let t = a
            .loaded
            .channel_info(input, c)
            .map_err(|e| err(ErrorKind::DeviceNotAvailable, e.to_string()))?
            .sample_type;
        asio_samples::size_of(t).map(|_| t).ok_or_else(|| {
            err(
                ErrorKind::UnsupportedConfig,
                format!("sample type {t} isn't handled"),
            )
        })
    };
    let kinds: Vec<SampleType> = (0..n_in)
        .map(|c| kind(true, c))
        .chain((0..n_out).map(|c| kind(false, c)))
        .collect::<Result<_, _>>()?;
    let frames = a.frames();
    // A rate the driver changed while its notice went to an earlier run: the streams
    // told now (they were opened at the old one).
    if let Ok(r) = a.loaded.rate()
        && r.is_finite()
        && r > 0.0
        && (r - a.rate as f64).abs() > 0.5
    {
        a.rate = r.round() as u32;
        let why = format!("the driver's rate changed to {} Hz", a.rate);
        a.tell(ErrorKind::StreamInvalidated, &why, None);
    }
    // SAFETY: `infos` outlives the call; CALLBACKS is static.
    if let Err(e) = unsafe {
        a.loaded
            .create_buffers(&mut infos, frames as i64, &CALLBACKS)
    } {
        // (A driver that half made them takes the next call only after this.)
        let _ = a.loaded.dispose_buffers();
        return Err(err(ErrorKind::UnsupportedConfig, e.to_string()));
    }
    // Every buffer given (a driver saying yes without one would be read as memory).
    if infos.iter().any(|i| i.buffers.iter().any(|b| b.is_null())) {
        let _ = a.loaded.dispose_buffers();
        return Err(err(
            ErrorKind::DeviceNotAvailable,
            "the driver gave no buffer for a channel",
        ));
    }
    let chs: Vec<Ch> = infos
        .iter()
        .zip(&kinds)
        .map(|(i, k)| Ch {
            halves: [i.buffers[0].cast(), i.buffers[1].cast()],
            kind: *k,
            size: asio_samples::size_of(*k).unwrap_or(4),
        })
        .collect();
    let (ins, outs) = {
        let mut chs = chs;
        let outs = chs.split_off(n_in);
        (chs, outs)
    };
    let (in_lat, out_lat) = a.loaded.latencies().unwrap_or((0, 0));
    LATENCY[0].store(in_lat, Ordering::Relaxed);
    LATENCY[1].store(out_lat, Ordering::Relaxed);
    let frames = frames as usize;
    let rt = Box::into_raw(Box::new(Rt {
        frames,
        rate: a.rate,
        ins,
        outs,
        input: a.input.take(),
        output: a.output.take(),
        scratch_in: vec![0.0; frames * n_in],
        scratch_out: vec![0.0; frames * n_out],
        output_ready: a.loaded.output_ready(),
        drv: a.loaded.raw(),
        start,
    }));
    a.rt = rt;
    // A new run: notices from the one before are no longer this one's; its own gate,
    // there before its state.
    a.run = RUN.fetch_add(1, Ordering::SeqCst) + 1;
    a.gate = Box::into_raw(Box::default());
    GATE.store(a.gate, Ordering::SeqCst);
    RT.store(rt, Ordering::SeqCst);
    if let Err(e) = a.loaded.start() {
        stop(a);
        return Err(err(ErrorKind::DeviceNotAvailable, e.to_string()));
    }
    Ok(())
}

/// The buffer switch: half `index` is the driver's to fill in (inputs) and ours to fill
/// (outputs). Counted in progress first, so a stop waits for it; one at a time (a driver
/// calling it again from another thread meanwhile gets nothing done).
pub(super) fn process(index: usize) {
    let g = GATE.load(Ordering::SeqCst);
    if g.is_null() || index > 1 {
        return;
    }
    // SAFETY: a gate is never freed.
    let gate = unsafe { &*g };
    let was = NO_WAIT.replace(true);
    gate.in_switch.fetch_add(1, Ordering::SeqCst);
    let p = RT.load(Ordering::SeqCst);
    // This gate's run still (a run set up meanwhile has a gate of its own: its state isn't
    // counted on this one).
    if !p.is_null()
        && GATE.load(Ordering::SeqCst) == g
        && gate
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    {
        // SAFETY: the state stays until no switch of its gate is in progress (`stop`),
        // and `busy` keeps this the only reference to it.
        switch(unsafe { &mut *p }, index);
        gate.busy.store(false, Ordering::Release);
    }
    gate.in_switch.fetch_sub(1, Ordering::SeqCst);
    NO_WAIT.set(was);
}

pub(super) fn switch(rt: &mut Rt, index: usize) {
    let now = rt.start.elapsed();
    let frames = rt.frames;
    let rate = rt.rate.max(1) as f64;
    let per =
        |k: usize| Duration::from_secs_f64(LATENCY[k].load(Ordering::Relaxed).max(0) as f64 / rate);
    if let Some(s) = &mut rt.input
        && !s.dead
        && !s.paused.load(Ordering::Acquire)
        && let Callback::In(cb) = &mut s.cb
    {
        let n = rt.ins.len();
        for (c, ch) in rt.ins.iter().enumerate() {
            // SAFETY: the driver's buffer of `frames` samples of its type.
            let b = unsafe { std::slice::from_raw_parts(ch.halves[index], frames * ch.size) };
            for f in 0..frames {
                rt.scratch_in[f * n + c] = asio_samples::read(ch.kind, b, f);
            }
        }
        // SAFETY: f32 samples, `len` of them, alive for the call.
        let data = unsafe {
            Data::from_parts(
                rt.scratch_in.as_mut_ptr().cast(),
                rt.scratch_in.len(),
                SampleFormat::F32,
            )
        };
        let ts = InputStreamTimestamp {
            callback: instant(now),
            capture: instant(now.saturating_sub(per(0))),
        };
        if catch_unwind(AssertUnwindSafe(|| cb(&data, &InputCallbackInfo::new(ts)))).is_err() {
            s.dead = true;
            PANICKED[0].store(s.id, Ordering::SeqCst);
        }
    }
    let n = rt.outs.len();
    if n > 0 {
        rt.scratch_out.fill(0.0);
        if let Some(s) = &mut rt.output
            && !s.dead
            && !s.paused.load(Ordering::Acquire)
            && let Callback::Out(cb) = &mut s.cb
        {
            // SAFETY: f32 samples, `len` of them, alive for the call.
            let mut data = unsafe {
                Data::from_parts(
                    rt.scratch_out.as_mut_ptr().cast(),
                    rt.scratch_out.len(),
                    SampleFormat::F32,
                )
            };
            let ts = OutputStreamTimestamp {
                callback: instant(now),
                playback: instant(now + per(1)),
            };
            if catch_unwind(AssertUnwindSafe(|| {
                cb(&mut data, &OutputCallbackInfo::new(ts))
            }))
            .is_err()
            {
                s.dead = true;
                rt.scratch_out.fill(0.0);
                PANICKED[1].store(s.id, Ordering::SeqCst);
            }
        }
        for (c, ch) in rt.outs.iter().enumerate() {
            // SAFETY: the driver's buffer of `frames` samples of its type.
            let b = unsafe { std::slice::from_raw_parts_mut(ch.halves[index], frames * ch.size) };
            for f in 0..frames {
                asio_samples::write(ch.kind, b, f, rt.scratch_out[f * n + c]);
            }
        }
    }
    if rt.output_ready {
        // SAFETY: the running driver's own method.
        unsafe { ((*(*rt.drv).vtbl).output_ready)(rt.drv) };
    }
}

pub(super) unsafe extern "C" fn buffer_switch(index: c_long, _direct: AsioBool) {
    process(index as usize);
}

pub(super) unsafe extern "C" fn switch_time_info(
    params: *mut AsioTime,
    index: c_long,
    _direct: AsioBool,
) -> *mut AsioTime {
    process(index as usize);
    params
}

/// The driver's notices, from whichever of its threads: atomics only.
pub(super) unsafe extern "C" fn rate_changed(rate: f64) {
    NEW_RATE.store(rate.to_bits(), Ordering::SeqCst);
    notify(N_RATE);
}

pub(super) unsafe extern "C" fn message(
    selector: c_long,
    value: c_long,
    _message: *mut c_void,
    _opt: *mut f64,
) -> c_long {
    match selector {
        kAsioSelectorSupported => c_long::from(matches!(
            value,
            kAsioEngineVersion
                | kAsioResetRequest
                | kAsioResyncRequest
                | kAsioLatenciesChanged
                | kAsioSupportsTimeInfo
        )),
        kAsioEngineVersion => 2,
        kAsioSupportsTimeInfo => 1,
        kAsioResetRequest => {
            notify(N_RESET);
            1
        }
        kAsioResyncRequest | kAsioLatenciesChanged => {
            notify(N_LATENCIES);
            1
        }
        _ => 0,
    }
}
