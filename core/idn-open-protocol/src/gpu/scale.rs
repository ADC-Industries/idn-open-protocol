//! Shrinking a picture on the GPU by whole factors, as the CPU's area scaling does.

use idn_common::sync::LockOk as _;
use super::*;

// Shrinking a picture on the GPU by whole factors: each sample the rounded mean of its
// block, as the CPU's area scaling gives it (`scale::area`), for a receiver that asks for a
// smaller picture (the 640-wide proxy) of one that is on the GPU: shrunk and encoded there
// with no picture read back (lab 349: a proxy of a 2160p program read it back each frame).

pub(super) const SHRINK_WGSL: &str = r#"
struct Plane { src_off: u32, src_row: u32, dst_off: u32, dst_row: u32, width: u32, rows: u32, fx: u32, fy: u32 };
struct Planes { p: array<Plane, 4> };
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> planes: Planes;

fn sample(at: u32) -> u32 {
    return (src[at >> 2u] >> ((at & 3u) * 8u)) & 255u;
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let p = planes.p[g.z];
    if (g.x * 4u >= p.width || g.y >= p.rows) { return; }
    let n = p.fx * p.fy;
    var word = 0u;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let x = g.x * 4u + i;
        if (x >= p.width) { break; }
        var sum = 0u;
        for (var j = 0u; j < p.fy; j = j + 1u) {
            let row = p.src_off * 4u + (g.y * p.fy + j) * p.src_row * 4u + x * p.fx;
            for (var k = 0u; k < p.fx; k = k + 1u) {
                sum = sum + sample(row + k);
            }
        }
        word = word | (((sum + n / 2u) / n) << (i * 8u));
    }
    dst[p.dst_off + g.y * p.dst_row + g.x] = word;
}
"#;

// Any smaller size: each sample the area mean of the source it covers, weighted by how
// much of each source sample is in it (as `scale::area`, in f32: within one step of it).
// Per output column and row its first source index and `span` weights, in `tabs`.
pub(super) const AREA_WGSL: &str = r#"
struct Plane { src_off: u32, src_row: u32, dst_off: u32, dst_row: u32, width: u32, rows: u32, col_tab: u32, row_tab: u32, span: u32, src_w: u32, src_h: u32, pad: u32 };
struct Planes { p: array<Plane, 4> };
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> planes: Planes;
@group(0) @binding(3) var<storage, read> tabs: array<u32>;

fn sample(at: u32) -> u32 {
    return (src[at >> 2u] >> ((at & 3u) * 8u)) & 255u;
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let p = planes.p[g.z];
    if (g.x * 4u >= p.width || g.y >= p.rows) { return; }
    let rb = p.row_tab + g.y * (p.span + 1u);
    let rfirst = tabs[rb];
    var word = 0u;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let x = g.x * 4u + i;
        if (x >= p.width) { break; }
        let cb = p.col_tab + x * (p.span + 1u);
        let cfirst = tabs[cb];
        var sum = 0.0;
        for (var j = 0u; j < p.span; j = j + 1u) {
            let wr = bitcast<f32>(tabs[rb + 1u + j]);
            let row = rfirst + j;
            if (wr == 0.0 || row >= p.src_h) { continue; }
            let at = p.src_off * 4u + row * p.src_row * 4u;
            for (var k = 0u; k < p.span; k = k + 1u) {
                let wc = bitcast<f32>(tabs[cb + 1u + k]);
                let col = cfirst + k;
                if (wc == 0.0 || col >= p.src_w) { continue; }
                sum = sum + wr * wc * f32(sample(at + col));
            }
        }
        word = word | (u32(clamp(floor(sum + 0.5), 0.0, 255.0)) << (i * 8u));
    }
    dst[p.dst_off + g.y * p.dst_row + g.x] = word;
}
"#;

/// For each of `n` outputs over `max` source samples: its first source index, then `span`
/// weights (each source sample's share of it; they sum to 1), as `tabs` words; and the
/// span (the most source samples one covers).
pub(super) fn area_tab(n: u32, max: u32, span: u32) -> Vec<u32> {
    let s = max as f64 / n as f64;
    let mut out = Vec::with_capacity((n * (span + 1)) as usize);
    for i in 0..n {
        let (a, b) = (i as f64 * s, (i + 1) as f64 * s);
        let first = (a.floor() as u32).min(max - 1);
        out.push(first);
        for k in 0..span {
            let at = (first + k) as f64;
            let w = ((at + 1.0).min(b) - at.max(a)).max(0.0) / s;
            out.push((w as f32).to_bits());
        }
    }
    out
}

