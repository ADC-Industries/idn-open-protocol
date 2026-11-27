//! The compositor on the GPU (spec/extensions/idn-switcher.md §S.9, stage 3): the program
//! bus with its transition (a stinger's clip over it) and fade to black in one compute pass
//! (`compose.wgsl`), through wgpu (Vulkan, DX12 or Metal). Pictures go up as they are
//! (each layer's planes as bytes, scaled and letterboxed in the shader) and only when
//! they change; the program comes back as planes. The arithmetic is the CPU's
//! (`compose.rs`): a picture that isn't scaled comes out the same to the bit (but for a
//! circle wipe's soft edge, within a step).

use crate::compose::{Direction, Kind};
use crate::frame::Frame;
use idn_common::sync::LockOk as _;
use idn_open_protocol::gpu::block_on;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

mod setup;
pub(crate) use setup::GpuSeed;
mod program_out;
mod upload;

/// Layers: program, preview, and a transition's material (a luma wipe's pattern or a
/// stinger's clip). Global layers (§S.5) are drawn into the scenes, not here.
pub(crate) const LAYERS: usize = 3;
/// The largest program, and the most a picture may take in its region (2160p with alpha).
const MAX_PIXELS: usize = 3840 * 2160;
const REGION_BYTES: usize = MAX_PIXELS * 3;
const WORKGROUP: u32 = 8;

/// A picture placed in the program.
#[derive(Clone)]
pub struct Layer {
    /// The picture as it arrived (any size).
    pub pic: Arc<Frame>,
    /// Where it goes: x (even), y, width, height in program pixels.
    pub rect: (usize, usize, usize, usize),
    /// The part shown: x0, y0, x1, y1 as fractions.
    pub crop: (f32, f32, f32, f32),
}

impl Layer {
    /// The whole picture fitted into `w` × `h` (aspect kept, centred), as `Frame::fitted`.
    pub fn fitted(pic: Arc<Frame>, w: usize, h: usize) -> Layer {
        let rect = if (pic.width, pic.height) == (w, h) {
            (0, 0, w, h)
        } else {
            let s = (w as f64 / pic.width.max(1) as f64).min(h as f64 / pic.height.max(1) as f64);
            let (sw, sh) = (
                ((pic.width as f64 * s).round() as usize).clamp(1, w),
                ((pic.height as f64 * s).round() as usize).clamp(1, h),
            );
            (((w - sw) / 2) & !1, (h - sh) / 2, sw, sh)
        };
        Layer {
            pic,
            rect,
            crop: (0.0, 0.0, 1.0, 1.0),
        }
    }
}

/// One program frame to make.
pub struct Scene {
    pub width: usize,
    pub height: usize,
    pub program: Layer,
    /// The transition under way: the preview, its kind, the position (0–1).
    pub transition: Option<(Layer, Kind, f32)>,
    pub ftb: f32,
    /// A luma wipe's pattern (with `transition`'s `Kind::Luma`), fitted to the frame.
    pub pattern: Option<Layer>,
    /// A stinger's clip, keyed over the bus (§S.14).
    pub stinger: Option<Layer>,
}

/// A buffer the GPU maps for the CPU, and whether that has happened.
pub(crate) struct Mapped {
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) ready: Arc<AtomicBool>,
    /// Its last mapping failed: waiting for it would never end.
    failed: Arc<AtomicBool>,
}

