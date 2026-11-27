//! Which encoders work on this machine and which one an output uses: the probe (each
//! candidate opened and fed a picture, in the preferred order; the rest kept as
//! alternatives), the device each opens on, an output's encoders as the processor and
//! format say (§N.13), and the CPU's H.264 for links and browsers.

use super::*;
use crate::encode::{open_encoder, test_config, test_picture};

impl Ffmpeg {
    /// Finds what this machine encodes in hardware and decodes, on [`DEFAULT_DEVICE`]
    /// (or `IDN_VAAPI_DEVICE`) and NVENC. Takes a fraction of a second: each candidate
    /// encoder is opened and fed a picture.
    pub fn probe() -> Self {
        let path = idn_common::switches::value("IDN_VAAPI_DEVICE")
            .unwrap_or_else(|| DEFAULT_DEVICE.into());
        Self::probe_device(Some(&path))
    }

    /// As [`probe`](Self::probe), with this VA-API render node (`None`: NVENC, AMF and
    /// Quick Sync only).
    pub fn probe_device(vaapi: Option<&str>) -> Self {
        ffi::quiet();
        let device = vaapi.and_then(|p| ffi::vaapi_device(p).ok()).map(Arc::new);
        let vaapi_path = device.as_ref().and(vaapi).map(str::to_owned);
        // FFmpeg's Vulkan and CUDA devices (Linux): Vulkan Video, and NVENC fed CUDA
        // frames. The Vulkan device on the GPU wgpu would choose (by its name).
        #[cfg(target_os = "linux")]
        let gpu = (!idn_common::switches::off("IDN_VULKAN_ENCODE")).then(gpu_name);
        #[cfg(target_os = "linux")]
        let (vulkan, cuda) = (
            gpu.as_ref()
                .and_then(|g| ffi::vulkan_device(g.as_deref()).ok().map(Arc::new)),
            ffi::cuda_device().ok().map(Arc::new),
        );
        #[cfg(not(target_os = "linux"))]
        let (vulkan, cuda): (Option<Arc<ffi::BufferRef>>, Option<Arc<ffi::BufferRef>>) =
            (None, None);
        let dev_for = |b: Backend| match b {
            Backend::Vaapi => device.clone(),
            Backend::Vulkan => vulkan.clone(),
            Backend::NvencCuda => cuda.clone(),
            _ => None,
        };
        // Each codec's GPU encoders, the preferred first: those that take pictures on the
        // GPU as they are (VA-API, NVENC through CUDA), then Vulkan Video, then the others;
        // `IDN_GPU_ENCODER` (vaapi, nvenc, nvenc-cuda, vulkan, amf, qsv) first of all.
        let mut order = vec![
            Backend::Vaapi,
            Backend::NvencCuda,
            Backend::Nvenc,
            Backend::Vulkan,
            Backend::Amf,
            Backend::Qsv,
        ];
        if let Some(first) =
            idn_common::switches::value("IDN_GPU_ENCODER").and_then(|v| backend_named(&v))
        {
            order.retain(|b| *b != first);
            order.insert(0, first);
        }
        let mut encoders = Vec::new();
        let mut gpu_alternatives = Vec::new();
        for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
            for &b in &order {
                let dev = dev_for(b);
                if matches!(b, Backend::Vaapi | Backend::Vulkan | Backend::NvencCuda)
                    && dev.is_none()
                {
                    continue;
                }
                let works = open_encoder(&dev, b, &test_config(c))
                    .and_then(|mut e| e.encode(&test_picture(), Fourcc::Shq2, 0, true))
                    .is_ok_and(|out| !out.is_empty());
                if works {
                    if encoders.iter().any(|e: &(Codec, Backend)| e.0 == c) {
                        gpu_alternatives.push((c, b));
                    } else {
                        encoders.push((c, b));
                    }
                }
            }
        }
        let decoders = [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .filter(|&c| ffi::Decoder::open(codec_id(c), None).is_ok())
            .collect();
        // NVDEC on the encoders' CUDA device where there is one: a second CUDA context
        // costs every program its own share of the GPU's memory.
        let hw_decode = device
            .as_deref()
            .map(ffi::HwDecode::vaapi)
            .or_else(|| cuda.as_deref().map(ffi::HwDecode::cuda_on))
            .or_else(|| ffi::HwDecode::cuda().ok())
            .or_else(|| ffi::HwDecode::d3d11va().ok())
            .map(Arc::new);
        // The CPU's encoders: H.264 for links and browser viewing without a GPU one, and
        // each codec for outputs set to the CPU.
        // (Up to four pictures: an encoder with a frame in flight gives the first later.)
        // Kvazaar and SVT-AV1 print their banners straight to stderr (not through
        // FFmpeg's log), dozens of lines at every program's start: not while probing.
        let works = |b: Backend, c: Codec| {
            stderr_quiet(|| {
                open_encoder(&None, b, &test_config(c)).is_ok_and(|mut e| {
                    (0..4).any(|i| {
                        e.encode(&test_picture(), Fourcc::Shq2, i, i == 0)
                            .is_ok_and(|out| !out.is_empty())
                    })
                })
            })
        };
        let cpu = [Backend::OpenH264, Backend::X264]
            .into_iter()
            .find(|&b| works(b, Codec::H264));
        let mut cpu_encoders: Vec<(Codec, Backend)> =
            cpu.map(|b| (Codec::H264, b)).into_iter().collect();
        for (c, b) in [
            (Codec::Hevc, Backend::Kvazaar),
            (Codec::Av1, Backend::SvtAv1),
        ] {
            if works(b, c) {
                cpu_encoders.push((c, b));
            }
        }
        // The probe's Vulkan and CUDA devices kept only for an encoder chosen on them; for
        // an alternative only, made again if it is used; else none. Each held memory,
        // threads and a share of the GPU for the program's life (a CUDA context: hundreds
        // of MB of the card's memory). NVDEC keeps its own reference to the CUDA device.
        #[cfg(target_os = "linux")]
        let (vulkan, cuda) = {
            (
                LazyDevice::after_probe(
                    vulkan,
                    Backend::Vulkan,
                    &encoders,
                    &gpu_alternatives,
                    move || {
                        let g = gpu.as_ref()?;
                        ffi::vulkan_device(g.as_deref()).ok().map(Arc::new)
                    },
                ),
                LazyDevice::after_probe(
                    cuda,
                    Backend::NvencCuda,
                    &encoders,
                    &gpu_alternatives,
                    || ffi::cuda_device().ok().map(Arc::new),
                ),
            )
        };
        #[cfg(not(target_os = "linux"))]
        let (vulkan, cuda) = (LazyDevice::none(), LazyDevice::none());
        Ffmpeg {
            hw_decode,
            picture_rate: None,
            device,
            vaapi_path,
            encoders,
            gpu_alternatives,
            vulkan,
            cuda,
            cpu,
            cpu_encoders,
            decoders,
            decode: DecoderChoice::env(),
        }
    }

