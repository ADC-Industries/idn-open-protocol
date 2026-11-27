//! A screen's picture (BGRA or RGBA, 8 bits) made planar 4:2:2 Y'CbCr on the GPU, with the
//! same fixed-point arithmetic as [`crate::color::bgra_to_planes`] (the same samples), into
//! a buffer laid out as a `gpu/1` slot is ([`crate::gpu::slot_layout`]). For a capture
//! whose pictures are taken on the GPU (SpeedHQ encoded there, a receiver on this machine
//! there: §E.27): only the RGB goes up, and the CPU neither converts nor encodes it.

use crate::color::ColorConfig;
use crate::gpu::{Buffers, GpuHandIn, GpuPicture, PlaneAt, Staging, slot_layout};
use std::io;

const WGSL: &str = r#"
struct P {
    w: u32, h: u32, stride: u32, y_off: u32,
    y_row: u32, cb_off: u32, c_row: u32, cr_off: u32,
    ky: vec4<i32>, kb: vec4<i32>, kr: vec4<i32>,
    lim: vec4<i32>,
};
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> p: P;

fn px(x: u32, y: u32) -> vec3<i32> {
    let v = src[y * p.stride + min(x, p.w - 1u)];
    return vec3<i32>(i32(v & 255u), i32((v >> 8u) & 255u), i32((v >> 16u) & 255u));
}

fn luma(c: vec3<i32>) -> u32 {
    let v = ((p.ky.x * c.x + p.ky.y * c.y + p.ky.z * c.z + 32768) >> 16u) + p.ky.w;
    return u32(clamp(v, p.lim.x, p.lim.y));
}

// Twice the pair's average (`s`), or one pixel alone (`one`).
fn chroma(k: vec4<i32>, s: vec3<i32>, one: bool) -> u32 {
    var v: i32;
    if one {
        v = ((k.x * s.x + k.y * s.y + k.z * s.z + 32768) >> 16u) + 128;
    } else {
        v = ((k.x * s.x + k.y * s.y + k.z * s.z + 65536) >> 17u) + 128;
    }
    return u32(clamp(v, p.lim.z, p.lim.w));
}

// Eight pixels of a row each: two words of luma, one of each chroma.
@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let x0 = g.x * 8u;
    let y = g.y;
    if x0 >= p.w || y >= p.h {
        return;
    }
    var yw = array<u32, 2>(0u, 0u);
    var cb = 0u;
    var cr = 0u;
    for (var i = 0u; i < 4u; i++) {
        let xa = x0 + 2u * i;
        let a = px(xa, y);
        let b = px(xa + 1u, y);
        let word = i / 2u;
        let sh = (i % 2u) * 16u;
        yw[word] |= (luma(a) | (luma(b) << 8u)) << sh;
        let one = xa + 1u >= p.w;
        var s = a + b;
        if one {
            s = a;
        }
        cb |= chroma(p.kb, s, one) << (8u * i);
        cr |= chroma(p.kr, s, one) << (8u * i);
    }
    let yi = p.y_off / 4u + y * p.y_row / 4u + g.x * 2u;
    dst[yi] = yw[0];
    dst[yi + 1u] = yw[1];
    dst[p.cb_off / 4u + y * p.c_row / 4u + g.x] = cb;
    dst[p.cr_off / 4u + y * p.c_row / 4u + g.x] = cr;
}
"#;

/// Makes RGB pictures planar on one device; keeps its buffers between pictures.
pub struct RgbToPlanes {
    hand: GpuHandIn,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    /// The picture's RGB on the GPU, packed rows.
    src: Option<wgpu::Buffer>,
    staging: Staging,
    /// Pictures handed out, taken back once nothing holds them.
    out: Buffers,
    size: (usize, usize),
}

