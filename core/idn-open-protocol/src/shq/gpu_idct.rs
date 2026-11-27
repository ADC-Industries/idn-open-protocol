//! SpeedHQ pictures reconstructed on the GPU (perf queue 2b): the CPU reads the bitstream
//! as far as the dequantised coefficients (`shq::decode_coefficients`), and the inverse
//! DCT and the writing of samples run here (`idct.wgsl`), straight into the compositor's
//! picture buffer. In f64, the CPU's precision, where the GPU has it; else, or when asked
//! (`IDN_SWITCHER_IDCT=f32`), in f32.

use super::{CoefPicture, idct_basis};

/// The arithmetic of the reconstruction (the CPU's too, [`super::idct_precision`]).
pub use super::idct::Precision;

impl Precision {
    /// The one to use on a device with `features`: f64 where it has it, unless the
    /// environment variable `env` (`IDN_SWITCHER_IDCT`, `IDN_OBS_IDCT`) says `f32`, or,
    /// with `env` unset, the machine's video settings say `fast` (§N.13).
    pub fn for_features(features: wgpu::Features, env: &str) -> Precision {
        let want = match std::env::var(env).ok().as_deref().map(str::trim) {
            Some("f32") => Precision::F32,
            Some("f64") => Precision::F64,
            _ => super::idct::machine_precision().unwrap_or(Precision::F64),
        };
        if want == Precision::F64 && features.contains(wgpu::Features::SHADER_F64) {
            Precision::F64
        } else {
            Precision::F32
        }
    }
}

/// Where a plane's coefficients are in the uploaded words and where its samples go.
#[derive(Debug, Clone, Copy)]
pub struct PlaneJob {
    pub width: u32,
    pub height: u32,
    pub bw: u32,
    pub bh: u32,
    /// Word offsets, in the coefficient buffer, of its block table and values.
    pub blocks: u32,
    pub values: u32,
    /// Byte offset of its first sample in the picture buffer.
    pub dst: u32,
    /// Each sample row written this many times, down to `out_height` rows: 2 puts a
    /// 4:2:0 picture's chroma into 4:2:2 planes (each row serving two picture rows, as
    /// the CPU's conversion does). [`pack`] gives 1 and the plane's height.
    pub rows: u32,
    pub out_height: u32,
    /// Bytes from one sample row to the next in the picture buffer; 0: the plane's width.
    pub stride: u32,
}

impl PlaneJob {
    /// 4:2:0 chroma written as 4:2:2: each row twice, down to the picture's `height`.
    pub fn doubled(self, height: u32) -> PlaneJob {
        PlaneJob {
            rows: 2,
            out_height: height,
            ..self
        }
    }
}

/// A picture's coefficients as words for upload (each plane's block table, then its
/// values two to a word) and each plane's job, its offsets counted from `base` (words) and
/// its samples at `dst[plane]` (bytes).
pub fn pack(c: &CoefPicture, base: u32, dst: [u32; 4]) -> (Vec<u32>, Vec<PlaneJob>) {
    let mut words = Vec::new();
    let mut jobs = pack_into(c, &mut words, dst);
    for j in &mut jobs {
        (j.blocks, j.values) = (j.blocks + base, j.values + base);
    }
    (words, jobs)
}

/// [`pack`] appending to `words` (offsets counted from its start): no second copy of a
/// 4K picture's 13 MB of words.
pub fn pack_into(c: &CoefPicture, words: &mut Vec<u32>, dst: [u32; 4]) -> Vec<PlaneJob> {
    let at = words.len();
    words.resize(at + words_len(c), 0);
    let mut jobs = pack_to(c, &mut words[at..], dst);
    // (Counted from `words`' start: from the picture's own, a second picture in the same
    // words was made from the first's.)
    for j in &mut jobs {
        (j.blocks, j.values) = (j.blocks + at as u32, j.values + at as u32);
    }
    jobs
}

/// The words a picture packs into ([`pack_to`]).
pub fn words_len(c: &CoefPicture) -> usize {
    c.planes
        .iter()
        .map(|p| p.blocks.len() + p.values.len().div_ceil(2))
        .sum()
}

