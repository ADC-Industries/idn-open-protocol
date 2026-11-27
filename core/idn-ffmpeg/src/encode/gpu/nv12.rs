//! A picture on the application's GPU drawn as NV12 into a buffer, scaled to an
//! encoder's size (bilinear): the step both ways into an encoder that can't be drawn into
//! directly share (NVENC through CUDA, Vulkan Video through FFmpeg's own device).

use crate::gpu_convert::err;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::io;

const NV12_WGSL: &str = r#"
struct P { yoff: u32, yrow: u32, sw: u32, sh: u32, cboff: u32, croff: u32, crow: u32, scw: u32, sch: u32, dw: u32, dh: u32, pitch: u32 };
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> p: P;

fn at(off: u32, row: u32, x: u32, y: u32) -> f32 {
    let i = off + y * row + x;
    return f32((src[i >> 2u] >> ((i & 3u) * 8u)) & 255u);
}
fn sample(off: u32, row: u32, w: u32, h: u32, fx: f32, fy: f32) -> f32 {
    let x = clamp(fx, 0.0, f32(w - 1u));
    let y = clamp(fy, 0.0, f32(h - 1u));
    let x0 = u32(floor(x));
    let y0 = u32(floor(y));
    let x1 = min(x0 + 1u, w - 1u);
    let y1 = min(y0 + 1u, h - 1u);
    let ax = x - f32(x0);
    let ay = y - f32(y0);
    return mix(mix(at(off, row, x0, y0), at(off, row, x1, y0), ax), mix(at(off, row, x0, y1), at(off, row, x1, y1), ax), ay);
}
fn byte(v: f32) -> u32 { return u32(round(clamp(v, 0.0, 255.0))); }

// One word (four bytes) of NV12 each: Y rows `pitch` apart, then the CbCr rows.
@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let words = p.pitch / 4u;
    if (g.x >= words) { return; }
    let ch = (p.dh + 1u) / 2u;
    var w = 0u;
    if (g.y < p.dh) {
        let fy = (f32(g.y) + 0.5) * f32(p.sh) / f32(p.dh) - 0.5;
        for (var i = 0u; i < 4u; i = i + 1u) {
            let x = g.x * 4u + i;
            if (x < p.dw) {
                let fx = (f32(x) + 0.5) * f32(p.sw) / f32(p.dw) - 0.5;
                w = w | (byte(sample(p.yoff, p.yrow, p.sw, p.sh, fx, fy)) << (8u * i));
            }
        }
        dst[g.y * words + g.x] = w;
    } else if (g.y < p.dh + ch) {
        let y = g.y - p.dh;
        let cw = (p.dw + 1u) / 2u;
        let fy = (f32(y) + 0.5) * f32(p.sch) / f32(ch) - 0.5;
        for (var j = 0u; j < 2u; j = j + 1u) {
            let cx = g.x * 2u + j;
            if (cx < cw) {
                let fx = (f32(cx) + 0.5) * f32(p.scw) / f32(cw) - 0.5;
                w = w | (byte(sample(p.cboff, p.crow, p.scw, p.sch, fx, fy)) << (16u * j));
                w = w | (byte(sample(p.croff, p.crow, p.scw, p.sch, fx, fy)) << (16u * j + 8u));
            }
        }
        dst[g.y * words + g.x] = w;
    }
}
"#;

/// A picture drawn as NV12 (scaled to the encoder's size) into a buffer: the step both
/// ways into an encoder that can't be drawn into (NVENC through CUDA, Vulkan Video
/// through FFmpeg's own device) share.
pub(crate) struct Nv12Draw {
    hand: GpuHandIn,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
}

/// The rows' pitch for an NV12 picture `dw` wide: 256-byte aligned.
pub(crate) fn nv12_pitch(dw: u32) -> u32 {
    (dw.div_ceil(4) * 4).next_multiple_of(256)
}

/// An NV12 buffer's size: Y rows, then half as many CbCr rows.
pub(crate) fn nv12_size(dh: u32, pitch: u32) -> u64 {
    u64::from(pitch) * u64::from(dh + dh.div_ceil(2))
}

impl Nv12Draw {
    pub(crate) fn new(hand: &GpuHandIn) -> Nv12Draw {
        let d = &hand.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("into NVENC"),
            source: wgpu::ShaderSource::Wgsl(NV12_WGSL.into()),
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
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("into NVENC"),
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
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("into NVENC"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("into NVENC"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("into NVENC"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Nv12Draw {
            hand: hand.clone(),
            pipeline,
            layout,
            params,
        }
    }

    /// `pic` drawn into `out` as NV12 of `dw` × `dh`, rows `pitch` apart, the GPU done
    /// with it on return.
    pub(crate) fn draw(
        &self,
        pic: &GpuPicture,
        out: &wgpu::Buffer,
        (dw, dh): (u32, u32),
        pitch: u32,
    ) -> io::Result<()> {
        let [y, cb, cr] = &pic.planes[..] else {
            return Err(err("a picture without three planes"));
        };
        let words = [
            y.offset as u32,
            y.row_bytes,
            y.width,
            y.height,
            cb.offset as u32,
            cr.offset as u32,
            cb.row_bytes,
            cb.width,
            cb.height,
            dw,
            dh,
            pitch,
        ];
        let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.hand.queue.write_buffer(&self.params, 0, &bytes);
        let group = self.hand.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: pic.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: out.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.params.as_entire_binding(),
                },
            ],
        });
        let mut enc = self.hand.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups((pitch / 4).div_ceil(64), dh + dh.div_ceil(2), 1);
        }
        let sub = self.hand.queue.submit([enc.finish()]);
        self.hand
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(sub),
                timeout: Some(std::time::Duration::from_secs(2)),
            })
            .map_err(err)?;
        Ok(())
    }
}

