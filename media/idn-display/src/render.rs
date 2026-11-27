//! Drawing a picture on a surface: placed as its view says (fitted with its aspect kept by
//! default, black around; turned, filled, stretched, inside an overscan, a video wall's
//! tile; idn-display.md §D.8), a still picture in its place (the fallback's logo or
//! slides), the identify overlay over it, and a thumbnail of it all.

use crate::receive::{Kind, Picture};
use crate::view::{Placement, View};
use idn_open_protocol::gpu::block_on;

/// The view's part of the turned picture over the viewport (`area`: left, top, width,
/// height of it, 0 to 1) and its turn (`turn.x`: 0, 1, 2 or 3 quarter turns clockwise),
/// shared by both shaders.
const VIEW: &str = r#"
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
struct V { area: vec4<f32>, turn: vec4<f32> };

fn turned(a: vec2<f32>, r: f32) -> vec2<f32> {
    if (r < 0.5) { return a; }
    if (r < 1.5) { return vec2<f32>(a.y, 1.0 - a.x); }
    if (r < 2.5) { return vec2<f32>(1.0 - a.x, 1.0 - a.y); }
    return vec2<f32>(1.0 - a.y, a.x);
}

fn place(i: u32, v: V) -> Out {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = turned(v.area.xy + uv * v.area.zw, v.turn.x);
    return o;
}
"#;

/// A fullscreen triangle, sampling the picture over the viewport it is given.
const SHADER: &str = r#"
@group(0) @binding(0) var pic: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> vw: V;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    return place(i, vw);
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(pic, smp, i.uv).rgb, 1.0);
}

// The overlay: its own alpha, blended over.
@fragment
fn fs_over(i: Out) -> @location(0) vec4<f32> {
    return textureSample(pic, smp, i.uv);
}
"#;

const SHADER_YUV: &str = r#"
struct K { a: vec4<f32>, b: vec4<f32> };
@group(0) @binding(0) var py: texture_2d<f32>;
@group(0) @binding(1) var pcb: texture_2d<f32>;
@group(0) @binding(2) var pcr: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;
@group(0) @binding(4) var<uniform> k: K;
@group(0) @binding(5) var<uniform> vw: V;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    return place(i, vw);
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    // k.a: Y' scale, Y' offset, Cr→R, Cb→G; k.b: Cr→G, Cb→B.
    let y = (textureSample(py, smp, i.uv).r - k.a.y) * k.a.x;
    let cb = textureSample(pcb, smp, i.uv).r - 128.0 / 255.0;
    let cr = textureSample(pcr, smp, i.uv).r - 128.0 / 255.0;
    let rgb = vec3<f32>(y + k.a.z * cr, y + k.a.w * cb + k.b.x * cr, y + k.b.y * cb);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// The shader's terms for `c` (SDR): Y' scale, Y' offset (of 1), Cr→R, Cb→G, Cr→G, Cb→B.
pub fn terms(c: &idn_open_protocol::color::ColorConfig) -> [f32; 8] {
    let (kr, kb) = c.matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    let (ys, cs, yo) = if c.limited {
        (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
    } else {
        (1.0, 1.0, 0.0)
    };
    [
        ys as f32,
        yo as f32,
        (2.0 * (1.0 - kr) * cs) as f32,
        (-2.0 * (1.0 - kb) * kb / kg * cs) as f32,
        (-2.0 * (1.0 - kr) * kr / kg * cs) as f32,
        (2.0 * (1.0 - kb) * cs) as f32,
        0.0,
        0.0,
    ]
}

/// The view uniform's bytes for a placement: the area and the turn.
fn view_bytes(p: &Placement) -> Vec<u8> {
    let [a0, b0, a1, b1] = p.uv;
    [
        a0,
        b0,
        a1 - a0,
        b1 - b0,
        (p.rotation / 90) as f32,
        0.0,
        0.0,
        0.0,
    ]
    .iter()
    .flat_map(|v| v.to_le_bytes())
    .collect()
}

/// The picture as the GPU holds it.
enum Held {
    Rgba(wgpu::Texture),
    /// Y, Cb, Cr textures and the terms' buffer.
    Yuv([wgpu::Texture; 3], wgpu::Buffer),
}

/// A picture on the GPU: its textures, their bind group (with its view's buffer), its
/// size and its chroma planes' size.
struct Shown {
    held: Held,
    group: wgpu::BindGroup,
    size: (u32, u32),
    chroma: Option<(u32, u32)>,
    view: wgpu::Buffer,
}

/// What is drawn: the feed's picture, the still one (logo, slide), or black.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Feed,
    Still,
    Black,
}

