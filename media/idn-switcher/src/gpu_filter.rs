//! An input's filters on the GPU (spec/extensions/idn-switcher.md §S.23): a chain of
//! table filters (colour correction, LUT, chroma, colour and luma keys) run on the
//! compositor's device, the picture going up as it came (SpeedHQ coefficients are
//! reconstructed there, as the compositor does) and coming back filtered — the CPU's
//! result to the bit. On the CPU a 33-point LUT and a correction on a 4K camera cost
//! about two and a third cores, the reconstruction they forced included, and a chroma key
//! one and a half. Pictures that came as planes stay on the CPU (`worth_it`:
//! their way up and back cost more than the filters); chains with a mask or
//! scroll stay there too.

use crate::filter::{Chain, GpuStep};
use crate::frame::Frame;
use idn_common::sync::LockOk as _;
use idn_open_protocol::shq::Plane;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(2);
const GROUP: u32 = 64;

/// The process's filter runner, once the compositor has a GPU ([`install`]).
static RUNNER: OnceLock<Mutex<FilterGpu>> = OnceLock::new();
/// Set after the GPU failed once: the CPU filters from then on.
static FAILED: AtomicBool = AtomicBool::new(false);

/// Makes `f` the filter runner (the compositor's device), unless
/// `IDN_SWITCHER_FILTERS_CPU` is set.
pub fn install(f: FilterGpu) {
    if !idn_common::switches::on("IDN_SWITCHER_FILTERS_CPU") {
        let _ = RUNNER.set(Mutex::new(f));
    }
}

/// `f` through `chain` on the GPU, if it runs there; `None`: on the CPU instead (no GPU,
/// a step only the CPU has, or the GPU failed — logged once).
pub fn apply(chain: &Chain, f: &Frame) -> Option<Frame> {
    if FAILED.load(Ordering::Relaxed) {
        return None;
    }
    let runner = RUNNER.get()?;
    let steps = chain.gpu_steps()?;
    if !worth_it(&steps, f) {
        return None;
    }
    match runner.lock_ok().run(&steps, f) {
        Ok(out) => Some(out),
        Err(e) => {
            FAILED.store(true, Ordering::Relaxed);
            idn_open_protocol::log::log(&format!(
                "switcher: input filters on the CPU from now on ({e})"
            ));
            None
        }
    }
}

/// Whether `f` is filtered on the GPU: a camera's SpeedHQ coefficients, reconstructed
/// there (on the CPU the whole picture would be decoded first: a sharpen on a 4K camera
/// 53% of a core there, 62% here). A picture that came as planes stays here, its way up
/// and back costing more than any filter (a 4K60 file on a 2160p30 program: a LUT 109%
/// against 86%, a correction and a sharpen 160% against 125% recorded).
fn worth_it(_steps: &[GpuStep], f: &Frame) -> bool {
    f.coefs.is_some()
}

/// The filter pipeline and its buffers on the compositor's device.
pub struct FilterGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    recon: crate::idct::Recon,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    /// Two picture buffers (a step reads one, writes the other), the read-back copy and
    /// the coefficients' words, with their size in bytes.
    bufs: Option<(
        wgpu::Buffer,
        wgpu::Buffer,
        wgpu::Buffer,
        wgpu::Buffer,
        u64,
        u64,
    )>,
    /// Tables up, by the address of the chain's table (a chain keeps its tables).
    tables: Vec<(Arc<Vec<u32>>, wgpu::Buffer)>,
    /// Each step's parameters (a uniform buffer a step, kept).
    params: Vec<wgpu::Buffer>,
    words: Vec<u32>,
    errors: Arc<Mutex<Option<String>>>,
}

fn round4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