/// [`pack_into`] a slice of exactly [`words_len`] words: straight into the GPU queue's own
/// staging memory, so the packed picture isn't copied a second time on its way (13 MB a
/// 2160p picture).
pub fn pack_to(c: &CoefPicture, words: &mut [u32], dst: [u32; 4]) -> Vec<PlaneJob> {
    let mut jobs = Vec::with_capacity(c.planes.len());
    let mut at = 0usize;
    for (pl, p) in c.planes.iter().enumerate() {
        let blocks = at as u32;
        words[at..at + p.blocks.len()].copy_from_slice(&p.blocks);
        at += p.blocks.len();
        let values = at as u32;
        // Two values a word, the first in the low half: on a little-endian machine the
        // values' own bytes, copied (value by value took 3 ms of a grainy 2160p picture).
        let n = p.values.len().div_ceil(2);
        let w = &mut words[at..at + n];
        at += n;
        if p.values.len() % 2 == 1 {
            w[n - 1] = 0;
        }
        #[cfg(target_endian = "little")]
        bytemuck::cast_slice_mut::<u32, i16>(w)[..p.values.len()].copy_from_slice(&p.values);
        #[cfg(not(target_endian = "little"))]
        for (w, v) in w.iter_mut().zip(p.values.chunks(2)) {
            *w = v[0] as u16 as u32 | (v.get(1).copied().unwrap_or(0) as u16 as u32) << 16;
        }
        jobs.push(PlaneJob {
            width: p.width as u32,
            height: p.height as u32,
            bw: p.bw as u32,
            bh: p.bh as u32,
            blocks,
            values,
            dst: dst[pl.min(3)],
            rows: 1,
            out_height: p.height as u32,
            stride: 0,
        });
    }
    jobs
}

/// A frame's pictures as coefficients on their way to the GPU: added with where their
/// samples go, then packed straight into a mapped staging buffer and copied up at once
/// (packed into memory zeroed for them and then copied again, a 4K camera's 13 MB was
/// written three times each frame).
/// A picture added to an [`Upload`]: it, where its words start, its planes' places, and
/// the height its 4:2:0 chroma is doubled to.
type Added = (std::sync::Arc<CoefPicture>, usize, [u32; 4], Option<u32>);

pub struct Upload {
    pics: Vec<Added>,
    len: usize,
    staging: crate::gpu::Staging,
    buffer: Option<wgpu::Buffer>,
}

impl Default for Upload {
    fn default() -> Self {
        Upload {
            pics: Vec::new(),
            len: 0,
            staging: crate::gpu::Staging::new("coefficients up"),
            buffer: None,
        }
    }
}

impl Upload {
    /// None for the next frame.
    pub fn clear(&mut self) {
        self.pics.clear();
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.pics.is_empty()
    }

    /// Picture `c`, its planes' samples at `dst` (bytes); with `doubled`, its chroma rows
    /// each written twice down to that height (4:2:0 into 4:2:2).
    pub fn add(&mut self, c: std::sync::Arc<CoefPicture>, dst: [u32; 4], doubled: Option<u32>) {
        let n = words_len(&c);
        self.pics.push((c, self.len, dst, doubled));
        self.len += n;
    }

