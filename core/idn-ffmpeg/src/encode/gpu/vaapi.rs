//! A picture on the application's GPU written straight into a VA-API encoder's input
//! surface (spec/extensions/idn-extensions.md §E.27): the surface exported as DMA-BUF, its
//! Y and interleaved CbCr planes imported into the application's Vulkan device as render
//! targets (once per surface of the encoder's pool), and drawn from the picture's planar
//! Y'CbCr, scaled to the encoder's size on the way (bilinear; 4:2:2 or 4:2:0 in, NV12
//! out). Nothing comes to the CPU's memory: a 2160p recording read 17 MB back and wrote
//! 12 MB into the surface each frame before, and converted it to NV12 on the CPU.

use crate::ffi;
use crate::gpu_convert::err;
use crate::dmabuf::{Shape, import};
use ffmpeg_sys_next as ff;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::collections::HashMap;
use std::io;

const DRAW_WGSL: &str = r#"
struct P { yoff: u32, yrow: u32, sw: u32, sh: u32, cboff: u32, croff: u32, crow: u32, scw: u32, sch: u32, dw: u32, dh: u32, pad: u32 };
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<uniform> p: P;

fn at(off: u32, row: u32, x: u32, y: u32) -> f32 {
    let i = off + y * row + x;
    return f32((src[i >> 2u] >> ((i & 3u) * 8u)) & 255u);
}