impl Mapped {
    pub(crate) fn new(
        device: &wgpu::Device,
        label: &str,
        size: u64,
        usage: wgpu::BufferUsages,
        mapped: bool,
    ) -> Mapped {
        Mapped {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: mapped,
            }),
            ready: Arc::new(AtomicBool::new(mapped)),
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Waits until it is mapped (`Err` if it can't be).
    pub(crate) fn wait(
        &self,
        device: &wgpu::Device,
        submission: Option<wgpu::SubmissionIndex>,
    ) -> Result<(), String> {
        while !self.ready.load(Ordering::Acquire) {
            if self.failed.load(Ordering::Acquire) {
                return Err("GPU: a buffer could not be mapped".into());
            }
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: submission.clone(),
                    timeout: Some(WAIT),
                })
                .map_err(|e| format!("GPU: {e}"))?;
        }
        Ok(())
    }

    /// Asks for it to be mapped once the work submitted so far is done.
    pub(crate) fn map(&self, mode: wgpu::MapMode) {
        self.ready.store(false, Ordering::Release);
        self.failed.store(false, Ordering::Release);
        let (ready, failed) = (self.ready.clone(), self.failed.clone());
        self.buffer.slice(..).map_async(mode, move |r| match r {
            Ok(()) => ready.store(true, Ordering::Release),
            Err(_) => failed.store(true, Ordering::Release),
        });
    }
}

/// A picture on the GPU to copy into its region: its buffer and its planes' copies.
type GpuJob = (Arc<wgpu::Buffer>, [idn_open_protocol::gpu::PlaneJob; 3]);

/// A frame submitted and not yet read out: which readback buffer, its size.
struct Pending {
    index: usize,
    submission: wgpu::SubmissionIndex,
    size: (usize, usize),
    /// Its program was copied into `share[index]` too (a GPU of its own memory, §E.27).
    shared: bool,
    /// Read back into this memory (not when the program stays on the GPU only).
    read: bool,
    /// Read back by [`Gpu::read_into`], straight into the output's shared memory.
    ring: bool,
}

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    globals: wgpu::Buffer,
    layers: wgpu::Buffer,
    out: wgpu::Buffer,
    /// Two of each on the way up and down, so one frame's pictures are written and the
    /// one before it read out while the GPU works on a third.
    readback: [Mapped; 2],
    staging: [Mapped; 2],
    /// The pictures, in regions of `region` bytes, and what each region holds (per set
    /// of pictures: one, or two when unified).
    pics: wgpu::Buffer,
    region: usize,
    /// Frames in a row the scene's pictures were smaller than `region` while the HDR pass
    /// didn't fit.
    small_for: u32,
    held: [Vec<Option<Arc<Frame>>>; 2],
    /// Per region: its HDR picture was copied up raw (a luma wipe's pattern), not lifted;
    /// a picture wanted the other way is uploaded again.
    held_raw: [Vec<bool>; 2],
    /// One memory (an integrated GPU, wgpu's `MAPPABLE_PRIMARY_BUFFERS`): the pictures are
    /// written straight into the buffers the shader reads (`staging`, taking turns, each
    /// with its own pictures) and the program read straight from the one it writes
    /// (`readback`), with no copies between them on the GPU: at 2160p those were 150 MB
    /// a frame through the memory the CPU's own copies need. Else (`pics`, `out`) through
    /// staging and readback copies.
    unified: bool,
    /// This frame's copies from staging to the pictures.
    copies: Vec<(u64, u64)>,
    /// Pictures that came as coefficients, reconstructed here (perf queue 2b): the
    /// pipeline, the buffer their words go up in, and this frame's words, jobs (each
    /// plane's) and regions to clear first.
    recon: crate::idct::Recon,
    /// This frame's pictures as coefficients, on their way up.
    coefs: crate::idct::Upload,
    recon_jobs: Vec<crate::idct::PlaneJob>,
    /// Pictures kept on the GPU (§E.27): this frame's copies of them into their regions,
    /// and the pipeline that copies (4:2:0 chroma rows doubled into the regions' 4:2:2).
    gpu_jobs: Vec<GpuJob>,
    plane_copy: idn_open_protocol::gpu::PlaneCopy,
    clears: Vec<(u64, u64)>,
    /// Which staging and readback buffers the next frame uses.
    next: usize,
    /// The readback buffer the frame last read out is in, while it stays there (one
    /// memory: the buffer the shader wrote), for its output to share GPU to GPU (§E.27).
    last_out: Option<usize>,
    /// Shrinks the program on the GPU for the multiview's tile ([`program_small`]), made
    /// when first needed.
    ///
    /// [`program_small`]: Self::program_small
    shrink: Option<idn_open_protocol::gpu::PlaneShrink>,
    /// Receivers on this machine take the program GPU to GPU (set by the switcher each
    /// frame). On a GPU of its own memory the program's buffer holds the next frame by the
    /// time one is sent, so each frame is copied, on the GPU, into one of these two
    /// buffers, kept until that frame's turn comes round again.
    pub share_program: bool,
    /// Nothing wants the program's samples in this memory (set by the switcher each
    /// frame): it stays on the GPU, never read back (a 2160p program was 17 MB copied out
    /// each frame for a receiver taking it on the GPU, and for nobody once nothing took
    /// it). Its frames come out with their sizes and no samples.
    pub gpu_only: bool,
    /// Only receivers reading this machine's shared memory want the program's samples
    /// (set by the switcher each frame): its frames come out with their sizes and no
    /// samples, and [`read_into`](Self::read_into) copies the read-back straight into
    /// that memory (§E.26). Not on one memory while the program is shared GPU to GPU (the
    /// buffer read is the one shared).
    pub ring_only: bool,
    /// The readback buffer a frame came out of still mapped, for `read_into`.
    mapped: std::cell::Cell<Option<usize>>,
    share: Option<[wgpu::Buffer; 2]>,
    /// A program passed through as it came (not composed), uploaded for receivers on this
    /// machine that take it on the GPU: once here instead of once in each of them.
    passed: Option<wgpu::Buffer>,
    /// Frames submitted and not yet read out, oldest first (at most two).
    in_flight: VecDeque<Pending>,
    size: (usize, usize),
    /// The adapter, for the state: "AMD Radeon 680M (Vulkan)".
    pub name: String,
    /// The last frame's time (ms): pictures written for upload, waiting for the GPU, the
    /// planes read out.
    pub timing: (f64, f64, f64),
    /// The GPU's first complaint since the last frame (wgpu's default panics: the frame
    /// clock stopped, and the program with it).
    errors: Arc<std::sync::Mutex<Option<String>>>,
    /// An HDR program's pass beside the 8-bit one (R.24, `gpu_hdr`), once asked for.
    hdr: Option<crate::gpu_hdr::HdrPath>,
    /// The luma wipe's pattern of the frame being uploaded (HDR: copied, not lifted).
    raw_lift: Option<Arc<Frame>>,
}

