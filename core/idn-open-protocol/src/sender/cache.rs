//! The output's encodes, shared by its receivers: SpeedHQ (on the CPU, and on the GPU
//! where the picture is there), scaled pictures, and this machine's rings (§E.26, §E.27).

use super::*;

impl EncodeCache {
    /// Numbers `f` as sent while receivers on this machine may use the rings.
    pub(super) fn number(&self, f: &Arc<MediaFrame>) {
        if !self.machine_live.load(Ordering::Relaxed) {
            return;
        }
        let mut n = self.numbers.lock_ok();
        n.0 += 1;
        let number = n.0;
        if n.1.len() >= 1024 {
            n.1.pop_front();
        }
        n.1.push_back((Arc::downgrade(f), Arc::as_ptr(f) as usize, number));
    }

    /// `pic` encoded as SpeedHQ on the application's GPU, where the picture is (§E.27): no
    /// read-back and none of the CPU's cores; `None` where it can't (the CPU encodes).
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_encode(
        &self,
        pic: &crate::gpu::GpuPicture,
        fourcc: Fourcc,
        width: usize,
        height: usize,
        opt: &EncodeOptions,
    ) -> Option<Vec<u8>> {
        if !crate::node::video::shq_on_gpu_allowed() {
            return None;
        }
        let mut g = self.gpu_shq.lock_ok();
        if g.is_none() {
            let hand = self.gpu.lock_ok().clone()?;
            *g = Some(make_gpu_shq(&hand));
        }
        let e = g.as_mut()?.as_mut().ok()?;
        if !self.gpu_shq_said.swap(true, Ordering::Relaxed) {
            crate::log::log("SpeedHQ encoded on the GPU, from the pictures there");
        }
        let out = e.encode(pic, fourcc, width, height, opt);
        if out.is_none() {
            *self.gpu_shq_missed.lock_ok() = Some(Instant::now());
        }
        out
    }

    /// Makes the GPU's SpeedHQ encoder ahead of the first receiver that needs it (see
    /// [`Sender::use_gpu`](super::Sender::use_gpu)).
    #[cfg(feature = "gpu")]
    pub(super) fn warm_gpu_shq(&self, hand: &crate::gpu::GpuHandIn) {
        let mut g = self.gpu_shq.lock_ok();
        if g.is_none() {
            *g = Some(make_gpu_shq(hand));
        }
    }

