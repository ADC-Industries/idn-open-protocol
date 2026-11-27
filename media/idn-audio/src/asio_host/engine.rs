//! The engine thread: it loads the driver and makes every call to it; the streams'
//! opening and closing, the driver's notices.

use super::*;

pub(super) enum Req {
    Probe(DriverId, Sender<Result<Probe, Error>>),
    /// Answers the stream's buffer size (kept up to date).
    Open(Box<Open>, Sender<Result<Arc<AtomicU32>, Error>>),
    /// `tx`: answered once done (none: from a place that can't wait).
    Close {
        input: bool,
        id: u64,
        tx: Option<Sender<Result<(), Error>>>,
    },
}

/// The engine thread's channel, and when it began (the streams' time base).
pub(super) struct Engine {
    pub(super) tx: Sender<Req>,
    pub(super) start: Instant,
}

pub(super) fn engine() -> &'static Engine {
    static E: OnceLock<Engine> = OnceLock::new();
    E.get_or_init(|| {
        let (tx, rx) = channel::<Req>();
        let start = Instant::now();
        let _ = std::thread::Builder::new()
            .name("idn-asio-host".into())
            .spawn(move || {
                NO_WAIT.set(true);
                // Its apartment, once, for its whole life (drivers come and go in it;
                // ending it with the last one unloaded DLLs whose threads lived on).
                // SAFETY: COM on this thread, never uninitialised.
                #[cfg(windows)]
                let _ = unsafe {
                    windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                    )
                };
                let mut st = State::default();
                loop {
                    let gone = match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(req) => {
                            if catch_unwind(AssertUnwindSafe(|| st.handle(req, start))).is_err() {
                                idn_open_protocol::log::log(
                                    "idn-audio: the ASIO host's thread caught a panic",
                                );
                            }
                            false
                        }
                        Err(RecvTimeoutError::Timeout) => false,
                        Err(RecvTimeoutError::Disconnected) => true,
                    };
                    if catch_unwind(AssertUnwindSafe(|| st.notices(start))).is_err() {
                        idn_open_protocol::log::log(
                            "idn-audio: the ASIO host's thread caught a panic",
                        );
                    }
                    pump();
                    if gone {
                        break;
                    }
                }
            });
        Engine { tx, start }
    })
}

/// The window messages for this thread (drivers that made windows or post to it). Linux
/// has no such messages: nothing to do.
pub(super) fn pump() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        let mut m = MSG::default();
        // SAFETY: a message structure of this thread's, filled and handed back at once.
        unsafe {
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
    }
}

/// To the engine without waiting.
pub(super) fn post(r: Req) {
    let _ = engine().tx.send(r);
}

/// A request to the engine thread, and its answer.
pub(super) fn engine_call<T: Send + 'static>(
    req: impl FnOnce(Sender<Result<T, Error>>) -> Req,
) -> Result<T, Error> {
    if NO_WAIT.get() {
        return Err(err(
            ErrorKind::InvalidInput,
            "not from the ASIO host's own threads (an audio or error callback)",
        ));
    }
    let (tx, rx) = channel();
    engine()
        .tx
        .send(req(tx))
        .map_err(|_| err(ErrorKind::HostUnavailable, "the ASIO thread ended"))?;
    rx.recv_timeout(ENGINE_WAIT).map_err(|_| {
        err(
            ErrorKind::DeviceNotAvailable,
            "the ASIO driver didn't answer",
        )
    })?
}

/// The driver's notices and the streams' panics, as atomics (set from any of the driver's
/// threads, the audio one included: nothing there locks or allocates). `RUN` numbers the
/// driver's runs; a notice carries the run it came in, so one from a driver let go of
/// never lands on the next.
pub(super) static RUN: AtomicU64 = AtomicU64::new(0);
/// The notices pending and their run, as one: `run << 3 | bits` (two atomics let a stale
/// run's bits pass for a new one's, or drop a current one; bug run 2026-11-25).
pub(super) static PENDING: AtomicU64 = AtomicU64::new(0);
pub(super) const N_RESET: u64 = 1;
pub(super) const N_RATE: u64 = 2;
pub(super) const N_LATENCIES: u64 = 4;
pub(super) static NEW_RATE: AtomicU64 = AtomicU64::new(0);
/// A stream whose callback panicked (input, output; 0: none).
pub(super) static PANICKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
/// The driver's latencies (frames: input, output), for the capture and playback times.
pub(super) static LATENCY: [AtomicI64; 2] = [AtomicI64::new(0), AtomicI64::new(0)];

