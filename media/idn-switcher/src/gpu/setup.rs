//! The compositor made: its device (its own, or the application's), pipelines and buffers.

use super::*;

impl Gpu {
    /// A GPU compositor for `width` × `height`, on the best adapter there is (not a
    /// software one); `Err` says why there isn't one.
    pub fn new(width: usize, height: usize) -> Result<Gpu, String> {
        // Its buffers are sized for 2160p (an 8K program's would be refused by the GPU).
        if width * height > MAX_PIXELS {
            return Err(format!(
                "{width}x{height} is more than the GPU compositor's 2160p"
            ));
        }
        // Windows: Direct3D 12 (unless WGPU_BACKEND says): what shares pictures with the
        // monitor and OBS (§E.27; OBS's Direct3D 11 opens only Direct3D 12's textures);
        // its shaders by DXC (see instance_descriptor).
        let instance = wgpu::Instance::new(idn_open_protocol::gpu::instance_descriptor());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU: {e}"))?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu
            && !idn_common::switches::on("IDN_SWITCHER_SOFTWARE_GPU")
        {
            return Err(format!("only a software GPU ({})", info.name));
        }
        let name = format!("{} ({:?})", info.name, info.backend);
        // Six 2160p pictures with alpha in one buffer; twice that where the GPU has it,
        // for an HDR program's 16-bit pictures (R.24).
        let mut limits = wgpu::Limits::default();
        let need = (REGION_BYTES * LAYERS) as u64;
        let have = adapter.limits();
        if (have.max_storage_buffer_binding_size as u64) < need || have.max_buffer_size < need {
            return Err(format!("{name}: storage buffers too small"));
        }
        let ask = (2 * need)
            .min(have.max_buffer_size)
            .min(have.max_storage_buffer_binding_size as u64);
        limits.max_storage_buffer_binding_size = ask as _;
        limits.max_buffer_size = ask;
        let unified = info.device_type == wgpu::DeviceType::IntegratedGpu
            && adapter
                .features()
                .contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS)
            && !idn_common::switches::on("IDN_SWITCHER_GPU_COPIES");
        // f64 shaders where there are: pictures reconstructed as the CPU would.
        // (And importing DMA-BUF, or NV12 textures on Direct3D 12, where offered: pictures
        // the GPU decoded, kept there, §E.27.)
        let mut features = adapter.features()
            & (wgpu::Features::SHADER_F64
                | wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
                | wgpu::Features::TEXTURE_FORMAT_NV12);
        if unified {
            features |= wgpu::Features::MAPPABLE_PRIMARY_BUFFERS;
        }
        // (Shared memory readable by the GPU where it can: cameras on this machine,
        // pass S2.)
        let (device, queue) = idn_open_protocol::gpu::request_device(
            &adapter,
            &wgpu::DeviceDescriptor {
                label: Some("idn-switcher"),
                required_limits: limits,
                required_features: features,
                ..Default::default()
            },
        )
        .map_err(|e| format!("{name}: {e}"))?;
        let errors = Arc::new(std::sync::Mutex::new(None::<String>));
        {
            let errors = errors.clone();
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                let mut slot = errors.lock_ok();
                slot.get_or_insert_with(|| e.to_string());
            }));
        }
        Ok(Gpu::build(
            device, queue, name, unified, width, height, errors,
        ))
    }

    /// What makes another compositor on this one's device: kept by the engine so a frame
    /// clock started again after a panic composes on the same device, where the inputs
    /// keep their pictures (§E.27); a new device can't use their buffers.
    pub(crate) fn seed(&self) -> GpuSeed {
        GpuSeed {
            device: self.device.clone(),
            queue: self.queue.clone(),
            name: self.name.clone(),
            unified: self.unified,
            errors: self.errors.clone(),
        }
    }

    /// A new compositor on the device of [`Gpu::seed`]'s compositor (its buffers new).
    pub(crate) fn from_seed(seed: &GpuSeed, width: usize, height: usize) -> Gpu {
        Gpu::build(
            seed.device.clone(),
            seed.queue.clone(),
            seed.name.clone(),
            seed.unified,
            width,
            height,
            seed.errors.clone(),
        )
    }

    /// Composite on a wgpu device the host already owns, instead of opening one. Pass a
    /// clone of the host's `Device` and `Queue` (cheap, reference-counted handles) so the
    /// process keeps a single wgpu device — some Vulkan loaders race with more than one.
    /// `name` shows in the switcher state (empty for a default).
    ///
    /// The device must have been created with storage buffers large enough for six 2160p
    /// pictures (`max_storage_buffer_binding_size` and `max_buffer_size`); `Err` says if not.
    /// Enable `MAPPABLE_PRIMARY_BUFFERS` on an integrated GPU for the no-copy path, and
    /// `SHADER_F64` for CPU-exact reconstruction (it falls back to f32 without it). An
    /// adopted device keeps the host's own uncaptured-error handler; the compositor still
    /// reports stalls and failures through the switcher state and the frame results.
    pub fn with_device(
        device: wgpu::Device,
        queue: wgpu::Queue,
        name: &str,
        width: usize,
        height: usize,
    ) -> Result<Gpu, String> {
        if width * height > MAX_PIXELS {
            return Err(format!(
                "{width}x{height} is more than the GPU compositor's 2160p"
            ));
        }
        let need = (REGION_BYTES * LAYERS) as u64;
        let have = device.limits();
        if have.max_storage_buffer_binding_size < need || have.max_buffer_size < need {
            return Err(format!(
                "the device's storage buffers are too small: need {need} bytes for \
                 max_storage_buffer_binding_size and max_buffer_size"
            ));
        }
        let unified = device
            .features()
            .contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS)
            && !idn_common::switches::on("IDN_SWITCHER_GPU_COPIES");
        let name = if name.is_empty() {
            "host-provided device".to_owned()
        } else {
            name.to_owned()
        };
        // The host owns the device's uncaptured-error handler; leave it in place.
        let errors = Arc::new(std::sync::Mutex::new(None::<String>));
        Ok(Gpu::build(
            device, queue, name, unified, width, height, errors,
        ))
    }

    /// The common setup once a device exists: pipeline, buffers and state.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build(
        device: wgpu::Device,
        queue: wgpu::Queue,
        name: String,
        unified: bool,
        width: usize,
        height: usize,
        errors: Arc<std::sync::Mutex<Option<String>>>,
    ) -> Gpu {
        let recon = crate::idct::Recon::new(
            &device,
            crate::idct::Precision::for_features(device.features(), "IDN_SWITCHER_IDCT"),
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("compose"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../compose.wgsl").into()),
        });
        let entry = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let entries = [
            entry(0, wgpu::BufferBindingType::Uniform),
            entry(1, wgpu::BufferBindingType::Uniform),
            entry(2, wgpu::BufferBindingType::Storage { read_only: false }),
            entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
        ];
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("compose"),
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compose"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("compose"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let uniform = |label, size| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let globals = uniform("globals", 64);
        let layers = uniform("layers", (LAYERS * 112) as u64);
        let (pics, staging) = Gpu::pictures(&device, 4, unified);
        let (out, readback) = Gpu::outputs(&device, width, height, unified);
        let plane_copy = idn_open_protocol::gpu::PlaneCopy::new(&device);
        Gpu {
            device,
            queue,
            pipeline,
            layout,
            globals,
            layers,
            out,
            readback,
            staging,
            pics,
            region: 0,
            small_for: 0,
            held: [vec![None; LAYERS], vec![None; LAYERS]],
            held_raw: [vec![false; LAYERS], vec![false; LAYERS]],
            unified,
            copies: Vec::new(),
            recon,
            coefs: Default::default(),
            recon_jobs: Vec::new(),
            gpu_jobs: Vec::new(),
            plane_copy,
            clears: Vec::new(),
            next: 0,
            last_out: None,
            shrink: None,
            share_program: false,
            gpu_only: false,
            ring_only: false,
            mapped: std::cell::Cell::new(None),
            share: None,
            passed: None,
            in_flight: VecDeque::new(),
            size: (width, height),
            name,
            timing: (0.0, 0.0, 0.0),
            errors,
            hdr: None,
            raw_lift: None,
        }
    }

    /// The pictures' buffer and its two staging buffers (mapped from the start); unified,
    /// the two are the pictures' buffers themselves (and the first a stand-in).
    pub(super) fn pictures(
        device: &wgpu::Device,
        size: usize,
        unified: bool,
    ) -> (wgpu::Buffer, [Mapped; 2]) {
        let size = size.max(4) as u64;
        let pics = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pictures"),
            size: if unified { 4 } else { size },
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // (Unified: cleared on the GPU for pictures reconstructed into them.)
        let usage = if unified {
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST
        } else {
            wgpu::BufferUsages::COPY_SRC
        };
        let up = || {
            Mapped::new(
                device,
                "pictures up",
                size,
                wgpu::BufferUsages::MAP_WRITE | usage,
                true,
            )
        };
        (pics, [up(), up()])
    }

    /// Words of output a row: luma, chroma (8 pixels an invocation).
    pub(super) fn strides(width: usize) -> (usize, usize) {
        let groups = width.div_ceil(8);
        (groups * 2, groups)
    }

    /// The program's buffer and its two readback buffers; unified, the two are the
    /// program's buffers themselves (and the first a stand-in).
    pub(super) fn outputs(
        device: &wgpu::Device,
        width: usize,
        height: usize,
        unified: bool,
    ) -> (wgpu::Buffer, [Mapped; 2]) {
        let (ys, cs) = Gpu::strides(width);
        let bytes = ((ys + 2 * cs) * height * 4) as u64;
        let out = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("program"),
            size: if unified { 4 } else { bytes },
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let usage = if unified {
            wgpu::BufferUsages::STORAGE
        } else {
            wgpu::BufferUsages::COPY_DST
        };
        let down = || {
            Mapped::new(
                device,
                "program down",
                bytes,
                wgpu::BufferUsages::MAP_READ | usage,
                false,
            )
        };
        (out, [down(), down()])
    }
}

/// See [`Gpu::seed`].
#[derive(Clone)]
pub(crate) struct GpuSeed {
    device: wgpu::Device,
    queue: wgpu::Queue,
    name: String,
    unified: bool,
    errors: Arc<std::sync::Mutex<Option<String>>>,
}
