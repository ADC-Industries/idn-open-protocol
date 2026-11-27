//! Pictures reconstructed on the GPU and read back: the samples
//! `idn_open_protocol::shq::reconstruct` gives (bit for bit in f64; in f32 a sample one
//! step off now and then), for receivers whose consumer takes pictures in memory (OBS).
//! The inverse DCT was half a 2160p picture's decode on the CPU.

use crate::idct::{PlaneJob, Precision, Recon, pack_into};
use idn_common::sync::LockOk as _;
use idn_open_protocol::gpu::block_on;
use idn_open_protocol::shq::{CoefPicture, Format, Picture, Plane};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How long a picture's reconstruction may take before the GPU counts as stuck.
const WAIT: Duration = Duration::from_secs(2);

/// A GPU device, shared by every [`Reconstructor`] of the process.
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// The adapter, for the log: `Radeon 680M (Vulkan)`.
    pub name: String,
    /// The reconstruction's arithmetic on this device (f64 where it has it, unless the
    /// environment asks for f32).
    pub precision: Precision,
    /// The device has f64 shaders.
    pub has_f64: bool,
    /// The first error the device reported (a lost device, an out-of-memory): wgpu's
    /// default handler panics.
    errors: Arc<Mutex<Option<String>>>,
}

impl Gpu {
    /// The process's GPU, opened on first use; `None` without a usable one (none, or only
    /// a software one). `env` names the variable that asks for f32 (`IDN_OBS_IDCT=f32`).
    pub fn shared(env: &str) -> Option<Arc<Gpu>> {
        static GPU: OnceLock<io::Result<Arc<Gpu>>> = OnceLock::new();
        GPU.get_or_init(|| Gpu::open(env).map(Arc::new))
            .as_ref()
            .ok()
            .cloned()
    }

    /// Why there is no shared GPU, if there isn't.
    pub fn open(env: &str) -> io::Result<Gpu> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| io::Error::other(format!("no GPU: {e}")))?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            return Err(io::Error::other(format!(
                "only a software GPU ({})",
                info.name
            )));
        }
        let name = format!("{} ({:?})", info.name, info.backend);
        let features = adapter.features() & wgpu::Features::SHADER_F64;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("idn-shq-gpu"),
            required_features: features,
            // The adapter's own limits (the defaults' 128 MiB a binding is less than an 8K
            // picture needs).
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| io::Error::other(format!("{name}: {e}")))?;
        let errors = Arc::new(Mutex::new(None::<String>));
        {
            let errors = errors.clone();
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                *errors.lock_ok() = Some(e.to_string());
            }));
        }
        let precision = Precision::for_features(device.features(), env);
        let has_f64 = device.features().contains(wgpu::Features::SHADER_F64);
        Ok(Gpu {
            device,
            queue,
            name,
            precision,
            has_f64,
            errors,
        })
    }

    /// Reconstruct on a wgpu device the host already owns, instead of opening one with
    /// [`shared`](Gpu::shared). Use this to keep a single wgpu device in the process — some
    /// Vulkan loaders race when a process stands up more than one. The host passes a clone
    /// of its `Device` and `Queue` (both are cheap, reference-counted handles). `env` names
    /// the variable that forces f32 arithmetic, as for [`open`](Gpu::open); it still falls
    /// back to f32 on a device without f64 shaders.
    ///
    /// An adopted device keeps the host's own uncaptured-error handler (an opened one gets
    /// ours): a wgpu error reaches whatever the host installed, while
    /// [`Reconstructor::reconstruct`] still returns `Err` when a submission or read-back
    /// fails.
    pub fn adopt(device: wgpu::Device, queue: wgpu::Queue, env: &str) -> Gpu {
        let precision = Precision::for_features(device.features(), env);
        let has_f64 = device.features().contains(wgpu::Features::SHADER_F64);
        Gpu {
            device,
            queue,
            name: "host-provided device".into(),
            precision,
            has_f64,
            // Not populated: the host owns the device and its uncaptured-error handler.
            errors: Arc::new(Mutex::new(None)),
        }
    }

    /// An error the device reported since the last look: taken, so one picture's error
    /// isn't every later picture's, in every source, for good (E1-1).
    fn check(&self) -> io::Result<()> {
        match self.errors.lock_ok().take() {
            Some(e) => Err(io::Error::other(format!("GPU: {e}"))),
            None => Ok(()),
        }
    }
}

