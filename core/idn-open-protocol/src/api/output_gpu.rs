//! An output's pictures on the GPU (§E.27): sent from the application's device, offered to
//! readers on this machine, tapped as GPU textures.

use super::*;

impl Output {
    /// Whether receivers on this machine take this output's pictures through shared memory
    /// now (§E.26): while they do, a picture kept on the GPU for them is worth keeping
    /// ([`send_planes_gpu`](Output::send_planes_gpu)). Cheap: for every frame.
    pub fn readers_on_this_machine(&self) -> bool {
        self.sender.on_this_machine()
    }

    /// Whether a picture sent on the GPU too ([`send_planes_gpu`](Output::send_planes_gpu))
    /// is of use now: a receiver on this machine takes pictures there, or the next one
    /// starts that (it makes the GPU ring, which is then offered). A picture that isn't on
    /// the GPU already is worth uploading for it only then. Cheap: for every frame.
    #[cfg(feature = "gpu")]
    pub fn wants_gpu_pictures(&self) -> bool {
        self.sender.wants_gpu_pictures()
    }

    /// The GPU this application draws or computes with (its wgpu device), for pictures sent
    /// with [`send_planes_gpu`](Output::send_planes_gpu); `None` stops using it.
    #[cfg(feature = "gpu")]
    pub fn use_gpu(&self, gpu: Option<crate::gpu::GpuHandIn>) {
        self.sender.use_gpu(gpu);
    }

    /// As [`send_planes`](Output::send_planes), with the same picture on the GPU given with
    /// [`use_gpu`](Output::use_gpu) too: in `src` (`STORAGE`), its planes where `from` says
    /// (rows and starts on 4 bytes). Receivers on this machine on the same GPU that take
    /// pictures there ([`InputConfig::gpu`](crate::input::InputConfig::gpu)) get it GPU to
    /// GPU, and while only such receivers read this output's shared memory its samples
    /// aren't copied there at all (`gpu/1`, §E.27). `src` has been copied when this returns.
    /// Linux with Vulkan for now; elsewhere the same as `send_planes`.
    #[cfg(feature = "gpu")]
    pub fn send_planes_gpu(
        &self,
        f: PlanarFrame,
        src: &wgpu::Buffer,
        from: &[crate::gpu::PlaneAt],
    ) -> Result<(), SendError> {
        self.send_planes_by(f, true, |m| self.sender.send_gpu(m, src, from))
    }

    /// As [`send_planes_gpu`](Output::send_planes_gpu), `f`'s planes with their sizes and
    /// no samples: the picture is on the GPU only, for receivers that take it there,
    /// while [`samples_wanted`](Output::samples_wanted) is false. A receiver or tap that
    /// comes meanwhile gets no picture from it (the next with samples is its first).
    #[cfg(feature = "gpu")]
    pub fn send_planes_gpu_sizes(
        &self,
        f: PlanarFrame,
        src: &wgpu::Buffer,
        from: &[crate::gpu::PlaneAt],
    ) -> Result<(), SendError> {
        self.send_planes_by(f, false, |m| self.sender.send_gpu(m, src, from))
    }

    /// As [`send_planes_filled`](Output::send_planes_filled), with the same picture on the
    /// GPU in `src` too, as [`send_planes_gpu_sizes`](Output::send_planes_gpu_sizes) has
    /// it. `src` is used on the GPU before `fill` is called, so it must not be the mapped
    /// memory `fill` reads.
    #[cfg(feature = "gpu")]
    pub fn send_planes_gpu_filled(
        &self,
        f: PlanarFrame,
        src: &wgpu::Buffer,
        from: &[crate::gpu::PlaneAt],
        fill: &mut crate::machine::PlaneFill,
    ) -> Result<(), SendError> {
        self.send_planes_by(f, false, |m| {
            self.sender.send_gpu_filled(m, src, from, fill)
        })
    }

    /// Whether everything taking this output's pictures takes them on the GPU now: this
    /// process's inputs given a GPU ([`InputConfig::gpu`](crate::input::InputConfig::gpu))
    /// and nothing else (no receiver over the network or this machine's shared memory, no
    /// recorder). A picture then needn't come to this memory: see
    /// [`send_planes_gpu_only`](Output::send_planes_gpu_only).
    #[cfg(feature = "gpu")]
    pub fn gpu_only(&self) -> bool {
        self.sender.gpu_only()
    }

    /// As [`gpu_only`](Output::gpu_only), with what takes the pictures otherwise (for
    /// logs).
    #[cfg(feature = "gpu")]
    pub fn gpu_only_why(&self) -> Result<(), &'static str> {
        self.sender.gpu_only_why()
    }

    /// A picture that is on the GPU only (a decoder's, kept there: §E.27), for this
    /// process's inputs that take it there, while [`gpu_only`](Output::gpu_only) holds:
    /// `f`'s planes have their sizes and no samples. A receiver that comes meanwhile gets
    /// no picture from it (the sender's next picture has its samples again).
    #[cfg(feature = "gpu")]
    pub fn send_planes_gpu_only(
        &self,
        f: PlanarFrame,
        picture: crate::gpu::GpuPicture,
    ) -> Result<(), SendError> {
        self.send_planes_by(f, false, |mut m| {
            m.gpu = Some(picture);
            self.sender.send(m)
        })
    }

    /// As [`tap_while`](Output::tap_while), taking pictures on the application's GPU
    /// ([`use_gpu`](Output::use_gpu)) where the output has them there: each frame's
    /// [`TappedFrame::gpu`] is then the tap's own copy, and its `planes` may have no
    /// samples. Such a tap doesn't make [`samples_wanted`](Output::samples_wanted) true:
    /// a recorder that encodes from the GPU leaves the picture there.
    #[cfg(feature = "gpu")]
    pub fn tap_gpu_while(
        &self,
        wanted: impl Fn() -> bool + Send + Sync + 'static,
        f: impl Fn(&TappedFrame) + Send + Sync + 'static,
    ) -> Tap {
        self.tap_by(Some(Arc::new(wanted)), true, f)
    }

    /// Whether a tap of this process on the GPU (a recorder encoding from there) wants
    /// pictures now: each must then come with its copy on the GPU or with its samples.
    #[cfg(feature = "gpu")]
    pub fn gpu_taps_wanting(&self) -> bool {
        self.sender.gpu_taps_wanting()
    }

    /// The application's GPU this output was given ([`use_gpu`](Output::use_gpu)).
    #[cfg(feature = "gpu")]
    pub fn gpu(&self) -> Option<crate::gpu::GpuHandIn> {
        self.sender.gpu_hand()
    }

    /// [`gpu`](Output::gpu) asked later, as often as needed (the GPU may be given after a
    /// recorder taps the output); `None` once the output is gone.
    #[cfg(feature = "gpu")]
    pub fn gpu_lookup(&self) -> impl Fn() -> Option<crate::gpu::GpuHandIn> + Send + Sync + 'static {
        let sender = Arc::downgrade(&self.sender);
        move || sender.upgrade()?.gpu_hand()
    }
}