fn round4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

const WAIT: Duration = Duration::from_secs(2);

/// Polls until `m` is mapped.
fn wait_mapped(device: &wgpu::Device, m: &Mapped) -> Result<(), String> {
    m.wait(device, None)
}

/// A program shrunk by [`Gpu::program_small_on_gpu`] (4:2:2, `w` × `h`), read back.
pub fn read_small(
    small: &idn_open_protocol::gpu::GpuPicture,
    hand: &idn_open_protocol::gpu::GpuHandIn,
    w: usize,
    h: usize,
) -> Option<Frame> {
    let mut p = small.read_planes(hand).ok()?.into_iter();
    Some(Frame {
        width: w,
        height: h,
        y: p.next()?,
        cb: p.next()?,
        cr: p.next()?,
        a: None,
        coefs: None,
        gpu: None,
    })
}

impl Gpu {
    /// An HDR program (R.24): each frame also composed in the HDR space from 16-bit
    /// pictures and read back as P216 ([`take_hdr`](Self::take_hdr)); `Sdr`: none.
    pub fn set_hdr(&mut self, transfer: idn_open_protocol::color::Transfer) {
        self.hdr = (transfer != idn_open_protocol::color::Transfer::Sdr)
            .then(|| crate::gpu_hdr::HdrPath::new(&self.device, &self.layout, transfer));
        // Every picture up again, to be lifted.
        self.held = [vec![None; LAYERS], vec![None; LAYERS]];
    }