/// One receiver's reconstruction: its pipeline and buffers on the shared [`Gpu`], reused
/// picture after picture (grown when a larger one comes).
pub struct Reconstructor {
    gpu: Arc<Gpu>,
    recon: Recon,
    /// (coefficient words, samples, the mappable copy of the samples), with their sizes.
    buffers: Option<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    words: Vec<u32>,
    jobs: Vec<PlaneJob>,
    /// Plane buffers handed back ([`Reconstructor::recycle`]), filled again: a fresh 16 MB
    /// a 2160p picture cost its page faults every time.
    spare: Vec<Vec<u8>>,
    /// Pictures kept on the GPU ([`Reconstructor::reconstruct_gpu`]), taken back once
    /// nothing holds them; and the coefficient words' buffer for them.
    kept: idn_open_protocol::gpu::Buffers,
    kept_up: crate::idct::Upload,
}

impl Reconstructor {
    pub fn new(gpu: Arc<Gpu>) -> Self {
        let precision = gpu.precision;
        Reconstructor::with_precision(gpu, precision)
    }

    /// With the given arithmetic (f64 only where the device has it: else f32).
    pub fn with_precision(gpu: Arc<Gpu>, precision: Precision) -> Self {
        let precision = if gpu.has_f64 {
            precision
        } else {
            Precision::F32
        };
        let recon = Recon::new(&gpu.device, precision);
        Reconstructor {
            gpu,
            recon,
            buffers: None,
            words: Vec::new(),
            jobs: Vec::new(),
            spare: Vec::new(),
            kept: idn_open_protocol::gpu::Buffers::new(
                "reconstructed (kept)",
                wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
                4,
            ),
            kept_up: Default::default(),
        }
    }

