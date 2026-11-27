//! Hardware video encoders and decoders for the IDN Open Protocol's compressed video
//! (`codec/1`, spec/extensions/idn-extensions.md §E.18), through the system's FFmpeg
//! libraries:
//!
//! - **Encoding, hardware only:** VA-API (AMD, also Intel, on Linux), NVENC (NVIDIA),
//!   AMF (AMD on Windows) and Quick Sync (Intel on Windows): H.264, HEVC and AV1 as the
//!   GPU allows. A machine without one offers no codecs, and its
//!   outputs stay on SpeedHQ.
//! - **Decoding:** FFmpeg's software decoders on every core, moving to the GPU (VA-API
//!   on Linux, NVDEC on NVIDIA, D3D11VA on Windows) at the
//!   next keyframe when software falls behind ([`DecoderChoice`]).
//!
//! ```no_run
//! use idn_open_protocol::api::OutputConfig;
//! use idn_open_protocol::codec::{CodecInput, CodecOutput, CodecUse};
//! use idn_open_protocol::input::InputConfig;
//! use std::sync::Arc;
//!
//! let ff = Arc::new(idn_ffmpeg::Ffmpeg::probe());
//! let mut out = OutputConfig::new("Remote room");
//! out.codec = Some(CodecOutput::new(ff.clone()));
//! let mut input = InputConfig::new("HOST (Remote room)");
//! input.codec = Some(CodecInput::new(ff, CodecUse::Prefer));
//! ```
//!
//! FFmpeg is LGPL; it is linked dynamically and is not part of this crate. For
//! distribution, use an FFmpeg built without GPL components (§E.18.1).

#[cfg(feature = "capture")]
pub mod capture;
#[cfg(all(feature = "gpu", windows))]
mod d3d12_gpu;
#[cfg(all(feature = "gpu", target_os = "linux"))]
mod dmabuf;
mod ffi;
pub mod gateway;
#[cfg(all(feature = "gpu", any(target_os = "linux", windows)))]
mod gpu_convert;
pub mod ingest;
pub mod media;
mod mux;
mod opus;
pub mod player;
#[cfg(feature = "capture")]
pub mod remote;
pub mod screens;
pub mod sitelink;
#[cfg(all(feature = "gpu", target_os = "linux"))]
mod va_gpu;
#[cfg(feature = "voice")]
pub mod voice_feed;
#[cfg(feature = "voice")]
pub mod voice_out;
#[cfg(all(feature = "capture", target_os = "linux"))]
pub mod wayland;
#[cfg(feature = "whep")]
pub mod whep;

use ffmpeg_sys_next as ff;
use idn_open_protocol::codec::{
    AudioDecoder, Codec, Decoded, DecoderFactory, Encoded, EncoderConfig, EncoderFactory,
    VideoDecoder, VideoEncoder,
};
use idn_open_protocol::shq::{Fourcc, Plane};
use idn_open_protocol::thin::Thin;
use std::io;
use std::sync::Arc;

mod backends;
mod choose;
mod decode;
mod download;
mod encode;
pub use backends::Backend;
pub use choose::output_codec;
pub(crate) use decode::*;
pub use download::*;
pub(crate) use encode::*;

/// The render node used when none is given (`IDN_VAAPI_DEVICE` overrides it).
pub const DEFAULT_DEVICE: &str = "/dev/dri/renderD128";

/// The system's FFmpeg as an encoder and decoder adapter for `codec/1`.
pub struct Ffmpeg {
    /// The VA-API device, for decoding on Linux (and probing the encoders).
    device: Option<Arc<ffi::BufferRef>>,
    /// Its render node: each encoder opens its own device there ([`Ffmpeg::encode_device`]).
    vaapi_path: Option<String>,
    encoders: Vec<(Codec, Backend)>,
    /// The other GPU encoders that work here, each codec's (for measuring and choosing,
    /// `IDN_GPU_ENCODER`).
    gpu_alternatives: Vec<(Codec, Backend)>,
    /// FFmpeg's Vulkan device (Vulkan Video) and CUDA device (NVENC fed CUDA frames),
    /// where they open.
    vulkan: Arc<choose::LazyDevice>,
    cuda: Arc<choose::LazyDevice>,
    /// A CPU H.264 encoder ([`Ffmpeg::h264`]).
    cpu: Option<Backend>,
    /// The CPU's encoder of each codec it has (OpenH264 or libx264, Kvazaar, SVT-AV1),
    /// for outputs whose processor is the CPU ([`Ffmpeg::for_output`]).
    cpu_encoders: Vec<(Codec, Backend)>,
    decoders: Vec<Codec>,
    /// The program's choice (or `IDN_DECODER`); `None`: the machine's ([`Ffmpeg::decoding`]).
    decode: Option<DecoderChoice>,
    /// The GPU that decodes: VA-API (AMD, Intel on Linux), NVDEC (NVIDIA) or D3D11VA
    /// (any GPU on Windows), the first this machine has.
    hw_decode: Option<Arc<ffi::HwDecode>>,
    /// The most pictures a second its decoders hand over ([`Ffmpeg::with_picture_rate`]).
    picture_rate: Option<(u32, u32)>,
}

