//! Scenes drawn on the GPU (spec/extensions/idn-switcher.md §S.12, Wave 1 stage 1e): a
//! scene's items in one compute pass (`scene.wgsl`) on the compositor's own wgpu device,
//! with `scene::render`'s arithmetic — an upright item scaled with the CPU's taps into its
//! whole-pixel rectangle, a turned one sampled through its inverse placement — so the
//! scene comes out as the CPU draws it (within a step where float coordinates round the
//! other way). Its sources go up once each (kept while they are the same picture; a
//! picture that came as SpeedHQ coefficients is reconstructed here), the scene comes back
//! as planes with alpha.

use crate::frame::{Frame, Ycc};
use crate::scene::{Blend, Item, ItemSource};
use idn_common::sync::LockOk as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// An item and its border: two operations each.
const MAX_OPS: usize = 2 * crate::scene::MAX_ITEMS;
/// Ten vec4 an operation.
const OP_BYTES: usize = 176;
const WORKGROUP: u32 = 8;
const WAIT: Duration = Duration::from_secs(2);

fn round4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// What an operation draws.
enum Paint {
    Picture(Arc<Frame>),
    Colour(Ycc),
}

/// One operation: a picture or a colour placed on the canvas.
struct Op {
    paint: Paint,
    place: (f64, f64, f64, f64),
    crop: (f64, f64, f64, f64),
    degrees: f32,
    o: u32,
    blend: Blend,
    /// Corner radius, canvas pixels (0 square).
    radius: f64,
    /// Keyed by the source's alpha.
    keyed: bool,
}

pub struct SceneGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    globals: wgpu::Buffer,
    ops: wgpu::Buffer,
    /// The sources, each at its own place; what is held where (kept while the same
    /// picture is drawn again).
    pics: wgpu::Buffer,
    held: Vec<(Arc<Frame>, usize, usize)>,
    /// The most one buffer may hold on this device.
    limit: usize,
    /// The scene's planes and their mappable copy, for each size drawn lately (the
    /// program's and the mixes', S.15), newest last.
    outs: Vec<(wgpu::Buffer, wgpu::Buffer, (usize, usize))>,
    /// Scenes made, written again once nothing else holds them.
    made: Vec<Arc<Frame>>,
    recon: crate::idct::Recon,
    /// The scene's pictures as coefficients, on their way up.
    coefs: crate::idct::Upload,
    jobs: Vec<crate::idct::PlaneJob>,
    clears: Vec<(u64, u64)>,
    /// Pictures kept on the GPU (§E.27), copied into the buffer there: each one's buffer
    /// and its planes' copies.
    gpu_jobs: Vec<(Arc<wgpu::Buffer>, [idn_open_protocol::gpu::PlaneJob; 3])>,
    plane_copy: idn_open_protocol::gpu::PlaneCopy,
    /// Buffers scenes kept on the GPU are drawn into ([`render`](Self::render) with
    /// `keep`), handed out as their pictures and taken back once nothing holds them.
    kept: idn_open_protocol::gpu::Buffers,
    errors: Arc<std::sync::Mutex<Option<String>>>,
    /// The last scene's time (ms): building and uploading, the GPU, reading out.
    pub timing: (f64, f64, f64),
}