    /// The picture's samples made and left on the GPU, laid out as a `gpu/1` slot is
    /// (rows on 256 bytes; 4:2:0 chroma as it is), for a receiver that draws there (the
    /// monitor, §E.27): nothing comes back to memory. The work is submitted, not waited
    /// for. An error means the GPU can't be relied on (reconstruct on the CPU).
    pub fn reconstruct_gpu(
        &mut self,
        c: &Arc<CoefPicture>,
    ) -> io::Result<idn_open_protocol::gpu::GpuPicture> {
        self.gpu.check()?;
        let sizes: Vec<(u32, u32)> = c
            .planes
            .iter()
            .take(4)
            .map(|p| (p.width as u32, p.height as u32))
            .collect();
        if sizes.is_empty() {
            return Err(io::Error::other("an empty picture"));
        }
        let (planes, bytes) = idn_open_protocol::gpu::slot_layout(&sizes);
        // The same limit as the read-back's (lab 524(c): this path, the switcher's, had
        // none, so the test limit never took and an over-large picture went to the GPU):
        // the samples' buffer, and the coefficients' (about as large) under it.
        let most = most_bytes(&self.gpu.device);
        if bytes > most {
            // Its own kind: that picture only, not the GPU failing (bug run I5-6).
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "a picture too large for this GPU's buffers ({} MB)",
                    bytes >> 20
                ),
            ));
        }
        let mut dst = [0u32; 4];
        for (d, p) in dst.iter_mut().zip(&planes) {
            *d = p.offset as u32;
        }
        // Packed straight into a mapped staging buffer (no zeroed copy first).
        let hand = idn_open_protocol::gpu::GpuHandIn {
            device: self.gpu.device.clone(),
            queue: self.gpu.queue.clone(),
        };
        self.kept_up.clear();
        self.kept_up.add(c.clone(), dst, None);
        let (words, mut jobs) = self.kept_up.upload(&hand)?;
        for (j, p) in jobs.iter_mut().zip(&planes) {
            j.stride = p.row_bytes;
        }
        let device = &self.gpu.device;
        let out = self.kept.get(device, bytes);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("reconstruct (kept)"),
        });
        // Samples are ORed in, four to a word: from zero.
        enc.clear_buffer(&out, 0, None);
        self.recon.record(device, &mut enc, words, &out, &jobs);
        self.gpu.queue.submit([enc.finish()]);
        Ok(idn_open_protocol::gpu::GpuPicture {
            buffer: out,
            planes,
        })
    }

    /// A picture's planes back, once its samples are used, for the next one's.
    pub fn recycle(&mut self, p: Picture) {
        self.spare.extend(p.planes.into_iter().map(|pl| pl.data));
        self.spare.truncate(8);
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// The arithmetic this reconstructor uses.
    pub fn precision(&self) -> Precision {
        self.recon.precision
    }

    /// The picture's samples, as `shq::reconstruct` gives them. An error means the GPU
    /// can't be relied on: the caller reconstructs on the CPU from then on.
    pub fn reconstruct(&mut self, c: &CoefPicture) -> io::Result<Picture> {
        // Planes one after another in the sample buffer, each from a 4-byte boundary (the
        // shader writes four samples a word).
        let mut dst = [0u32; 4];
        let mut at = 0u64;
        for (pl, p) in c.planes.iter().enumerate().take(4) {
            dst[pl] = at as u32;
            at += ((p.width * p.height) as u64).div_ceil(4) * 4;
        }
        if at == 0 {
            return Err(io::Error::other("an empty picture"));
        }
        self.words.clear();
        self.jobs = pack_into(c, &mut self.words, dst);
        let (device, queue) = (&self.gpu.device, &self.gpu.queue);
        let word_bytes = (self.words.len() as u64 * 4).max(4);
        // Over what one binding may be: this picture on the CPU, the GPU untouched (an 8K
        // picture's buffers went over the default 128 MiB, and the error that made turned
        // the GPU off for every source; bug round E, E1-1).
        let most = most_bytes(device);
        let slack = |n: u64| n * 5 / 4 / 4 * 4 + 4;
        if word_bytes > most || at > most {
            // Its own kind: that picture only, not the GPU failing (bug run I5-6).
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "a picture too large for this GPU's buffers ({} MB)",
                    word_bytes.max(at) >> 20
                ),
            ));
        }
        let fits = self
            .buffers
            .as_ref()
            .is_some_and(|(w, s, _)| w.size() >= word_bytes && s.size() >= at);
        if !fits {
            use wgpu::BufferUsages as U;
            let mk = |label, size: u64, usage| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    // (Room to grow, within the limit.)
                    size: slack(size).min(most / 4 * 4),
                    usage,
                    mapped_at_creation: false,
                })
            };
            self.buffers = Some((
                mk("coefficients", word_bytes, U::STORAGE | U::COPY_DST),
                mk("samples", at, U::STORAGE | U::COPY_SRC | U::COPY_DST),
                mk("read back", at, U::MAP_READ | U::COPY_DST),
            ));
        }
        let (words, samples, back) = self.buffers.as_ref().expect("made above");
        // The words as the GPU reads them (little-endian): no copy where the machine is.
        // (wgpu's staging view is write-only: packing into it straight isn't offered.)
        #[cfg(target_endian = "little")]
        queue.write_buffer(words, 0, bytemuck::cast_slice(&self.words));
        #[cfg(not(target_endian = "little"))]
        queue.write_buffer(
            words,
            0,
            &self
                .words
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("reconstruct"),
        });
        // Samples are ORed in, four to a word: from zero.
        enc.clear_buffer(samples, 0, Some(at));
        self.recon
            .record(device, &mut enc, words, samples, &self.jobs);
        enc.copy_buffer_to_buffer(samples, 0, back, 0, at);
        let submission = queue.submit([enc.finish()]);
        let (done, failed) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        {
            let (done, failed) = (done.clone(), failed.clone());
            back.slice(..at)
                .map_async(wgpu::MapMode::Read, move |r| match r {
                    Ok(()) => done.store(true, Ordering::Release),
                    Err(_) => failed.store(true, Ordering::Release),
                });
        }
        while !done.load(Ordering::Acquire) {
            if failed.load(Ordering::Acquire) {
                return Err(io::Error::other("GPU: the samples could not be read back"));
            }
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission.clone()),
                    timeout: Some(WAIT),
                })
                .map_err(|e| io::Error::other(format!("GPU: {e}")))?;
        }
        self.gpu.check()?;
        let planes = {
            let data = back
                .slice(..at)
                .get_mapped_range()
                .map_err(|e| io::Error::other(format!("GPU: {e}")))?;
            let spare = &mut self.spare;
            c.planes
                .iter()
                .zip(dst)
                .map(|(p, d)| {
                    let src = &data[d as usize..d as usize + p.width * p.height];
                    // Mapped GPU memory the CPU doesn't cache (an APU's): streaming loads,
                    // as the switcher reads its program back; a plain copy was most of a
                    // 2160p picture's time. (A recycled buffer is the size already.)
                    let mut v = spare.pop().unwrap_or_default();
                    v.resize(src.len(), 0);
                    idn_open_protocol::mem::copy_uncached(&mut v, src);
                    Plane {
                        width: p.width,
                        height: p.height,
                        data: v,
                    }
                })
                .collect()
        };
        back.unmap();
        Ok(Picture {
            fourcc: c.fourcc,
            width: c.width,
            height: c.height,
            format: Format::New,
            quality: 0,
            short_dc: false,
            high_precision: false,
            planes,
            precise: None,
            coefficients: None,
        })
    }
}