    /// A blank picture of `f`'s size encoded on the GPU once, apart, the first time that
    /// size comes (the result thrown away): the encoder's buffers for it made and its
    /// shaders run before the first receiver needs them (a 2160p switcher's frame still took
    /// 62-69 ms at a receiver's join, lab 380). It doesn't count as the GPU encoding yet.
    #[cfg(feature = "gpu")]
    pub(super) fn warm_gpu_shq_size(self: &Arc<Self>, f: &MediaFrame) {
        if !crate::node::video::shq_on_gpu_allowed()
            || f.planes.len() < 3
            || !matches!(f.fourcc, Fourcc::Shq0 | Fourcc::Shq2)
        {
            return;
        }
        let sizes: Vec<(u32, u32)> = f.planes[..3]
            .iter()
            .map(|p| (p.width as u32, p.height as u32))
            .collect();
        let key = (f.width, f.height, f.fourcc);
        {
            let mut w = self.gpu_shq_warmed.lock_ok();
            if w.contains(&key) || w.len() >= 8 {
                return;
            }
            w.push(key);
        }
        let cache = self.clone();
        let _ = std::thread::Builder::new()
            .name("idn-gpu-shq-warm".into())
            .spawn(move || {
                let Some(hand) = cache.gpu.lock_ok().clone() else {
                    return;
                };
                let (planes, bytes) = crate::gpu::slot_layout(&sizes);
                let pic = crate::gpu::GpuPicture {
                    buffer: Arc::new(hand.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("SpeedHQ warm-up"),
                        size: bytes,
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    })),
                    planes,
                };
                let mut g = cache.gpu_shq.lock_ok();
                if g.is_none() {
                    *g = Some(make_gpu_shq(&hand));
                }
                if let Some(Ok(e)) = g.as_mut() {
                    let _ = e.encode(&pic, key.2, key.0, key.1, &EncodeOptions::default());
                }
            });
    }

    /// The GPU encodes SpeedHQ now: it was made, and hasn't missed a picture lately.
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_shq_ready(&self) -> bool {
        // (Once it has encoded: made ahead, before any receiver needed it, it isn't yet.)
        self.gpu_shq_said.load(Ordering::Relaxed) && self.gpu_shq_made()
    }

    /// The GPU's encoder is made (ahead, or by a first picture) and hasn't missed one
    /// lately: a receiver that can't take this machine's rings is encoded there from its
    /// first picture (counted as wanting samples until the first, it made a 2160p program
    /// read back as it joined, 26-100 ms of a frame, lab 380).
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_shq_made(&self) -> bool {
        // (Not while the machine's processor is the CPU: pictures are read back for it.)
        crate::node::video::shq_on_gpu_allowed()
            && matches!(*self.gpu_shq.lock_ok(), Some(Ok(_)))
            && self
                .gpu_shq_missed
                .lock_ok()
                .is_none_or(|t| t.elapsed() > Duration::from_secs(5))
    }

    /// `f`'s number as sent; `None` if it came before the rings were wanted, or is far
    /// behind.
    pub(super) fn number_of(&self, f: &Arc<MediaFrame>) -> Option<u64> {
        let addr = Arc::as_ptr(f) as usize;
        let n = self.numbers.lock_ok();
        n.1.iter()
            .rev()
            .find(|(w, a, _)| *a == addr && w.upgrade().is_some_and(|x| Arc::ptr_eq(&x, f)))
            .map(|e| e.2)
    }

    /// `f`'s picture in this output's picture ring for receivers on this machine (§E.26),
    /// written once however many receivers ask, and never after a newer one; a ring too
    /// small for it is replaced by a larger one (a new generation, offered again).
    pub(super) fn machine_picture(&self, f: &Arc<MediaFrame>) -> InRing {
        self.machine_picture_by(f, None)
    }

    /// As [`machine_picture`](Self::machine_picture), `f`'s planes having their sizes only
    /// and `fill` writing their samples (see [`Sender::send_filled`]).
    pub(super) fn machine_picture_by(
        &self,
        f: &Arc<MediaFrame>,
        fill: Option<&mut crate::machine::PlaneFill>,
    ) -> InRing {
        let Some(number) = self.number_of(f) else {
            return InRing::Stale;
        };
        let mut m = self.machine.lock_ok();
        if let Some(&(_, seq)) = m.pictures_written.iter().find(|e| e.0 == number) {
            return InRing::At(seq);
        }
        if m.pictures_written.back().is_some_and(|e| e.0 > number) {
            return InRing::Stale;
        }
        let need = if fill.is_some() {
            crate::machine::PIC_HEAD
                + f.planes.iter().map(|p| p.width * p.height).sum::<usize>()
                + f.metadata.as_deref().map_or(0, str::len)
        } else {
            crate::machine::picture_bytes(&f.planes, f.metadata.as_deref())
        };
        if m.pic.as_ref().is_none_or(|r| !r.fits(need)) {
            let Ok(r) =
                crate::machine::Ring::create("idn-machine-p", need, crate::machine::PIC_SLOTS)
            else {
                return InRing::No;
            };
            m.pic = Some(Arc::new(r));
            m.generation += 1;
        }
        let Some(control) = m.control() else {
            return InRing::No;
        };
        #[cfg(feature = "gpu")]
        let gpu = m.gpu_written.iter().find(|e| e.0 == number).map(|e| e.1);
        #[cfg(not(feature = "gpu"))]
        let gpu = None;
        // Every reader takes it on the GPU: its samples aren't copied here at all.
        let gpu_only = gpu.is_some() && m.cpu_takers == 0;
        // No samples to write for those that read them here (a picture on the GPU only).
        if fill.is_none() && f.samples_missing() && !gpu_only {
            return InRing::No;
        }
        m.pic_seq += 1;
        let seq = m.pic_seq;
        let parts = crate::machine::Parts {
            fourcc: f.fourcc,
            width: f.width,
            height: f.height,
            frame_rate: f.frame_rate,
            timecode: f.timecode,
            timestamp: f.timestamp,
            planes: &f.planes,
            metadata: f.metadata.as_deref(),
            gpu,
            gpu_only,
        };
        let ring = m.pic.as_ref().expect("made above");
        let put = match fill {
            Some(fill) => crate::machine::put_picture_filled(ring, seq, &parts, fill),
            None => crate::machine::put_picture(ring, seq, &parts),
        };
        if !put {
            return InRing::No;
        }
        control.publish(false, seq);
        if m.pictures_written.len() >= 8 {
            m.pictures_written.pop_front();
        }
        m.pictures_written.push_back((number, seq));
        InRing::At(seq)
    }

    /// `f`'s sound in this output's sound ring (§E.26), as [`machine_picture`] does.
    pub(super) fn machine_sound(&self, f: &Arc<MediaFrame>) -> InRing {
        let Some(number) = self.number_of(f) else {
            return InRing::Stale;
        };
        let mut m = self.machine.lock_ok();
        if let Some(&(_, seq)) = m.sound_written.iter().find(|e| e.0 == number) {
            return InRing::At(seq);
        }
        if m.sound_written.back().is_some_and(|e| e.0 > number) {
            return InRing::Stale;
        }
        let need = crate::machine::sound_bytes(&f.audio, f.metadata.as_deref());
        if m.sound.as_ref().is_none_or(|r| !r.fits(need)) {
            // Room for longer blocks than this one (a source's block length varies).
            let Ok(r) = crate::machine::Ring::create(
                "idn-machine-a",
                need.max(64 << 10),
                crate::machine::AUDIO_SLOTS,
            ) else {
                return InRing::No;
            };
            m.sound = Some(Arc::new(r));
            m.generation += 1;
        }
        let Some(control) = m.control() else {
            return InRing::No;
        };
        m.sound_seq += 1;
        let seq = m.sound_seq;
        let ring = m.sound.clone().expect("made above");
        if !crate::machine::put_sound(
            &ring,
            seq,
            f.sample_rate,
            f.timecode,
            f.timestamp,
            &f.audio,
            f.metadata.as_deref(),
        ) {
            return InRing::No;
        }
        control.publish(true, seq);
        if m.sound_written.len() >= 64 {
            m.sound_written.pop_front();
        }
        m.sound_written.push_back((number, seq));
        InRing::At(seq)
    }

    /// `f`'s picture, which the application has on its GPU in `src` (planes `from`), copied
    /// into this output's GPU ring for receivers on this machine that take it there
    /// (§E.27), now: the application may use `src` again as soon as this returns. The ring
    /// is made while receivers on this machine may use the rings (so it is in their offer);
    /// pictures go in once one of them takes them on the GPU.
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_picture(
        &self,
        f: &Arc<MediaFrame>,
        src: &wgpu::Buffer,
        from: &[crate::gpu::PlaneAt],
    ) {
        if !self.machine_live.load(Ordering::Relaxed) {
            return;
        }
        let Some(number) = self.number_of(f) else {
            return;
        };
        let Some(hand) = self.gpu.lock_ok().clone() else {
            return;
        };
        let sizes: Vec<(u32, u32)> = from.iter().map(|p| (p.width, p.height)).collect();
        let (to, need) = crate::gpu::slot_layout(&sizes);
        let (ring, slot, gseq, control) = {
            let mut m = self.machine.lock_ok();
            if m.gpu_failed {
                return;
            }
            if m.gpu.as_ref().is_none_or(|r| r.slot_bytes() < need) {
                match crate::gpu::GpuRing::create(&hand, need) {
                    Ok(r) => {
                        m.gpu = Some(Arc::new(r));
                        m.generation += 1;
                    }
                    Err(e) => {
                        crate::log::log(&format!(
                            "machine: pictures on the GPU: {e}; through shared memory"
                        ));
                        m.gpu_failed = true;
                        return;
                    }
                }
            }
            if m.gpu_takers == 0 {
                return;
            }
            let Some(control) = m.control() else {
                return;
            };
            m.gpu_seq += 1;
            let gseq = m.gpu_seq;
            let slot = (gseq % crate::gpu::GPU_SLOTS as u64) as usize;
            (m.gpu.clone().expect("made above"), slot, gseq, control)
        };
        let Some(stamp) = control.gpu_stamp(slot) else {
            return;
        };
        stamp.store(gseq * 2 + 1, Ordering::Release);
        if let Err(e) = ring.write(slot, src, from, &to) {
            // Never read as this picture.
            stamp.store(0, Ordering::Release);
            crate::log::log(&format!("machine: a picture on the GPU: {e}"));
            return;
        }
        stamp.store(gseq * 2, Ordering::Release);
        let mut m = self.machine.lock_ok();
        if m.gpu_written.len() >= 8 {
            m.gpu_written.pop_front();
        }
        m.gpu_written.push_back((number, (slot as u32, gseq)));
    }

    /// The rings' offer to a receiver on this machine: (generation, the message).
    pub(super) fn machine_offer(&self) -> Option<(u64, String)> {
        let mut m = self.machine.lock_ok();
        let control = m.control()?;
        #[cfg(feature = "gpu")]
        let gpu = m.gpu.as_ref().map_or(String::new(), |g| g.offer_attrs());
        #[cfg(not(feature = "gpu"))]
        let gpu = String::new();
        Some((
            m.generation,
            crate::machine::offer_xml(
                m.generation,
                &control,
                m.pic.as_deref(),
                m.sound.as_deref(),
                &gpu,
            ),
        ))
    }

    /// `f` scaled to `w` × `h`, computed once however many receivers ask.
    pub(super) fn scaled_for(&self, f: &Arc<MediaFrame>, w: usize, h: usize) -> Arc<Vec<Plane>> {
        let addr = Arc::as_ptr(f) as usize;
        let cell = {
            let mut c = self.scaled.lock_ok();
            let found = c.iter().find(|(weak, a, size, _)| {
                *a == addr && *size == (w, h) && weak.upgrade().is_some_and(|x| Arc::ptr_eq(&x, f))
            });
            match found {
                Some((_, _, _, cell)) => cell.clone(),
                None => {
                    let cell = Arc::new(std::sync::OnceLock::new());
                    if c.len() >= 16 {
                        c.pop_front();
                    }
                    c.push_back((Arc::downgrade(f), addr, (w, h), cell.clone()));
                    cell
                }
            }
        };
        cell.get_or_init(|| Arc::new(scaled(f, w, h))).clone()
    }

    /// `f`'s picture on the GPU shrunk there to `w` × `h` (its planes sized as
    /// [`scaled`](super::frames::scaled) sizes them, by whole factors), once per frame and
    /// size however many receivers take it; `None` where it can't be (another factor, no
    /// GPU): the CPU scales it, from its samples.
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_scaled_for(
        &self,
        f: &Arc<MediaFrame>,
        w: usize,
        h: usize,
    ) -> Option<crate::gpu::GpuPicture> {
        let pic = f.gpu.as_ref()?;
        let sizes = super::frames::scaled_sizes(f, w, h);
        let n = sizes.len().min(pic.planes.len());
        if !crate::gpu::PlaneShrink::fits_any(&pic.planes[..n], &sizes[..n]) {
            return None;
        }
        let addr = Arc::as_ptr(f) as usize;
        let cell = {
            let mut c = self.gpu_scaled.lock_ok();
            let found = c.iter().find(|(weak, a, size, _)| {
                *a == addr && *size == (w, h) && weak.upgrade().is_some_and(|x| Arc::ptr_eq(&x, f))
            });
            match found {
                Some((_, _, _, cell)) => cell.clone(),
                None => {
                    let cell = Arc::new(std::sync::OnceLock::new());
                    if c.len() >= 8 {
                        c.pop_front();
                    }
                    c.push_back((Arc::downgrade(f), addr, (w, h), cell.clone()));
                    cell
                }
            }
        };
        cell.get_or_init(|| {
            let hand = self.gpu.lock_ok().clone()?;
            let shrink = {
                let mut s = self.gpu_shrink.lock_ok();
                let key = crate::gpu::device_key(&hand.device);
                if s.as_ref().is_none_or(|(k, _)| key.is_none() || *k != key) {
                    *s = Some((key, Arc::new(crate::gpu::PlaneShrink::new(&hand.device))));
                }
                s.as_ref().expect("made above").1.clone()
            };
            let out = shrink.run(&hand, pic, &sizes[..n]);
            if out.is_none() {
                // Pictures with their samples again for a while (the CPU scales them).
                *self.gpu_shq_missed.lock_ok() = Some(Instant::now());
            }
            out
        })
        .clone()
    }

    pub(super) fn coded_audio_for(
        &self,
        content_id: Option<u64>,
    ) -> Option<Arc<crate::codec::CodedAudio>> {
        let id = content_id?;
        let c = self.coded_audio.lock_ok();
        c.iter().find(|(k, _)| *k == id).map(|(_, a)| a.clone())
    }
}

