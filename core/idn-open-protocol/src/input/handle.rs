//! The application's side of an input: opening it, reading events, commands to the worker,
//! and weaving interlaced fields.

use super::*;

impl Input {
    /// Starts receiving in the background. Returns once the local ports are open;
    /// connecting happens afterwards and is reported as [`InputEvent::State`] events.
    /// Fails only if local ports can't be opened (e.g. a fixed `quic_port` in use).
    pub fn open(mut cfg: InputConfig) -> io::Result<Self> {
        // The site's options for this input are those of its receiver name (§E.20).
        if let Some(sc) = cfg.secure.as_mut() {
            sc.input_scope
                .get_or_insert_with(|| cfg.receiver_name.clone());
        }
        // With the IDN Node's encryption, its Discovery Server and interface unless set
        // here (spec/extensions/idn-node.md §N.1).
        if let Some(m) = cfg
            .secure
            .as_ref()
            .and_then(|s| s.site.as_ref())
            .and_then(|e| e.node_machine())
        {
            if cfg.discovery_server.is_none() {
                cfg.discovery_server = m.discovery_server;
            }
            if cfg.interface.is_none() {
                cfg.interface = m.interface;
            }
        }
        // The program's own choice, else the machine's (§N.13), else on.
        cfg.parallel_decode = Some(
            cfg.parallel_decode
                .or(crate::node::video::machine().parallel_decode)
                .unwrap_or(true),
        );
        let queue = Arc::new(Queue {
            q: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            cap: cfg.queue.max(1),
            stats: Default::default(),
            hx_broken: AtomicBool::new(false),
            mc_rejected: Arc::default(),
            spare_rgba: Mutex::default(),
        });
        let (cmd, cmd_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let node = cfg
            .secure
            .as_ref()
            .and_then(|s| s.site.as_ref())
            .and_then(|e| e.node_register("input", &cfg.source, None));
        if cfg.local
            && cfg.rgba.is_none()
            && let Some((port, sender)) = crate::local::find(&cfg.source)
        {
            let (q, st) = (queue.clone(), stop.clone());
            let want = crate::local::Want {
                max_size: cfg.max_size,
                // (A local sender's 16-bit planes come with an HDR or deep picture only.)
                precise: cfg.precise || cfg.precise_hdr.is_some(),
                picture_rate: cfg.picture_rate,
                #[cfg(feature = "gpu")]
                gpu: cfg.gpu.is_some(),
            };
            let worker = std::thread::Builder::new()
                .name("input-local".into())
                .spawn(move || {
                    crate::local::run(
                        port,
                        sender,
                        st,
                        want,
                        cmd_rx,
                        |e| q.push(e),
                        |what| {
                            let mut s = q.stats.lock_ok();
                            match what {
                                crate::local::Counted::Video => s.video += 1,
                                crate::local::Counted::Audio => s.audio += 1,
                                crate::local::Counted::Thinned => s.thinned += 1,
                            }
                        },
                    )
                })?;
            return Ok(Input {
                queue,
                cmd,
                stop,
                worker: Some(worker),
                _node: node,
            });
        }
        // An input for sound alone (a routed receiver's): its network thread hands every
        // block on, so it asks for real-time scheduling as the router's does (R.30 step 4).
        let sound_only = !cfg.receive_video;
        let w = Worker::new(cfg, queue.clone(), stop.clone())?;
        let worker = std::thread::Builder::new()
            .name("input".into())
            .spawn(move || {
                if sound_only {
                    idn_common::rt::audio_this_thread_quick();
                }
                w.run(cmd_rx)
            })?;
        Ok(Input {
            queue,
            cmd,
            stop,
            worker: Some(worker),
            _node: node,
        })
    }

    /// The next event, waiting up to `timeout`; `None` if nothing arrived.
    pub fn recv(&self, timeout: Duration) -> Option<InputEvent> {
        let mut q = self.queue.q.lock_ok();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(e) = q.pop_front() {
                return Some(e);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            q = self
                .queue
                .ready
                .wait_timeout(q, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// The next event if one is waiting, without waiting.
    pub fn try_recv(&self) -> Option<InputEvent> {
        self.queue.q.lock_ok().pop_front()
    }

    /// Current counters and connection details.
    pub fn stats(&self) -> InputStats {
        let mut st = self.queue.stats.lock_ok().clone();
        st.mc_rejected = self.queue.mc_rejected.load(Ordering::Relaxed);
        st
    }

    /// Tells the source whether it is on program (live) and/or preview, so cameras can
    /// light their tally lamps.
    pub fn set_tally(&self, on_program: bool, on_preview: bool) {
        let _ = self.cmd.send(Command::Tally(on_program, on_preview));
    }

    /// Sends an XML metadata message to the source (e.g. a PTZ command). Messages over
    /// 1 MB are not sent: senders accept no more on the control connection.
    pub fn send_metadata(&self, xml: &str) {
        if xml.len() <= 1 << 20 {
            let _ = self.cmd.send(Command::Metadata(xml.into()));
        }
    }

    /// Sends keyboard and mouse events to the source (R.25): a source that takes control
    /// says so in its capabilities (`idn_input="true"`, [`crate::remote::CAPABILITY`]),
    /// and acts on them only from devices of its site. At most
    /// [`crate::remote::MAX_EVENTS`] a call.
    pub fn send_control(&self, events: &[crate::remote::Control]) {
        if !events.is_empty() {
            self.send_metadata(&crate::remote::to_xml(events));
        }
    }

    /// Switches between the full-quality and the low-bandwidth stream.
    pub fn set_low_bandwidth(&self, low: bool) {
        let _ = self.cmd.send(Command::LowBandwidth(low));
    }

    /// Asks IDN sources for pictures that fit inside (width, height), or the full size
    /// with `None` (`adapt/1`). NDI sources ignore it.
    pub fn set_max_size(&self, size: Option<(u32, u32)>) {
        let _ = self.cmd.send(Command::MaxSize(size));
    }

    /// Changes the audio channels asked for ([`InputConfig::audio_channels`]); IDN
    /// sources switch at the next block.
    pub fn set_audio_channels(&self, channels: Option<Vec<u16>>) {
        let _ = self.cmd.send(Command::AudioChannels(channels));
    }

    /// Switches decoding two pictures at once on or off
    /// ([`InputConfig::parallel_decode`]).
    pub fn set_parallel_decode(&self, on: bool) {
        let _ = self.cmd.send(Command::ParallelDecode(on));
    }

    /// Switches delivering SpeedHQ pictures as coefficients on or off
    /// ([`InputConfig::coefficients`]), from the next picture: an application choosing
    /// between reconstructing on the GPU and the CPU's own decoder, without reconnecting.
    pub fn set_coefficients(&self, on: bool) {
        let _ = self.cmd.send(Command::Coefficients(on));
    }

    /// Stops using the GPU given in [`InputConfig::gpu`]: pictures in this memory from
    /// the next session on, which starts at once. For an application whose GPU failed
    /// (a lost device): its inputs went on handing over pictures there, and after a lost
    /// device every one was black.
    #[cfg(feature = "gpu")]
    pub fn stop_using_gpu(&self) {
        let _ = self.cmd.send(Command::GpuOff);
    }

    /// Gives a picture's RGBA buffer ([`VideoIn::rgba`]) back once the application is
    /// done with it, to be filled again by a later picture. Optional, but worth it from
    /// 4K up: a new 4K buffer costs about 4 ms of page faults per picture on Linux.
    pub fn recycle_rgba(&self, buf: Vec<u8>) {
        let mut spare = self.queue.spare_rgba.lock_ok();
        if spare.len() < SPARE_RGBA {
            spare.push(buf);
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The worker notices `stop` within 50 ms, unless it is inside a connect or a
        // secure negotiation (up to 5–10 s): then it is left to finish on its own, and
        // the application doesn't wait (IN-4).
        if let Some(w) = self.worker.take() {
            crate::pool::join_within(w, Duration::from_millis(500));
        }
    }
}

/// Combines the two fields of an interlaced frame (field 1 holds the even lines, field
/// 2 the odd lines) into one picture. `None` if they don't belong together: a different
/// layout or width, or heights more than one line apart (a source that changed format
/// between the fields, or a bad one).
pub fn weave_fields(f1: &Picture, f2: &Picture) -> Option<Picture> {
    let rows_fit = |a: usize, b: usize| a == b || a == b + 1;
    let fits = f1.fourcc == f2.fourcc
        && f1.width == f2.width
        && rows_fit(f1.height, f2.height)
        && f1.planes.len() == f2.planes.len()
        && f1.planes.iter().zip(&f2.planes).all(|(a, b)| {
            a.width == b.width
                && rows_fit(a.height, b.height)
                && a.data.len() == a.width * a.height
                && b.data.len() == b.width * b.height
        });
    if !fits {
        return None;
    }
    let weave = |a: &Plane, b: &Plane| {
        let mut data = Vec::with_capacity(a.data.len() + b.data.len());
        for y in 0..a.height {
            data.extend_from_slice(&a.data[y * a.width..(y + 1) * a.width]);
            if y < b.height {
                data.extend_from_slice(&b.data[y * b.width..(y + 1) * b.width]);
            }
        }
        Plane {
            width: a.width,
            height: a.height + b.height,
            data,
        }
    };
    Some(Picture {
        fourcc: f1.fourcc,
        width: f1.width,
        height: f1.height + f2.height,
        format: f1.format,
        quality: f1.quality,
        short_dc: f1.short_dc,
        high_precision: f1.high_precision,
        planes: f1
            .planes
            .iter()
            .zip(&f2.planes)
            .map(|(a, b)| weave(a, b))
            .collect(),
        precise: None,
        coefficients: None,
    })
}