    /// The pictures packed and copied up (submitted ahead of what follows on the queue):
    /// the coefficient buffer and each plane's job, for [`Recon::record`].
    pub fn upload(
        &mut self,
        hand: &crate::gpu::GpuHandIn,
    ) -> std::io::Result<(&wgpu::Buffer, Vec<PlaneJob>)> {
        let bytes = (self.len.max(1) * 4) as u64;
        // Within the GPU's limits, as the samples' buffer is (bug run K7-5: a dense picture
        // past them raised a validation error, and the input left the GPU for good): that
        // picture only refused, its own kind.
        let limits = hand.device.limits();
        let most = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        if bytes > most {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!(
                    "a picture's coefficients too large for this GPU's buffers ({} MB)",
                    bytes >> 20
                ),
            ));
        }
        if self.buffer.as_ref().is_none_or(|b| b.size() < bytes) {
            self.buffer = Some(
                hand.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("coefficients"),
                    size: (bytes * 5 / 4)
                        .min(most / 4 * 4)
                        .max(bytes)
                        .next_multiple_of(4),
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
            );
        }
        let buffer = self.buffer.as_ref().expect("made above");
        let (pics, len) = (&self.pics, self.len);
        let mut jobs = Vec::new();
        self.staging.upload(
            hand,
            bytes,
            |view| {
                // SAFETY: the staging buffer's mapped memory, at least `len` words, only
                // written (it may be write-combined: never read here).
                let words = unsafe {
                    std::slice::from_raw_parts_mut(
                        view.as_raw_element_ptr().as_ptr() as *mut u32,
                        len,
                    )
                };
                for (c, at, dst, doubled) in pics {
                    let n = words_len(c);
                    let mut js = pack_to(c, &mut words[*at..*at + n], *dst);
                    for (k, j) in js.iter_mut().enumerate() {
                        (j.blocks, j.values) = (j.blocks + *at as u32, j.values + *at as u32);
                        if let Some(h) = doubled
                            && k > 0
                        {
                            *j = j.doubled(*h);
                        }
                    }
                    jobs.extend(js);
                }
                true
            },
            |enc, up| enc.copy_buffer_to_buffer(up, 0, buffer, 0, bytes),
        )?;
        Ok((buffer, jobs))
    }
}

/// The reconstruction pipeline for one precision.
pub struct Recon {
    pub precision: Precision,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    basis: wgpu::Buffer,
}

impl Recon {
    pub fn new(device: &wgpu::Device, precision: Precision) -> Recon {
        let scalar = match precision {
            Precision::F64 => "f64",
            Precision::F32 => "f32",
        };
        let source = include_str!("gpu_idct.wgsl").replace("SCALAR", scalar);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("idct"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
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
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("idct"),
            entries: &[
                entry(0, wgpu::BufferBindingType::Uniform),
                entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
                entry(2, wgpu::BufferBindingType::Storage { read_only: true }),
                entry(3, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("idct"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("idct"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        // The CPU's basis, exactly (or rounded to f32).
        let table = idct_basis();
        let bytes: Vec<u8> = match precision {
            Precision::F64 => table.iter().flat_map(|v| v.to_le_bytes()).collect(),
            Precision::F32 => table
                .iter()
                .flat_map(|v| (*v as f32).to_le_bytes())
                .collect(),
        };
        let basis = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("idct basis"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        basis
            .slice(..)
            .get_mapped_range_mut()
            .expect("mapped at creation")
            .copy_from_slice(&bytes);
        basis.unmap();
        Recon {
            precision,
            pipeline,
            layout,
            basis,
        }
    }

    /// Records the jobs' reconstruction: coefficients from `coefs`, samples into `pics`,
    /// whose planes' bytes must be zero beforehand (they are ORed in, four to a word).
    pub fn record(
        &self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        coefs: &wgpu::Buffer,
        pics: &wgpu::Buffer,
        jobs: &[PlaneJob],
    ) {
        for j in jobs {
            // A workgroup a block, on a grid at most 65,535 wide.
            let n = j.bw * j.bh;
            let gx = n.clamp(1, 65_535);
            let words = [
                j.width,
                j.height,
                j.bw,
                j.bh,
                j.blocks,
                j.values,
                j.dst,
                gx,
                j.rows.max(1),
                j.out_height,
                j.stride,
                0,
            ];
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let job = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("idct job"),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: true,
            });
            job.slice(..)
                .get_mapped_range_mut()
                .expect("mapped at creation")
                .copy_from_slice(&bytes);
            job.unmap();
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("idct"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: job.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.basis.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: coefs.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: pics.as_entire_binding(),
                    },
                ],
            });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("idct"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(gx, n.div_ceil(gx), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::block_on;
    use crate::shq::encode::{EncodeInput, EncodeOptions, encode};
    use crate::shq::{Fourcc, Limits, Plane, VideoParams, decode, decode_coefficients};

    /// A device with f64 shaders if the GPU has them; `None` (the test passes, saying
    /// so) without a GPU.
    fn device() -> Option<(wgpu::Device, wgpu::Queue, wgpu::Features)> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        if adapter.get_info().device_type == wgpu::DeviceType::Cpu {
            eprintln!("no GPU: test skipped");
            return None;
        }
        let features = adapter.features() & wgpu::Features::SHADER_F64;
        let (d, q) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("idct test"),
            required_features: features,
            ..Default::default()
        }))
        .ok()?;
        Some((d, q, features))
    }

    /// A camera-like 4:2:2 or 4:2:0 picture, coded.
    fn coded(w: usize, h: usize, quality: u8, fourcc: Fourcc) -> (VideoParams, Vec<u8>) {
        let mut seed = 0x1234_5678u32;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % 9) as i32 - 4
        };
        let mut plane = |pw: usize, ph: usize, base: i32| Plane {
            width: pw,
            height: ph,
            data: (0..pw * ph)
                .map(|i| {
                    let (x, y) = (i % pw, i / pw);
                    let v = base
                        + ((x / 37 + y / 23) % 6) as i32 * 20
                        + ((x as f32 / 9.0).sin() * 30.0) as i32
                        + noise();
                    v.clamp(16, 235) as u8
                })
                .collect(),
        };
        let ch = if fourcc == Fourcc::Shq0 {
            h.div_ceil(2)
        } else {
            h
        };
        let planes = vec![
            plane(w, h, 60),
            plane(w.div_ceil(2), ch, 100),
            plane(w.div_ceil(2), ch, 120),
        ];
        let payload = encode(
            &EncodeInput {
                fourcc,
                width: w,
                height: h,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions {
                quality,
                ..Default::default()
            },
        );
        let vp = VideoParams {
            fourcc: if fourcc == Fourcc::Shq0 {
                *b"SHQ0"
            } else {
                *b"SHQ2"
            },
            width: w as u32,
            height: h as u32,
            field_id: 0,
        };
        (vp, payload)
    }