/// Where compressed video is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderChoice {
    /// Software on every core (each picture's slices in parallel), moving to the GPU at
    /// the next keyframe if software falls behind the stream (a single-slice 4K camera,
    /// say). The default: on an AMD APU (Radeon 680M) software decoded a 4-slice 4K
    /// H.264 stream at 184 pictures/s with 5 ms from access unit to picture, the GPU at
    /// 90 with about 40 ms, the download from its memory being the cost. Pictures larger
    /// than 1080p go to the GPU at the next keyframe all the same
    /// (chosen by CPU; a 2160p30 H.264 receiver 1.1 cores in software, ~0.3 on VA-API).
    #[default]
    Auto,
    /// The GPU (VA-API, NVDEC or D3D11VA, whichever the machine has) where it decodes
    /// the codec, else software.
    Gpu,
    /// Software only.
    Software,
}

impl DecoderChoice {
    /// `IDN_DECODER`: `auto`, `gpu` or `software` (else automatic).
    pub fn from_env() -> Self {
        Self::env().unwrap_or_default()
    }

    /// `IDN_DECODER`, if it names a choice.
    fn env() -> Option<Self> {
        match idn_common::switches::value("IDN_DECODER").as_deref() {
            Some("auto") => Some(DecoderChoice::Auto),
            Some("gpu") => Some(DecoderChoice::Gpu),
            Some("software") => Some(DecoderChoice::Software),
            _ => None,
        }
    }
}

impl From<idn_open_protocol::node::video::Decoding> for DecoderChoice {
    fn from(d: idn_open_protocol::node::video::Decoding) -> Self {
        use idn_open_protocol::node::video::Decoding;
        match d {
            Decoding::Auto => DecoderChoice::Auto,
            Decoding::Gpu => DecoderChoice::Gpu,
            Decoding::Software => DecoderChoice::Software,
        }
    }
}

impl Ffmpeg {
    /// Where to decode. Without a choice here or in `IDN_DECODER`: the machine's setting
    /// in the IDN Node when each decoder opens ([`idn_open_protocol::node::video`]), else
    /// [`DecoderChoice::Auto`].
    pub fn set_decoder(&mut self, choice: DecoderChoice) {
        self.decode = Some(choice);
    }

    /// Where decoders opened now decode: the program's choice, else the machine's, else
    /// automatic.
    pub fn decoding(&self) -> DecoderChoice {
        self.decode
            .or_else(|| {
                idn_open_protocol::node::video::machine()
                    .decoding
                    .map(Into::into)
            })
            .unwrap_or_default()
    }

    /// The same machine's FFmpeg, decoding as `choice` (no probing again).
    pub fn with_decoder(&self, choice: DecoderChoice) -> Self {
        Ffmpeg {
            device: self.device.clone(),
            vaapi_path: self.vaapi_path.clone(),
            encoders: self.encoders.clone(),
            gpu_alternatives: self.gpu_alternatives.clone(),
            vulkan: self.vulkan.clone(),
            cuda: self.cuda.clone(),
            cpu: self.cpu,
            cpu_encoders: self.cpu_encoders.clone(),
            decoders: self.decoders.clone(),
            decode: Some(choice),
            hw_decode: self.hw_decode.clone(),
            picture_rate: self.picture_rate,
        }
    }