impl RgbToPlanes {
    /// Its pipeline on `hand`'s device.
    pub fn new(hand: &GpuHandIn) -> RgbToPlanes {
        let d = &hand.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("RGB to planes"),
            source: wgpu::ShaderSource::Wgsl(WGSL.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        use wgpu::BufferBindingType as T;
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("RGB to planes"),
            entries: &[
                entry(0, T::Storage { read_only: true }),
                entry(1, T::Storage { read_only: false }),
                entry(2, T::Uniform),
            ],
        });
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("RGB to planes"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("RGB to planes"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("RGB to planes"),
            size: 96,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        RgbToPlanes {
            hand: hand.clone(),
            pipeline,
            layout,
            params,
            src: None,
            staging: Staging::new("RGB staging"),
            out: Buffers::new(
                "RGB as planes",
                wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
                4,
            ),
            size: (0, 0),
        }
    }

    /// `data` (`height` rows `stride` bytes apart; BGRA, or RGBA with `bgr` false) as
    /// 4:2:2 planes (Y, Cb, Cr) on the GPU, as `cfg` says (matrix and range; alpha is
    /// dropped). The work is submitted, not waited for: what uses the picture on this
    /// queue comes after it.
    pub fn convert(
        &mut self,
        data: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        bgr: bool,
        cfg: &ColorConfig,
    ) -> io::Result<GpuPicture> {
        let (w, h) = (width, height);
        if w == 0 || h == 0 || stride < w * 4 || data.len() < stride * (h - 1) + w * 4 {
            return Err(io::Error::other(
                "RGB to planes: a picture of the wrong shape",
            ));
        }
        let d = &self.hand.device;
        let cw = w.div_ceil(2);
        let (planes, bytes) = slot_layout(&[
            (w as u32, h as u32),
            (cw as u32, h as u32),
            (cw as u32, h as u32),
        ]);
        let src_bytes = (w * h * 4) as u64;
        if self.size != (w, h) {
            self.size = (w, h);
            self.src = Some(d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("RGB up"),
                size: src_bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let out = self.out.get(d, bytes);
        let mut k = crate::color::coeffs(cfg);
        if bgr {
            for row in [&mut k.y, &mut k.cb, &mut k.cr] {
                row.swap(0, 2);
            }
        }
        let lim = if cfg.limited {
            [16, 235, 16, 240]
        } else {
            [0, 255, 0, 255]
        };
        let mut words: Vec<u8> = Vec::with_capacity(96);
        for v in [
            w as u32,
            h as u32,
            w as u32,
            planes[0].offset as u32,
            planes[0].row_bytes,
            planes[1].offset as u32,
            planes[1].row_bytes,
            planes[2].offset as u32,
        ] {
            words.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            k.y[0], k.y[1], k.y[2], k.y_off, k.cb[0], k.cb[1], k.cb[2], 0, k.cr[0], k.cr[1],
            k.cr[2], 0,
        ]
        .into_iter()
        .chain(lim)
        {
            words.extend_from_slice(&v.to_le_bytes());
        }
        self.hand.queue.write_buffer(&self.params, 0, &words);
        let src = self.src.as_ref().expect("made above");
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("RGB to planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: src.as_entire_binding(),
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
        // The RGB into a mapped staging buffer (rows packed), up, and converted.
        let row = w * 4;
        self.staging.upload(
            &self.hand,
            src_bytes,
            |dst| {
                if stride == row {
                    dst.copy_from_slice(&data[..row * h]);
                } else {
                    for y in 0..h {
                        dst.slice(y * row..(y + 1) * row)
                            .copy_from_slice(&data[y * stride..y * stride + row]);
                    }
                }
                true
            },
            |enc, up| {
                enc.copy_buffer_to_buffer(up, 0, src, 0, src_bytes);
                let mut pass = enc.begin_compute_pass(&Default::default());
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.dispatch_workgroups((w as u32).div_ceil(8).div_ceil(64), h as u32, 1);
            },
        )?;
        let planes: Vec<PlaneAt> = planes;
        Ok(GpuPicture {
            buffer: out,
            planes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{AlphaMode, bgra_to_planes, rgba_to_planes};
    use crate::shq::Fourcc;

    /// The same samples as the CPU's converter, BGRA and RGBA, an odd width and padded
    /// rows, limited and full range; pictures one after another through the staging.
    #[test]
    fn the_gpu_converts_as_the_cpu_does() {
        let Some(hand) = crate::gpu::open_device("test", true) else {
            eprintln!("no GPU here: not tested");
            return;
        };
        let mut conv = RgbToPlanes::new(&hand);
        for (n, &(w, h, pad, bgr, limited)) in [
            (64usize, 8usize, 0usize, true, true),
            (37, 5, 12, true, true),
            (1917, 9, 4, false, true),
            (333, 7, 0, true, false),
            (333, 7, 0, true, false),
        ]
        .iter()
        .enumerate()
        {
            let stride = w * 4 + pad;
            let data: Vec<u8> = (0..stride * h)
                .map(|i| ((i * 7919 + n * 31) % 251) as u8)
                .collect();
            let cfg = ColorConfig {
                limited,
                ..Default::default()
            };
            let convert = if bgr { bgra_to_planes } else { rgba_to_planes };
            let want = convert(&data, w, h, stride, Fourcc::Shq2, AlphaMode::Opaque, &cfg);
            let got = conv.convert(&data, w, h, stride, bgr, &cfg).unwrap();
            let back = got.read_planes(&hand).unwrap();
            for k in 0..3 {
                assert_eq!(back[k], want[k].data, "picture {n}, plane {k}");
            }
        }
    }
}