impl SceneGpu {
    pub(crate) fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        precision: crate::idct::Precision,
        errors: Arc<std::sync::Mutex<Option<String>>>,
    ) -> SceneGpu {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
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
        let entries = [
            entry(0, wgpu::BufferBindingType::Uniform),
            entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
            entry(2, wgpu::BufferBindingType::Storage { read_only: false }),
            entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
        ];
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene"),
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("scene"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let buffer = |label, size: usize, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size as u64,
                usage,
                mapped_at_creation: false,
            })
        };
        use wgpu::BufferUsages as U;
        let globals = buffer("scene globals", 48, U::UNIFORM | U::COPY_DST);
        let ops = buffer("scene items", MAX_OPS * OP_BYTES, U::STORAGE | U::COPY_DST);
        let pics = buffer("scene pictures", 4, U::STORAGE | U::COPY_DST);
        let l = device.limits();
        let limit = l.max_storage_buffer_binding_size.min(l.max_buffer_size) as usize;
        let recon = crate::idct::Recon::new(&device, precision);
        let plane_copy = idn_open_protocol::gpu::PlaneCopy::new(&device);
        SceneGpu {
            device,
            queue,
            pipeline,
            layout,
            globals,
            ops,
            pics,
            held: Vec::new(),
            limit,
            outs: Vec::new(),
            made: Vec::new(),
            recon,
            coefs: Default::default(),
            jobs: Vec::new(),
            clears: Vec::new(),
            gpu_jobs: Vec::new(),
            plane_copy,
            kept: idn_open_protocol::gpu::Buffers::new(
                "scene kept",
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                4,
            ),
            errors,
            timing: (0.0, 0.0, 0.0),
        }
    }

    /// Draws `items` over a transparent `w` × `h` canvas, as [`crate::scene::render`]
    /// does; `pic(id)` gives an input's newest picture. `opaque` (as
    /// [`crate::scene::opaque`] says): its alpha, 255 everywhere, not read back (a quarter
    /// of what comes back) and the picture without one. `keep`: not read back at all, the
    /// picture kept on the GPU (its Y, Cb and Cr; no alpha), for an output that sends it
    /// from there. `Err` says why it couldn't (the caller draws it on the CPU).
    pub fn render(
        &mut self,
        w: usize,
        h: usize,
        items: &[Item],
        pic: &dyn Fn(u32) -> Option<Arc<Frame>>,
        opaque: bool,
        keep: bool,
    ) -> Result<Arc<Frame>, String> {
        let t0 = std::time::Instant::now();
        let ops = operations(w, h, items, pic);
        // Each picture once, however many operations show it.
        let mut sources: Vec<Arc<Frame>> = Vec::new();
        let index: Vec<Option<usize>> = ops
            .iter()
            .map(|op| match &op.paint {
                Paint::Picture(p) => Some(
                    sources
                        .iter()
                        .position(|s| Arc::ptr_eq(s, p))
                        .unwrap_or_else(|| {
                            sources.push(p.clone());
                            sources.len() - 1
                        }),
                ),
                Paint::Colour(_) => None,
            })
            .collect();
        let offs = self.upload(&sources)?;
        // The operations' table.
        let mut table = Vec::with_capacity(ops.len() * OP_BYTES);
        let (cw, n) = (Frame::chroma_width(w), ops.len().min(MAX_OPS));
        for (op, src) in ops.iter().zip(&index).take(n) {
            let o = encode(op, src.map(|s| (&sources[s], offs[s])), w, h);
            table.extend_from_slice(&o);
        }
        let groups = w.div_ceil(8);
        let (ys, cs) = (groups * 2, groups);
        let mut globals = Vec::with_capacity(48);
        for x in [
            w as u32,
            h as u32,
            cw as u32,
            n as u32,
            ys as u32,
            cs as u32,
            (ys * h) as u32,
            (ys * h + cs * h) as u32,
            (ys * h + 2 * cs * h) as u32,
            0,
            0,
            0,
        ] {
            globals.extend_from_slice(&x.to_le_bytes());
        }
        let bytes = ((2 * ys + 2 * cs) * h * 4) as u64;
        if let Some(k) = self.outs.iter().position(|o| o.2 == (w, h)) {
            let o = self.outs.remove(k);
            self.outs.push(o);
        } else {
            let buffer = |label, usage| {
                self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: bytes,
                    usage,
                    mapped_at_creation: false,
                })
            };
            use wgpu::BufferUsages as U;
            let o = (
                buffer("scene out", U::STORAGE | U::COPY_SRC),
                buffer("scene down", U::MAP_READ | U::COPY_DST),
                (w, h),
            );
            if self.outs.len() >= 6 {
                self.outs.remove(0);
            }
            self.outs.push(o);
        }
        let kept = keep.then(|| self.kept.get(&self.device, bytes));
        let (out, down, _) = self.outs.last().expect("made above");
        let out = kept.as_deref().unwrap_or(out);
        self.queue.write_buffer(&self.globals, 0, &globals);
        if !table.is_empty() {
            self.queue.write_buffer(&self.ops, 0, &table);
        }
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene"),
            });
        if !self.coefs.is_empty() {
            let hand = idn_open_protocol::gpu::GpuHandIn {
                device: self.device.clone(),
                queue: self.queue.clone(),
            };
            let (coef_buf, jobs) = self.coefs.upload(&hand).map_err(|e| {
                // Too large for its buffers: that frame alone (bug run L4-4).
                if e.kind() == std::io::ErrorKind::FileTooLarge {
                    format!("{}{e}", crate::TOO_LARGE)
                } else {
                    format!("GPU: {e}")
                }
            })?;
            self.jobs = jobs;
            for &(at, size) in &self.clears {
                enc.clear_buffer(&self.pics, at, Some(size));
            }
            self.recon
                .record(&self.device, &mut enc, coef_buf, &self.pics, &self.jobs);
        }
        for (src, jobs) in &self.gpu_jobs {
            self.plane_copy
                .encode(&self.device, &mut enc, src, &self.pics, jobs)
                .map_err(|e| e.to_string())?;
        }
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.ops.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: out.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.pics.as_entire_binding(),
                },
            ],
        });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("scene"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(
                (cs as u32).div_ceil(WORKGROUP),
                (h as u32).div_ceil(WORKGROUP),
                1,
            );
        }
        if let Some(buffer) = kept {
            // Drawn there and left there: what takes it next on this queue runs after.
            self.queue.submit([enc.finish()]);
            if let Some(e) = self.errors.lock_ok().take() {
                return Err(format!("GPU: {e}"));
            }
            use idn_open_protocol::gpu::{GpuHandIn, GpuPicture, PlaneAt};
            let plane = |offset: usize, row: usize, width: usize| PlaneAt {
                offset: offset as u64,
                row_bytes: (row * 4) as u32,
                width: width as u32,
                height: h as u32,
            };
            let picture = GpuPicture {
                buffer,
                planes: vec![
                    plane(0, ys, w),
                    plane(ys * h * 4, cs, cw),
                    plane((ys + cs) * h * 4, cs, cw),
                ],
            };
            let hand = GpuHandIn {
                device: self.device.clone(),
                queue: self.queue.clone(),
            };
            self.timing = (t0.elapsed().as_secs_f64() * 1000.0, 0.0, 0.0);
            return Ok(Arc::new(Frame::on_gpu(picture, hand)));
        }
        enc.copy_buffer_to_buffer(out, 0, down, 0, bytes);
        let submission = self.queue.submit([enc.finish()]);
        // 0 waiting, 1 mapped, 2 failed.
        let mapped = Arc::new(AtomicU8::new(0));
        {
            let mapped = mapped.clone();
            down.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                mapped.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release)
            });
        }
        let t1 = std::time::Instant::now();
        while mapped.load(Ordering::Acquire) == 0 {
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission.clone()),
                    timeout: Some(WAIT),
                })
                .map_err(|e| format!("GPU: {e}"))?;
        }
        if let Some(e) = self.errors.lock_ok().take() {
            return Err(format!("GPU: {e}"));
        }
        if mapped.load(Ordering::Acquire) != 1 {
            return Err("GPU: the scene could not be read back".into());
        }
        let t2 = std::time::Instant::now();
        // A scene made before and no longer held anywhere is written again: a new one's
        // first touch of 33 MB at 2160p cost more than the copy.
        let k = match self.made.iter_mut().position(|f| {
            (f.width, f.height) == (w, h) && f.a.is_none() == opaque && Arc::get_mut(f).is_some()
        }) {
            Some(k) => k,
            None => {
                if self.made.len() >= 12 {
                    self.made.remove(0);
                }
                self.made.push(Arc::new(Frame {
                    width: w,
                    height: h,
                    y: vec![0; w * h],
                    cb: vec![0; cw * h],
                    cr: vec![0; cw * h],
                    a: (!opaque).then(|| vec![0; w * h]),
                    coefs: None,
                    gpu: None,
                }));
                self.made.len() - 1
            }
        };
        {
            let f = Arc::get_mut(&mut self.made[k]).expect("held only here");
            let data = down
                .slice(..)
                .get_mapped_range()
                .map_err(|e| format!("GPU read back: {e}"))?;
            use idn_open_protocol::mem::copy_uncached;
            let (yb, cb) = (ys * 4, cs * 4);
            let a0 = (ys + 2 * cs) * h * 4;
            // Rows not padded (a width of whole invocations): each plane in one pass.
            if yb == w && cb == cw {
                let (c0, c1) = (ys * h * 4, (ys + cs) * h * 4);
                copy_uncached(&mut f.y, &data[..w * h]);
                copy_uncached(&mut f.cb, &data[c0..c0 + cw * h]);
                copy_uncached(&mut f.cr, &data[c1..c1 + cw * h]);
                if let Some(fa) = f.a.as_mut() {
                    copy_uncached(fa, &data[a0..a0 + w * h]);
                }
            } else {
                for r in 0..h {
                    copy_uncached(&mut f.y[r * w..(r + 1) * w], &data[r * yb..r * yb + w]);
                    if let Some(fa) = f.a.as_mut() {
                        copy_uncached(
                            &mut fa[r * w..(r + 1) * w],
                            &data[a0 + r * yb..a0 + r * yb + w],
                        );
                    }
                    let c0 = ys * h * 4 + r * cb;
                    copy_uncached(&mut f.cb[r * cw..(r + 1) * cw], &data[c0..c0 + cw]);
                    let c1 = (ys + cs) * h * 4 + r * cb;
                    copy_uncached(&mut f.cr[r * cw..(r + 1) * cw], &data[c1..c1 + cw]);
                }
            }
        }
        down.unmap();
        let ms = |a: std::time::Instant, b: std::time::Instant| (b - a).as_secs_f64() * 1000.0;
        self.timing = (ms(t0, t1), ms(t1, t2), ms(t2, std::time::Instant::now()));
        Ok(self.made[k].clone())
    }

    /// Puts the sources up (those held from the last scene stay where they are) and gives
    /// each one's planes' byte offsets.
    fn upload(&mut self, sources: &[Arc<Frame>]) -> Result<Vec<[u32; 4]>, String> {
        // Coefficients past the GPU's buffers: refused before anything here changes (bug
        // run M7-1/M7-4).
        let words: usize = sources
            .iter()
            .filter_map(|p| p.coefs.as_ref())
            .map(|c| idn_open_protocol::shq::gpu_idct::words_len(c))
            .sum();
        let limits = self.device.limits();
        let most = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        if (words as u64) * 4 > most {
            return Err(format!(
                "{}{} MB of coefficients in one scene",
                crate::TOO_LARGE,
                (words * 4) >> 20
            ));
        }
        self.coefs.clear();
        self.jobs.clear();
        self.clears.clear();
        self.gpu_jobs.clear();
        // Kept: the held pictures drawn again; the rest of the buffer is free.
        let mut placed: Vec<(usize, usize)> = Vec::new();
        let mut at: Vec<Option<usize>> = sources
            .iter()
            .map(|p| {
                let h = self.held.iter().find(|h| Arc::ptr_eq(&h.0, p))?;
                placed.push((h.1, h.2));
                Some(h.1)
            })
            .collect();
        let mut fresh = Vec::new();
        for (k, p) in sources.iter().enumerate() {
            if at[k].is_some() {
                continue;
            }
            let size = round4(layout_of(p).1);
            // The first gap big enough between the pictures placed.
            placed.sort_unstable();
            let mut base = 0;
            for &(b, s) in &placed {
                if base + size <= b {
                    break;
                }
                base = base.max(b + s);
            }
            placed.push((base, size));
            at[k] = Some(base);
            fresh.push(k);
        }
        let end = placed.iter().map(|&(b, s)| b + s).max().unwrap_or(4).max(4);
        if end > self.limit {
            return Err(format!(
                "the scene's pictures ({} MB) are more than the GPU takes in one buffer",
                end >> 20
            ));
        }
        if end as u64 > self.pics.size() {
            // A bigger buffer, with room to grow: everything goes up again.
            let size = round4((end * 5 / 4).min(self.limit));
            self.pics = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("scene pictures"),
                size: size as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            fresh = (0..sources.len()).collect();
        }
        for k in fresh {
            let base = at[k].expect("placed");
            let (offs, size) = layout_of(&sources[k]);
            // Kept on the GPU (§E.27): copied into its place there, as the compositor
            // does; rows not whole words can't be copied so: read back.
            let px;
            let p = match &sources[k] {
                p if let Some(g) = &p.gpu => {
                    let (w, cw) = (p.width, Frame::chroma_width(p.width));
                    if w.is_multiple_of(4) && cw.is_multiple_of(4) {
                        use idn_open_protocol::gpu::{PlaneAt, PlaneJob};
                        let from = &g.picture.planes;
                        let job = |k: usize, width: usize| PlaneJob {
                            from: from[k],
                            to: PlaneAt {
                                offset: (base + offs[k]) as u64,
                                row_bytes: width as u32,
                                width: width as u32,
                                height: p.height as u32,
                            },
                            double_rows: (from[k].height as usize) < p.height,
                        };
                        self.gpu_jobs.push((
                            g.picture.buffer.clone(),
                            [job(0, w), job(1, cw), job(2, cw)],
                        ));
                        continue;
                    }
                    px = Arc::new(p.pixels());
                    &px
                }
                p => p,
            };
            if let Some(c) = &p.coefs {
                let dst = offs.map(|o| (base + o) as u32);
                let doubled =
                    (c.fourcc == idn_open_protocol::shq::Fourcc::Shq0).then_some(p.height as u32);
                self.coefs.add(c.clone(), dst, doubled);
                self.clears.push((base as u64, round4(size) as u64));
                continue;
            }
            for (k, data) in [Some(&p.y), Some(&p.cb), Some(&p.cr), p.a.as_ref()]
                .into_iter()
                .enumerate()
            {
                if let Some(data) = data.filter(|d| !d.is_empty()) {
                    let at = base + offs[k];
                    // Whole words: the last few bytes padded.
                    let whole = data.len() & !3;
                    if whole > 0 {
                        self.queue
                            .write_buffer(&self.pics, at as u64, &data[..whole]);
                    }
                    if whole < data.len() {
                        let mut tail = [0u8; 4];
                        tail[..data.len() - whole].copy_from_slice(&data[whole..]);
                        self.queue
                            .write_buffer(&self.pics, (at + whole) as u64, &tail);
                    }
                }
            }
        }
        self.held = sources
            .iter()
            .zip(&at)
            .map(|(p, a)| (p.clone(), a.expect("placed"), round4(layout_of(p).1)))
            .collect();
        Ok(sources
            .iter()
            .zip(&at)
            .map(|(p, a)| layout_of(p).0.map(|o| (a.expect("placed") + o) as u32))
            .collect())
    }
}

