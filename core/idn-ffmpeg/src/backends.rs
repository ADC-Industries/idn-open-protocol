//! The hardware and software that encode each codec here (`Backend`): their FFmpeg
//! encoder names, low-latency options and names for people.

use super::*;

/// Which hardware encodes a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// VA-API: AMD and Intel GPUs.
    Vaapi,
    /// NVENC: NVIDIA GPUs, pictures from system memory.
    Nvenc,
    /// NVENC fed CUDA frames (NVIDIA on Linux): pictures on the application's Vulkan
    /// device handed over through CUDA's external memory, none through system memory.
    NvencCuda,
    /// Vulkan Video (FFmpeg's `*_vulkan`): any GPU whose Vulkan driver encodes (NVIDIA,
    /// AMD, Intel on Linux); pictures on the application's device drawn into its images.
    Vulkan,
    /// AMF: AMD GPUs on Windows.
    Amf,
    /// Quick Sync (oneVPL): Intel GPUs on Windows.
    Qsv,
    /// On the CPU, H.264 only, for links and browser viewing where there is no GPU
    /// encoder (idn-remote.md X.12.6): OpenH264 (BSD; in the FFmpeg the installer
    /// fetches)...
    OpenH264,
    /// ...or the system FFmpeg's libx264 (Linux: the user's own library; nothing GPL is
    /// distributed).
    X264,
    /// On the CPU, H.265 / HEVC: Kvazaar (BSD; in the FFmpeg the installers carry).
    Kvazaar,
    /// On the CPU, AV1: SVT-AV1 (BSD), low-delay real-time mode.
    SvtAv1,
}

impl Backend {
    /// Whether it encodes on the CPU (pictures from this memory, no GPU at all).
    pub fn on_cpu(self) -> bool {
        matches!(
            self,
            Backend::OpenH264 | Backend::X264 | Backend::Kvazaar | Backend::SvtAv1
        )
    }

    /// Whether it takes planar 4:2:0 only (three planes, not NV12).
    pub(crate) fn planar(self) -> bool {
        matches!(self, Backend::OpenH264 | Backend::Kvazaar | Backend::SvtAv1)
    }

    /// Its name for people: "NVENC", "OpenH264 (CPU)", ….
    pub fn label(self) -> &'static str {
        match self {
            Backend::Vaapi => "VA-API",
            Backend::Nvenc => "NVENC",
            Backend::NvencCuda => "NVENC (CUDA)",
            Backend::Vulkan => "Vulkan Video",
            Backend::Amf => "AMF",
            Backend::Qsv => "Quick Sync",
            Backend::OpenH264 => "OpenH264",
            Backend::X264 => "x264",
            Backend::Kvazaar => "Kvazaar",
            Backend::SvtAv1 => "SVT-AV1",
        }
    }

    pub(crate) fn encoder_name(self, c: Codec) -> &'static str {
        match (self, c) {
            (Backend::OpenH264, _) => "libopenh264",
            (Backend::X264, _) => "libx264",
            (Backend::Kvazaar, _) => "libkvazaar",
            (Backend::SvtAv1, _) => "libsvtav1",
            (Backend::Vaapi, Codec::H264) => "h264_vaapi",
            (Backend::Vaapi, Codec::Hevc) => "hevc_vaapi",
            (Backend::Vaapi, Codec::Av1) => "av1_vaapi",
            (Backend::Nvenc | Backend::NvencCuda, Codec::H264) => "h264_nvenc",
            (Backend::Nvenc | Backend::NvencCuda, Codec::Hevc) => "hevc_nvenc",
            (Backend::Nvenc | Backend::NvencCuda, Codec::Av1) => "av1_nvenc",
            (Backend::Vulkan, Codec::H264) => "h264_vulkan",
            (Backend::Vulkan, Codec::Hevc) => "hevc_vulkan",
            (Backend::Vulkan, Codec::Av1) => "av1_vulkan",
            (Backend::Amf, Codec::H264) => "h264_amf",
            (Backend::Amf, Codec::Hevc) => "hevc_amf",
            (Backend::Amf, Codec::Av1) => "av1_amf",
            (Backend::Qsv, Codec::H264) => "h264_qsv",
            (Backend::Qsv, Codec::Hevc) => "hevc_qsv",
            (Backend::Qsv, Codec::Av1) => "av1_qsv",
        }
    }

    /// Low-latency settings (§E.18.3): constant bit rate, no lookahead, an access unit
    /// delimiter on H.264/HEVC (NDI receivers expect one, §7.1.1).
    pub(crate) fn options(self, c: Codec, pixels: usize) -> Vec<(&'static str, &'static str)> {
        let mut o = match self {
            // Realtime: no lookahead, CBR; OpenH264 has no access unit delimiters option.
            Backend::OpenH264 => return vec![("rc_mode", "bitrate"), ("allow_skip_frames", "0")],
            // Kvazaar: its fastest preset, without its groups of pictures (no pictures
            // reordered), one frame in flight (none: 54 pictures/s of 1080p on 16 threads,
            // one: 94, its default three: 158), an access unit delimiter, and the
            // parameter sets with every intra picture (`vps-period=1`; its default sends
            // them with the first picture only, and a receiver joining later can't start).
            Backend::Kvazaar => {
                return vec![(
                    "kvazaar-params",
                    "preset=ultrafast,gop=0,owf=1,aud=1,vps-period=1",
                )];
            }
            // SVT-AV1: the fastest preset, low delay (no pictures reordered) at a constant
            // bit rate, its real-time mode (130 pictures/s of 1080p on 16 threads).
            Backend::SvtAv1 => {
                return vec![("preset", "12"), ("svtav1-params", "pred-struct=1:rc=2:rtc=1")];
            }
            Backend::X264 => vec![("preset", "veryfast"), ("tune", "zerolatency")],
            // Above 1080p, two pictures in flight: the upload of one overlaps the encode
            // of the other (one frame more latency). One at a time held 4K to about 37
            // fps on a Radeon 680M (FFmpeg's default depth gave 53).
            Backend::Vaapi => vec![
                ("rc_mode", "CBR"),
                ("async_depth", if pixels > 1920 * 1080 { "2" } else { "1" }),
            ],
            // Ultra-low-latency tuning, constant bit rate, one picture in flight (its access
            // unit delimiters are on by default).
            Backend::Vulkan => {
                return vec![
                    ("tune", "ull"),
                    ("usage", "stream"),
                    ("rc_mode", "cbr"),
                    ("async_depth", "1"),
                ];
            }
            Backend::Nvenc | Backend::NvencCuda => vec![
                ("preset", "p1"),
                ("tune", "ull"),
                ("zerolatency", "1"),
                ("delay", "0"),
                ("forced-idr", "1"),
                ("rc", "cbr"),
            ],
            // Forced keyframes are IDR pictures with both (a frame marked I).
            Backend::Amf => vec![
                (
                    "usage",
                    if c == Codec::Av1 {
                        "lowlatency"
                    } else {
                        "ultralowlatency"
                    },
                ),
                ("quality", "speed"),
                ("rc", "cbr"),
            ],
            Backend::Qsv => vec![
                ("preset", "veryfast"),
                ("async_depth", "1"),
                ("look_ahead", "0"),
                ("forced_idr", "1"),
            ],
        };
        if c != Codec::Av1 {
            o.push(("aud", "1"));
        }
        o
    }
}

