//! The HDR program on the GPU (roadmap R.24 stages 1 and 2; idn-switcher.md §S.34): beside
//! the 8-bit program (which the preview, multiview, monitor and receivers on this machine
//! keep), a second pass of the same compositor (`compose.wgsl` built with `HDR`) composes
//! the program in the HDR space from 16-bit pictures and writes it as P216, read back for
//! the program's output. Each picture is lifted into that space once, when it comes into
//! its region (`lift.wgsl`: SDR as BT.2408 maps it). Any program size: the passes are
//! dispatched over its width and height.

use crate::frame::Frame;
use idn_open_protocol::color::Transfer;

/// A picture to lift this frame: its region's base and its planes' offsets (sample
/// indices), its size and whether it has alpha.
#[derive(Clone, Copy)]
pub(crate) struct LiftJob {
    pub base: u32,
    pub offs: [u32; 4],
    pub w: u32,
    pub h: u32,
    pub alpha: bool,
    /// Not a picture but a luma wipe's pattern: its levels copied as they are (scaled
    /// to 16 bits), not lifted, so the HDR wipe runs as the SDR one.
    pub raw: bool,
}

/// The HDR pass's state.
pub(crate) struct HdrPath {
    pub transfer: Transfer,
    lift: wgpu::ComputePipeline,
    lift_layout: wgpu::BindGroupLayout,
    pub compose: wgpu::ComputePipeline,
    /// The 16-bit pictures (unified: one per set of pictures, as the 8-bit ones).
    pub pics: Vec<wgpu::Buffer>,
    pics_size: u64,
    /// The P216 program and its two readback buffers (unified: the readbacks are written).
    pub out: wgpu::Buffer,
    pub readback: [crate::gpu::Mapped; 2],
    out_size: (usize, usize),
    /// The layers' table with border colours in the HDR space.
    pub layers: wgpu::Buffer,
    /// This frame's lifts, and its pictures that came with their own 16-bit planes.
    pub jobs: Vec<LiftJob>,
    pub direct: Vec<(LiftJob, std::sync::Arc<crate::frame::Hdr16>)>,
    /// Frames submitted with an HDR pass: their readback index.
    pub pending: std::collections::VecDeque<usize>,
    /// The last HDR program read back (P216), for the program's output.
    pub last: Option<Vec<u16>>,
    /// The 16-bit pictures fit the device's buffers (twice the 8-bit ones: 2160p
    /// pictures may not): else the program is made HDR on the CPU meanwhile.
    pub fits: bool,
}

/// The 16-bit pictures' size for an 8-bit one of `bytes`.
fn pics16(bytes: u64) -> u64 {
    (bytes * 2).max(4)
}