    /// A VA-API device of its own for an encoder. Mesa's VA-API driver takes one lock per
    /// device for every call, and a GPU decoder holds it while it waits for its picture:
    /// on the shared device a program and three ISOs encoded at 4–10 pictures/s next to
    /// four file players decoding on the GPU, and at 30 on their own devices. The shared
    /// device if a new one won't open.
    pub(crate) fn encode_device(&self) -> Option<Arc<ffi::BufferRef>> {
        self.vaapi_path
            .as_deref()
            .and_then(|p| ffi::vaapi_device(p).ok().map(Arc::new))
            .or_else(|| self.device.clone())
    }

    /// The hardware that encodes each codec.
    pub fn encoders(&self) -> &[(Codec, Backend)] {
        &self.encoders
    }

    /// The other GPU encoders that work here (each codec's, after the one used).
    pub fn gpu_alternatives(&self) -> &[(Codec, Backend)] {
        &self.gpu_alternatives
    }

    /// The device an encoder of `b` opens on: VA-API's (one of its own), FFmpeg's
    /// Vulkan or CUDA device; none for the others.
    pub(crate) fn device_for(&self, b: Backend) -> Option<Arc<ffi::BufferRef>> {
        match b {
            Backend::Vaapi => self.encode_device(),
            Backend::Vulkan => self.vulkan.get(),
            Backend::NvencCuda => self.cuda.get(),
            _ => None,
        }
    }

    /// The same machine's FFmpeg with `b` as `c`'s encoder (it must work here: one of
    /// [`encoders`](Self::encoders) or [`gpu_alternatives`](Self::gpu_alternatives)).
    pub fn with_encoder(&self, c: Codec, b: Backend) -> Self {
        let mut f = self.with_decoder(self.decode.unwrap_or_default());
        f.decode = self.decode;
        f.encoders.retain(|e| e.0 != c);
        f.encoders.push((c, b));
        f
    }