impl EncodeCache {
    const MAX: usize = 16;

    pub(super) fn get_or_encode(
        &self,
        key: Option<CacheKey>,
        enc: impl FnOnce() -> Vec<u8>,
    ) -> Arc<Vec<u8>> {
        let Some(key) = key else {
            self.shq_encodes.fetch_add(1, Ordering::Relaxed);
            return Arc::new(enc());
        };
        let cell = {
            let mut e = self.entries.lock_ok();
            match e.iter().find(|(k, _)| *k == key) {
                Some((_, c)) => c.clone(),
                None => {
                    if e.len() >= Self::MAX {
                        e.remove(0);
                    }
                    let c = Arc::new(std::sync::OnceLock::new());
                    e.push((key, c.clone()));
                    c
                }
            }
        };
        // Outside the cache lock: other pictures are encoded meanwhile.
        cell.get_or_init(|| {
            self.shq_encodes.fetch_add(1, Ordering::Relaxed);
            Arc::new(enc())
        })
        .clone()
    }
}

/// The GPU's SpeedHQ encoder, or `Err` (logged) when this GPU can't run it: SpeedHQ is then
/// encoded on the CPU, and the device's other users carry on.
#[cfg(feature = "gpu")]
fn make_gpu_shq(hand: &crate::gpu::GpuHandIn) -> Result<crate::shq::gpu_encode::GpuShqEncoder, ()> {
    crate::shq::gpu_encode::GpuShqEncoder::new(hand).map_err(|e| {
        crate::log::log(&format!(
            "SpeedHQ on the CPU: the GPU's encoder failed ({e})"
        ));
    })
}
