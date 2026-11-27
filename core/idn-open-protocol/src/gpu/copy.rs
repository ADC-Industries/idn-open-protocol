//! Copying planes between buffers on the GPU, each plane's rows from one layout to another.

use super::*;

// Copying planes between buffers on the GPU, each plane's rows from the source's layout to
// the destination's: into a slot (the application's rows may be packed otherwise), or out
// of one into another program's own layout (the switcher's compositor, 4:2:0 to 4:2:2).

pub(super) const COPY_WGSL: &str = r#"
struct Plane { src_off: u32, src_row: u32, dst_off: u32, dst_row: u32, words: u32, rows: u32, half: u32, pad: u32 };
struct Planes { p: array<Plane, 4> };
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> planes: Planes;

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let p = planes.p[g.z];
    if (g.x >= p.words || g.y >= p.rows) { return; }
    dst[p.dst_off + g.y * p.dst_row + g.x] = src[p.src_off + (g.y >> p.half) * p.src_row + g.x];
}
"#;

/// Copies planes between two buffers of a device on the GPU, recorded into the caller's
/// command encoder.
pub struct PlaneCopy {
    pub(super) pipeline: wgpu::ComputePipeline,
    pub(super) layout: wgpu::BindGroupLayout,
}

/// One plane to copy: where it is in the source, where it goes, and whether each source
/// row serves two destination rows (4:2:0 chroma into a 4:2:2 picture; the destination's
/// `height` then twice the source's).
#[derive(Clone, Copy, Debug)]
pub struct PlaneJob {
    /// The plane in the source buffer.
    pub from: PlaneAt,
    /// Where it goes in the destination buffer.
    pub to: PlaneAt,
    /// Each source row written twice (4:2:0 chroma into 4:2:2).
    pub double_rows: bool,
}

impl PlaneCopy {
    /// Its pipeline on `device`.
    pub fn new(device: &wgpu::Device) -> PlaneCopy {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu/1 plane copy"),
            source: wgpu::ShaderSource::Wgsl(COPY_WGSL.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty,
            count: None,
        };
        let storage = |read_only| wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu/1 plane copy"),
            entries: &[
                entry(0, storage(true)),
                entry(1, storage(false)),
                entry(
                    2,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gpu/1 plane copy"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("gpu/1 plane copy"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        PlaneCopy { pipeline, layout }
    }

    /// Records copying up to four planes of `src` into `dst` (both `STORAGE`; rows and
    /// starts on 4 bytes, each row's words fitting both rows). Its parameters go in a
    /// buffer of their own, so several copies may be recorded for one submission.
    pub fn encode(
        &self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        jobs: &[PlaneJob],
    ) -> io::Result<()> {
        if jobs.is_empty() || jobs.len() > 4 {
            return Err(io::Error::other("gpu/1: planes"));
        }
        let mut words = [0u32; 32];
        let (mut max_words, mut max_rows) = (0u32, 0u32);
        for (k, j) in jobs.iter().enumerate() {
            let (f, t) = (j.from, j.to);
            let w = f.width.div_ceil(4);
            let rows = if j.double_rows {
                f.height * 2
            } else {
                f.height
            }
            .min(t.height);
            if f.offset % 4 != 0
                || t.offset % 4 != 0
                || f.row_bytes % 4 != 0
                || t.row_bytes % 4 != 0
                || f.row_bytes < w * 4
                || t.row_bytes < w * 4
                || f.width != t.width
                || f.offset + f.row_bytes as u64 * f.height as u64 > src.size()
                || t.offset + t.row_bytes as u64 * rows as u64 > dst.size()
            {
                return Err(io::Error::other("gpu/1: a plane outside its buffer"));
            }
            words[k * 8..k * 8 + 7].copy_from_slice(&[
                (f.offset / 4) as u32,
                f.row_bytes / 4,
                (t.offset / 4) as u32,
                t.row_bytes / 4,
                w,
                rows,
                u32::from(j.double_rows),
            ]);
            max_words = max_words.max(w);
            max_rows = max_rows.max(rows);
        }
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu/1 plane copy"),
            size: 4 * 32,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        {
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            params
                .slice(..)
                .get_mapped_range_mut()
                .map_err(|e| io::Error::other(format!("gpu/1: {e}")))?
                .copy_from_slice(&bytes);
        }
        params.unmap();
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: src.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: dst.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(max_words.div_ceil(64), max_rows, jobs.len() as u32);
        Ok(())
    }
}