    /// The CPU's encoder of each codec it has.
    pub fn cpu_encoders(&self) -> &[(Codec, Backend)] {
        &self.cpu_encoders
    }

    /// The same machine's FFmpeg, encoding for an output as `processor` and `format`
    /// say (§N.13): the GPU's encoders (automatic: the GPU's, or the CPU's for a format
    /// the GPU hasn't), or the CPU's; only `format`'s codec when it forces one. No
    /// encoders at all for SpeedHQ only.
    pub fn for_output(
        &self,
        processor: idn_open_protocol::node::video::Processor,
        format: idn_open_protocol::node::video::Format,
    ) -> Self {
        use idn_open_protocol::node::video::{Format, Processor};
        let mut f = self.with_decoder(self.decode.unwrap_or_default());
        f.decode = self.decode;
        let forced = format.codec();
        f.encoders = match (processor, forced) {
            (_, None) if format == Format::SpeedHq => Vec::new(),
            (Processor::Cpu, _) => self.cpu_encoders.clone(),
            (Processor::Gpu, _) => self.encoders.clone(),
            // Automatic with a format forced: the GPU's, else the CPU's.
            (Processor::Auto, Some(c)) => self
                .encoders
                .iter()
                .chain(self.cpu_encoders.iter())
                .find(|e| e.0 == c)
                .copied()
                .into_iter()
                .collect(),
            (Processor::Auto, None) => self.encoders.clone(),
        };
        if let Some(c) = forced {
            f.encoders.retain(|e| e.0 == c);
        }
        f
    }

    /// The CPU H.264 encoder here, if any (OpenH264, else libx264).
    pub fn cpu_h264(&self) -> Option<Backend> {
        self.cpu
    }

    /// An H.264 encoder for `cfg`: the GPU's, else the CPU's (site links and browser
    /// viewing, which need H.264 whatever the machine has; idn-remote.md X.12.6); which
    /// one it is.
    pub fn h264(&self, cfg: &EncoderConfig) -> io::Result<(Box<dyn VideoEncoder>, Backend)> {
        let cfg = EncoderConfig {
            codec: Codec::H264,
            ..cfg.clone()
        };
        if let Some(&(_, b)) = self.encoders.iter().find(|e| e.0 == Codec::H264)
            && let Ok(e) = open_encoder(&self.device_for(b), b, &cfg)
        {
            return Ok((e, b));
        }
        let b = self.cpu.ok_or_else(|| {
            io::Error::other(
                "no H.264 encoder here (no GPU one, and FFmpeg has neither OpenH264 nor libx264)",
            )
        })?;
        Ok((open_encoder(&None, b, &cfg)?, b))
    }
}

/// An output's compressed video as the program's own choices (`own_encoding`,
/// `own_format`) and, where it made none, the machine's video settings (§N.13) say:
/// `None` for SpeedHQ only, or where nothing here encodes what was chosen. The policy
/// is fixed (the site's `codec.policy` doesn't change it) when a format is forced.
pub fn output_codec(
    ff: &Arc<Ffmpeg>,
    own_encoding: Option<idn_open_protocol::node::video::Encoding>,
    own_format: Option<idn_open_protocol::node::video::Format>,
) -> Option<idn_open_protocol::codec::CodecOutput> {
    use idn_open_protocol::codec::{CodecOutput, CodecPolicy};
    use idn_open_protocol::node::video::{Encoding, Format, Processor, machine};
    let m = machine();
    let encoding = own_encoding.or(m.encoding).unwrap_or(Encoding::Auto);
    // (`cpu`, as it was: SpeedHQ only.)
    let format = match own_format.or(m.format) {
        _ if own_encoding == Some(Encoding::Cpu) => Format::SpeedHq,
        Some(f) => f,
        None if encoding == Encoding::Cpu => Format::SpeedHq,
        None => Format::Auto,
    };
    let processor = m.processor.unwrap_or(Processor::Auto);
    let (policy, fixed) = match format {
        Format::SpeedHq => return None,
        Format::Auto => (encoding.policy()?, false),
        _ => (CodecPolicy::Force, true),
    };
    let chosen = Arc::new(ff.for_output(processor, format));
    if idn_open_protocol::codec::EncoderFactory::codecs(&*chosen).is_empty() {
        if let Some(c) = format.codec() {
            idn_open_protocol::log::log(&format!(
                "no {} encoder on the {} here: SpeedHQ instead",
                c.name(),
                if processor == Processor::Cpu {
                    "CPU"
                } else {
                    "GPU or CPU"
                }
            ));
        }
        return None;
    }
    Some(CodecOutput {
        policy,
        fixed,
        ..CodecOutput::new(chosen)
    })
}

