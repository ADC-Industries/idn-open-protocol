//! A file's pictures: packets decoded (or passed through), the decoder switched,
//! pictures judged on time or late, decoded ahead, handed over (on the GPU too).

use super::*;

impl Play {
    pub(super) fn on_video_packet(&mut self, p: &ReadPacket) {
        let tb = self.vstream.as_ref().unwrap().time_base;
        if let (Some(pass), Some(pts)) = (&self.pass, p.pts) {
            if p.dts.is_some_and(|d| d != pts) {
                idn_open_protocol::log::log(
                    "player: B-frames after all: no pass-through from here",
                );
                self.ctl.status.lock_ok().pass_through = Err("the file has B-frames".into());
                self.pass = None;
            } else if let Some(data) = to_annex_b(&p.data, pass.length_size) {
                self.coded.push_back((
                    Self::us(tb, pts) + self.loop_offset,
                    CodedPicture {
                        codec: pass.codec,
                        keyframe: p.keyframe,
                        data,
                        extra: if p.keyframe {
                            pass.params.clone()
                        } else {
                            Vec::new()
                        },
                    },
                ));
                while self.coded.len() > 32 {
                    self.coded.pop_front();
                }
            }
        }
        if p.keyframe
            && matches!(
                self.trial,
                Trial::WantSoftware | Trial::WantHardware | Trial::WantHardwareFirst
            )
            && let Some(pts) = p.pts
        {
            self.switch_decoder(Self::us(tb, pts) + self.loop_offset);
        }
        // The retiring decoder gets the packets too, for the pictures before the switch.
        if let Some((mut old, key_t)) = self.retiring.take() {
            let _ = old.send(p);
            let mut reached = false;
            while let Ok(Some((f, ts))) = old.receive() {
                let Some(ts) = ts else { continue };
                let t = Self::us(tb, ts) + self.loop_offset;
                if t >= key_t {
                    reached = true;
                    continue;
                }
                if t < self.skip_until {
                    continue;
                }
                if let Ok(pf) = self.picture(f, t) {
                    self.queue_video(t, pf);
                }
            }
            if !reached {
                self.retiring = Some((old, key_t));
            } else if old.uses_device() && self.trial == Trial::Software {
                // The hardware one, while software is measured: kept for a switch back.
                old.flush();
                self.parked_hw = Some(old);
            }
        }
        // Resting to catch up with the sound (`picture_late`): from the next keyframe again.
        if self.catch_up != Catch::No {
            if self.catch_up == Catch::Wanted
                && let Some(d) = self.vdec.as_mut()
            {
                // What it holds would come out later still.
                d.flush();
            }
            self.catch_up = Catch::Resting;
            if !p.keyframe {
                if let Some(t) = p.pts.map(|pts| Self::us(tb, pts) + self.loop_offset) {
                    self.last_video_t = self.last_video_t.max(t);
                    if let Some(pf) = self.coded_only(t) {
                        self.queue_video(t, pf);
                    }
                }
                return;
            }
            self.catch_up = Catch::No;
        }
        // Stop / StopAfter: while nothing needs pictures, the compressed ones go as they
        // are and the decoder rests; it starts again, clean, at a keyframe.
        if matches!(self.cfg.idle, IdleDecode::Stop | IdleDecode::StopAfter(_)) {
            let t = p.pts.map(|pts| Self::us(tb, pts) + self.loop_offset);
            let idle = !self.needed();
            if idle || (self.stopped && !p.keyframe) {
                // Stopping: what the decoder holds is dropped (drained at a seam, those
                // old pictures came out seconds late and pushed the clock back).
                if !self.stopped
                    && let Some(d) = self.vdec.as_mut()
                {
                    d.flush();
                }
                self.stopped = true;
                // (Nothing decodes while stopped: the trial's parked hardware decoder lets
                // its GPU surfaces go; a switch back opens one.)
                self.parked_hw = None;
                if let Some(t) = t {
                    // (A looped file's next pass is placed after the last picture.)
                    self.last_video_t = self.last_video_t.max(t);
                    if let Some(pf) = self.coded_only(t) {
                        self.queue_video(t, pf);
                    }
                }
                return;
            }
            if self.stopped {
                if let Some(d) = self.vdec.as_mut() {
                    d.flush();
                }
                self.stopped = false;
            }
        }
        // A hardware decoder that fails before its first picture can't take this video
        // (a 4:4:4 JPEG cover, a profile it lacks): software from here, the packet again
        // when it is a keyframe (a cover is one picture: it was lost for the whole song).
        let mut retried = false;
        'decode: loop {
            let Some(d) = self.vdec.as_mut() else { return };
            let hw_fresh = d.uses_device() && self.decoded == 0;
            let t_dec = Instant::now();
            let mut failed: Option<io::Error> = d.send(p).err();
            loop {
                if failed.is_some() {
                    break;
                }
                // The decoder's frame, borrowed until its next receive (nothing below
                // touches the decoder).
                let (frame, ts, gpu) = match self.vdec.as_mut().unwrap().receive_owned() {
                    Ok(Some(x)) => x,
                    Ok(None) => break,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                };
                let Some(ts) = ts else { continue };
                let t = Self::us(tb, ts) + self.loop_offset;
                if t < self.skip_until {
                    continue;
                }
                self.last_video_t = self.last_video_t.max(t);
                // A picture already past its time: the decoder holds pictures back (frame
                // threads, B-frames) longer than the read-ahead. Push the clock back by the
                // lateness, so that the sound waiting in the queue is delayed by the same
                // amount and stays with the picture (sound 0.2 s early at NDI
                // receivers).
                if self.clock_started && !self.paused {
                    let now_media = self.media0 + self.wall0.elapsed().as_micros() as i64;
                    if t < now_media {
                        self.picture_late(Duration::from_micros((now_media - t) as u64));
                    }
                }
                let hardware = gpu;
                self.take_picture(frame, gpu, t);
                let mut st = self.ctl.status.lock_ok();
                if st.decoder.is_none() {
                    st.decoder = Some(format!(
                        "{} ({})",
                        self.vstream
                            .as_ref()
                            .map_or("video".into(), |s| codec_name(s.codec_id)),
                        match (hardware, &self.downloads, self.ff.hw_decode.as_deref()) {
                            (true, Some(_), Some(hw)) => format!(
                                "{}, {} downloading at once",
                                hw.name,
                                crate::download_depth()
                            ),
                            (true, _, Some(hw)) => hw.name.into(),
                            _ => "software".into(),
                        }
                    ));
                }
                // With download helpers, their time is counted when they finish.
                if !(hardware && self.downloads.is_some()) {
                    st.decode_ms = st.decode_ms * 0.9 + t_dec.elapsed().as_secs_f64() * 100.0;
                }
                let decode_ms = st.decode_ms;
                drop(st);
                self.decoded += 1;
                self.played += 1;
                self.judge(hardware, decode_ms);
            }
            if let Some(e) = failed {
                if hw_fresh && !retried && self.software_now(&e) {
                    retried = true;
                    if p.keyframe {
                        continue 'decode;
                    }
                } else {
                    idn_open_protocol::log::log(&format!("player: {e}"));
                }
            }
            break;
        }
    }

    /// A decoded picture of time `t`: to the download helpers when it is on the GPU and
    /// they run, else downloaded (if need be) and converted here; queued either way.
    pub(super) fn take_picture(&mut self, frame: Frame, gpu: bool, t: i64) {
        // Warm: decoded, but left where it is while nothing needs it.
        if self.cfg.idle == IdleDecode::Warm
            && !self.needed()
            && let Some(pf) = self.coded_only(t)
        {
            drop(frame); // (a GPU surface goes back to the decoder)
            return self.queue_video(t, pf);
        }
        // Above the rate wanted: only the compressed picture, if anything passes it.
        if let Some(th) = &mut self.thin {
            th.arrived(t * 10);
            if !th.keep() {
                drop(frame);
                if let Some(pf) = self.coded_only(t) {
                    self.queue_video(t, pf);
                }
                return;
            }
        }
        // Kept on the GPU (§E.27): while only this process's inputs on the application's
        // GPU take the pictures, converted there and handed over as they are.
        #[cfg(all(feature = "gpu", target_os = "linux"))]
        let why = if gpu {
            self.out.gpu_only_why()
        } else {
            Err("")
        };
        #[cfg(all(feature = "gpu", target_os = "linux"))]
        let gpu_only = why.is_ok();
        #[cfg(all(feature = "gpu", target_os = "linux"))]
        if gpu && self.va_said != Some(gpu_only) && self.ctl.gpu.lock_ok().is_some() {
            self.va_said = Some(gpu_only);
            idn_open_protocol::log::log(&match why {
                Ok(()) => {
                    "player: pictures kept on the GPU (only this process's inputs on it take them)"
                        .to_owned()
                }
                Err(w) => format!("player: pictures downloaded ({w} takes them here)"),
            });
        }
        #[cfg(all(feature = "gpu", target_os = "linux"))]
        if gpu_only && let Some(pic) = self.on_gpu(&frame) {
            let at = &pic.planes;
            let (w, h) = (at[0].width as usize, at[0].height as usize);
            let layout = if at[1].height as usize == h {
                Fourcc::Shq2
            } else {
                Fourcc::Shq0
            };
            let sizes = at
                .iter()
                .map(|p| idn_open_protocol::shq::Plane {
                    width: p.width as usize,
                    height: p.height as usize,
                    data: Vec::new(),
                })
                .collect();
            let coded = self.coded_for(t);
            drop(frame); // (the surface goes back to the decoder)
            let pf = PlanarFrame {
                layout,
                width: w,
                height: h,
                planes: Arc::new(sizes),
                timestamp: None,
                timecode: None,
                audio: None,
                coded,
            };
            self.pass_pictures += 1;
            self.queue.insert((t, 0), Item::VideoGpu(pf, pic));
            return;
        }
        if gpu && self.downloads.is_some() {
            // To a helper; the queue gets it when it is done.
            let coded = self.coded_for(t);
            let dl = self.downloads.as_mut().expect("checked");
            if dl.jobs.send((dl.generation, frame, t, coded)).is_ok() {
                dl.pending.insert(t);
            }
            return;
        }
        match frame.downloaded(&self.sw).and_then(|f| self.picture(f, t)) {
            Ok(pf) => self.queue_video(t, pf),
            Err(e) => idn_open_protocol::log::log(&format!("player: {e}")),
        }
    }

    pub(super) fn hand_over(&mut self, k: (i64, u8)) {
        let Some(item) = self.queue.remove(&k) else {
            return;
        };
        let tc = self.tc0 + k.0 * 10;
        self.last_tc = self.last_tc.max(tc);
        match item {
            Item::Video(mut f) => {
                f.timecode = Some(tc);
                let _ = self.out.send_planes(f);
            }
            #[cfg(all(feature = "gpu", target_os = "linux"))]
            Item::VideoGpu(mut f, pic) => {
                f.timecode = Some(tc);
                if self.out.gpu_only() {
                    let _ = self.out.send_planes_gpu_only(f, pic);
                } else if let Some(hand) = self.ctl.gpu.lock_ok().clone()
                    && let Ok(data) = pic.read_planes(&hand)
                {
                    // Someone needs the samples since it was queued: read back, once.
                    let planes = f
                        .planes
                        .iter()
                        .zip(data)
                        .map(|(p, data)| idn_open_protocol::shq::Plane {
                            width: p.width,
                            height: p.height,
                            data,
                        })
                        .collect();
                    f.planes = Arc::new(planes);
                    let _ = self.out.send_planes(f);
                }
            }
            Item::Audio(a, None) => {
                let _ = self.out.send_audio(a, Some(tc));
            }
            Item::Audio(a, Some(c)) => {
                let _ = self.out.send_audio_coded(a, Some(tc), c);
            }
        }
        let mut st = self.ctl.status.lock_ok();
        // Within the file: a looped file's media time keeps growing across passes.
        let t = k.0.max(0) as u64;
        st.position = Duration::from_micros(
            match st.duration.map(|d| d.as_micros() as u64).filter(|&d| d > 0) {
                Some(d) if self.cfg.looped => t % d,
                _ => t,
            },
        );
        if !matches!(st.state, PlayerState::Failed(_)) {
            st.state = PlayerState::Playing;
        }
    }

    /// Reopens the video decoder the other way, at a keyframe: what the old one still
    /// holds comes out first, then the new one starts clean from the keyframe (leading
    /// pictures of an open GOP are skipped by the decoder, as at any stream start).
    pub(super) fn switch_decoder(&mut self, key_t: i64) {
        let to_software = self.trial == Trial::WantSoftware;
        let measure = self.trial == Trial::WantHardwareFirst;
        let Some(stream) = self.vstream.as_ref() else {
            return;
        };
        let device = if to_software {
            None
        } else {
            self.ff.hw_decode.as_deref()
        };
        // Back to hardware: the trial's own hardware decoder, its GPU context already set
        // up (flushed: it starts clean at this keyframe as a new one would).
        let opened = match (to_software, self.parked_hw.take()) {
            (false, Some(mut d)) => {
                d.flush();
                Ok(d)
            }
            _ => FileDecoder::open(stream, device),
        };
        match opened {
            Ok(d) => {
                self.retiring = self.vdec.take().map(|old| (old, key_t));
                self.vdec = Some(d);
                let mut st = self.ctl.status.lock_ok();
                self.trial = if to_software {
                    Trial::Software
                } else if measure {
                    Trial::First
                } else {
                    Trial::Settled
                };
                st.decoder = None;
                st.decode_ms = 0.0;
                self.decoded = 0;
            }
            Err(e) => {
                idn_open_protocol::log::log(&format!(
                    "player: could not reopen the decoder: {e}; keeping the current one"
                ));
                self.trial = Trial::Settled;
            }
        }
    }

    /// The retiring decoder dropped (a loop's seam, a seek) — unless it is the trial's
    /// hardware one while software is measured: flushed and parked for the switch back
    /// (dropped, the switch back set up a new one, the sound stall of lab 453-B again).
    pub(super) fn park_retiring(&mut self) {
        if let Some((mut old, _)) = self.retiring.take()
            && old.uses_device()
            && self.trial == Trial::Software
        {
            old.flush();
            self.parked_hw = Some(old);
        }
    }

    /// The hardware decoder failed before its first picture: a software one instead, for
    /// the rest of the file. `false` if none opens.
    pub(super) fn software_now(&mut self, e: &io::Error) -> bool {
        let Some(stream) = self.vstream.as_ref() else {
            return false;
        };
        match FileDecoder::open(stream, None) {
            Ok(d) => {
                idn_open_protocol::log::log(&format!(
                    "player: the hardware decoder can't take this video ({e}); software decoding"
                ));
                self.vdec = Some(d);
                self.retiring = None;
                self.trial = Trial::Settled;
                self.ctl.status.lock_ok().decoder = None;
                true
            }
            Err(_) => false,
        }
    }

    /// A picture `late` past its time. The clock is pushed back by it, so that the sound
    /// waits with the picture (the decoder's own latency at the start) — but once a file
    /// with sound plays (45 pictures decoded), the sound keeps time: pushed back at every
    /// late picture, it had a gap as long each time (555 underruns in 30 s of a 4K60 file
    /// under a switcher's 4K scene). A picture over four frames late then rests
    /// the decoder until the next keyframe, the compressed pictures going as they are, so
    /// that reading keeps the sound's pace; the pictures freeze for the rest of the GOP.
    pub(super) fn picture_late(&mut self, late: Duration) {
        self.behind += late;
        if self.astream.is_some() && self.played >= 45 {
            // A decoder just switched to (the trial's switch) starting up: its first
            // pictures come late once; neither the clock nor a rest for them.
            if self.decoded < 8 {
                return;
            }
            let period = self
                .vstream
                .as_ref()
                .map_or(Duration::from_millis(33), |s| {
                    Duration::from_micros(
                        1_000_000 * s.frame_rate.1.max(1) as u64 / s.frame_rate.0.max(1) as u64,
                    )
                });
            if late > 4 * period && self.catch_up == Catch::No {
                self.catch_up = Catch::Wanted;
                self.ctl.status.lock_ok().caught_up += 1;
            }
            return;
        }
        self.wall0 += late;
        self.ctl.status.lock_ok().shifted += late;
    }

    /// `frame` (a VA-API surface) converted on the application's GPU; `None` without one,
    /// or where it can't be (said once, then downloaded as before).
    #[cfg(all(feature = "gpu", target_os = "linux"))]
    pub(super) fn on_gpu(&mut self, frame: &Frame) -> Option<idn_open_protocol::gpu::GpuPicture> {
        if self.va.is_none() {
            let hand = self.ctl.gpu.lock_ok().clone()?;
            self.va = Some(crate::va_gpu::VaToGpu::new(&hand).map_err(|e| {
                idn_open_protocol::log::log(&format!("player: {e}; pictures downloaded"));
            }));
        }
        let conv = self.va.as_mut()?.as_mut().ok()?;
        match conv.picture(frame) {
            Ok(p) => Some(p),
            Err(e) => {
                idn_open_protocol::log::log(&format!("player: {e}; pictures downloaded"));
                self.va = Some(Err(()));
                None
            }
        }
    }

    /// Queues a picture. One already past its time (the decoder or the GPU couldn't keep
    /// up, or the end of a pass took a while) holds the clock back by its lateness, so
    /// that the sound waits with it and overdue pictures don't go out in a burst, of
    /// which a receiver's connection keeps only the last.
    pub(super) fn queue_video(&mut self, t: i64, pf: PlanarFrame) {
        // (A compressed picture alone, as it was read, isn't the decoder's lateness.)
        if self.clock_started && !self.paused && !pf.planes.is_empty() {
            let now_media = self.media0 + self.wall0.elapsed().as_micros() as i64;
            if t < now_media {
                self.picture_late(Duration::from_micros((now_media - t) as u64));
            }
        }
        // Only a pass's first picture is kept (a still has one): video's aren't held.
        self.pass_pictures += 1;
        self.last_picture = (self.pass_pictures == 1 && !pf.planes.is_empty()).then(|| pf.clone());
        self.queue.insert((t, 0), Item::Video(pf));
    }

    /// The file's compressed picture of time `t` alone, for receivers passing it through
    /// while nothing needs the samples.
    pub(super) fn coded_only(&mut self, t: i64) -> Option<PlanarFrame> {
        let c = self.coded_for(t)?;
        let s = self.vstream.as_ref()?;
        Some(PlanarFrame {
            layout: Fourcc::Shq2,
            width: s.width,
            height: s.height,
            planes: Arc::default(),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: Some(c),
        })
    }

    /// The compressed picture of time `t`; older ones never got a picture.
    pub(super) fn coded_for(&mut self, t: i64) -> Option<CodedPicture> {
        while self.coded.front().is_some_and(|c| c.0 < t) {
            self.coded.pop_front();
        }
        match self.coded.front() {
            Some(c) if c.0 == t => self.coded.pop_front().map(|c| c.1),
            _ => None,
        }
    }

    /// A decoded picture as planes ([`to_planar`]), with its compressed form when passing
    /// through.
    pub(super) fn picture(&mut self, f: FrameRef, t: i64) -> io::Result<PlanarFrame> {
        let coded = self.coded_for(t);
        let rotation = self.vstream.as_ref().map_or(0, |s| s.rotation);
        if let Some(s) = self.vstream.as_ref() {
            f.default_range(s.color_range);
        }
        to_planar(f, &mut self.scaler, &mut self.scaled, coded).map(|pf| rotated(pf, rotation))
    }

    /// After enough pictures, measures the decoder over windows of 90: whether pictures
    /// came late (the clock shifted for them), not a picture's time (a 4K30 picture took
    /// 20 ms from the GPU with its download, the GPU clocked down, and two at a time kept
    /// up), and the process's CPU a picture. Hardware first; then software
    /// for a window, at a keyframe. Both keeping up: software only if it saves a tenth of
    /// the CPU (a 4K30 H.264 file had gone to software decoding, which kept up at
    /// 151% of a core against 121% on the GPU); else the one that keeps up, or the less
    /// late. The choice is remembered per codec, size and rate. (The process's CPU: in a
    /// program doing more, such as the switcher, its other work is about the same in the
    /// two windows.)
    pub(super) fn judge(&mut self, hardware: bool, decode_ms: f64) {
        if self.decoded < 45 {
            return;
        }
        let shifted = self.behind;
        // Windows while nothing needed the pictures don't count, nor the one when that
        // changed (decoding catching up as a receiver joined).
        let idle = self.cfg.idle != IdleDecode::Always && self.pass.is_some() && !self.needs.0;
        if self.decoded == 45 || idle || idle != self.window_idle {
            self.window = (self.decoded, shifted);
            self.window_cpu = idn_open_protocol::process_cpu_time();
            self.window_idle = idle;
            return;
        }
        if self.decoded < self.window.0 + 90 {
            return;
        }
        let period = self.vstream.as_ref().map_or(1.0 / 30.0, |s| {
            s.frame_rate.1.max(1) as f64 / s.frame_rate.0.max(1) as f64
        });
        let n = (self.decoded - self.window.0) as f64;
        // The share of the window's time the pictures came late by, and its CPU.
        let late = shifted.saturating_sub(self.window.1).as_secs_f64() / (n * period);
        let now = idn_open_protocol::process_cpu_time();
        let cpu = now.saturating_sub(self.window_cpu).as_secs_f64() * 1e3 / n;
        self.window = (self.decoded, shifted);
        self.window_cpu = now;
        const LATE: f64 = 0.05;
        match self.trial {
            // Late in two windows running: software at once.
            Trial::First
                if hardware && late > LATE && !std::mem::replace(&mut self.late_once, true) => {}
            Trial::First if hardware => {
                idn_open_protocol::log::log(&format!(
                    "player: hardware decoding {} ({:.0}% late, {cpu:.1} ms of CPU a picture, {decode_ms:.1} ms a picture with its download); trying software decoding",
                    if late > LATE {
                        "falls behind"
                    } else {
                        "keeps up"
                    },
                    late * 100.0
                ));
                (self.hw_late, self.hw_cpu) = (late, cpu);
                self.trial = Trial::WantSoftware;
            }
            Trial::First => self.late_once = false,
            Trial::Remembered if late <= LATE => self.late_once = false,
            Trial::Remembered if !std::mem::replace(&mut self.late_once, true) => {}
            Trial::Remembered => {
                self.late_once = false;
                idn_open_protocol::log::log(&format!(
                    "player: {} decoding, as measured before, falls behind ({:.0}% late, {cpu:.1} ms of CPU a picture); measuring again",
                    if hardware { "hardware" } else { "software" },
                    late * 100.0
                ));
                if hardware {
                    (self.hw_late, self.hw_cpu) = (late, cpu);
                    self.trial = Trial::WantSoftware;
                } else if self.ff.hw_decode.is_some() {
                    self.trial = Trial::WantHardwareFirst;
                } else {
                    self.trial = Trial::Settled;
                }
            }
            Trial::Software => {
                let (sw_ok, hw_ok) = (late <= LATE, self.hw_late <= LATE);
                let software = match (sw_ok, hw_ok) {
                    (true, true) => cpu <= self.hw_cpu * 0.9,
                    (true, false) => true,
                    (false, true) => false,
                    (false, false) => late < self.hw_late,
                };
                idn_open_protocol::log::log(&format!(
                    "player: {} decoding (software {:.0}% late, {cpu:.1} ms of CPU a picture; hardware {:.0}%, {:.1} ms)",
                    if software {
                        "staying with software"
                    } else {
                        "back to hardware"
                    },
                    late * 100.0,
                    self.hw_late * 100.0,
                    self.hw_cpu
                ));
                if self.cfg.remember_decoder
                    && let Some(s) = self.vstream.as_ref()
                {
                    remember(&decoder_key(s), software);
                }
                self.trial = if software {
                    // (Its GPU surfaces let go: software from here.)
                    self.parked_hw = None;
                    Trial::Settled
                } else {
                    Trial::WantHardware
                };
            }
            _ => {}
        }
    }

    /// Media decoded ahead of now (µs).
    ///
    /// With sound and pictures, the less of the two: a file stores its sound in chunks
    /// up to a second ahead of the pictures, and reading stopped on queued sound alone
    /// left each picture to be read after its time, the lateness holding the clock back
    /// again and again (a file with AAC played at a fraction of its speed).
    pub(super) fn ahead(&self) -> i64 {
        let last = |kind: u8| self.queue.keys().rev().find(|k| k.1 == kind).map(|k| k.0);
        let pending = self
            .downloads
            .as_ref()
            .and_then(|d| d.pending.last().copied());
        let video = last(0).max(pending);
        let audio = last(1);
        let last = match (self.vstream.is_some(), self.astream.is_some()) {
            (true, true) => match (video, audio) {
                (Some(v), Some(a)) => v.min(a),
                _ => return 0,
            },
            (true, false) => match video {
                Some(v) => v,
                None => return 0,
            },
            _ => match video.max(audio) {
                Some(t) => t,
                None => return 0,
            },
        };
        let now = self.media0 + self.wall0.elapsed().as_micros() as i64;
        last - now
    }

    /// Takes the pictures the download helpers finished; with `wait`, waits that long
    /// for one.
    pub(super) fn collect_downloads(&mut self, wait: Option<Duration>) {
        let Some(dl) = self.downloads.as_mut() else {
            return;
        };
        let mut first = wait.and_then(|w| dl.done.recv_timeout(w).ok());
        let mut done = Vec::new();
        while let Some((generation, t, r, secs)) = first.take().or_else(|| dl.done.try_recv().ok())
        {
            if generation == dl.generation {
                dl.pending.remove(&t);
                done.push((t, r, secs));
            }
        }
        for (t, r, secs) in done {
            {
                // Its download and conversion count as decoding time.
                let mut st = self.ctl.status.lock_ok();
                st.decode_ms = st.decode_ms * 0.9 + secs * 100.0;
            }
            match r {
                Ok(pf) => self.queue_video(t, pf),
                Err(e) => idn_open_protocol::log::log(&format!("player: {e}")),
            }
        }
    }

    /// Enough pictures on their way to the download helpers.
    pub(super) fn downloads_full(&self) -> bool {
        self.downloads
            .as_ref()
            .is_some_and(|d| d.pending.len() >= 2 * crate::download_depth())
    }

    /// Whether a picture due at or before `t` is still downloading.
    pub(super) fn downloading_before(&self, t: i64) -> bool {
        self.downloads
            .as_ref()
            .and_then(|d| d.pending.first())
            .is_some_and(|&p| p <= t)
    }

    /// The next queued item is due within a few milliseconds: reading ahead stops so
    /// that it goes out on time (decoding a picture here takes ~10 ms at 4K, and several
    /// in a row held the hand-over up).
    pub(super) fn due_soon(&self) -> bool {
        if !self.clock_started || self.paused {
            return false;
        }
        self.queue.keys().next().is_some_and(|&(t, _)| {
            let due = self.wall0 + Duration::from_micros((t - self.media0).max(0) as u64);
            due <= Instant::now() + Duration::from_millis(4)
        })
    }
}
