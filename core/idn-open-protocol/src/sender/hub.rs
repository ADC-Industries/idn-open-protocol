//! Compressed video for receivers that take it: one encoder per codec and shape, fed
//! every picture, its packets queued for each receiver (§E.10).

use super::*;

impl CodecHub {
    /// The compressed form of picture `content_id`, if it came with one.
    pub(super) fn coded_for(
        &self,
        content_id: Option<u64>,
    ) -> Option<Arc<crate::codec::CodedPicture>> {
        let id = content_id?;
        let c = self.coded.lock_ok();
        c.iter().find(|(k, _)| *k == id).map(|(_, p)| p.clone())
    }

    /// The codec of the source's own compressed pictures, when it has them (a file or a
    /// gateway passing them through, R.12): receivers that decode it get them without an
    /// encode.
    pub(super) fn passing(&self) -> Option<crate::codec::Codec> {
        self.coded.lock_ok().back().map(|(_, p)| p.codec)
    }

    /// Whether a picture on the GPU may be tried with the encoders now: there is a GPU and
    /// none has missed lately.
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_try(&self) -> bool {
        // (Not with the machine's processor set to the CPU, §N.13.)
        crate::node::video::machine().processor != Some(crate::node::video::Processor::Cpu)
            && self.gpu.lock_ok().is_some()
            && self
                .gpu_missed
                .lock_ok()
                .is_none_or(|t| t.elapsed() > Duration::from_secs(5))
    }

    /// Whether the encoders take pictures from the GPU now (their receivers need no
    /// samples).
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_ready(&self) -> bool {
        self.gpu_ok.load(Ordering::Relaxed) && self.gpu_try()
    }

    /// Feeds a picture (or audio) to every running encoder.
    pub(super) fn push(&self, f: &Arc<MediaFrame>) {
        // A picture only on the GPU (planes with sizes, no samples) is encoded there where
        // the encoder takes it; else it can't be here (its sound goes as the serve loop
        // sends it).
        #[cfg(feature = "gpu")]
        let on_gpu = f.gpu.is_some() && self.gpu_try();
        #[cfg(not(feature = "gpu"))]
        let on_gpu = false;
        if f.samples_missing() && !on_gpu {
            return;
        }
        for s in self.slots.lock_ok().iter() {
            s.input.push(f.clone());
        }
    }