/// A picture's planes' offsets within its place, and the bytes it takes (as the
/// compositor lays them out).
fn layout_of(pic: &Frame) -> ([usize; 4], usize) {
    let (w, h) = (pic.width, pic.height);
    // (As coefficients, or kept on the GPU: its sizes from its dimensions.)
    let coefs = pic.deferred();
    let len = |v: &Vec<u8>, n: usize| if coefs { n } else { v.len() };
    let (ly, lc) = (
        round4(len(&pic.y, w * h)),
        round4(len(&pic.cb, Frame::chroma_width(w) * h)),
    );
    let la = pic.a.as_ref().map_or(0, |a| round4(len(a, w * h)));
    ([0, ly, ly + lc, ly + 2 * lc], ly + 2 * lc + la)
}

/// The scene's operations, bottom to top, as `scene::render` draws them: each visible
/// item's border, then the item.
fn operations(
    w: usize,
    h: usize,
    items: &[Item],
    pic: &dyn Fn(u32) -> Option<Arc<Frame>>,
) -> Vec<Op> {
    let mut ops = Vec::new();
    for it in items.iter().filter(|i| i.visible && i.opacity > 0.0) {
        let place = (
            it.rect.0 as f64 * w as f64,
            it.rect.1 as f64 * h as f64,
            it.rect.2 as f64 * w as f64,
            it.rect.3 as f64 * h as f64,
        );
        if place.2 < 1.0 || place.3 < 1.0 {
            continue;
        }
        let paint = match &it.source {
            ItemSource::Input(id) => match pic(*id) {
                Some(p) => Paint::Picture(p),
                None => continue,
            },
            ItemSource::Colour(c) => Paint::Colour(*c),
        };
        let o = ((it.opacity.clamp(0.0, 1.0) * 256.0).round() as u32).min(256);
        if o == 0 {
            continue;
        }
        let radius = crate::scene::corner_radius(it, place);
        if it.border > 0 {
            let b = it.border as f64;
            ops.push(Op {
                paint: Paint::Colour(it.border_colour),
                place: (
                    place.0 - b,
                    place.1 - b,
                    place.2 + 2.0 * b,
                    place.3 + 2.0 * b,
                ),
                crop: (0.0, 0.0, 1.0, 1.0),
                degrees: it.rotation,
                o,
                blend: Blend::Normal,
                radius: if radius > 0.0 { radius + b } else { 0.0 },
                keyed: true,
            });
        }
        let c = it.crop;
        ops.push(Op {
            paint,
            place,
            crop: (c.0 as f64, c.1 as f64, c.2 as f64, c.3 as f64),
            degrees: it.rotation,
            o,
            blend: it.blend,
            radius,
            keyed: it.keyed,
        });
    }
    ops
}