    /// The GPU's samples against the CPU's decode: in f64 the same; in f32 at most one
    /// step off, rarely.
    #[test]
    fn the_gpu_reconstructs_what_the_cpu_decodes() {
        let Some((device, queue, features)) = device() else {
            return;
        };
        for (w, h, q) in [(1920usize, 1080usize, 92u8), (723, 301, 60)] {
            let (vp, payload) = coded(w, h, q, Fourcc::Shq2);
            let cpu = decode(&vp, &payload, &Limits::default()).unwrap();
            let c = decode_coefficients(&vp, &payload, &Limits::default()).unwrap();
            // Planes one after another in the picture buffer, 4-byte aligned.
            let mut dst = [0u32; 4];
            let mut at = 0u32;
            for (pl, p) in cpu.planes.iter().enumerate() {
                dst[pl] = at;
                at += (p.data.len() as u32).div_ceil(4) * 4;
            }
            let (words, jobs) = pack(&c, 0, dst);
            let mk = |label, size: u64, usage| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage,
                    mapped_at_creation: false,
                })
            };
            use wgpu::BufferUsages as U;
            let coefs = mk("coefs", (words.len() * 4) as u64, U::STORAGE | U::COPY_DST);
            let pics = mk("pics", at as u64, U::STORAGE | U::COPY_SRC | U::COPY_DST);
            let back = mk("back", at as u64, U::MAP_READ | U::COPY_DST);
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            queue.write_buffer(&coefs, 0, &bytes);
            let mut precisions = vec![Precision::F32];
            if features.contains(wgpu::Features::SHADER_F64) {
                precisions.push(Precision::F64);
            } else {
                eprintln!("this GPU has no f64 shaders: f32 only");
            }
            for precision in precisions {
                let recon = Recon::new(&device, precision);
                let mut enc = device.create_command_encoder(&Default::default());
                enc.clear_buffer(&pics, 0, None);
                recon.record(&device, &mut enc, &coefs, &pics, &jobs);
                enc.copy_buffer_to_buffer(&pics, 0, &back, 0, at as u64);
                queue.submit([enc.finish()]);
                back.slice(..).map_async(wgpu::MapMode::Read, |_| {});
                device
                    .poll(wgpu::PollType::Wait {
                        submission_index: None,
                        timeout: None,
                    })
                    .unwrap();
                let (mut off, mut worst) = (0usize, 0i32);
                {
                    let got = back.slice(..).get_mapped_range().unwrap();
                    for (pl, p) in cpu.planes.iter().enumerate() {
                        let g = &got[dst[pl] as usize..dst[pl] as usize + p.data.len()];
                        for (a, b) in g.iter().zip(&p.data) {
                            let d = (*a as i32 - *b as i32).abs();
                            worst = worst.max(d);
                            off += usize::from(d != 0);
                        }
                    }
                }
                back.unmap();
                let total: usize = cpu.planes.iter().map(|p| p.data.len()).sum();
                eprintln!(
                    "{w}x{h} Q{q} {precision:?}: {off} of {total} samples differ, at most by {worst}"
                );
                match precision {
                    Precision::F64 => assert_eq!(off, 0, "f64 must match the CPU"),
                    Precision::F32 => {
                        assert!(worst <= 1, "f32 within one step");
                        assert!(off * 1000 < total, "f32: rarely off");
                    }
                }
            }
        }
    }
    /// 4:2:0 (NDI's pictures above 1080p) into the switcher's 4:2:2 planes: each chroma
    /// row written twice, the CPU's decode with its rows repeated (odd heights too).
    #[test]
    fn four_two_zero_chroma_fills_two_rows() {
        let Some((device, queue, features)) = device() else {
            return;
        };
        let precision = if features.contains(wgpu::Features::SHADER_F64) {
            Precision::F64
        } else {
            Precision::F32
        };
        let recon = Recon::new(&device, precision);
        for (w, h) in [(2560usize, 1440usize), (722, 301)] {
            let (vp, payload) = coded(w, h, 92, Fourcc::Shq0);
            let cpu = decode(&vp, &payload, &Limits::default()).unwrap();
            let c = decode_coefficients(&vp, &payload, &Limits::default()).unwrap();
            // What the switcher's CPU path makes: 4:2:2 planes, chroma rows repeated.
            let cw = w.div_ceil(2);
            let want: Vec<Vec<u8>> = cpu
                .planes
                .iter()
                .enumerate()
                .map(|(pl, p)| {
                    if pl == 0 {
                        return p.data.clone();
                    }
                    (0..h)
                        .flat_map(|row| p.data[(row / 2) * cw..][..cw].iter().copied())
                        .collect()
                })
                .collect();
            let mut dst = [0u32; 4];
            let mut at = 0u32;
            for (pl, p) in want.iter().enumerate() {
                dst[pl] = at;
                at += (p.len() as u32).div_ceil(4) * 4;
            }
            let (words, mut jobs) = pack(&c, 0, dst);
            for j in jobs.iter_mut().skip(1) {
                *j = j.doubled(h as u32);
            }
            let mk = |label, size: u64, usage| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage,
                    mapped_at_creation: false,
                })
            };
            use wgpu::BufferUsages as U;
            let coefs = mk("coefs", (words.len() * 4) as u64, U::STORAGE | U::COPY_DST);
            let pics = mk("pics", at as u64, U::STORAGE | U::COPY_SRC | U::COPY_DST);
            let back = mk("back", at as u64, U::MAP_READ | U::COPY_DST);
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            queue.write_buffer(&coefs, 0, &bytes);
            let mut enc = device.create_command_encoder(&Default::default());
            enc.clear_buffer(&pics, 0, None);
            recon.record(&device, &mut enc, &coefs, &pics, &jobs);
            enc.copy_buffer_to_buffer(&pics, 0, &back, 0, at as u64);
            queue.submit([enc.finish()]);
            back.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .unwrap();
            let (mut off, mut worst) = (0usize, 0u8);
            {
                let got = back.slice(..).get_mapped_range().unwrap();
                for (pl, p) in want.iter().enumerate() {
                    let g = &got[dst[pl] as usize..dst[pl] as usize + p.len()];
                    for (a, b) in g.iter().zip(p) {
                        worst = worst.max(a.abs_diff(*b));
                        off += usize::from(a != b);
                    }
                }
            }
            back.unmap();
            let total: usize = want.iter().map(|p| p.len()).sum();
            eprintln!(
                "{w}x{h} SHQ0 as 4:2:2 {precision:?}: {off} of {total} differ, at most {worst}"
            );
            match precision {
                Precision::F64 => assert_eq!(off, 0),
                Precision::F32 => assert!(worst <= 1 && off * 1000 < total),
            }
        }
    }
}