// Bilinear at (fx, fy) in a plane of w × h.
fn sample(off: u32, row: u32, w: u32, h: u32, fx: f32, fy: f32) -> f32 {
    let x = clamp(fx, 0.0, f32(w - 1u));
    let y = clamp(fy, 0.0, f32(h - 1u));
    let x0 = u32(floor(x));
    let y0 = u32(floor(y));
    let x1 = min(x0 + 1u, w - 1u);
    let y1 = min(y0 + 1u, h - 1u);
    let ax = x - f32(x0);
    let ay = y - f32(y0);
    let top = mix(at(off, row, x0, y0), at(off, row, x1, y0), ax);
    let bot = mix(at(off, row, x0, y1), at(off, row, x1, y1), ax);
    return mix(top, bot, ay) / 255.0;
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_y(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let fx = pos.x * f32(p.sw) / f32(p.dw) - 0.5;
    let fy = pos.y * f32(p.sh) / f32(p.dh) - 0.5;
    return vec4<f32>(sample(p.yoff, p.yrow, p.sw, p.sh, fx, fy), 0.0, 0.0, 1.0);
}

@fragment
fn fs_c(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let cw = f32((p.dw + 1u) / 2u);
    let ch = f32((p.dh + 1u) / 2u);
    let fx = pos.x * f32(p.scw) / cw - 0.5;
    let fy = pos.y * f32(p.sch) / ch - 0.5;
    return vec4<f32>(
        sample(p.cboff, p.crow, p.scw, p.sch, fx, fy),
        sample(p.croff, p.crow, p.scw, p.sch, fx, fy),
        0.0,
        1.0,
    );
}
"#;

pub(crate) struct VaWriter {
    hand: GpuHandIn,
    luma: wgpu::RenderPipeline,
    chroma: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    /// Each surface's Y and CbCr planes as render targets, by surface.
    cache: HashMap<usize, (wgpu::Texture, wgpu::Texture)>,
    /// The encoder's pool the cache is for, held (as `va_gpu`'s).
    frames: *mut ff::AVBufferRef,
}

// SAFETY: wgpu's types are Send and Sync; the frames reference is FFmpeg's refcounted
// buffer, used only by the writer's owner (the encoder).
unsafe impl Send for VaWriter {}

impl Drop for VaWriter {
    fn drop(&mut self) {
        self.cache.clear();
        if !self.frames.is_null() {
            // SAFETY: our own reference, taken with av_buffer_ref.
            unsafe { ff::av_buffer_unref(&mut self.frames) };
        }
    }
}

impl VaWriter {
    /// On `hand`'s device, which must import DMA-BUF.
    pub(crate) fn new(hand: &GpuHandIn) -> io::Result<VaWriter> {
        if !hand
            .device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Err(err("this device doesn't import DMA-BUF"));
        }
        let d = &hand.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("into the encoder"),
            source: wgpu::ShaderSource::Wgsl(DRAW_WGSL.into()),
        });
        let entry = |binding, ty, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty,
            count: None,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("into the encoder"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    wgpu::ShaderStages::FRAGMENT,
                ),
                entry(
                    1,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    wgpu::ShaderStages::FRAGMENT,
                ),
            ],
        });
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("into the encoder"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |fs: &str, format| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("into the encoder"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let luma = pipeline("fs_y", wgpu::TextureFormat::R8Unorm);
        let chroma = pipeline("fs_c", wgpu::TextureFormat::Rg8Unorm);
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("into the encoder"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(VaWriter {
            hand: hand.clone(),
            luma,
            chroma,
            layout,
            params,
            cache: HashMap::new(),
            frames: std::ptr::null_mut(),
        })
    }

    /// `pic` drawn into the encoder's surface `hw` (VA-API, NV12) of `size`, the GPU done
    /// with it on return.
    pub(crate) fn write(
        &mut self,
        hw: &ffi::Frame,
        pic: &GpuPicture,
        (dw, dh): (usize, usize),
    ) -> io::Result<()> {
        let (dw, dh) = (dw as u32, dh as u32);
        let [y, cb, cr] = &pic.planes[..] else {
            return Err(err("a picture without three planes"));
        };
        // SAFETY: FFmpeg calls on a live surface; the DRM mapping lives until the GPU's
        // work on it is waited for.
        unsafe {
            let f = hw.0;
            let frames = (*f).hw_frames_ctx;
            if frames.is_null() {
                return Err(err("a surface without its frames"));
            }
            if self.frames.is_null() || (*self.frames).data != (*frames).data {
                self.cache.clear();
                if !self.frames.is_null() {
                    ff::av_buffer_unref(&mut self.frames);
                }
                self.frames = ff::av_buffer_ref(frames);
            }
            // The surface: VA-API's id (`data[3]`), else the frame's image (a Vulkan
            // frame's `AVVkFrame`, `data[0]`): Vulkan Video's frames are written the same way.
            let surface = if (*f).data[3].is_null() {
                (*f).data[0] as usize
            } else {
                (*f).data[3] as usize
            };
            let m = ffi::Frame::new()?;
            (*m.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let flags = ff::AV_HWFRAME_MAP_READ as i32 | ff::AV_HWFRAME_MAP_WRITE as i32;
            if ff::av_hwframe_map(m.0, f, flags) < 0 {
                return Err(err("mapping an encoder surface as DMA-BUF"));
            }
            if !self.cache.contains_key(&surface) {
                let d = &*((*m.0).data[0] as *const ff::AVDRMFrameDescriptor);
                let (planes, shape) = crate::dmabuf::planes(d)?;
                if shape != Shape::SemiPlanar || planes.len() < 2 {
                    return Err(err(format!("an encoder surface laid out as {shape:?}")));
                }
                let (cw, ch) = (dw.div_ceil(2), dh.div_ceil(2));
                let ty = import(
                    &self.hand,
                    &planes[0],
                    wgpu::TextureFormat::R8Unorm,
                    (dw, dh),
                    true,
                )?;
                let tc = import(
                    &self.hand,
                    &planes[1],
                    wgpu::TextureFormat::Rg8Unorm,
                    (cw, ch),
                    true,
                )?;
                if self.cache.len() >= 64 {
                    self.cache.clear();
                }
                self.cache.insert(surface, (ty, tc));
            }
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
                0,
            ];
            let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.hand.queue.write_buffer(&self.params, 0, &bytes);
            let group = self
                .hand
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: pic.buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: self.params.as_entire_binding(),
                        },
                    ],
                });
            let (ty, tc) = &self.cache[&surface];
            let mut enc = self.hand.device.create_command_encoder(&Default::default());
            for (target, pipeline) in [(ty, &self.luma), (tc, &self.chroma)] {
                let view = target.create_view(&Default::default());
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("into the encoder"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
            let sub = self.hand.queue.submit([enc.finish()]);
            self.hand
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(sub),
                    timeout: Some(std::time::Duration::from_secs(2)),
                })
                .map_err(err)?;
            drop(m);
        }
        Ok(())
    }
}
