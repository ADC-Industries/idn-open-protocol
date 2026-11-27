//! A sender's pictures on the GPU (§E.27): taps on this machine that read them there, and
//! pictures copied down only when a receiver needs them on the CPU.

use super::*;

impl Sender {
    /// As [`tap`](Self::tap), for one that takes pictures on the GPU too (§E.27).
    #[cfg(feature = "gpu")]
    pub fn tap_gpu(&self, f: FrameTap) -> u64 {
        let id = self.tap(f);
        self.gpu_taps.lock_ok().insert(id);
        id
    }

    /// [`tap_gpu`](Self::tap_gpu), wanting pictures only while `wanted` says so.
    #[cfg(feature = "gpu")]
    pub fn tap_gpu_while(&self, wanted: Arc<dyn Fn() -> bool + Send + Sync>, f: FrameTap) -> u64 {
        let id = self.tap_gpu(f);
        self.tap_wanted.lock_ok().insert(id, wanted);
        id
    }

    /// A tap on the GPU wants pictures now (an idle recorder doesn't).
    #[cfg(feature = "gpu")]
    pub(crate) fn gpu_taps_wanting(&self) -> bool {
        let wanted = self.tap_wanted.lock_ok();
        self.gpu_taps
            .lock_ok()
            .iter()
            .any(|id| wanted.get(id).is_none_or(|w| w()))
    }

    /// The application's GPU ([`use_gpu`](Self::use_gpu)).
    #[cfg(feature = "gpu")]
    pub fn gpu_hand(&self) -> Option<crate::gpu::GpuHandIn> {
        self.cache.gpu.lock_ok().clone()
    }

    /// `src`'s planes copied on the GPU into a buffer of the taps' own (kept for the next
    /// pictures once no tap holds it): a picture sent from the application's GPU, for this
    /// process's taps that take pictures there. `None` without a GPU.
    #[cfg(feature = "gpu")]
    pub(super) fn gpu_copy(
        &self,
        src: &wgpu::Buffer,
        from: &[crate::gpu::PlaneAt],
    ) -> Option<crate::gpu::GpuPicture> {
        let hand = self.cache.gpu.lock_ok().clone()?;
        // Repacked by the compute pass gpu/1's rings use: the application's buffer may be
        // one the shader reads and nothing copies from (an integrated GPU's).
        let sizes: Vec<(u32, u32)> = from.iter().map(|p| (p.width, p.height)).collect();
        let (to, end) = crate::gpu::slot_layout(&sizes);
        let buffer = self.gpu_copies.0.lock_ok().get(&hand.device, end);
        let jobs: Vec<crate::gpu::PlaneJob> = from
            .iter()
            .zip(&to)
            .map(|(&from, &to)| crate::gpu::PlaneJob {
                from,
                to,
                double_rows: false,
            })
            .collect();
        let mut enc = hand.device.create_command_encoder(&Default::default());
        {
            let mut copier = self.gpu_copies.1.lock_ok();
            // (By the device's native handle: wgpu's ids start again in a new instance,
            // as after the compositor's GPU is made again.)
            let key = crate::gpu::device_key(&hand.device);
            if copier
                .as_ref()
                .is_none_or(|(d, _)| key.is_none() || *d != key)
            {
                *copier = Some((key, crate::gpu::PlaneCopy::new(&hand.device)));
            }
            let (_, c) = copier.as_ref().expect("made above");
            c.encode(&hand.device, &mut enc, src, &buffer, &jobs).ok()?;
        }
        hand.queue.submit([enc.finish()]);
        Some(crate::gpu::GpuPicture { buffer, planes: to })
    }

    /// Whether everything that takes this output's pictures now takes them on the GPU:
    /// this process's inputs given a GPU, and nothing else (no session with a receiver,
    /// no multicast, no other tap). A picture then needn't come to
    /// this memory at all.
    #[cfg(feature = "gpu")]
    pub fn gpu_only(&self) -> bool {
        self.gpu_only_why().is_ok()
    }

