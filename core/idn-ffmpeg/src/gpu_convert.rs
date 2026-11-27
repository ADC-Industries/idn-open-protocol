//! Pictures the GPU decoded, converted there into planar Y'CbCr laid out as `gpu/1`'s
//! slots are (spec/extensions/idn-extensions.md §E.27): the step VA-API's (`va_gpu`) and
//! Direct3D 12's (`d3d12_gpu`) decoded surfaces share, once each surface's planes are
//! textures on the application's device.

use idn_open_protocol::gpu::{GpuHandIn, GpuPicture, slot_layout};
use std::io;
use std::sync::Arc;

pub(crate) fn err(m: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("pictures on the GPU: {m}"))
}

const CONVERT_WGSL: &str = r#"
struct P { w: u32, h: u32, cw: u32, ch: u32, yrow: u32, crow: u32, cb: u32, cr: u32, mode: u32, p1: u32, p2: u32, p3: u32 };
@group(0) @binding(0) var t0: texture_2d<f32>;
@group(0) @binding(1) var t1: texture_2d<f32>;
@group(0) @binding(2) var t2: texture_2d<f32>;
@group(0) @binding(3) var<storage, read_write> dst: array<u32>;
@group(0) @binding(4) var<uniform> p: P;

fn byte(v: f32) -> u32 { return u32(round(clamp(v, 0.0, 1.0) * 255.0)); }

// RGB (mode 3: a screen's picture, BGRx or RGBx as a texture): BT.709, limited range.
fn rgb(x: u32, y: u32) -> vec3<f32> {
    return textureLoad(t0, vec2<i32>(i32(min(x, p.w - 1u)), i32(y)), 0).rgb;
}
fn y709(c: vec3<f32>) -> f32 { return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)); }

// Luma at (x, y). Packed (YUYV, an RGBA texel per two pixels): Y0 in red, Y1 in blue.
fn luma(x: u32, y: u32) -> f32 {
    if (p.mode == 3u) {
        return (16.0 + 219.0 * y709(rgb(x, y))) / 255.0;
    }
    if (p.mode == 2u) {
        let t = textureLoad(t0, vec2<i32>(i32(x / 2u), i32(y)), 0);
        return select(t.r, t.b, (x & 1u) == 1u);
    }
    return textureLoad(t0, vec2<i32>(i32(x), i32(y)), 0).r;
}

// Cb (`k` 1) or Cr (`k` 2) at chroma (x, y).
fn chroma(k: u32, x: u32, y: u32) -> f32 {
    if (p.mode == 3u) {
        // 4:2:2: the two pixels' average.
        let c = (rgb(2u * x, y) + rgb(2u * x + 1u, y)) * 0.5;
        let l = y709(c);
        return select((128.0 + 224.0 * (c.r - l) / 1.5748) / 255.0, (128.0 + 224.0 * (c.b - l) / 1.8556) / 255.0, k == 1u);
    }
    let at = vec2<i32>(i32(x), i32(y));
    if (p.mode == 0u) {
        // Interleaved: the first byte (Cb) in red, the second (Cr) in green.
        let c = textureLoad(t1, at, 0);
        return select(c.g, c.r, k == 1u);
    }
    if (p.mode == 1u) {
        return select(textureLoad(t2, at, 0).r, textureLoad(t1, at, 0).r, k == 1u);
    }
    // Packed: Cb in green, Cr in alpha of the texel for pixels 2x, 2x+1.
    let t = textureLoad(t0, at, 0);
    return select(t.a, t.g, k == 1u);
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let x4 = g.x * 4u;
    let y = g.y;
    var word = 0u;
    if (g.z == 0u) {
        if (x4 >= p.w || y >= p.h) { return; }
        for (var i = 0u; i < 4u; i = i + 1u) {
            word = word | (byte(luma(min(x4 + i, p.w - 1u), y)) << (8u * i));
        }
        dst[y * p.yrow + g.x] = word;
    } else {
        if (x4 >= p.cw || y >= p.ch) { return; }
        for (var i = 0u; i < 4u; i = i + 1u) {
            word = word | (byte(chroma(g.z, min(x4 + i, p.cw - 1u), y)) << (8u * i));
        }
        let at = select(p.cr, p.cb, g.z == 1u);
        dst[at + y * p.crow + g.x] = word;
    }
}
"#;

/// The compute pass from a surface's plane textures to a slot-shaped buffer.
pub(crate) struct Converter {
    hand: GpuHandIn,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    /// Buffers handed out, taken back when no picture holds them.
    pool: idn_open_protocol::gpu::Buffers,
}

impl Converter {
    pub(crate) fn new(hand: &GpuHandIn) -> Converter {
        let d = &hand.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("decoded to planes"),
            source: wgpu::ShaderSource::Wgsl(CONVERT_WGSL.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty,
            count: None,
        };
        let tex = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("decoded to planes"),
            entries: &[
                entry(0, tex),
                entry(1, tex),
                entry(2, tex),
                entry(
                    3,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    4,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        });
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("decoded to planes"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("decoded to planes"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("decoded to planes"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Converter {
            hand: hand.clone(),
            pipeline,
            layout,
            params,
            pool: idn_open_protocol::gpu::Buffers::new(
                "decoded on the GPU",
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                6,
            ),
        }
    }

    fn buffer(&mut self, size: u64) -> Arc<wgpu::Buffer> {
        self.pool.get(&self.hand.device, size)
    }

    /// The planes in `tex` (Y; then chroma by `mode`: 0 interleaved Cb Cr in the second,
    /// 1 Cb and Cr in the second and third, 2 packed YUYV in the first; 3: RGB in the
    /// first, made 4:2:2 BT.709 limited range) into a picture of
    /// `size` with chroma `chroma`, the GPU done with it on return.
    pub(crate) fn convert(
        &mut self,
        tex: [&wgpu::TextureView; 3],
        mode: u32,
        (w, h): (u32, u32),
        (cw, ch): (u32, u32),
    ) -> io::Result<GpuPicture> {
        let (planes, bytes) = slot_layout(&[(w, h), (cw, ch), (cw, ch)]);
        let out = self.buffer(bytes);
        let words = [
            w,
            h,
            cw,
            ch,
            planes[0].row_bytes / 4,
            planes[1].row_bytes / 4,
            (planes[1].offset / 4) as u32,
            (planes[2].offset / 4) as u32,
            mode,
            0,
            0,
            0,
        ];
        let bytes_p: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        let q = &self.hand.queue;
        q.write_buffer(&self.params, 0, &bytes_p);
        let view = |k: usize| wgpu::BindGroupEntry {
            binding: k as u32,
            resource: wgpu::BindingResource::TextureView(tex[k]),
        };
        let group = self
            .hand
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    view(0),
                    view(1),
                    view(2),
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: out.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: self.params.as_entire_binding(),
                    },
                ],
            });
        let mut enc = self.hand.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(w.div_ceil(4).div_ceil(64), h.max(ch), 3);
        }
        let sub = q.submit([enc.finish()]);
        self.hand
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(sub),
                timeout: Some(std::time::Duration::from_secs(2)),
            })
            .map_err(err)?;
        Ok(GpuPicture {
            buffer: out,
            planes,
        })
    }
}