/// The most source samples an output covers, `n` outputs over `max`.
pub(super) fn area_span(n: u32, max: u32) -> u32 {
    let s = max as f64 / n as f64;
    (0..n)
        .map(|i| {
            let (a, b) = (i as f64 * s, (i + 1) as f64 * s);
            (b.ceil() as u32).min(max) - (a.floor() as u32).min(max - 1)
        })
        .max()
        .unwrap_or(1)
}

/// Shrinks a picture on the GPU per plane ([`PlaneShrink::run`]): by whole factors, each
/// sample exactly the CPU's; to any smaller size, within one step of it.
pub struct PlaneShrink {
    pub(super) pipeline: wgpu::ComputePipeline,
    pub(super) layout: wgpu::BindGroupLayout,
    pub(super) area: wgpu::ComputePipeline,
    pub(super) area_layout: wgpu::BindGroupLayout,
    pub(super) out: std::sync::Mutex<Buffers>,
}

impl PlaneShrink {
    /// [`run`](Self::run) for sizes that aren't whole factors: by area weights.
    pub(super) fn run_area(
        &self,
        hand: &GpuHandIn,
        pic: &GpuPicture,
        sizes: &[(u32, u32)],
    ) -> Option<GpuPicture> {
        let from = &pic.planes[..sizes.len().min(pic.planes.len())];
        if !Self::fits_any(from, sizes) {
            return None;
        }
        let (to, bytes) = slot_layout(sizes);
        let mut words = [0u32; 48];
        let mut tabs: Vec<u32> = Vec::new();
        let (mut max_words, mut max_rows) = (0u32, 0u32);
        for (k, (f, t)) in from.iter().zip(&to).enumerate() {
            if f.offset + f.row_bytes as u64 * f.height as u64 > pic.buffer.size() {
                return None;
            }
            let span = area_span(t.width, f.width).max(area_span(t.height, f.height));
            let col_tab = tabs.len() as u32;
            tabs.extend(area_tab(t.width, f.width, span));
            let row_tab = tabs.len() as u32;
            tabs.extend(area_tab(t.height, f.height, span));
            words[k * 12..k * 12 + 12].copy_from_slice(&[
                (f.offset / 4) as u32,
                f.row_bytes / 4,
                (t.offset / 4) as u32,
                t.row_bytes / 4,
                t.width,
                t.height,
                col_tab,
                row_tab,
                span,
                f.width,
                f.height,
                0,
            ]);
            max_words = max_words.max(t.width.div_ceil(4));
            max_rows = max_rows.max(t.height);
        }
        let device = &hand.device;
        let buffer = self.out.lock_ok().get(device, bytes);
        let upload = |label, data: &[u32], usage| {
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (data.len() * 4) as u64,
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bytes: Vec<u8> = data.iter().flat_map(|w| w.to_le_bytes()).collect();
            hand.queue.write_buffer(&b, 0, &bytes);
            b
        };
        let params = upload("gpu/1 area", &words, wgpu::BufferUsages::UNIFORM);
        let tabs = upload("gpu/1 area weights", &tabs, wgpu::BufferUsages::STORAGE);
        fn entry(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: b.as_entire_binding(),
            }
        }
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.area_layout,
            entries: &[
                entry(0, &pic.buffer),
                entry(1, &buffer),
                entry(2, &params),
                entry(3, &tabs),
            ],
        });
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.area);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(max_words.div_ceil(64), max_rows, from.len() as u32);
        }
        hand.queue.submit([enc.finish()]);
        Some(GpuPicture { buffer, planes: to })
    }
}

// SAFETY: fields of the types checked above (with BufferPool), and std's Send + Sync.
unsafe impl Send for PlaneShrink {}
// SAFETY: as above.
unsafe impl Sync for PlaneShrink {}