/// For games (`--game`): pictures shown as soon as they're drawn (R.25 stage 2).
pub static GAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Immediate, else mailbox, for games; else the screen's refresh (FIFO).
fn present_mode(game: bool, modes: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    [wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox]
        .into_iter()
        .find(|m| game && modes.contains(m))
        .unwrap_or(wgpu::PresentMode::Fifo)
}

/// The pipelines for one target format: the RGBA picture, the Y'CbCr one, the overlay.
struct Pipes {
    rgba: wgpu::RenderPipeline,
    yuv: wgpu::RenderPipeline,
    over: wgpu::RenderPipeline,
}

impl Pipes {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        layout: &wgpu::BindGroupLayout,
        layout_yuv: &wgpu::BindGroupLayout,
    ) -> Pipes {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("display"),
            source: wgpu::ShaderSource::Wgsl(format!("{VIEW}{SHADER}").into()),
        });
        let module_yuv = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("display yuv"),
            source: wgpu::ShaderSource::Wgsl(format!("{VIEW}{SHADER_YUV}").into()),
        });
        let pipe = |module: &wgpu::ShaderModule,
                    layout: &wgpu::BindGroupLayout,
                    fs: &str,
                    blend: Option<wgpu::BlendState>| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("display"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Pipes {
            rgba: pipe(&module, layout, "fs", None),
            yuv: pipe(&module_yuv, layout_yuv, "fs", None),
            over: pipe(
                &module,
                layout,
                "fs_over",
                Some(wgpu::BlendState::ALPHA_BLENDING),
            ),
        }
    }
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipes: Pipes,
    /// For thumbnails (RGBA), made the first time one is asked for.
    thumb_pipes: Option<Pipes>,
    layout: wgpu::BindGroupLayout,
    layout_yuv: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// The overlay's: its pixels stay sharp.
    nearest: wgpu::Sampler,
    /// The feed's picture, the still one, the overlay.
    picture: Option<Shown>,
    still: Option<Shown>,
    overlay: Option<Shown>,
    /// What is drawn, and how the feed's picture is placed.
    pub mode: Mode,
    pub view: View,
    /// The GPU's name, for the log.
    pub adapter: String,
}