/// An operation as the shader's `Op` (176 bytes); a colour has no source.
fn encode(op: &Op, src: Option<(&Arc<Frame>, [u32; 4])>, w: usize, h: usize) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::with_capacity(OP_BYTES);
    let f4 = |v: &mut Vec<u8>, a: [f64; 4]| {
        a.iter()
            .for_each(|x| v.extend_from_slice(&(*x as f32).to_le_bytes()))
    };
    let u4 =
        |v: &mut Vec<u8>, a: [u32; 4]| a.iter().for_each(|x| v.extend_from_slice(&x.to_le_bytes()));
    let (x, y, iw, ih) = op.place;
    let crop = op.crop;
    // A colour is a 2 × 2 picture of it (as on the CPU), opaque.
    let (sw, sh, alpha, offs) = match src {
        Some((p, offs)) => (p.width, p.height, p.a.is_some() && op.keyed, offs),
        None => (2, 2, false, [0; 4]),
    };
    let scw = Frame::chroma_width(sw);
    let turned = op.degrees.rem_euclid(360.0).abs() > 1e-3;
    // (Rounded corners: sampled through the placement, as the CPU does.)
    let upright = !turned && op.radius <= 0.0 && x >= -0.5 && y >= -0.5;
    let th = (op.degrees as f64).to_radians();
    let (cos, sin) = (th.cos(), th.sin());
    let (cx, cy) = (x + iw / 2.0, y + ih / 2.0);
    let (tx, ty, irect, exact) = if upright {
        let (wi, hi) = (iw.round() as usize, ih.round() as usize);
        let (xi, yi) = (
            (x.round().max(0.0) as usize) & !1,
            y.round().max(0.0) as usize,
        );
        if wi == 0 || hi == 0 || xi >= w || yi >= h {
            // Off the canvas: nothing to draw (an empty rectangle).
            ([0.0; 4], [0.0; 4], [0; 4], false)
        } else {
            let cwi = Frame::chroma_width(wi);
            let tx = [
                crop.0 * sw as f64,
                (crop.2 - crop.0) * sw as f64 / wi as f64,
                crop.0 * scw as f64,
                (crop.2 - crop.0) * scw as f64 / cwi as f64,
            ];
            let ty = [
                crop.1 * sh as f64,
                (crop.3 - crop.1) * sh as f64 / hi as f64,
                0.0,
                0.0,
            ];
            let whole = crop == (0.0, 0.0, 1.0, 1.0);
            (
                tx,
                ty,
                [xi as u32, yi as u32, wi as u32, hi as u32],
                whole && (sw, sh) == (wi, hi) && src.is_some(),
            )
        }
    } else {
        // The turned rectangle's bounds on the canvas.
        let corners = [(-iw, -ih), (iw, -ih), (iw, ih), (-iw, ih)].map(|(u, v)| {
            let (u, v) = (u / 2.0, v / 2.0);
            (cx + u * cos - v * sin, cy + u * sin + v * cos)
        });
        let lo = |v: [f64; 4]| v.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = |v: [f64; 4]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let (xs, ys) = (corners.map(|p| p.0), corners.map(|p| p.1));
        let (x0, x1) = (lo(xs).floor().max(0.0), hi(xs).ceil().min(w as f64));
        let (y0, y1) = (lo(ys).floor().max(0.0), hi(ys).ceil().min(h as f64));
        let r = if x1 <= x0 || y1 <= y0 {
            [0; 4]
        } else {
            [x0 as u32, y0 as u32, x1 as u32, y1 as u32]
        };
        ([0.0; 4], [0.0; 4], r, false)
    };
    f4(&mut v, [x, y, iw, ih]);
    f4(&mut v, [crop.0, crop.1, crop.2, crop.3]);
    f4(&mut v, [cos, sin, cx, cy]);
    f4(&mut v, tx);
    f4(&mut v, ty);
    u4(&mut v, irect);
    u4(&mut v, [sw as u32, sh as u32, scw as u32, alpha as u32]);
    u4(&mut v, offs);
    let blend = match op.blend {
        Blend::Normal => 0,
        Blend::Add => 1,
        Blend::Multiply => 2,
        Blend::Screen => 3,
    };
    let colour = match op.paint {
        Paint::Colour(c) => Some(c),
        Paint::Picture(_) => None,
    };
    let flags = exact as u32 | (colour.is_some() as u32) << 1;
    u4(&mut v, [op.o, blend, upright as u32, flags]);
    let c = colour.unwrap_or(Ycc::BLACK);
    u4(&mut v, [c.0 as u32, c.1 as u32, c.2 as u32, 0]);
    f4(&mut v, [op.radius, 0.0, 0.0, 0.0]);
    debug_assert_eq!(v.len(), OP_BYTES);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene;

    /// A picture with structure (gradients), so scaling and positions show; alpha too.
    fn pattern(w: usize, h: usize, seed: u8, alpha: bool) -> Frame {
        let mut f = Frame::black(w, h);
        let cw = Frame::chroma_width(w);
        for r in 0..h {
            for x in 0..w {
                f.y[r * w + x] = (16 + (x * 7 + r * 3 + seed as usize * 31) % 220) as u8;
            }
            for c in 0..cw {
                f.cb[r * cw + c] = (16 + (c * 5 + r + seed as usize * 17) % 224) as u8;
                f.cr[r * cw + c] = (16 + (c * 3 + r * 2 + seed as usize * 13) % 224) as u8;
            }
        }
        if alpha {
            f.a = Some(
                (0..w * h)
                    .map(|i| ((i % w) * 255 / w.max(2)) as u8)
                    .collect(),
            );
        }
        f
    }

    /// Samples (of all four planes) more than a step apart, and the largest difference.
    fn compare(a: &Frame, b: &Frame) -> (usize, u8) {
        let planes = [
            (&a.y, &b.y),
            (&a.cb, &b.cb),
            (&a.cr, &b.cr),
            (a.a.as_ref().unwrap(), b.a.as_ref().unwrap()),
        ];
        let mut far = 0;
        let mut most = 0;
        for (x, y) in planes {
            assert_eq!(x.len(), y.len());
            for (p, q) in x.iter().zip(y.iter()) {
                let d = p.abs_diff(*q);
                most = most.max(d);
                far += (d > 1) as usize;
            }
        }
        (far, most)
    }

    fn item(source: ItemSource, rect: (f32, f32, f32, f32)) -> Item {
        Item {
            source,
            rect,
            ..Default::default()
        }
    }

    /// Scenes drawn on the GPU as the CPU draws them: upright items (whole and the same
    /// size: to the bit; scaled, cropped, blended, faded, bordered: within a step), and
    /// turned ones (within a step but for a few samples on their edges, where the GPU's
    /// float coordinates land the other side).
    #[test]
    fn scenes_on_the_gpu_as_on_the_cpu() {
        let _one = crate::GPU_TESTS.lock_ok();
        let (w, h) = (1280, 720);
        let g = match crate::gpu::Gpu::new(w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
        };
        let mut sg = g.scene_drawer();
        let cam = Arc::new(pattern(w, h, 1, false));
        let cam2 = Arc::new(pattern(1920, 1080, 2, false));
        let third = Arc::new(pattern(w, 180, 3, true));
        let logo = Arc::new(pattern(301, 157, 4, true));
        let pic = |id: u32| -> Option<Arc<Frame>> {
            Some(match id {
                1 => cam.clone(),
                2 => cam2.clone(),
                3 => third.clone(),
                4 => logo.clone(),
                _ => return None,
            })
        };
        let red = Ycc(81, 90, 240);
        let upright = vec![
            item(ItemSource::Input(1), (0.0, 0.0, 1.0, 1.0)),
            Item {
                crop: (0.1, 0.2, 0.7, 0.9),
                border: 4,
                border_colour: red,
                ..item(ItemSource::Input(2), (0.55, 0.05, 0.4, 0.4))
            },
            item(ItemSource::Input(3), (0.0, 0.75, 1.0, 0.25)),
            Item {
                opacity: 0.6,
                blend: Blend::Screen,
                ..item(ItemSource::Input(4), (0.03, 0.04, 0.2, 0.18))
            },
            Item {
                blend: Blend::Multiply,
                ..item(ItemSource::Colour(red), (0.3, 0.3, 0.1, 0.1))
            },
            item(ItemSource::Input(9), (0.0, 0.0, 0.5, 0.5)),
        ];
        let turned = vec![
            item(ItemSource::Input(1), (0.0, 0.0, 1.0, 1.0)),
            Item {
                rotation: 12.0,
                border: 3,
                ..item(ItemSource::Input(4), (0.6, 0.1, 0.3, 0.3))
            },
            Item {
                rotation: -30.0,
                opacity: 0.5,
                blend: Blend::Add,
                ..item(ItemSource::Input(2), (0.1, 0.4, 0.4, 0.4))
            },
            Item {
                rotation: 45.0,
                ..item(ItemSource::Colour(red), (0.45, 0.45, 0.1, 0.1))
            },
            // Off the left edge, upright: the CPU's slow path.
            item(ItemSource::Input(3), (-0.1, 0.8, 0.6, 0.2)),
            // Rounded (upright and turned, bordered), and a picture's alpha ignored.
            Item {
                radius: 0.2,
                border: 3,
                border_colour: red,
                ..item(ItemSource::Input(2), (0.05, 0.05, 0.3, 0.3))
            },
            Item {
                radius: 0.5,
                rotation: 20.0,
                keyed: false,
                ..item(ItemSource::Input(4), (0.7, 0.6, 0.2, 0.3))
            },
            Item {
                keyed: false,
                ..item(ItemSource::Input(3), (0.0, 0.5, 0.5, 0.125))
            },
        ];
        // The whole camera turned (the case the GPU is for).
        let whole = vec![Item {
            rotation: 7.5,
            ..item(ItemSource::Input(2), (0.0, 0.0, 1.0, 1.0))
        }];
        for (name, items, edges) in [
            ("upright", &upright, 0),
            ("turned", &turned, w * h / 200),
            ("whole turned", &whole, w * h / 200),
        ] {
            let cpu = scene::render(w, h, items, &pic);
            let gpu = sg.render(w, h, items, &pic, false, false).unwrap();
            let (far, most) = compare(&cpu, &gpu);
            eprintln!(
                "{name}: {far} samples more than a step apart (most {most}); {:?}",
                sg.timing
            );
            assert!(
                far <= edges,
                "{name}: {far} samples more than a step apart (most {most})"
            );
        }
        // Whole, at its own size: to the bit.
        let items = vec![item(ItemSource::Input(1), (0.0, 0.0, 1.0, 1.0))];
        let (far, most) = compare(
            &scene::render(w, h, &items, &pic),
            &sg.render(w, h, &items, &pic, false, false).unwrap(),
        );
        assert_eq!((far, most), (0, 0));
        // An empty scene: transparent black.
        let empty = sg.render(w, h, &[], &pic, false, false).unwrap();
        assert_eq!(compare(&scene::canvas(w, h), &empty), (0, 0));
    }
}