/// The most bytes one buffer may hold on `device` (its limits), or less with
/// `IDN_SHQ_GPU_MOST_MB` (for trying the per-picture fallback on a GPU that takes 8K,
/// lab 488(h)).
fn most_bytes(device: &wgpu::Device) -> u64 {
    let limits = device.limits();
    let most = limits
        .max_storage_buffer_binding_size
        .min(limits.max_buffer_size);
    match idn_common::switches::value("IDN_SHQ_GPU_MOST_MB")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(mb) => most.min(mb << 20),
        None => most,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
    use idn_open_protocol::shq::{Fourcc, Limits, VideoParams, decode_coefficients, reconstruct};

    /// A grainy camera-like picture, coded: its coefficients.
    fn coefficients(w: usize, h: usize, fourcc: Fourcc) -> CoefPicture {
        let mut seed = 0x2545_f491u32;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % 21) as i32 - 10
        };
        let mut plane = |pw: usize, ph: usize, base: i32| Plane {
            width: pw,
            height: ph,
            data: (0..pw * ph)
                .map(|i| {
                    let (x, y) = (i % pw, i / pw);
                    (base
                        + ((x / 37 + y / 23) % 6) as i32 * 20
                        + ((x as f32 / 9.0).sin() * 30.0) as i32
                        + noise())
                    .clamp(16, 235) as u8
                })
                .collect(),
        };
        // NDI 6.3 sends 4:2:0 above 1080p (spec §3.1).
        let ch = if fourcc == Fourcc::Shq0 {
            h.div_ceil(2)
        } else {
            h
        };
        let mut planes = vec![
            plane(w, h, 60),
            plane(w.div_ceil(2), ch, 100),
            plane(w.div_ceil(2), ch, 120),
        ];
        if fourcc == Fourcc::Shq7 {
            planes.push(plane(w, h, 128));
        }
        let payload = encode(
            &EncodeInput {
                fourcc,
                width: w,
                height: h,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions {
                quality: 92,
                ..Default::default()
            },
        );
        let vp = VideoParams {
            fourcc: match fourcc {
                Fourcc::Shq0 => *b"SHQ0",
                Fourcc::Shq7 => *b"SHQ7",
                _ => *b"SHQ2",
            },
            width: w as u32,
            height: h as u32,
            field_id: 0,
        };
        decode_coefficients(&vp, &payload, &Limits::default()).unwrap()
    }

    /// The samples read back equal the CPU's: in f64 exactly, in f32 at most a step off
    /// and rarely; odd sizes, 4:2:0 and alpha included. Skipped (saying so) without a GPU.
    /// A device handed in with `adopt` reconstructs the same samples as one `shared` opens
    /// — so a host can give the crate its own single device. Skipped without a GPU.
    #[test]
    fn an_adopted_device_matches_an_opened_one() {
        let Some(owned) = Gpu::shared("IDN_TEST_IDCT") else {
            return;
        };
        let adopted = Arc::new(Gpu::adopt(
            owned.device.clone(),
            owned.queue.clone(),
            "IDN_TEST_IDCT",
        ));
        assert_eq!(adopted.has_f64, owned.has_f64);
        assert_eq!(adopted.precision, owned.precision);
        let c = coefficients(96, 64, Fourcc::Shq2);
        let a = Reconstructor::new(owned).reconstruct(&c).unwrap();
        let b = Reconstructor::new(adopted).reconstruct(&c).unwrap();
        assert_eq!(a.planes.len(), b.planes.len());
        for (pa, pb) in a.planes.iter().zip(&b.planes) {
            assert_eq!(pa.data, pb.data, "adopted and opened devices must agree");
        }
    }

    /// Kept on the GPU in a slot's layout (rows on 256 bytes): the same samples as read
    /// back, picture after picture through the kept buffers.
    #[test]
    fn kept_on_the_gpu_as_read_back() {
        let Some(gpu) = Gpu::shared("IDN_TEST_IDCT") else {
            eprintln!("no GPU: test skipped");
            return;
        };
        let hand = idn_open_protocol::gpu::GpuHandIn {
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
        };
        let mut r = Reconstructor::new(gpu.clone());
        for (w, h, fourcc) in [
            (1920, 1080, Fourcc::Shq2),
            (723, 301, Fourcc::Shq7),
            (722, 302, Fourcc::Shq0),
            (1920, 1080, Fourcc::Shq2),
            (1920, 1080, Fourcc::Shq2),
        ] {
            let c = coefficients(w, h, fourcc);
            let want = r.reconstruct(&c).unwrap();
            let kept = r.reconstruct_gpu(&Arc::new(c.clone())).unwrap();
            assert!(kept.planes[0].row_bytes.is_multiple_of(256));
            let got = kept.read_planes(&hand).unwrap();
            assert_eq!(got.len(), want.planes.len());
            for (k, (g, p)) in got.iter().zip(&want.planes).enumerate() {
                assert_eq!(g, &p.data, "{w}x{h} {fourcc:?} plane {k}");
            }
        }
    }

    #[test]
    fn read_back_samples_are_the_cpus() {
        let Some(gpu) = Gpu::shared("IDN_TEST_IDCT") else {
            eprintln!("no GPU: test skipped");
            return;
        };
        let mut r = Reconstructor::new(gpu.clone());
        for (w, h, fourcc) in [
            (1920, 1080, Fourcc::Shq2),
            (723, 301, Fourcc::Shq7),
            (64, 48, Fourcc::Shq2),
            (2560, 1440, Fourcc::Shq0),
            (722, 302, Fourcc::Shq0),
        ] {
            let c = coefficients(w, h, fourcc);
            let cpu = reconstruct(&c);
            let got = r.reconstruct(&c).unwrap();
            assert_eq!(got.planes.len(), cpu.planes.len());
            let (mut off, mut worst, mut total) = (0usize, 0u8, 0usize);
            for (g, c) in got.planes.iter().zip(&cpu.planes) {
                assert_eq!((g.width, g.height), (c.width, c.height));
                for (a, b) in g.data.iter().zip(&c.data) {
                    worst = worst.max(a.abs_diff(*b));
                    off += usize::from(a != b);
                }
                total += c.data.len();
            }
            eprintln!(
                "{w}x{h} {fourcc:?} {:?} on {}: {off} of {total} samples differ, at most by {worst}",
                gpu.precision, gpu.name
            );
            match gpu.precision {
                Precision::F64 => assert_eq!(off, 0),
                Precision::F32 => assert!(worst <= 1 && off * 1000 < total),
            }
        }
    }

    /// A 2160p picture: the GPU (upload, reconstruction, read back) against the CPU
    /// (cargo test --release -p idn-shq-gpu -- --ignored --nocapture).
    #[test]
    #[ignore = "timing"]
    fn per_2160p_picture() {
        let Some(gpu) = Gpu::shared("IDN_TEST_IDCT") else {
            return;
        };
        let c = coefficients(3840, 2160, Fourcc::Shq2);
        let mut r = Reconstructor::new(gpu.clone());
        r.reconstruct(&c).unwrap();
        let t = std::time::Instant::now();
        for _ in 0..20 {
            std::hint::black_box(r.reconstruct(&c).unwrap());
        }
        let g = t.elapsed().as_secs_f64() * 50.0;
        let t = std::time::Instant::now();
        for _ in 0..5 {
            std::hint::black_box(reconstruct(&c));
        }
        let cpu = t.elapsed().as_secs_f64() * 200.0;
        println!(
            "2160p: GPU {:?} ({}) {g:.2} ms a picture, read back included; CPU one thread {cpu:.2} ms",
            gpu.precision, gpu.name
        );
    }
}