    /// The HDR program last read back (P216, `width × height × 2` samples), once.
    pub fn take_hdr(&mut self) -> Option<Vec<u16>> {
        self.hdr.as_mut()?.last.take()
    }

    /// A drawer of scenes on this compositor's device (§S.12, stage 1e).
    pub fn scene_drawer(&self) -> crate::gpu_scene::SceneGpu {
        crate::gpu_scene::SceneGpu::new(
            self.device.clone(),
            self.queue.clone(),
            self.recon.precision,
            self.errors.clone(),
        )
    }

    /// A runner of inputs' filters on this compositor's device (§S.23).
    pub fn filter_gpu(&self) -> crate::gpu_filter::FilterGpu {
        crate::gpu_filter::FilterGpu::new(
            self.device.clone(),
            self.queue.clone(),
            self.recon.precision,
            self.errors.clone(),
        )
    }

    /// Whether pictures reconstructed here are as the CPU's (f64) or within a step.
    pub fn precision(&self) -> crate::idct::Precision {
        self.recon.precision
    }

    /// Makes the frame.
    pub fn compose(&mut self, s: &Scene) -> Result<Frame, String> {
        let mut f = Frame {
            width: 0,
            height: 0,
            y: Vec::new(),
            cb: Vec::new(),
            cr: Vec::new(),
            a: None,
            coefs: None,
            gpu: None,
        };
        self.compose_into(s, &mut f)?;
        Ok(f)
    }

    /// Makes the frame into `f` and waits for it; `f`'s planes' memory is used again (a
    /// new frame's first touch of 25 MB at 2160p costs milliseconds).
    pub fn compose_into(&mut self, s: &Scene, f: &mut Frame) -> Result<(), String> {
        self.submit(s)?;
        while self.finish(f)? {}
        self.check()
    }

    fn check(&self) -> Result<(), String> {
        match self.errors.lock_ok().take() {
            Some(e) => Err(format!("GPU: {e}")),
            None => Ok(()),
        }
    }

    /// Pipelined: starts frame `s` on the GPU and reads the frame started before it into
    /// `f` (false for the first, when there is none), so that the CPU's part of one frame
    /// and the GPU's of the next run at once. The program is a frame late.
    pub fn compose_next(&mut self, s: &Scene, f: &mut Frame) -> Result<bool, String> {
        self.last_out = None;
        self.submit(s)?;
        let ready = if self.in_flight.len() > 1 {
            self.finish(f)?
        } else {
            false
        };
        self.check()?;
        Ok(ready)
    }

    /// Reads the frames still in flight, the last into `f` (false if there were none): the
    /// program goes on without the compositor (its picture as it is), so none is read out
    /// stale when composing starts again.
    pub fn take_last(&mut self, f: &mut Frame) -> Result<bool, String> {
        self.last_out = None;
        let mut got = false;
        while !self.in_flight.is_empty() {
            got = self.finish(f)?;
        }
        Ok(got)
    }