/// The backend `IDN_GPU_ENCODER` names.
fn backend_named(v: &str) -> Option<Backend> {
    Some(match v {
        "vaapi" => Backend::Vaapi,
        "nvenc" => Backend::Nvenc,
        "nvenc-cuda" => Backend::NvencCuda,
        "vulkan" => Backend::Vulkan,
        "amf" => Backend::Amf,
        "qsv" => Backend::Qsv,
        _ => return None,
    })
}

/// The name of the GPU wgpu would choose (high performance), for FFmpeg's Vulkan device
/// to be on the same one.
#[cfg(all(target_os = "linux", feature = "gpu"))]
fn gpu_name() -> Option<String> {
    let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    d.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(d);
    let a =
        idn_open_protocol::gpu::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
    (a.get_info().device_type != wgpu::DeviceType::Cpu).then(|| a.get_info().name)
}
#[cfg(all(target_os = "linux", not(feature = "gpu")))]
fn gpu_name() -> Option<String> {
    None
}

/// A GPU device for encoders: the probe's, kept; or made when first asked for (an
/// alternative encoder's, used only if the preferred one is set aside); or none.
pub(crate) struct LazyDevice {
    cell: std::sync::OnceLock<Option<Arc<ffi::BufferRef>>>,
    make: Box<dyn Fn() -> Option<Arc<ffi::BufferRef>> + Send + Sync>,
}

impl LazyDevice {
    // (Used off Linux only: no VA-API device there.)
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(crate) fn none() -> Arc<Self> {
        let d = LazyDevice {
            cell: std::sync::OnceLock::new(),
            make: Box::new(|| None),
        };
        let _ = d.cell.set(None);
        Arc::new(d)
    }

    /// After the probe: `probed` kept if an encoder chosen uses `b`, dropped and made again
    /// on demand if only an alternative does, dropped for good otherwise.
    // (Used on Linux only: the VA-API device.)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn after_probe(
        probed: Option<Arc<ffi::BufferRef>>,
        b: Backend,
        encoders: &[(Codec, Backend)],
        alternatives: &[(Codec, Backend)],
        make: impl Fn() -> Option<Arc<ffi::BufferRef>> + Send + Sync + 'static,
    ) -> Arc<Self> {
        let d = LazyDevice {
            cell: std::sync::OnceLock::new(),
            make: Box::new(make),
        };
        if encoders.iter().any(|e| e.1 == b) {
            let _ = d.cell.set(probed);
        } else if !alternatives.iter().any(|e| e.1 == b) {
            let _ = d.cell.set(None);
        }
        Arc::new(d)
    }

    pub(crate) fn get(&self) -> Option<Arc<ffi::BufferRef>> {
        self.cell.get_or_init(|| (self.make)()).clone()
    }
}

/// Runs `f` with this process's stderr sent to /dev/null (Linux): for libraries that write
/// there themselves. Others' lines in those milliseconds are lost too, so only for the
/// probe. Elsewhere `f` as it is.
fn stderr_quiet<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "linux")]
    {
        use idn_common::sync::LockOk as _;
        use std::os::fd::AsRawFd;
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = ONE.lock_ok();
        let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") else {
            return f();
        };
        // SAFETY: plain descriptor calls; fd 2 is put back before returning.
        let saved = unsafe { libc::dup(2) };
        // SAFETY: as above; `null` stays open for the call.
        if saved < 0 || unsafe { libc::dup2(null.as_raw_fd(), 2) } < 0 {
            if saved >= 0 {
                // SAFETY: `saved` is the descriptor dup gave above, ours to close.
                unsafe { libc::close(saved) };
            }
            return f();
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // SAFETY: fd 2 put back from `saved`, which is then ours to close.
        unsafe {
            libc::dup2(saved, 2);
            libc::close(saved);
        }
        r.unwrap_or_else(|p| std::panic::resume_unwind(p))
    }
    #[cfg(not(target_os = "linux"))]
    f()
}