impl HdrPath {
    pub fn new(
        device: &wgpu::Device,
        compose_layout: &wgpu::BindGroupLayout,
        transfer: Transfer,
    ) -> HdrPath {
        let src = include_str!("compose.wgsl")
            .replace("const HDR: bool = false;", "const HDR: bool = true;")
            .replace("const S: f32 = 1.0;", "const S: f32 = 256.0;");
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("compose hdr"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compose hdr"),
            bind_group_layouts: &[Some(compose_layout)],
            immediate_size: 0,
        });
        let compose = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("compose hdr"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
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
        let lift_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lift"),
            entries: &[
                entry(0, wgpu::BufferBindingType::Uniform),
                entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
                entry(2, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let lift_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lift"),
            source: wgpu::ShaderSource::Wgsl(
                (idn_open_protocol::color::transfer::wgsl() + include_str!("lift.wgsl")).into(),
            ),
        });
        let lift = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("lift"),
            layout: Some(
                &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lift"),
                    bind_group_layouts: &[Some(&lift_layout)],
                    immediate_size: 0,
                }),
            ),
            module: &lift_module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let small = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let rb = || {
            crate::gpu::Mapped::new(
                device,
                "program hdr down",
                4,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                false,
            )
        };
        HdrPath {
            transfer,
            lift,
            lift_layout,
            compose,
            pics: Vec::new(),
            pics_size: 0,
            out: small("program hdr"),
            readback: [rb(), rb()],
            out_size: (0, 0),
            layers: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("layers hdr"),
                size: (crate::gpu::LAYERS * 112) as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            jobs: Vec::new(),
            direct: Vec::new(),
            pending: Default::default(),
            last: None,
            fits: true,
        }
    }

    /// The 16-bit pictures for 8-bit ones of `bytes` (two sets on one memory); made again
    /// (and so empty: every picture lifted again) when that grows.
    pub fn ensure_pics(&mut self, device: &wgpu::Device, bytes: u64, unified: bool) -> bool {
        let want = pics16(bytes);
        let lim = device.limits();
        if want > lim.max_buffer_size || want > lim.max_storage_buffer_binding_size {
            if self.fits {
                idn_open_protocol::log::log(&format!(
                    "switcher: the HDR program on the CPU while its pictures ({} MB at 16 bits) are more than the GPU's buffers take",
                    want >> 20
                ));
            }
            self.fits = false;
            self.pics.clear();
            self.pics_size = 0;
            return false;
        }
        if !self.fits {
            idn_open_protocol::log::log("switcher: the HDR program on the GPU again");
        }
        self.fits = true;
        let sets = if unified { 2 } else { 1 };
        if self.pics_size == want && self.pics.len() == sets {
            return false;
        }
        self.pics = (0..sets)
            .map(|_| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("pictures hdr"),
                    size: want,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .collect();
        self.pics_size = want;
        true
    }

    /// The P216 program's buffers for a `w` × `h` program.
    pub fn ensure_out(&mut self, device: &wgpu::Device, w: usize, h: usize, unified: bool) {
        if self.out_size == (w, h) {
            return;
        }
        let bytes = (w * h * 4).max(4) as u64;
        self.out = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("program hdr"),
            size: if unified { 4 } else { bytes },
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let usage = if unified {
            wgpu::BufferUsages::STORAGE
        } else {
            wgpu::BufferUsages::COPY_DST
        };
        let rb = || {
            crate::gpu::Mapped::new(
                device,
                "program hdr down",
                bytes,
                wgpu::BufferUsages::MAP_READ | usage,
                false,
            )
        };
        self.readback = [rb(), rb()];
        self.out_size = (w, h);
        self.pending.clear();
    }

    /// The layers' table for the HDR pass: as the 8-bit one, its PiP borders' colours
    /// (8-bit SDR) lifted (each layer 112 bytes; the colour at 80..92 as three f32).
    pub fn table(&self, sdr: &[u8]) -> Vec<u8> {
        let mut t = sdr.to_vec();
        for l in t.as_chunks_mut::<112>().0 {
            let f = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            let (y, cb, cr) = (f(&l[80..84]), f(&l[84..88]), f(&l[88..92]));
            if y == 0.0 && cb == 0.0 && cr == 0.0 {
                continue;
            }
            let v = idn_open_protocol::color::to_hdr::to_hdr_exact(
                y.clamp(0.0, 255.0) as u8,
                cb.clamp(0.0, 255.0) as u8,
                cr.clamp(0.0, 255.0) as u8,
                self.transfer,
                203.0,
            );
            for (k, x) in v.iter().enumerate() {
                l[80 + 4 * k..84 + 4 * k].copy_from_slice(&(*x as f32).to_le_bytes());
            }
        }
        t
    }

    /// The lifts of this frame's new pictures, recorded: each region cleared, then lifted
    /// from `src` (the 8-bit pictures the compositor reads) into `dst` (16-bit).
    pub fn record_lifts(
        &self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        region: usize,
    ) {
        let tf = match self.transfer {
            Transfer::Hlg => 1.0f32,
            _ => 0.0,
        };
        for j in &self.jobs {
            let at = j.base as u64 * 2;
            let size = (region as u64 * 2).min(dst.size().saturating_sub(at)) & !3;
            if size > 0 {
                enc.clear_buffer(dst, at, Some(size));
            }
            let cw = Frame::chroma_width(j.w as usize) as u32;
            let mut u = Vec::with_capacity(48);
            for x in j.offs {
                u.extend_from_slice(&x.to_le_bytes());
            }
            for x in [j.w, j.h, cw, j.alpha as u32] {
                u.extend_from_slice(&x.to_le_bytes());
            }
            for x in [tf, 203.0, j.raw as u32 as f32, 0.0] {
                u.extend_from_slice(&x.to_le_bytes());
            }
            use wgpu::util::DeviceExt;
            let ub = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("lift job"),
                contents: &u,
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lift"),
                layout: &self.lift_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: ub.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: dst.as_entire_binding(),
                    },
                ],
            });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lift"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.lift);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(cw.div_ceil(64), j.h, 1);
        }
    }

    /// This frame's pictures with their own 16-bit planes, written into their regions of
    /// `dst` as they are (sample index i at byte 2i).
    pub fn write_direct(&self, queue: &wgpu::Queue, dst: &wgpu::Buffer) {
        for (j, h) in &self.direct {
            for (k, plane) in [&h.y, &h.cb, &h.cr].into_iter().enumerate() {
                let at = j.offs[k] as u64 * 2;
                let mut bytes: Vec<u8> = Vec::with_capacity(plane.len() * 2 + 2);
                for v in plane.iter() {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                // (Whole words: a plane of an odd count gets a zero after it.)
                if !bytes.len().is_multiple_of(4) {
                    bytes.extend_from_slice(&[0, 0]);
                }
                if at + bytes.len() as u64 <= dst.size() {
                    queue.write_buffer(dst, at, &bytes);
                }
            }
        }
    }
}