    /// Starts frame `s`.
    fn submit(&mut self, s: &Scene) -> Result<(), String> {
        // (A frame left mapped for `read_into` and not taken: its buffer comes round.)
        self.release_read();
        let (w, h) = (s.width, s.height);
        if (w, h) != self.size {
            // Frames under way at the old size are let go with their buffers: read later,
            // they waited on the new ones, never mapped. The
            // pipelined caller then has no frame ready this once, as at the start.
            self.in_flight.clear();
            (self.out, self.readback) = Gpu::outputs(&self.device, w, h, self.unified);
            self.share = None;
            self.size = (w, h);
        }
        let i = self.next;
        let (ys, cs) = Gpu::strides(w);
        let t0 = std::time::Instant::now();
        // The layers' table: 7 vec4 each (112 bytes).
        let mut table = vec![0u8; LAYERS * 112];
        let mut put = |i: usize, l: &Layer, opacity: f32, mode: f32, offs: [u32; 4]| {
            let p = &l.pic;
            let mut v: Vec<u8> = Vec::with_capacity(112);
            let f4 = |v: &mut Vec<u8>, a: [f32; 4]| {
                a.iter().for_each(|x| v.extend_from_slice(&x.to_le_bytes()))
            };
            let u4 = |v: &mut Vec<u8>, a: [u32; 4]| {
                a.iter().for_each(|x| v.extend_from_slice(&x.to_le_bytes()))
            };
            f4(
                &mut v,
                [
                    l.rect.0 as f32,
                    l.rect.1 as f32,
                    l.rect.2 as f32,
                    l.rect.3 as f32,
                ],
            );
            f4(&mut v, [l.crop.0, l.crop.1, l.crop.2, l.crop.3]);
            u4(
                &mut v,
                [
                    p.width as u32,
                    p.height as u32,
                    Frame::chroma_width(p.width) as u32,
                    p.a.is_some() as u32,
                ],
            );
            u4(&mut v, offs);
            // (No border: the table keeps its place for one.)
            f4(&mut v, [0.0; 4]);
            f4(&mut v, [0.0; 4]);
            // Shown at its own size, whole: read as it is.
            let exact =
                (l.rect.2, l.rect.3) == (p.width, p.height) && l.crop == (0.0, 0.0, 1.0, 1.0);
            f4(
                &mut v,
                [
                    (opacity.clamp(0.0, 1.0) * 256.0).round(),
                    mode,
                    exact as u32 as f32,
                    0.0,
                ],
            );
            table[i * 112..(i + 1) * 112].copy_from_slice(&v);
        };
        let mut scene: Vec<&Arc<Frame>> = vec![&s.program.pic];
        scene.extend(s.transition.as_ref().map(|t| &t.0.pic));
        // The transition's material: a luma wipe's pattern, else a stinger's clip.
        let luma = matches!(&s.transition, Some((_, Kind::Luma { .. }, _)));
        let material = if luma {
            s.pattern.as_ref()
        } else {
            s.stinger.as_ref()
        };
        scene.extend(material.map(|m| &m.pic));
        // (A luma wipe's pattern is levels, not a picture: not lifted for HDR.)
        self.raw_lift = luma.then(|| material.map(|m| m.pic.clone())).flatten();
        let mut offs = self.upload(&scene)?.into_iter();
        put(0, &s.program, 1.0, 0.0, offs.next().expect("program"));
        let (kind, dir, soft, t) = match &s.transition {
            Some((l, kind, t)) => {
                // A zoom's preview: placed where it has grown to (around the frame's
                // centre), the shader showing it there over the program.
                let zoomed;
                let shown = if *kind == Kind::Zoom {
                    let (zx, zy, zw, zh) = crate::compose::zoom_rect(l.rect.2, l.rect.3, *t);
                    zoomed = Layer {
                        pic: l.pic.clone(),
                        rect: (l.rect.0 + zx, l.rect.1 + zy, zw.max(2), zh.max(1)),
                        crop: l.crop,
                    };
                    &zoomed
                } else {
                    l
                };
                put(1, shown, 1.0, 0.0, offs.next().expect("preview"));
                match kind {
                    Kind::Cut | Kind::Stinger(_) => (0.0, 0.0, 0.0, 0.0),
                    Kind::Mix => (1.0, 0.0, 0.0, *t),
                    // Without its pattern: a mix, as on the CPU.
                    Kind::Luma { soft, invert, .. } if s.pattern.is_some() => {
                        (4.0, *invert as u32 as f32, soft.clamp(0.0, 1.0), *t)
                    }
                    Kind::Luma { .. } => (1.0, 0.0, 0.0, *t),
                    Kind::Fade => (3.0, 0.0, 0.0, *t),
                    // (Nothing grown yet: the program alone.)
                    Kind::Zoom if crate::compose::zoom_rect(l.rect.2, l.rect.3, *t).2 < 2 => {
                        (0.0, 0.0, 0.0, 0.0)
                    }
                    Kind::Zoom => (5.0, 0.0, 0.0, *t),
                    Kind::Wipe(d, soft) => {
                        let d = match d {
                            Direction::Left => 0.0,
                            Direction::Right => 1.0,
                            Direction::Down => 2.0,
                            Direction::Up => 3.0,
                            Direction::Circle => 4.0,
                        };
                        (2.0, d, *soft, *t)
                    }
                }
            }
            None => (0.0, 0.0, 0.0, 0.0),
        };
        let stinger_on = match material {
            Some(m) => {
                put(LAYERS - 1, m, 1.0, 1.0, offs.next().expect("material"));
                !luma
            }
            None => false,
        };
        let mut globals = Vec::with_capacity(64);
        let cw = Frame::chroma_width(w);
        for x in [w as u32, h as u32, cw as u32, 0] {
            globals.extend_from_slice(&x.to_le_bytes());
        }
        for x in [
            ys as u32,
            cs as u32,
            (ys * h) as u32,
            (ys * h + cs * h) as u32,
        ] {
            globals.extend_from_slice(&x.to_le_bytes());
        }
        for x in [
            kind,
            dir,
            soft,
            t.clamp(0.0, 1.0),
            s.ftb,
            s.transition.is_some() as u32 as f32,
            stinger_on as u32 as f32,
            0.0,
        ] {
            globals.extend_from_slice(&x.to_le_bytes());
        }
        self.queue.write_buffer(&self.globals, 0, &globals);
        self.queue.write_buffer(&self.layers, 0, &table);
        let (out, pics) = if self.unified {
            (&self.readback[i].buffer, &self.staging[i].buffer)
        } else {
            (&self.out, &self.pics)
        };
        let entries = [
            wgpu::BindGroupEntry {
                binding: 0,
                resource: self.globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: self.layers.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: out.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: pics.as_entire_binding(),
            },
        ];
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("compose"),
            layout: &self.layout,
            entries: &entries,
        });
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("compose"),
            });
        // The new pictures up (the staging buffer is mapped again for its next turn).
        let uploaded = !self.copies.is_empty() || self.unified;
        if self.unified {
            // The shader reads it: unmapped, written or not (and mapped by now: the frame
            // two before, which used it, is done).
            wait_mapped(&self.device, &self.staging[i])?;
            self.staging[i].buffer.unmap();
        } else if uploaded {
            self.staging[i].buffer.unmap();
            for &(at, size) in &self.copies {
                enc.copy_buffer_to_buffer(&self.staging[i].buffer, at, &self.pics, at, size);
            }
        }
        // Pictures that came as coefficients: reconstructed into their regions first.
        if !self.coefs.is_empty() {
            // Packed into a mapped staging buffer and copied up ahead of this frame's
            // work (an earlier submission on the queue).
            let hand = self.hand_in();
            let (coef_buf, jobs) = self.coefs.upload(&hand).map_err(|e| {
                // Too large for its buffers: that frame alone (bug run L4-4).
                if e.kind() == std::io::ErrorKind::FileTooLarge {
                    format!("{}{e}", crate::TOO_LARGE)
                } else {
                    format!("GPU: {e}")
                }
            })?;
            self.recon_jobs = jobs;
            for &(at, size) in &self.clears {
                enc.clear_buffer(pics, at, Some(size));
            }
            self.recon
                .record(&self.device, &mut enc, coef_buf, pics, &self.recon_jobs);
        }
        // Pictures kept on the GPU: copied into their regions there.
        for (src, jobs) in &self.gpu_jobs {
            self.plane_copy
                .encode(&self.device, &mut enc, src, pics, jobs)
                .map_err(|e| e.to_string())?;
        }
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("compose"),
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
        // HDR (R.24): this frame's new pictures lifted into the HDR space, then the same
        // composition there, as P216, to its own readback.
        let hdr_frame = if let Some(mut hp) = self.hdr.take_if(|h| h.fits) {
            hp.ensure_out(&self.device, w, h, self.unified);
            self.queue.write_buffer(&hp.layers, 0, &hp.table(&table));
            let set = if self.unified { i } else { 0 };
            let pics16 = hp.pics[set].clone();
            hp.write_direct(&self.queue, &pics16);
            hp.record_lifts(&self.device, &mut enc, pics, &pics16, self.region);
            let out16 = if self.unified {
                hp.readback[i].buffer.clone()
            } else {
                hp.out.clone()
            };
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("compose hdr"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.globals.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: hp.layers.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: out16.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: pics16.as_entire_binding(),
                    },
                ],
            });
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("compose hdr"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&hp.compose);
                pass.set_bind_group(0, &bind, &[]);
                pass.dispatch_workgroups(
                    (cs as u32).div_ceil(WORKGROUP),
                    (h as u32).div_ceil(WORKGROUP),
                    1,
                );
            }
            if !self.unified {
                enc.copy_buffer_to_buffer(&hp.out, 0, &hp.readback[i].buffer, 0, hp.out.size());
            }
            self.hdr = Some(hp);
            true
        } else {
            false
        };
        let read = !self.gpu_only;
        let ring = read && self.ring_only && !(self.unified && self.share_program);
        // The readback buffer was read out and unmapped before its turn came again.
        if !self.unified && read {
            enc.copy_buffer_to_buffer(&self.out, 0, &self.readback[i].buffer, 0, self.out.size());
        }
        let shared = !self.unified && self.share_program;
        if shared {
            let size = self.out.size();
            let share = self.share.get_or_insert_with(|| {
                let b = || {
                    self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("program shared"),
                        size,
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                };
                [b(), b()]
            });
            enc.copy_buffer_to_buffer(&self.out, 0, &share[i], 0, size);
        } else if !self.share_program {
            self.share = None;
        }
        let submission = self.queue.submit([enc.finish()]);
        if uploaded {
            self.staging[i].map(wgpu::MapMode::Write);
        }
        if hdr_frame && let Some(hp) = self.hdr.as_mut() {
            hp.readback[i].map(wgpu::MapMode::Read);
            hp.pending.push_back(i);
        }
        if read {
            self.readback[i].map(wgpu::MapMode::Read);
        }
        // (The frame two before this one, which used these buffers, has been read out.)
        self.in_flight.push_back(Pending {
            index: i,
            submission,
            size: (w, h),
            shared,
            read,
            ring,
        });
        self.next = 1 - i;
        self.timing.0 = t0.elapsed().as_secs_f64() * 1000.0;
        Ok(())
    }

    /// Reads the oldest frame in flight into `f` (waiting for it); false if there is none.
    fn finish(&mut self, f: &mut Frame) -> Result<bool, String> {
        let Some(p) = self.in_flight.pop_front() else {
            return Ok(false);
        };
        let t1 = std::time::Instant::now();
        // Its HDR program, when it had one: read out as P216 (`take_hdr`).
        if let Some(hp) = self.hdr.as_mut()
            && hp.pending.front() == Some(&p.index)
        {
            hp.pending.pop_front();
            let rb = &hp.readback[p.index];
            rb.wait(&self.device, Some(p.submission.clone()))?;
            let n = p.size.0 * p.size.1 * 2;
            let mut v = hp.last.take().unwrap_or_default();
            v.clear();
            v.reserve(n);
            {
                let data = rb
                    .buffer
                    .slice(..)
                    .get_mapped_range()
                    .map_err(|e| format!("GPU read back: {e}"))?;
                // One bulk copy into it as it is (the GPU's memory read in wide loads:
                // sample by sample it took 27 ms a 2160p program; zeroed first, 33 MB more
                // written). P216 is little-endian, as the CPU is.
                let k = (n * 2).min(data.len()) & !1;
                // SAFETY: `v` has room for n samples (2n bytes); k bytes written, k/2
                // samples set.
                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), v.as_mut_ptr().cast::<u8>(), k);
                    v.set_len(k / 2);
                }
                v.resize(n, 0);
            }
            rb.buffer.unmap();
            rb.ready.store(false, Ordering::Release);
            hp.last = Some(v);
        }
        if !p.read {
            // On the GPU only: done there, its sizes here and no samples.
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(p.submission.clone()),
                    timeout: Some(std::time::Duration::from_secs(2)),
                })
                .map_err(|e| format!("GPU: {e}"))?;
            (f.width, f.height, f.a) = (p.size.0, p.size.1, None);
            f.y.clear();
            f.cb.clear();
            f.cr.clear();
            self.last_out = (self.unified || p.shared).then_some(p.index);
            let t2 = std::time::Instant::now();
            self.timing.1 = (t2 - t1).as_secs_f64() * 1000.0;
            self.timing.2 = 0.0;
            return Ok(true);
        }
        self.release_read();
        let rb = &self.readback[p.index];
        rb.wait(&self.device, Some(p.submission.clone()))?;
        let t2 = std::time::Instant::now();
        let (w, h) = p.size;
        if p.ring {
            // Left mapped for `read_into`: its sizes here and no samples.
            (f.width, f.height, f.a) = (w, h, None);
            f.y.clear();
            f.cb.clear();
            f.cr.clear();
            self.mapped.set(Some(p.index));
            self.last_out = (self.unified || p.shared).then_some(p.index);
            self.timing.1 = (t2 - t1).as_secs_f64() * 1000.0;
            self.timing.2 = 0.0;
            return Ok(true);
        }
        let (ys, cs) = Gpu::strides(w);
        let cw = Frame::chroma_width(w);
        (f.width, f.height, f.a) = (w, h, None);
        // Sized, not cleared: at the same size nothing is filled before the copy.
        f.y.resize(w * h, 0);
        f.cb.resize(cw * h, 0);
        f.cr.resize(cw * h, 0);
        {
            let data = rb
                .buffer
                .slice(..)
                .get_mapped_range()
                .map_err(|e| format!("GPU read back: {e}"))?;
            let (yb, cb) = (ys * 4, cs * 4);
            // Mapped GPU memory: read with streaming loads (plain copies of it were a
            // quarter of a 2160p60 switcher's CPU).
            use idn_open_protocol::mem::copy_uncached;
            // Rows are padded to whole invocations; read in one pass where they aren't.
            if yb == w {
                copy_uncached(&mut f.y, &data[..w * h]);
                copy_uncached(&mut f.cb, &data[ys * h * 4..ys * h * 4 + cw * h]);
                let c1 = (ys + cs) * h * 4;
                copy_uncached(&mut f.cr, &data[c1..c1 + cw * h]);
            } else {
                for r in 0..h {
                    copy_uncached(&mut f.y[r * w..(r + 1) * w], &data[r * yb..r * yb + w]);
                }
                for r in 0..h {
                    let c0 = ys * h * 4 + r * cb;
                    copy_uncached(&mut f.cb[r * cw..(r + 1) * cw], &data[c0..c0 + cw]);
                }
                for r in 0..h {
                    let c1 = (ys + cs) * h * 4 + r * cb;
                    copy_uncached(&mut f.cr[r * cw..(r + 1) * cw], &data[c1..c1 + cw]);
                }
            }
        }
        rb.buffer.unmap();
        rb.ready.store(false, Ordering::Release);
        self.last_out = (self.unified || p.shared).then_some(p.index);
        let ms = |a: std::time::Instant, b: std::time::Instant| (b - a).as_secs_f64() * 1000.0;
        self.timing.1 = ms(t1, t2);
        self.timing.2 = ms(t2, std::time::Instant::now());
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