/// A texture of `size` and `format` to draw from.
fn texture(
    device: &wgpu::Device,
    (w, h): (u32, u32),
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("picture"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Writes `data` (rows of `w * bpp` bytes) into `tex`.
fn write(queue: &wgpu::Queue, tex: &wgpu::Texture, data: &[u8], (w, h): (u32, u32), bpp: u32) {
    queue.write_texture(
        tex.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * bpp),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    )
}

impl Renderer {
    /// On `surface` (made from `instance`), `width × height` pixels. A real GPU first,
    /// else a software one (a machine without a GPU still shows something).
    pub fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Renderer, String> {
        let pick = |fallback| {
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: fallback,
                compatible_surface: Some(&surface),
                apply_limit_buckets: false,
            }))
        };
        let adapter = pick(false)
            .or_else(|_| pick(true))
            .map_err(|e| format!("no GPU for the display: {e}"))?;
        let name = adapter.get_info().name;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("idn-display"),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        let caps = surface.get_capabilities(&adapter);
        // Not an sRGB format: the picture's samples are already gamma-coded, and go to the
        // screen as they are.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or("the surface takes no format")?;
        // For games (`--game`, R.25 stage 2): each picture on the screen at once, a tear
        // rather than a wait for the screen's next refresh (else the next one free).
        let game = GAME.load(std::sync::atomic::Ordering::Relaxed);
        let present_mode = present_mode(game, &caps.present_modes);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: if game { 1 } else { 2 },
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let uniform_entry = |binding, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                tex_entry(0),
                sampler_entry(1),
                uniform_entry(2, wgpu::ShaderStages::VERTEX),
            ],
        });
        let layout_yuv = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                sampler_entry(3),
                uniform_entry(4, wgpu::ShaderStages::FRAGMENT),
                uniform_entry(5, wgpu::ShaderStages::VERTEX),
            ],
        });
        let pipes = Pipes::new(&device, format, &layout, &layout_yuv);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor::default());
        Ok(Renderer {
            device,
            queue,
            surface,
            config,
            pipes,
            thumb_pipes: None,
            layout,
            layout_yuv,
            sampler,
            nearest,
            picture: None,
            still: None,
            overlay: None,
            mode: Mode::Feed,
            view: View::default(),
            adapter: name,
        })
    }

    /// This renderer's GPU, for pictures handed over GPU to GPU (§E.27).
    pub fn hand_in(&self) -> idn_open_protocol::gpu::GpuHandIn {
        idn_open_protocol::gpu::GpuHandIn {
            device: self.device.clone(),
            queue: self.queue.clone(),
        }
    }

    /// The surface's size.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// The surface's new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 && (width, height) != (self.config.width, self.config.height) {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// An RGBA picture's slot (`slot` kept when it is one of this size), sampled with
    /// `sampler`.
    fn rgba_slot(&self, slot: Option<Shown>, size: (u32, u32), nearest: bool) -> Shown {
        if let Some(s) = slot
            && matches!(s.held, Held::Rgba(_))
            && s.size == size
        {
            return s;
        }
        let tex = texture(&self.device, size, wgpu::TextureFormat::Rgba8Unorm);
        let view = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("view"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tv = tex.create_view(&Default::default());
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&tv),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(if nearest {
                        &self.nearest
                    } else {
                        &self.sampler
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: view.as_entire_binding(),
                },
            ],
        });
        Shown {
            held: Held::Rgba(tex),
            group,
            size,
            chroma: None,
            view,
        }
    }

    /// The picture to show from now on.
    pub fn upload(&mut self, p: &Picture) {
        let size = (p.width, p.height);
        match &p.kind {
            Kind::Rgba(rgba) => {
                let old = self.picture.take();
                let slot = self.rgba_slot(old, size, false);
                if let Held::Rgba(tex) = &slot.held {
                    write(&self.queue, tex, rgba, size, 4);
                }
                self.picture = Some(slot);
            }
            Kind::Planes { chroma, rgb, .. } | Kind::Gpu { chroma, rgb, .. } => {
                let fresh = !matches!(&self.picture, Some(s) if matches!(s.held, Held::Yuv(..)) && s.size == size && s.chroma == Some(*chroma));
                if fresh {
                    let r8 = wgpu::TextureFormat::R8Unorm;
                    let t = [
                        texture(&self.device, size, r8),
                        texture(&self.device, *chroma, r8),
                        texture(&self.device, *chroma, r8),
                    ];
                    let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("terms"),
                        size: 32,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let view = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("view"),
                        size: 32,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let views: Vec<wgpu::TextureView> = t
                        .iter()
                        .map(|t| t.create_view(&Default::default()))
                        .collect();
                    let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: &self.layout_yuv,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&views[0]),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&views[1]),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(&views[2]),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: buf.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 5,
                                resource: view.as_entire_binding(),
                            },
                        ],
                    });
                    self.picture = Some(Shown {
                        held: Held::Yuv(t, buf),
                        group,
                        size,
                        chroma: Some(*chroma),
                        view,
                    });
                }
                if let Some(Shown {
                    held: Held::Yuv(t, buf),
                    ..
                }) = &self.picture
                {
                    match &p.kind {
                        Kind::Planes { y, cb, cr, .. } => {
                            write(&self.queue, &t[0], y, size, 1);
                            write(&self.queue, &t[1], cb, *chroma, 1);
                            write(&self.queue, &t[2], cr, *chroma, 1);
                        }
                        // On this GPU already (§E.27): copied into the textures there.
                        Kind::Gpu { picture, .. } => {
                            let mut enc = self.device.create_command_encoder(&Default::default());
                            for (k, tex) in t.iter().enumerate() {
                                picture.copy_plane(&mut enc, k, tex);
                            }
                            self.queue.submit([enc.finish()]);
                        }
                        Kind::Rgba(_) => {}
                    }
                    let k = terms(rgb);
                    let bytes: Vec<u8> = k.iter().flat_map(|v| v.to_le_bytes()).collect();
                    self.queue.write_buffer(buf, 0, &bytes);
                }
            }
        }
    }

    /// The still picture (an RGBA logo or slide), or none.
    pub fn set_still(&mut self, p: Option<&Picture>) {
        match p {
            Some(Picture {
                width,
                height,
                kind: Kind::Rgba(rgba),
            }) => {
                let old = self.still.take();
                let slot = self.rgba_slot(old, (*width, *height), false);
                if let Held::Rgba(tex) = &slot.held {
                    write(&self.queue, tex, rgba, (*width, *height), 4);
                }
                self.still = Some(slot);
            }
            _ => self.still = None,
        }
    }

    /// The overlay (straight RGBA drawn over the whole surface, sharp), or none.
    pub fn set_overlay(&mut self, o: Option<(u32, u32, &[u8])>) {
        match o {
            Some((w, h, rgba)) if w > 0 && h > 0 => {
                let old = self.overlay.take();
                let slot = self.rgba_slot(old, (w, h), true);
                if let Held::Rgba(tex) = &slot.held {
                    write(&self.queue, tex, rgba, (w, h), 4);
                }
                self.overlay = Some(slot);
            }
            _ => self.overlay = None,
        }
    }

    /// The feed's picture size, once there is one.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        self.picture.as_ref().map(|s| s.size)
    }

    /// Where the picture is drawn on the surface now (x, y, width, height in pixels), once
    /// there is one: for placing a pointer on it (remote control, R.25).
    pub fn picture_rect(&self) -> Option<(f32, f32, f32, f32)> {
        let s = self.picture.as_ref()?;
        let [x, y, w, h] =
            crate::view::place((self.config.width, self.config.height), s.size, &self.view)?
                .viewport;
        Some((x, y, w, h))
    }

    /// Where the picture goes on the surface: as large as fits, its aspect kept, centred.
    #[cfg(test)]
    pub fn fit(surface: (u32, u32), picture: (u32, u32)) -> (f32, f32, f32, f32) {
        let [x, y, w, h] = crate::view::place(surface, picture, &View::default())
            .expect("a picture fits")
            .viewport;
        (x, y, w, h)
    }

    /// The draws for a target of `size`: (slot, pipeline kind, placement), back to front.
    fn layers(&self, size: (u32, u32)) -> Vec<(&Shown, u8, Placement)> {
        let mut out = Vec::new();
        match self.mode {
            Mode::Feed => {
                if let Some(s) = &self.picture
                    && let Some(p) = crate::view::place(size, s.size, &self.view)
                {
                    out.push((
                        s,
                        if matches!(s.held, Held::Yuv(..)) {
                            1
                        } else {
                            0
                        },
                        p,
                    ));
                }
            }
            Mode::Still => {
                // The logo or slide on this screen alone: turned and inside the overscan as
                // the feed, fitted, never cut to a wall's tile.
                let v = View {
                    rotation: self.view.rotation,
                    overscan: self.view.overscan,
                    ..Default::default()
                };
                if let Some(s) = &self.still
                    && let Some(p) = crate::view::place(size, s.size, &v)
                {
                    out.push((s, 0, p));
                }
            }
            Mode::Black => {}
        }
        if let Some(s) = &self.overlay {
            out.push((
                s,
                2,
                Placement {
                    viewport: [0.0, 0.0, size.0 as f32, size.1 as f32],
                    uv: [0.0, 0.0, 1.0, 1.0],
                    rotation: 0,
                },
            ));
        }
        out
    }

    /// Records the drawing into `target` of `size` with `pipes`: black, then the layers.
    fn record(
        &self,
        pipes: &Pipes,
        target: &wgpu::TextureView,
        size: (u32, u32),
    ) -> wgpu::CommandBuffer {
        let layers = self.layers(size);
        for (s, _, p) in &layers {
            self.queue.write_buffer(&s.view, 0, &view_bytes(p));
        }
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
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
            for (s, kind, p) in &layers {
                let [x, y, w, h] = p.viewport;
                // Inside the target, at least a pixel (a viewport past its edge is refused).
                let (x, y) = (x.clamp(0.0, size.0 as f32), y.clamp(0.0, size.1 as f32));
                let (w, h) = (w.min(size.0 as f32 - x), h.min(size.1 as f32 - y));
                if w < 0.5 || h < 0.5 {
                    continue;
                }
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
                pass.set_pipeline(match kind {
                    0 => &pipes.rgba,
                    1 => &pipes.yuv,
                    _ => &pipes.over,
                });
                pass.set_bind_group(0, &s.group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        enc.finish()
    }

    /// Draws: black, the picture (if there is one yet) placed on it, the overlay.
    pub fn draw(&mut self) -> Result<(), String> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            other => return Err(format!("the screen: {other:?}")),
        };
        let view = frame.texture.create_view(&Default::default());
        let cmd = self.record(&self.pipes, &view, (self.config.width, self.config.height));
        self.queue.submit([cmd]);
        self.queue.present(frame);
        Ok(())
    }

    /// What the screen shows, drawn again `width` pixels wide (the screen's shape kept,
    /// 16 to 1920): its RGBA and size.
    pub fn thumbnail(&mut self, width: u32) -> Result<(u32, u32, Vec<u8>), String> {
        let (sw, sh) = (self.config.width.max(1), self.config.height.max(1));
        let w = width.clamp(16, 1920).min(sw.max(16));
        let h = ((w as u64 * sh as u64 / sw as u64) as u32).max(1);
        let format = wgpu::TextureFormat::Rgba8Unorm;
        if self.thumb_pipes.is_none() {
            self.thumb_pipes = Some(Pipes::new(
                &self.device,
                format,
                &self.layout,
                &self.layout_yuv,
            ));
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("thumbnail"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let tv = tex.create_view(&Default::default());
        let pipes = self.thumb_pipes.as_ref().expect("made above");
        let draw = self.record(pipes, &tv, (w, h));
        let row = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("thumbnail"),
            size: row as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let sub = self.queue.submit([draw, enc.finish()]);
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r.is_ok());
        });
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let _ = self.device.poll(wgpu::PollType::Wait {
                submission_index: Some(sub.clone()),
                timeout: Some(std::time::Duration::from_millis(200)),
            });
            match rx.try_recv() {
                Ok(true) => break,
                Ok(false) => return Err("the thumbnail couldn't be read from the GPU".into()),
                Err(_) if std::time::Instant::now() >= end => {
                    buf.unmap();
                    return Err("the thumbnail wasn't read from the GPU in time".into());
                }
                Err(_) => {}
            }
        }
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        {
            let data = slice
                .get_mapped_range()
                .map_err(|e| format!("the thumbnail: {e:?}"))?;
            for y in 0..h as usize {
                rgba.extend_from_slice(&data[y * row as usize..y * row as usize + w as usize * 4]);
            }
        }
        buf.unmap();
        Ok((w, h, rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::Renderer;

    /// The shader's sums, done here, against the library's conversion (to RGBA on the
    /// CPU): the same to within a step, for both matrices and both ranges.
    #[test]
    fn the_shader_converts_as_the_library_does() {
        use idn_open_protocol::color::{AlphaMode, ColorConfig, Matrix, planes_to_rgba_into};
        use idn_open_protocol::shq::Plane;
        let samples: Vec<(u8, u8, u8)> = (0..=255u32)
            .step_by(15)
            .flat_map(|y| [(y, 128, 128), (y, 40, 210), (y, 230, 60), (y, 90, 90)])
            .map(|(y, b, r)| (y as u8, b as u8, r as u8))
            .collect();
        let n = samples.len();
        for matrix in [Matrix::Bt709, Matrix::Bt601] {
            for limited in [true, false] {
                let c = ColorConfig {
                    matrix,
                    limited,
                    ..Default::default()
                };
                // One row; 4:4:4 so each pixel has its own chroma.
                let planes = vec![
                    Plane {
                        width: n,
                        height: 1,
                        data: samples.iter().map(|s| s.0).collect(),
                    },
                    Plane {
                        width: n,
                        height: 1,
                        data: samples.iter().map(|s| s.1).collect(),
                    },
                    Plane {
                        width: n,
                        height: 1,
                        data: samples.iter().map(|s| s.2).collect(),
                    },
                ];
                let mut rgba = Vec::new();
                planes_to_rgba_into(&planes, n, 1, AlphaMode::Opaque, &c, &mut rgba);
                let k = super::terms(&c);
                for (i, &(y, cb, cr)) in samples.iter().enumerate() {
                    let yv = (y as f32 / 255.0 - k[1]) * k[0];
                    let (b, r) = (
                        cb as f32 / 255.0 - 128.0 / 255.0,
                        cr as f32 / 255.0 - 128.0 / 255.0,
                    );
                    let rgb = [yv + k[2] * r, yv + k[3] * b + k[4] * r, yv + k[5] * b];
                    for ch in 0..3 {
                        let shader = (rgb[ch].clamp(0.0, 1.0) * 255.0).round() as i32;
                        let lib = rgba[i * 4 + ch] as i32;
                        assert!(
                            (shader - lib).abs() <= 1,
                            "{matrix:?} limited {limited}: {y},{cb},{cr} channel {ch}: shader {shader}, library {lib}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn pictures_fit_with_their_aspect() {
        // 16:9 on 16:9: the whole screen.
        assert_eq!(
            Renderer::fit((1920, 1080), (1280, 720)),
            (0.0, 0.0, 1920.0, 1080.0)
        );
        // 16:9 on 4:3: bars above and below.
        assert_eq!(
            Renderer::fit((1024, 768), (1920, 1080)),
            (0.0, 96.0, 1024.0, 576.0)
        );
        // A vertical mix on a landscape screen: bars at the sides.
        assert_eq!(
            Renderer::fit((1920, 1080), (1080, 1920)),
            (656.25, 0.0, 607.5, 1080.0)
        );
    }
}