pub(super) fn notify(bit: u64) {
    let run = RUN.load(Ordering::SeqCst);
    let _ = PENDING.try_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
        Some(if cur >> 3 == run {
            cur | bit
        } else {
            run << 3 | bit
        })
    });
}

/// The driver running, on the engine thread.
pub(super) struct Active {
    pub(super) id: DriverId,
    pub(super) loaded: std::mem::ManuallyDrop<Loaded>,
    pub(super) frames: Arc<AtomicU32>,
    /// The size the first stream asked for (kept through resets), if any.
    pub(super) asked_frames: Option<u32>,
    pub(super) rate: u32,
    pub(super) probe: Probe,
    /// The streams' slots while stopped (in the switch's Rt while running), their ids,
    /// and their error callbacks (always here: called on this thread only).
    pub(super) input: Option<Slot>,
    pub(super) output: Option<Slot>,
    pub(super) ids: [Option<u64>; 2],
    pub(super) errors: Vec<(u64, ErrCb)>,
    /// The switch's state while running (null: stopped), the run it is, and its gate.
    pub(super) rt: *mut Rt,
    pub(super) run: u64,
    pub(super) gate: *mut Gate,
    /// A switch stuck past a stop: the driver set aside, never released (its switch may
    /// still be in it).
    pub(super) poisoned: bool,
}

impl Active {
    pub(super) fn idle(&self) -> bool {
        self.ids == [None, None]
    }

    pub(super) fn frames(&self) -> u32 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Tells the streams (all, or `only`) that `why`.
    pub(super) fn tell(&mut self, kind: ErrorKind, why: &str, only: Option<u64>) {
        for (id, f) in &mut self.errors {
            if only.is_none_or(|o| o == *id) {
                let e = err(kind, why.to_owned());
                let _ = catch_unwind(AssertUnwindSafe(|| f(e)));
            }
        }
    }

    /// Stream `id` (direction `k`) let go of: its id and error callback first, then its
    /// slot dropped (a closure whose drop panics leaves nothing half done).
    pub(super) fn remove(&mut self, k: usize, id: u64) {
        self.ids[k] = None;
        self.errors.retain(|(i, _)| *i != id);
        let gone = if k == 0 {
            self.input.take()
        } else {
            self.output.take()
        };
        let _ = catch_unwind(AssertUnwindSafe(move || drop(gone)));
    }
}

impl Drop for Active {
    /// Stopped before the driver is released (and the switch's state freed after it); a
    /// driver set aside isn't released.
    fn drop(&mut self) {
        stop(self);
        *ACTIVE.lock_ok() = None;
        if !self.poisoned {
            // SAFETY: dropped once, here.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.loaded) };
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) active: Option<Active>,
}

impl State {
    fn handle(&mut self, req: Req, start: Instant) {
        match req {
            Req::Probe(id, tx) => {
                let p = self.probe(&id);
                // Kept here (only answers from the driver itself; not "another runs").
                if !p
                    .as_ref()
                    .is_err_and(|e| e.message() == Some(OTHER_RUNNING))
                {
                    keep(&id, &p);
                }
                let _ = tx.send(p);
            }
            Req::Open(open, tx) => {
                let id = open.id.clone();
                let r = self.open(*open, start);
                // Kept as it runs now (the defaults see what it has), else asked again.
                match self.active.as_ref().filter(|a| a.id == id) {
                    Some(a) => {
                        let mut p = a.probe.clone();
                        p.rate = a.rate;
                        keep(&id, &Ok(p));
                    }
                    None => forget(&id),
                }
                let _ = tx.send(r);
            }
            Req::Close { input, id, tx } => {
                self.close(input, id, start);
                if let Some(tx) = tx {
                    let _ = tx.send(Ok(()));
                }
            }
        }
    }