    /// The same machine's FFmpeg, its decoders handing over at most about `fps` pictures
    /// a second: of a stream at twice that or more, every second picture (or third, …)
    /// is decoded but neither downloaded from the GPU nor converted. For a program that
    /// uses pictures at one rate (a switcher's 2160p30 program: a 4K60 input's GPU
    /// download held it to about 30 pictures a second, the download being the cost).
    pub fn with_picture_rate(&self, fps: (u32, u32)) -> Self {
        let mut f = self.with_decoder(DecoderChoice::Auto);
        f.decode = self.decode;
        f.picture_rate = Some(fps);
        f
    }
}

fn codec_id(c: Codec) -> ff::AVCodecID {
    match c {
        Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
        Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
        Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod surface_probe {
    use super::*;

    /// A VA-API encoder's input surface as DMA-BUF: its layers, formats, modifiers and
    /// pitches (for writing it from the GPU). Run by hand: `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn vaapi_encoder_surface_layout() {
        let ff = Ffmpeg::probe();
        let Some(&(codec, backend)) = ff.encoders.iter().find(|(_, b)| *b == Backend::Vaapi) else {
            eprintln!("no VA-API encoder");
            return;
        };
        let cfg = EncoderConfig {
            codec,
            width: 3840,
            height: 2160,
            frame_rate: (30, 1),
            bitrate: 20_000_000,
            keyframe_interval: 60,
        };
        let mut e = open_ff_encoder(&ff.encode_device(), backend, &cfg, false).unwrap();
        let (hw, m) = e.enc.surface().expect("a mapped surface");
        drop(m);
        // SAFETY: frames of ours; `hw` is the encoder's VA-API surface, mapped as DRM PRIME,
        // its descriptor (`data[0]`) alive with `drm`.
        unsafe {
            let drm = ffi::Frame::new().unwrap();
            (*drm.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let r = ff::av_hwframe_map(
                drm.0,
                hw.0,
                ff::AV_HWFRAME_MAP_READ as i32 | ff::AV_HWFRAME_MAP_WRITE as i32,
            );
            assert!(r >= 0, "map as DRM PRIME: {r}");
            let d = &*((*drm.0).data[0] as *const ff::AVDRMFrameDescriptor);
            eprintln!(
                "{codec:?}: {} objects, {} layers",
                d.nb_objects, d.nb_layers
            );
            for o in &d.objects[..d.nb_objects as usize] {
                eprintln!("  object size {} modifier {:#x}", o.size, o.format_modifier);
            }
            for l in &d.layers[..d.nb_layers as usize] {
                eprintln!(
                    "  layer {:?} planes {}",
                    l.format.to_le_bytes(),
                    l.nb_planes
                );
                for p in &l.planes[..l.nb_planes as usize] {
                    eprintln!(
                        "    object {} offset {} pitch {}",
                        p.object_index, p.offset, p.pitch
                    );
                }
            }
        }
    }
}

#[cfg(all(test, feature = "gpu", target_os = "linux"))]
mod gpu_encode_tests {
    use super::*;
    use idn_open_protocol::gpu::block_on;
    use idn_open_protocol::gpu::{GpuHandIn, GpuPicture, slot_layout};

    /// A picture on the GPU drawn into a VA-API encoder's surface (§E.27) comes out of the
    /// decoder as it went in: at its size, and scaled to half. Needs a GPU (`--ignored`).
    #[test]
    #[ignore]
    fn a_picture_on_the_gpu_is_encoded_as_it_is() {
        let ff = Ffmpeg::probe();
        // Every way here a picture on the GPU goes into an encoder: VA-API's surfaces,
        // Vulkan Video's images, NVENC's CUDA frames.
        let backends: Vec<Backend> = ff
            .encoders
            .iter()
            .chain(&ff.gpu_alternatives)
            .filter(|(c, b)| {
                *c == Codec::H264
                    && matches!(b, Backend::Vaapi | Backend::Vulkan | Backend::NvencCuda)
            })
            .map(|e| e.1)
            .collect();
        if backends.is_empty() {
            eprintln!("no H.264 encoder that takes pictures on the GPU");
            return;
        }
        let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        d.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(d);
        let adapter = block_on(instance.request_adapter(&Default::default())).unwrap();
        let features = adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
        if features.is_empty() {
            eprintln!("no DMA-BUF import");
            return;
        }
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: features,
            ..Default::default()
        }))
        .unwrap();
        let hand = GpuHandIn { device, queue };
        // A 4:2:2 test pattern.
        let (w, h) = (1920u32, 1080u32);
        let cw = w / 2;
        let y = |x: u32, r: u32| ((x / 8 + r / 4) % 200 + 20) as u8;
        let cb = |x: u32, _r: u32| (96 + (x / 16) % 64) as u8;
        let cr = |_x: u32, r: u32| (160 - (r / 8) % 64) as u8;
        let (planes, bytes) = slot_layout(&[(w, h), (cw, h), (cw, h)]);
        let mut data = vec![0u8; bytes as usize];
        for (k, p) in planes.iter().enumerate() {
            for r in 0..p.height {
                for x in 0..p.width {
                    let v = match k {
                        0 => y(x, r),
                        1 => cb(x, r),
                        _ => cr(x, r),
                    };
                    data[(p.offset + u64::from(r * p.row_bytes) + u64::from(x)) as usize] = v;
                }
            }
        }
        let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        hand.queue.write_buffer(&buffer, 0, &data);
        let pic = GpuPicture {
            buffer: Arc::new(buffer),
            planes,
        };
        for (backend, (ew, eh)) in backends
            .iter()
            .flat_map(|b| [(*b, (1920usize, 1080usize)), (*b, (960, 540))])
        {
            eprintln!("{}:", backend.label());
            let cfg = EncoderConfig {
                codec: Codec::H264,
                width: ew,
                height: eh,
                frame_rate: (30, 1),
                bitrate: 20_000_000,
                keyframe_interval: 30,
            };
            let mut e = open_ff_encoder(&ff.device_for(backend), backend, &cfg, false).unwrap();
            let mut au = Vec::new();
            for i in 0..12 {
                let out = e
                    .encode_gpu(&pic, &hand, i, i == 0)
                    .expect("the GPU path")
                    .unwrap();
                au.extend(out);
            }
            let mut dec = ffi::Decoder::open(ff::AVCodecID::AV_CODEC_ID_H264, None).unwrap();
            let mut last = None;
            for (i, a) in au.iter().enumerate() {
                if let Some((f, _)) = dec.decode_frame(&a.data, i as i64).unwrap() {
                    last = Some(f);
                }
            }
            let f = last.expect("a decoded picture");
            let (py, sy) = f.plane(0);
            let (sx, sr) = (w as f64 / ew as f64, h as f64 / eh as f64);
            let mut err = 0f64;
            let mut n = 0f64;
            for r in (8..eh - 8).step_by(7) {
                for x in (8..ew - 8).step_by(5) {
                    // SAFETY: inside the decoded picture's luma plane.
                    let got = unsafe { *py.add(r * sy + x) } as f64;
                    let want = y((x as f64 * sx) as u32, (r as f64 * sr) as u32) as f64;
                    err += (got - want).abs();
                    n += 1.0;
                }
            }
            let mean = err / n;
            // Cb and Cr (the decoder's 4:2:0 planes 1 and 2) the right way round.
            let (mut ecb, mut ecr, mut m) = (0f64, 0f64, 0f64);
            let ((pb, sb), (pr, sr2)) = (f.plane(1), f.plane(2));
            for r in (4..eh / 2 - 4).step_by(5) {
                for x in (4..ew / 2 - 4).step_by(5) {
                    let (sx2, sy2) = ((2 * x) as f64 * sx, (2 * r) as f64 * sr);
                    // SAFETY: inside the decoded picture's chroma planes.
                    let (gb, gr) =
                        unsafe { (*pb.add(r * sb + x) as f64, *pr.add(r * sr2 + x) as f64) };
                    ecb += (gb - cb((sx2 / 2.0) as u32, sy2 as u32) as f64).abs();
                    ecr += (gr - cr((sx2 / 2.0) as u32, sy2 as u32) as f64).abs();
                    m += 1.0;
                }
            }
            eprintln!(
                "{ew}x{eh}: luma off by {mean:.2}, Cb {:.2}, Cr {:.2} on average",
                ecb / m,
                ecr / m
            );
            assert!(mean < 6.0, "{ew}x{eh}: {mean}");
            assert!(ecb / m < 6.0 && ecr / m < 6.0, "{ew}x{eh}: chroma");
        }
    }
}

#[cfg(test)]
mod tests;