    /// Adds a receiver's queue to the encoder for `key`, starting it if needed. The
    /// receiver starts on a keyframe.
    pub(super) fn subscribe(self: &Arc<Self>, key: EncKey, q: Arc<FrameQueue>) -> Arc<EncSlot> {
        let mut slots = self.slots.lock_ok();
        let slot = match slots.iter().find(|s| s.key == key) {
            Some(s) => s.clone(),
            None => {
                let s = Arc::new(EncSlot {
                    key,
                    input: Arc::default(),
                    subscribers: Mutex::default(),
                    keyframe: AtomicBool::new(true),
                    passing: AtomicBool::new(false),
                });
                slots.push(s.clone());
                let (hub, s2) = (self.clone(), s.clone());
                std::thread::spawn(move || {
                    // A panic (an encoder, a picture of the wrong size) must not leave the
                    // slot registered with its receivers waiting for good: it goes, and
                    // their queues close, so they connect again.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        hub.clone().encode_loop(s2.clone())
                    }));
                    if r.is_err() {
                        crate::log::log("compressed video: the encoder stopped (a panic)");
                        hub.slots.lock_ok().retain(|x| !Arc::ptr_eq(x, &s2));
                        for q in s2.subscribers.lock_ok().drain(..) {
                            q.close();
                        }
                    }
                });
                s
            }
        };
        slot.subscribers.lock_ok().push(q);
        slot.keyframe.store(true, Ordering::Relaxed);
        slot
    }

    /// The running encoders for pictures of `size` (`low`): their codecs and numbers of
    /// receivers.
    pub(super) fn running(
        &self,
        size: Option<(u32, u32)>,
        low: bool,
    ) -> Vec<(crate::codec::Codec, usize)> {
        let slots = self.slots.lock_ok();
        slots
            .iter()
            .filter(|s| s.key.size == size && s.key.low == low)
            .map(|s| (s.key.codec, s.subscribers.lock_ok().len()))
            .filter(|r| r.1 > 0)
            .collect()
    }

    pub(super) fn unsubscribe(&self, slot: &EncSlot, q: &Arc<FrameQueue>) {
        slot.subscribers.lock_ok().retain(|s| !Arc::ptr_eq(s, q));
    }

    /// Sends `f` to the slot's receivers, forgetting those that have gone.
    pub(super) fn deliver(slot: &EncSlot, f: Arc<MediaFrame>) {
        slot.subscribers
            .lock_ok()
            .retain(|q| q.push(f.clone()).is_some());
    }

    /// One encoder's thread: pictures in, access units out as HX frames (§E.18.3). Ends
    /// when no receiver uses it.
    pub(super) fn encode_loop(self: Arc<Self>, slot: Arc<EncSlot>) {
        let rx = FrameRx(slot.input.clone());
        // The open encoder and the picture shape it was opened for.
        let mut enc: Option<(Box<dyn crate::codec::VideoEncoder>, EncShape)> = None;
        let mut counter = u64::from_le_bytes(crate::random::secure_random::<8>()) >> 16;
        let (mut last_key, mut last_forced) = (Instant::now(), None::<Instant>);
        let mut failed_at: Option<Instant> = None;
        let fourcc = slot.key.codec.fourcc();
        // Passing pictures that came compressed straight through (from their keyframe on).
        let mut passing = false;
        loop {
            {
                // Stop when nobody is subscribed (under the hub's lock, so a new
                // subscriber either finds this slot running or starts another). A
                // receiver whose session ended is gone even if no frame came since: its
                // queue is closed.
                let mut slots = self.slots.lock_ok();
                let mut subs = slot.subscribers.lock_ok();
                subs.retain(|q| !q.is_closed());
                if subs.is_empty() {
                    drop(subs);
                    slots.retain(|s| !Arc::ptr_eq(s, &slot));
                    return;
                }
            }
            let Some(f) = rx.0.recv_timeout(Duration::from_millis(200)) else {
                continue;
            };
            // Audio on its own, pre-encoded frames, and pictures with alpha (which
            // compressed video can't carry) pass through as they are.
            let alpha = matches!(f.fourcc, Fourcc::Shq7 | Fourcc::Shq9);
            // (A picture sent compressed only, without its planes, goes on: passed
            // through, or dropped below if it would have to be encoded.)
            let coded_only = f.planes.is_empty() && self.coded_for(f.content_id).is_some();
            if (f.planes.is_empty() && !coded_only) || f.hx_video.is_some() || alpha {
                if alpha {
                    slot.keyframe.store(true, Ordering::Relaxed);
                }
                Self::deliver(&slot, f);
                continue;
            }
            let wants = Wants {
                video: true,
                size: slot.key.size,
                low: slot.key.low,
                ..Default::default()
            };
            let (w, h) = target_size(&f, &wants);
            // Already compressed in this codec at this size: as it is, no encode (R.12).
            let coded = self
                .coded_for(f.content_id)
                .filter(|c| c.codec == slot.key.codec && (w, h) == (f.width, f.height));
            // A receiver asked for a keyframe (a new one) while the source's own pictures
            // pass through mid-GOP: encoded from this picture's planes until the source's
            // next keyframe (which the pass-through takes up again, cleanly), rather than
            // the receiver waiting a GOP of several seconds.
            if passing
                && !f.planes.is_empty()
                && slot.keyframe.load(Ordering::Relaxed)
                && coded.as_ref().is_some_and(|c| !c.keyframe)
            {
                passing = false;
                slot.passing.store(false, Ordering::Relaxed);
                last_forced = None;
            }
            match coded {
                Some(c) if passing || c.keyframe => {
                    passing = true;
                    slot.passing.store(true, Ordering::Relaxed);
                    // The source's keyframe answers any receiver waiting for one.
                    if c.keyframe {
                        slot.keyframe.store(false, Ordering::Relaxed);
                    }
                    let packet = crate::hx::CompressedPacket {
                        fourcc,
                        pts: f.timecode,
                        dts: f.timecode,
                        counter,
                        flags: u32::from(c.keyframe) * crate::hx::FLAG_KEYFRAME,
                        data: &if c.keyframe && c.codec != crate::codec::Codec::Av1 {
                            crate::hx::with_parameter_sets(
                                &c.data,
                                &c.extra,
                                c.codec == crate::codec::Codec::Hevc,
                            )
                        } else {
                            c.data.clone()
                        },
                        extra: if c.keyframe { &c.extra } else { &[] },
                    }
                    .encode();
                    counter += 1;
                    self.passed.fetch_add(1, Ordering::Relaxed);
                    Self::deliver(
                        &slot,
                        Arc::new(MediaFrame {
                            captions: Vec::new(),
                            fourcc: f.fourcc,
                            width: w,
                            height: h,
                            planes: Arc::default(),
                            frame_rate: f.frame_rate,
                            audio: f.audio.clone(),
                            sample_rate: f.sample_rate,
                            timecode: f.timecode,
                            timestamp: f.timestamp,
                            metadata: f.metadata.clone(),
                            interlaced: false,
                            hx_video: Some((fourcc, c.keyframe, packet)),
                            hx_audio: None,
                            content_id: None,
                            precise: None,
                            #[cfg(feature = "gpu")]
                            gpu: None,
                        }),
                    );
                    continue;
                }
                Some(_) => {} // encode until the next compressed keyframe
                None if passing => {
                    // Back to encoding: start the encoder's stream on a keyframe.
                    passing = false;
                    slot.passing.store(false, Ordering::Relaxed);
                    slot.keyframe.store(true, Ordering::Relaxed);
                    last_forced = None;
                }
                None => {}
            }
            // Compressed only and not passing: nothing to encode it from (its sound
            // goes on). The output says it needs pictures meanwhile.
            if f.planes.is_empty() {
                if let Some(a) = audio_part(&f) {
                    Self::deliver(&slot, a);
                }
                continue;
            }
            let shape = (w, h, f.frame_rate);
            if enc.as_ref().is_none_or(|(_, sh)| *sh != shape)
                && failed_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
            {
                let fps = f.frame_rate.0 as f64 / f.frame_rate.1.max(1) as f64;
                let cfg = crate::codec::EncoderConfig {
                    codec: slot.key.codec,
                    width: w,
                    height: h,
                    frame_rate: f.frame_rate,
                    bitrate: self.out.bitrate(w, h),
                    keyframe_interval: (fps * crate::codec::KEYFRAME_INTERVAL.as_secs_f64())
                        .round()
                        .max(1.0) as u32,
                };
                match self.out.encoder.open(&cfg) {
                    Ok(e) => {
                        enc = Some((e, shape));
                        failed_at = None;
                        slot.keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        crate::log::log(&format!(
                            "{} encoder for {w}x{h} failed to open: {e}; sending SpeedHQ",
                            slot.key.codec.name()
                        ));
                        enc = None;
                        failed_at = Some(Instant::now());
                    }
                }
            }
            let Some((e, _)) = enc.as_mut() else {
                // No encoder: SpeedHQ rather than nothing.
                Self::deliver(&slot, f);
                continue;
            };
            // Keyframes every KEYFRAME_INTERVAL and on request, merged (§E.18.3).
            let asked = slot.keyframe.load(Ordering::Relaxed)
                && last_forced.is_none_or(|t| t.elapsed() >= crate::codec::KEYFRAME_MERGE);
            let force = asked || last_key.elapsed() >= crate::codec::KEYFRAME_INTERVAL;
            if asked {
                slot.keyframe.store(false, Ordering::Relaxed);
                last_forced = Some(Instant::now());
            }
            // From the GPU where the picture is there and the encoder takes it (scaled
            // there too); from its samples otherwise.
            // (The device taken out first: a lock held in the match's scrutinee lasts the
            // whole match, and `gpu_try` takes it again.)
            #[cfg(feature = "gpu")]
            let hand = self.gpu.lock_ok().clone();
            #[cfg(feature = "gpu")]
            let from_gpu = match (f.gpu.as_ref(), hand) {
                (Some(g), Some(hand)) if self.gpu_try() => {
                    let r = e.encode_gpu(g, &hand, f.timecode, force);
                    match &r {
                        Some(Ok(_)) => self.gpu_ok.store(true, Ordering::Relaxed),
                        _ => {
                            self.gpu_ok.store(false, Ordering::Relaxed);
                            *self.gpu_missed.lock_ok() = Some(Instant::now());
                        }
                    }
                    r
                }
                _ => None,
            };
            #[cfg(not(feature = "gpu"))]
            let from_gpu: Option<std::io::Result<Vec<crate::codec::Encoded>>> = None;
            let result = match from_gpu {
                Some(r) => r,
                // (A picture only on the GPU the encoder couldn't take: none this time;
                // the next come with their samples.)
                None if f.samples_missing() => continue,
                None => {
                    let planes = if (w, h) != (f.width, f.height) {
                        Arc::new(scaled(&f, w, h))
                    } else {
                        f.planes.clone()
                    };
                    e.encode(&planes, f.fourcc, f.timecode, force)
                }
            };
            let out = match result {
                Ok(o) => o,
                Err(err) => {
                    // As when it won't open: SpeedHQ, this frame on, and another try in
                    // 5 s (reopened at once, an encoder that opens but fails every frame
                    // was reopened every frame, and its receivers had no picture).
                    crate::log::log(&format!(
                        "{} encoder failed: {err}; sending SpeedHQ, trying again in 5 s",
                        slot.key.codec.name()
                    ));
                    enc = None;
                    failed_at = Some(Instant::now());
                    Self::deliver(&slot, f);
                    continue;
                }
            };
            let mut audio = Some(f.audio.clone());
            self.encoded.fetch_add(out.len() as u64, Ordering::Relaxed);
            for p in out {
                if p.keyframe {
                    last_key = Instant::now();
                }
                let hevc = slot.key.codec == crate::codec::Codec::Hevc;
                let data = if p.keyframe && slot.key.codec != crate::codec::Codec::Av1 {
                    crate::hx::with_parameter_sets(&p.data, &p.extra, hevc)
                } else {
                    p.data
                };
                let packet = crate::hx::CompressedPacket {
                    fourcc,
                    pts: f.timecode,
                    dts: f.timecode,
                    counter,
                    flags: u32::from(p.keyframe) * crate::hx::FLAG_KEYFRAME,
                    data: &data,
                    extra: if p.keyframe { &p.extra } else { &[] },
                }
                .encode();
                counter += 1;
                Self::deliver(
                    &slot,
                    Arc::new(MediaFrame {
                        captions: Vec::new(),
                        fourcc: f.fourcc,
                        width: w,
                        height: h,
                        planes: Arc::default(),
                        frame_rate: f.frame_rate,
                        audio: audio.take().unwrap_or_default(),
                        sample_rate: f.sample_rate,
                        timecode: f.timecode,
                        timestamp: f.timestamp,
                        metadata: f.metadata.clone(),
                        interlaced: false,
                        hx_video: Some((fourcc, p.keyframe, packet)),
                        hx_audio: None,
                        content_id: None,
                        precise: None,
                        #[cfg(feature = "gpu")]
                        gpu: None,
                    }),
                );
            }
            // Nothing out (an encoder starting): the frame's audio still goes.
            if let Some(a) = audio.filter(|a| !a.is_empty()) {
                Self::deliver(
                    &slot,
                    Arc::new(MediaFrame {
                        captions: Vec::new(),
                        fourcc: f.fourcc,
                        width: 0,
                        height: 0,
                        planes: Arc::default(),
                        frame_rate: f.frame_rate,
                        audio: a,
                        sample_rate: f.sample_rate,
                        timecode: f.timecode,
                        timestamp: f.timestamp,
                        metadata: None,
                        interlaced: false,
                        hx_video: None,
                        hx_audio: None,
                        content_id: None,
                        precise: None,
                        #[cfg(feature = "gpu")]
                        gpu: None,
                    }),
                );
            }
        }
    }
}