    fn probe(&self, id: &DriverId) -> Result<Probe, Error> {
        if let Some(a) = &self.active {
            // The driver running: what it was found to offer, at its rate now (not asked
            // about rates while it streams). Another: not loaded beside it.
            return if a.id == *id {
                let mut p = a.probe.clone();
                p.rate = a.rate;
                Ok(p)
            } else {
                Err(err(ErrorKind::DeviceBusy, OTHER_RUNNING))
            };
        }
        load(id).and_then(|l| probe_of(&l))
    }

    fn open(&mut self, o: Open, start: Instant) -> Result<Arc<AtomicU32>, Error> {
        if let Some(a) = &self.active
            && a.id != o.id
        {
            return Err(err(
                ErrorKind::DeviceBusy,
                "another ASIO driver is in use in this program (one at a time)",
            ));
        }
        if self.active.is_none() {
            let loaded = load(&o.id)?;
            let probe = probe_of(&loaded)?;
            *ACTIVE.lock_ok() = Some(o.id.clone());
            self.active = Some(Active {
                id: o.id.clone(),
                loaded: std::mem::ManuallyDrop::new(loaded),
                frames: Arc::new(AtomicU32::new(probe.frames.2)),
                asked_frames: None,
                rate: probe.rate,
                probe,
                input: None,
                output: None,
                ids: [None, None],
                errors: Vec::new(),
                rt: std::ptr::null_mut(),
                run: 0,
                gate: Box::into_raw(Box::default()),
                poisoned: false,
            });
        }
        let a = self.active.as_mut().expect("set above");
        // Checked before anything running is touched (a refused open is no dropout).
        if let Err(e) = admit(a, &o) {
            if a.idle() {
                self.active = None;
            }
            return Err(e);
        }
        stop(a);
        if a.poisoned {
            self.active = None;
            return Err(err(
                ErrorKind::DeviceNotAvailable,
                "the ASIO driver's buffer switch didn't end: open it again",
            ));
        }
        if a.idle() {
            if o.rate != a.rate {
                if let Err(e) = a.loaded.set_rate(o.rate as f64) {
                    self.active = None;
                    return Err(err(ErrorKind::UnsupportedConfig, e.to_string()));
                }
                // What it offers at the new rate (sizes may differ; an external clock
                // may keep another rate).
                match probe_of(&a.loaded) {
                    Ok(p) => {
                        a.rate = p.rate;
                        a.probe = p;
                    }
                    Err(_) => a.rate = o.rate,
                }
            }
            a.asked_frames = o.frames;
            a.frames.store(
                snap(o.frames.unwrap_or(a.probe.frames.2), &a.probe),
                Ordering::Relaxed,
            );
        }
        let (k, id) = (usize::from(!o.input), o.slot.id);
        if o.input {
            a.input = Some(o.slot);
        } else {
            a.output = Some(o.slot);
        }
        a.ids[k] = Some(id);
        a.errors.push((id, o.error));
        if let Err(e) = run(a, start) {
            // Back to what ran before.
            a.remove(k, id);
            if a.idle() {
                self.active = None;
            } else if let Err(e2) = run(a, start) {
                a.tell(ErrorKind::StreamInvalidated, &e2.to_string(), None);
            }
            return Err(e);
        }
        Ok(a.frames.clone())
    }

    fn close(&mut self, input: bool, id: u64, start: Instant) {
        let Some(a) = &mut self.active else { return };
        let k = usize::from(!input);
        // Not this stream's (already closed, or never opened): nothing stopped for it.
        if a.ids[k] != Some(id) {
            return;
        }
        stop(a);
        a.remove(k, id);
        forget(&a.id);
        if a.idle() || a.poisoned {
            // The driver let go (stopped by Active's drop, released on this thread).
            self.active = None;
            return;
        }
        if let Err(e) = run(a, start) {
            a.tell(ErrorKind::StreamInvalidated, &e.to_string(), None);
        }
    }