impl PlaneShrink {
    /// Its pipeline on `device`.
    pub fn new(device: &wgpu::Device) -> PlaneShrink {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu/1 shrink"),
            source: wgpu::ShaderSource::Wgsl(SHRINK_WGSL.into()),
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
            label: Some("gpu/1 shrink"),
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
            label: Some("gpu/1 shrink"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("gpu/1 shrink"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        // The any-size pass: the same, with the weights' table.
        let area_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu/1 area"),
            source: wgpu::ShaderSource::Wgsl(AREA_WGSL.into()),
        });
        let area_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu/1 area"),
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
                entry(3, storage(true)),
            ],
        });
        let area_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gpu/1 area"),
            bind_group_layouts: &[Some(&area_layout)],
            immediate_size: 0,
        });
        let area = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("gpu/1 area"),
            layout: Some(&area_pl),
            module: &area_module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        PlaneShrink {
            pipeline,
            layout,
            area,
            area_layout,
            out: std::sync::Mutex::new(Buffers::new(
                "gpu/1 shrunk",
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                4,
            )),
        }
    }

    /// Whether `from` planes shrink to `sizes` at all (each no larger, rows and starts on
    /// 4 bytes): by whole factors or not.
    pub fn fits_any(from: &[PlaneAt], sizes: &[(u32, u32)]) -> bool {
        from.len() == sizes.len()
            && (1..=4).contains(&from.len())
            && from.iter().zip(sizes).all(|(f, &(w, h))| {
                w > 0
                    && h > 0
                    && w <= f.width
                    && h <= f.height
                    && f.offset % 4 == 0
                    && f.row_bytes % 4 == 0
            })
    }

    /// Whether `from` planes shrink to `sizes` by whole factors (each plane its own).
    pub fn fits(from: &[PlaneAt], sizes: &[(u32, u32)]) -> bool {
        from.len() == sizes.len()
            && (1..=4).contains(&from.len())
            && from.iter().zip(sizes).all(|(f, &(w, h))| {
                w > 0
                    && h > 0
                    && f.width % w == 0
                    && f.height % h == 0
                    && f.offset % 4 == 0
                    && f.row_bytes % 4 == 0
            })
    }

    /// `pic`'s planes shrunk to `sizes` (Y, Cb, Cr …; see [`fits_any`](Self::fits_any))
    /// into a picture of its own on the GPU, laid out as [`slot_layout`] lays them;
    /// submitted (what is recorded on the queue after it sees it done): by whole factors
    /// where they are, else by area weights. `None` where it can't.
    pub fn run(
        &self,
        hand: &GpuHandIn,
        pic: &GpuPicture,
        sizes: &[(u32, u32)],
    ) -> Option<GpuPicture> {
        let from = &pic.planes[..sizes.len().min(pic.planes.len())];
        if !Self::fits(from, sizes) {
            return self.run_area(hand, pic, sizes);
        }
        let (to, bytes) = slot_layout(sizes);
        let mut words = [0u32; 32];
        let (mut max_words, mut max_rows) = (0u32, 0u32);
        for (k, (f, t)) in from.iter().zip(&to).enumerate() {
            if f.offset + f.row_bytes as u64 * f.height as u64 > pic.buffer.size() {
                return None;
            }
            words[k * 8..k * 8 + 8].copy_from_slice(&[
                (f.offset / 4) as u32,
                f.row_bytes / 4,
                (t.offset / 4) as u32,
                t.row_bytes / 4,
                t.width,
                t.height,
                f.width / t.width,
                f.height / t.height,
            ]);
            max_words = max_words.max(t.width.div_ceil(4));
            max_rows = max_rows.max(t.height);
        }
        let device = &hand.device;
        let buffer = self.out.lock_ok().get(device, bytes);
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu/1 shrink"),
            size: 4 * 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bytes_of: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        hand.queue.write_buffer(&params, 0, &bytes_of);
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: pic.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(max_words.div_ceil(64), max_rows, from.len() as u32);
        }
        hand.queue.submit([enc.finish()]);
        Some(GpuPicture { buffer, planes: to })
    }
}

/// Copies `from` planes of `src` into a slot laid out as `to`, submitted.
pub(super) struct Repack(PlaneCopy);

impl Repack {
    pub(super) fn new(device: &wgpu::Device) -> Repack {
        Repack(PlaneCopy::new(device))
    }

    pub(super) fn run(
        &self,
        hand: &GpuHandIn,
        src: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        from: &[PlaneAt],
        to: &[PlaneAt],
    ) -> io::Result<wgpu::SubmissionIndex> {
        if from.len() != to.len() {
            return Err(io::Error::other("gpu/1: planes"));
        }
        let jobs: Vec<PlaneJob> = from
            .iter()
            .zip(to)
            .map(|(&from, &to)| PlaneJob {
                from,
                to,
                double_rows: false,
            })
            .collect();
        let mut enc = hand.device.create_command_encoder(&Default::default());
        self.0.encode(&hand.device, &mut enc, src, dst, &jobs)?;
        Ok(hand.queue.submit([enc.finish()]))
    }
}