impl FilterGpu {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        precision: crate::idct::Precision,
        errors: Arc<Mutex<Option<String>>>,
    ) -> FilterGpu {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("filters"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_filter.wgsl").into()),
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
            label: Some("filters"),
            entries: &[
                entry(0, wgpu::BufferBindingType::Uniform),
                entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
                entry(2, wgpu::BufferBindingType::Storage { read_only: false }),
                entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("filters"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("filters"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let recon = crate::idct::Recon::new(&device, precision);
        FilterGpu {
            device,
            queue,
            recon,
            pipeline,
            layout,
            bufs: None,
            tables: Vec::new(),
            params: Vec::new(),
            words: Vec::new(),
            errors,
        }
    }

    fn table(&mut self, t: &Arc<Vec<u32>>) -> wgpu::Buffer {
        if let Some((_, b)) = self.tables.iter().find(|(k, _)| Arc::ptr_eq(k, t)) {
            return b.clone();
        }
        let b = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("filter table"),
            size: (t.len().max(1) * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&b, 0, bytemuck::cast_slice(t));
        // The tables of chains no longer used go (a LUT's is 4 MB).
        self.tables.retain(|(k, _)| Arc::strong_count(k) > 1);
        self.tables.push((t.clone(), b.clone()));
        b
    }

    pub(crate) fn run(&mut self, steps: &[GpuStep], f: &Frame) -> Result<Frame, String> {
        let (w, h) = (f.width, f.height);
        let cw = Frame::chroma_width(w);
        if w == 0 || h == 0 || steps.is_empty() {
            return Err("an empty picture".into());
        }
        let (ly, lc) = (round4(w * h), round4(cw * h));
        let off = [0, ly, ly + lc, ly + 2 * lc];
        let size = (ly + 2 * lc + ly) as u64;
        // The coefficients' words (a picture that came as them), else the planes go up.
        self.words.clear();
        let jobs = f.coefs.as_ref().map(|c| {
            let dst = off.map(|o| o as u32);
            let mut jobs = crate::idct::pack_into(c, &mut self.words, dst);
            if c.fourcc == idn_open_protocol::shq::Fourcc::Shq0 {
                for j in jobs.iter_mut().skip(1) {
                    *j = j.doubled(h as u32);
                }
            }
            jobs
        });
        let word_bytes = (self.words.len() as u64 * 4).max(4);
        if self
            .bufs
            .as_ref()
            .is_none_or(|b| b.4 < size || b.5 < word_bytes)
        {
            use wgpu::BufferUsages as U;
            let mk = |label, size: u64, usage| {
                self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage,
                    mapped_at_creation: false,
                })
            };
            let (size, word_bytes) = (size * 5 / 4 / 4 * 4 + 4, word_bytes * 5 / 4 / 4 * 4 + 4);
            self.bufs = Some((
                mk("filter a", size, U::STORAGE | U::COPY_SRC | U::COPY_DST),
                mk("filter b", size, U::STORAGE | U::COPY_SRC | U::COPY_DST),
                mk("filter read back", size, U::MAP_READ | U::COPY_DST),
                mk("filter coefficients", word_bytes, U::STORAGE | U::COPY_DST),
                size,
                word_bytes,
            ));
        }
        let tables: Vec<wgpu::Buffer> = steps.iter().map(|s| self.table(&s.table)).collect();
        let (a, b, back, coefs, _, _) = self.bufs.as_ref().expect("made above");
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("filters"),
            });
        match jobs {
            Some(jobs) => {
                self.queue
                    .write_buffer(coefs, 0, bytemuck::cast_slice(&self.words));
                enc.clear_buffer(a, 0, Some(size));
                self.recon.record(&self.device, &mut enc, coefs, a, &jobs);
            }
            None => {
                for (k, data) in [Some(&f.y), Some(&f.cb), Some(&f.cr), f.a.as_ref()]
                    .into_iter()
                    .enumerate()
                {
                    match data.filter(|d| !d.is_empty()) {
                        // Writes go in whole words: an odd-sized plane padded.
                        Some(d) if d.len() % 4 == 0 => self.queue.write_buffer(a, off[k] as u64, d),
                        Some(d) => {
                            let mut v = d.clone();
                            v.resize(round4(v.len()), 0);
                            self.queue.write_buffer(a, off[k] as u64, &v);
                        }
                        None => {}
                    }
                }
            }
        }
        // A coefficient picture carries its alpha in the coefficients (its `a` is an empty
        // placeholder): it has alpha when its FourCC says (keyed
        // graphics came out opaque when filtered on the GPU).
        let mut had_alpha = f.a.as_ref().is_some_and(|a| !a.is_empty())
            || f.coefs.as_ref().is_some_and(|c| c.fourcc.has_alpha());
        let (ny, nc) = ((w * h).div_ceil(4) as u32, (cw * h).div_ceil(4) as u32);
        let groups = (2 * ny + 2 * nc).div_ceil(GROUP);
        let params: Vec<[u32; 16]> = steps
            .iter()
            .map(|s| {
                // As `Params` in the shader.
                let p: [u32; 16] = [
                    s.op,
                    w as u32,
                    h as u32,
                    cw as u32,
                    off[0] as u32,
                    off[1] as u32,
                    off[2] as u32,
                    off[3] as u32,
                    ny,
                    nc,
                    ny,
                    had_alpha as u32,
                    s.opacity,
                    0,
                    0,
                    0,
                ];
                had_alpha |= (2..=4).contains(&s.op) || (s.op == 0 && s.opacity < 255);
                p
            })
            .collect();
        // One params buffer a step, kept and written again each picture, not a new one
        // each step of each picture.
        while self.params.len() < params.len() {
            self.params
                .push(self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("filter params"),
                    size: 64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
        }
        for (buf, p) in self.params.iter().zip(&params) {
            self.queue.write_buffer(buf, 0, bytemuck::cast_slice(p));
        }
        for (k, (p, t)) in self
            .params
            .iter()
            .zip(&tables)
            .take(params.len())
            .enumerate()
        {
            let (from, to) = if k % 2 == 0 { (a, b) } else { (b, a) };
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("filters"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: p.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: from.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: to.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: t.as_entire_binding(),
                    },
                ],
            });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("filter"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        let last = if steps.len() % 2 == 1 { b } else { a };
        enc.copy_buffer_to_buffer(last, 0, back, 0, size);
        let submission = self.queue.submit([enc.finish()]);
        let (done, failed) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        {
            let (done, failed) = (done.clone(), failed.clone());
            back.slice(..size)
                .map_async(wgpu::MapMode::Read, move |r| match r {
                    Ok(()) => done.store(true, Ordering::Release),
                    Err(_) => failed.store(true, Ordering::Release),
                });
        }
        while !done.load(Ordering::Acquire) {
            if failed.load(Ordering::Acquire) {
                return Err("GPU: a filtered picture could not be read back".into());
            }
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission.clone()),
                    timeout: Some(WAIT),
                })
                .map_err(|e| format!("GPU: {e}"))?;
        }
        if let Some(e) = self.errors.lock_ok().clone() {
            back.unmap();
            return Err(format!("GPU: {e}"));
        }
        let out = {
            let data = back
                .slice(..size)
                .get_mapped_range()
                .map_err(|e| format!("GPU: {e}"))?;
            let plane = |k: usize, n: usize| {
                let mut v = Plane::buffer(n);
                idn_open_protocol::mem::copy_uncached(&mut v, &data[off[k]..off[k] + n]);
                v
            };
            Frame {
                width: w,
                height: h,
                y: plane(0, w * h),
                cb: plane(1, cw * h),
                cr: plane(2, cw * h),
                a: had_alpha.then(|| plane(3, w * h)),
                coefs: None,
                gpu: None,
            }
        };
        back.unmap();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{Chain, items_from_json};
    use serde_json::json;

    fn chain(v: serde_json::Value) -> Chain {
        Chain::new(items_from_json(&v).unwrap(), None).unwrap()
    }

    /// A picture with every value in it: odd width, colour everywhere, and alpha.
    fn picture(w: usize, h: usize, alpha: bool) -> Frame {
        let mut s = 0x9e37_79b9u32;
        let mut next = move |lo: u32, hi: u32| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (lo + s % (hi - lo + 1)) as u8
        };
        let cw = Frame::chroma_width(w);
        Frame {
            width: w,
            height: h,
            y: (0..w * h).map(|_| next(16, 235)).collect(),
            cb: (0..cw * h).map(|_| next(16, 240)).collect(),
            cr: (0..cw * h).map(|_| next(16, 240)).collect(),
            a: alpha.then(|| (0..w * h).map(|_| next(0, 255)).collect()),
            coefs: None,
            gpu: None,
        }
    }

    /// A 17-point LUT that bends every colour (not the identity).
    fn cube(dir: &std::path::Path) -> String {
        let mut t = String::from("LUT_3D_SIZE 17\n");
        for b in 0..17 {
            for g in 0..17 {
                for r in 0..17 {
                    let (r, g, b) = (r as f32 / 16.0, g as f32 / 16.0, b as f32 / 16.0);
                    t += &format!("{} {} {}\n", r.powf(0.8), (g * 0.9 + b * 0.1), b * b);
                }
            }
        }
        let f = dir.join("bend.cube");
        std::fs::write(&f, t).unwrap();
        f.to_string_lossy().into_owned()
    }

    /// Every table filter, alone and chained, gives on the GPU exactly what it gives on
    /// the CPU: odd and even widths, with and without alpha. Skipped without a GPU.
    #[test]
    fn the_gpu_filters_as_the_cpu_does() {
        let _one = crate::GPU_TESTS.lock_ok();
        let gpu = match crate::gpu::Gpu::new(64, 64) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("no GPU here ({e}): skipped");
                return;
            }
        };
        let mut fg = gpu.filter_gpu();
        let dir = idn_common::testing::TempDir::new("idn-gpu-filter");
        let lut = cube(&dir);
        let chains = [
            json!([{"type": "correct", "gamma": 1.3, "contrast": 0.2, "hue": 25, "saturation": 1.4}]),
            json!([{"type": "correct", "brightness": -0.1, "opacity": 0.6}]),
            json!([{"type": "lut", "file": lut, "amount": 0.8}]),
            json!([{"type": "chroma_key", "colour": "#00c800", "similarity": 0.35}]),
            json!([{"type": "colour_key", "colour": "#3060a0", "similarity": 0.3, "smoothness": 0.1}]),
            json!([{"type": "luma_key", "min": 0.2, "min_smooth": 0.1, "max": 0.9}]),
            json!([{"type": "correct", "gamma": 0.9}, {"type": "lut", "file": lut},
                   {"type": "chroma_key"}, {"type": "luma_key", "min": 0.1}]),
            json!([{"type": "correct", "gamma": 1.0}, {"type": "sharpen", "amount": 0.7}]),
            json!([{"type": "luma_key", "min": 0.05}, {"type": "sharpen", "amount": 0.0}]),
            json!([{"type": "correct", "gamma": 1.2}, {"type": "sharpen", "amount": 1.0},
                   {"type": "chroma_key"}]),
        ];
        for v in chains {
            let c = chain(v.clone());
            let steps = c.gpu_steps().expect("table filters run on the GPU");
            for (w, h, alpha) in [(37, 9, false), (64, 16, true), (1, 3, false), (33, 2, true)] {
                let pic = picture(w, h, alpha);
                let want = c.apply(&pic, 0.0);
                let got = fg.run(&steps, &pic).unwrap();
                assert!(got.y == want.y, "{v} {w}x{h}: luma");
                assert!(
                    got.cb == want.cb && got.cr == want.cr,
                    "{v} {w}x{h}: chroma"
                );
                assert_eq!(got.a, want.a, "{v} {w}x{h}: alpha");
            }
        }
        // A camera's SpeedHQ coefficients: reconstructed on the GPU, then filtered (f64:
        // to the bit; f32: a step off now and then).
        use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
        use idn_open_protocol::shq::{Fourcc, Limits, VideoParams, decode_coefficients};
        let src = picture(96, 32, false);
        let planes = [
            Plane {
                width: 96,
                height: 32,
                data: src.y.clone(),
            },
            Plane {
                width: 48,
                height: 32,
                data: src.cb.clone(),
            },
            Plane {
                width: 48,
                height: 32,
                data: src.cr.clone(),
            },
        ];
        let payload = encode(
            &EncodeInput {
                fourcc: Fourcc::Shq2,
                width: 96,
                height: 32,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions {
                quality: 90,
                ..Default::default()
            },
        );
        let vp = VideoParams {
            fourcc: *b"SHQ2",
            width: 96,
            height: 32,
            field_id: 0,
        };
        let coefs = decode_coefficients(&vp, &payload, &Limits::default()).unwrap();
        let cam = Frame {
            y: Vec::new(),
            cb: Vec::new(),
            cr: Vec::new(),
            a: None,
            coefs: Some(Arc::new(coefs)),
            gpu: None,
            ..src
        };
        let exact = gpu.precision() == crate::idct::Precision::F64;
        for v in [
            json!([{"type": "lut", "file": lut}]),
            json!([{"type": "chroma_key"}]),
        ] {
            let c = chain(v.clone());
            let want = c.apply(&cam, 0.0);
            let got = fg.run(&c.gpu_steps().unwrap(), &cam).unwrap();
            let off = |a: &[u8], b: &[u8]| a.iter().zip(b).filter(|(x, y)| x != y).count();
            let n = off(&got.y, &want.y) + off(&got.cb, &want.cb) + off(&got.cr, &want.cr);
            assert!(
                if exact { n == 0 } else { n * 100 < got.y.len() },
                "{v}: {n} samples differ"
            );
        }
        // A scroll keeps the chain on the CPU; a sharpen runs there.
        assert!(
            chain(json!([{"type": "scroll", "x": 1}]))
                .gpu_steps()
                .is_none()
        );
        assert!(chain(json!([{"type": "sharpen"}])).gpu_steps().is_some());
        // Coefficients come up; planes, whatever the filters, don't.
        let light = chain(json!([{"type": "correct", "gamma": 1.1}, {"type": "sharpen"}]));
        let planes = picture(64, 16, false);
        assert!(!worth_it(&light.gpu_steps().unwrap(), &planes));
        assert!(worth_it(&light.gpu_steps().unwrap(), &cam));
        let lut = chain(json!([{"type": "lut", "file": cube(&dir)}]));
        assert!(!worth_it(&lut.gpu_steps().unwrap(), &planes));
    }
}