    /// The driver's notices and the streams' panics since the last look.
    fn notices(&mut self, start: Instant) {
        let pending = PENDING.swap(0, Ordering::SeqCst);
        let (notice_run, bits) = (pending >> 3, pending & 7);
        let panicked = [
            PANICKED[0].swap(0, Ordering::SeqCst),
            PANICKED[1].swap(0, Ordering::SeqCst),
        ];
        let Some(a) = &mut self.active else { return };
        for id in panicked.into_iter().filter(|i| *i != 0) {
            a.tell(
                ErrorKind::StreamInvalidated,
                "the stream's callback panicked: it is silent now",
                Some(id),
            );
        }
        // Only this run's (a driver let go of, or an earlier run, said nothing to this).
        if bits == 0 || notice_run != a.run || a.rt.is_null() {
            return;
        }
        if bits & N_LATENCIES != 0
            && let Ok((i, o)) = a.loaded.latencies()
        {
            LATENCY[0].store(i, Ordering::Relaxed);
            LATENCY[1].store(o, Ordering::Relaxed);
        }
        // A rate that really changed (some drivers say so at every clock lock; 0 or
        // nonsense: ignored): the switch's timing set up again, the streams told (they
        // were opened at the old rate).
        let r = f64::from_bits(NEW_RATE.load(Ordering::SeqCst));
        let rate_changed =
            bits & N_RATE != 0 && r.is_finite() && r > 0.0 && (r - a.rate as f64).abs() > 0.5;
        if bits & N_RESET != 0 || rate_changed {
            // The driver's settings changed (its panel): the buffers set up again for the
            // same streams, at the rate and size it has now.
            stop(a);
            forget(&a.id);
            if a.poisoned || a.idle() {
                self.active = None;
                return;
            }
            let was = a.rate;
            if let Ok(p) = probe_of(&a.loaded) {
                a.frames.store(
                    snap(a.asked_frames.unwrap_or(p.frames.2), &p),
                    Ordering::Relaxed,
                );
                a.rate = p.rate;
                a.probe = p;
            } else if rate_changed {
                a.rate = r.round() as u32;
            }
            if a.rate != was {
                a.tell(
                    ErrorKind::StreamInvalidated,
                    &format!("the driver's rate changed to {} Hz", a.rate),
                    None,
                );
            }
            if let Err(e) = run(a, start) {
                a.tell(ErrorKind::StreamInvalidated, &e.to_string(), None);
            }
        }
    }
}

/// Whether stream `o` can open on driver `a` as it is: its channels and their sample
/// types, its direction free, its rate (another only when nothing else uses the driver,
/// and one it can do), its buffer size (another only when alone).
pub(super) fn admit(a: &Active, o: &Open) -> Result<(), Error> {
    let have = if o.input {
        a.probe.inputs
    } else {
        a.probe.outputs
    };
    if o.channels == 0 || o.channels > have {
        return Err(err(
            ErrorKind::UnsupportedConfig,
            format!("{} channels asked, the driver has {have}", o.channels),
        ));
    }
    if a.ids[usize::from(!o.input)].is_some() {
        return Err(err(
            ErrorKind::DeviceBusy,
            "a stream of this direction is already open on the driver",
        ));
    }
    if o.rate != a.rate && (!a.idle() || !a.loaded.can_rate(o.rate as f64)) {
        return Err(err(
            ErrorKind::UnsupportedConfig,
            format!("the driver runs at {} Hz, not {}", a.rate, o.rate),
        ));
    }
    if !a.idle()
        && let Some(n) = o.frames
        && snap(n, &a.probe) != a.frames()
    {
        return Err(err(
            ErrorKind::UnsupportedConfig,
            format!("the driver runs {}-frame buffers", a.frames()),
        ));
    }
    for c in 0..o.channels {
        let t = a
            .loaded
            .channel_info(o.input, c)
            .map_err(|e| err(ErrorKind::DeviceNotAvailable, e.to_string()))?
            .sample_type;
        if asio_samples::size_of(t).is_none() {
            return Err(err(
                ErrorKind::UnsupportedConfig,
                format!("sample type {t} isn't handled"),
            ));
        }
    }
    Ok(())
}