    /// As [`gpu_only`](Self::gpu_only), with what takes the pictures otherwise.
    #[cfg(feature = "gpu")]
    pub fn gpu_only_why(&self) -> Result<(), &'static str> {
        // (A shared encoder only feeds sessions, counted below.)
        if self.multicast.is_some() {
            return Err("multicast");
        }
        if !self.clients.lock_ok().is_empty() {
            return Err("a receiver's session");
        }
        // (The GPU taps copied first, never held with `taps`: `samples_wanted_but` takes
        // them in the other order; bug run 4.)
        let gpu = self.gpu_taps.lock_ok().clone();
        let taps = self.taps.lock_ok();
        if taps.is_empty() {
            return Err("nothing");
        }
        if taps.iter().all(|t| gpu.contains(&t.0)) {
            Ok(())
        } else {
            Err("a tap or an input not on the GPU")
        }
    }

    /// The application's GPU, for pictures it sends on the GPU too ([`send_gpu`](Self::send_gpu)).
    #[cfg(feature = "gpu")]
    pub fn use_gpu(&self, hand: Option<crate::gpu::GpuHandIn>) {
        *self.cache.gpu.lock_ok() = hand.clone();
        if let Some(h) = &self.codec_hub {
            *h.gpu.lock_ok() = hand.clone();
        }
        // The GPU's SpeedHQ encoder made now, apart: made when the first receiver came, its
        // pipelines' making held up the application's GPU work (a 2160p switcher's frame
        // took 26-107 ms as a network receiver joined, lab 377-A).
        if let Some(hand) = hand
            && crate::node::video::shq_on_gpu_allowed()
        {
            let cache = self.cache.clone();
            let _ = std::thread::Builder::new()
                .name("idn-gpu-shq-warm".into())
                .spawn(move || cache.warm_gpu_shq(&hand));
        }
    }

    /// As [`send`](Self::send), with the picture also in `src` on the GPU given with
    /// [`use_gpu`](Self::use_gpu) (planes `from`, `STORAGE`): receivers on this machine on
    /// the same GPU take it from there (§E.27). `src` is copied before this returns.
    #[cfg(feature = "gpu")]
    pub fn send_gpu(&self, mut f: MediaFrame, src: &wgpu::Buffer, from: &[crate::gpu::PlaneAt]) {
        // This process's taps on the GPU (a recorder, an input) get a copy there: the
        // application's buffer holds the next picture by the time they use it.
        // And for receivers whose SpeedHQ is encoded there (§E.27).
        if f.gpu.is_none()
            && (self.gpu_taps_wanting()
                || (self.shq_sessions() && crate::node::video::shq_on_gpu_allowed())
                || self.codec_gpu_sessions())
        {
            f.gpu = self.gpu_copy(src, from);
        }
        let f = self.numbered(f);
        self.cache.gpu_picture(&f, src, from);
        self.dispatch(f);
    }

    /// Whether receivers on this machine may read this output's shared memory now
    /// (`machine/1`, §E.26): a sender with its pictures on the GPU need only keep them
    /// there for sharing (§E.27) while this holds.
    pub fn on_this_machine(&self) -> bool {
        self.cache.machine_live.load(Ordering::Relaxed)
    }

    /// Whether a picture sent with [`send_gpu`](Self::send_gpu) is of use now: a tap of
    /// this process on the GPU wants pictures (a recorder encoding from there), or
    /// receivers on this machine may read the rings, and either the GPU ring isn't made
    /// yet (the next GPU picture makes it and it is offered) or one of them takes pictures
    /// there.
    #[cfg(feature = "gpu")]
    pub fn wants_gpu_pictures(&self) -> bool {
        if self.cache.gpu.lock_ok().is_none() {
            return false;
        }
        // (Receivers whose SpeedHQ this output encodes: encoded there, §E.27.)
        if self.gpu_taps_wanting()
            || (self.shq_sessions() && crate::node::video::shq_on_gpu_wanted())
            || self.codec_gpu_sessions()
        {
            return true;
        }
        if !self.on_this_machine() {
            return false;
        }
        let m = self.cache.machine.lock_ok();
        !m.gpu_failed && (m.gpu.is_none() || m.gpu_takers > 0)
    }

    /// As [`send_filled`](Self::send_filled), the picture on the GPU in `src` too, as
    /// [`send_gpu`](Self::send_gpu) has it (`src` isn't the memory `fill` reads: it is
    /// used on the GPU first).
    #[cfg(feature = "gpu")]
    pub fn send_gpu_filled(
        &self,
        mut f: MediaFrame,
        src: &wgpu::Buffer,
        from: &[crate::gpu::PlaneAt],
        fill: &mut crate::machine::PlaneFill,
    ) {
        if f.gpu.is_none()
            && (self.gpu_taps_wanting()
                || (self.shq_sessions() && crate::node::video::shq_on_gpu_allowed())
                || self.codec_gpu_sessions())
        {
            f.gpu = self.gpu_copy(src, from);
        }
        let f = self.numbered(f);
        // The GPU ring first: the picture ring's header says where it is there.
        self.cache.gpu_picture(&f, src, from);
        self.ring_filled(&f, fill);
        self.dispatch(f);
    }
}
