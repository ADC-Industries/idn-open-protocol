//! The machine's video settings, kept by the IDN Node (spec/extensions/idn-node.md §N.13):
//! set in its control panel or from the IDN Site Manager, and used by every program on the
//! machine where the program chose nothing itself. A program's own setting always wins.
//!
//! What the library does with them on its own: the worker threads
//! ([`crate::set_worker_threads`] with 0, or never called) and "two pictures at once"
//! ([`InputConfig::parallel_decode`](crate::input::InputConfig::parallel_decode) left at
//! `None`). The idn-ffmpeg adapter takes [`VideoSettings::decoding`] for H.264/HEVC
//! decoders it wasn't given a choice for. [`VideoSettings::encoding`] is for the program
//! that builds an output's encoder: [`Encoding::policy`] gives its
//! [`CodecPolicy`](crate::codec::CodecPolicy). The sender takes [`VideoSettings::ndi`] for
//! outputs whose encoder doesn't say what NDI receivers get. [`VideoSettings::precision`]
//! is the SpeedHQ inverse DCT's arithmetic, on the CPU
//! ([`crate::shq::set_idct_precision`] with `None`, or never called) and on the GPU
//! where its own switch says nothing.
//!
//! The settings arrive with the Node's identity when the program connects and again
//! whenever they change; [`machine`] gives the latest (all unset in a program that
//! doesn't use the Node).

use crate::codec::{CodecPolicy, NdiVideo};
use crate::xml::Element;
use idn_common::sync::RwLockOk as _;
use std::sync::RwLock;

/// How outputs send video to IDN receivers that can decode compressed video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Compressed video from the GPU for pictures above 1080p, SpeedHQ below
    /// ([`CodecPolicy::Auto`], the default).
    Auto,
    /// Compressed video only to inputs that prefer it ([`CodecPolicy::Offer`]).
    Offer,
    /// SpeedHQ on the CPU for everyone: no compressed video.
    Cpu,
    /// Compressed video to every input that can decode it ([`CodecPolicy::Force`]).
    Force,
}

impl Encoding {
    /// `auto`, `offer`, `cpu` or `force`.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Auto => "auto",
            Encoding::Offer => "offer",
            Encoding::Cpu => "cpu",
            Encoding::Force => "force",
        }
    }

    /// The encoding of a name.
    pub fn from_name(s: &str) -> Option<Encoding> {
        match s {
            "auto" => Some(Encoding::Auto),
            "offer" => Some(Encoding::Offer),
            "cpu" => Some(Encoding::Cpu),
            "force" => Some(Encoding::Force),
            _ => None,
        }
    }

    /// The compressed-video policy for an output's encoder; `None` for [`Encoding::Cpu`]
    /// (give the output no encoder).
    pub fn policy(self) -> Option<CodecPolicy> {
        match self {
            Encoding::Auto => Some(CodecPolicy::Auto),
            Encoding::Offer => Some(CodecPolicy::Offer),
            Encoding::Cpu => None,
            Encoding::Force => Some(CodecPolicy::Force),
        }
    }
}

/// Which processor encodes an output's video (§N.13): SpeedHQ, and H.264/HEVC/AV1 when a
/// format is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Processor {
    /// The GPU where it helps: SpeedHQ there for pictures already on the GPU, compressed
    /// video from its encoder; the CPU otherwise (the default).
    Auto,
    /// The GPU: SpeedHQ and compressed video both on it where it can.
    Gpu,
    /// The CPU only: SpeedHQ on the CPU (pictures on the GPU read back), compressed video
    /// from the CPU's encoders (OpenH264, Kvazaar, SVT-AV1).
    Cpu,
}

impl Processor {
    /// `auto`, `gpu` or `cpu`.
    pub fn name(self) -> &'static str {
        match self {
            Processor::Auto => "auto",
            Processor::Gpu => "gpu",
            Processor::Cpu => "cpu",
        }
    }

    /// The processor of a name.
    pub fn from_name(s: &str) -> Option<Processor> {
        match s {
            "auto" => Some(Processor::Auto),
            "gpu" => Some(Processor::Gpu),
            "cpu" => Some(Processor::Cpu),
            _ => None,
        }
    }
}

/// What an output sends IDN receivers (§N.13): chosen for them, or one format for every
/// receiver that decodes it (SpeedHQ to the others, and to NDI receivers as their own
/// setting says).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// As [`Encoding`] says (the default: compressed video above 1080p).
    Auto,
    /// SpeedHQ only: no compressed video.
    SpeedHq,
    /// H.264 for every receiver that decodes it.
    H264,
    /// H.265 / HEVC for every receiver that decodes it.
    Hevc,
    /// AV1 for every receiver that decodes it.
    Av1,
}

impl Format {
    /// `auto`, `speedhq`, `h264`, `hevc` or `av1`.
    pub fn name(self) -> &'static str {
        match self {
            Format::Auto => "auto",
            Format::SpeedHq => "speedhq",
            Format::H264 => "h264",
            Format::Hevc => "hevc",
            Format::Av1 => "av1",
        }
    }

    /// The format of a name (`h265` too).
    pub fn from_name(s: &str) -> Option<Format> {
        match s {
            "auto" => Some(Format::Auto),
            "speedhq" => Some(Format::SpeedHq),
            "h264" => Some(Format::H264),
            "hevc" | "h265" => Some(Format::Hevc),
            "av1" => Some(Format::Av1),
            _ => None,
        }
    }

    /// The compressed codec it forces, if one.
    pub fn codec(self) -> Option<crate::codec::Codec> {
        match self {
            Format::H264 => Some(crate::codec::Codec::H264),
            Format::Hevc => Some(crate::codec::Codec::Hevc),
            Format::Av1 => Some(crate::codec::Codec::Av1),
            Format::Auto | Format::SpeedHq => None,
        }
    }
}

/// Whether SpeedHQ may be encoded on the GPU (pictures already there): not with the
/// machine's processor set to the CPU, nor with `IDN_GPU_SHQ=0`.
pub fn shq_on_gpu_allowed() -> bool {
    !idn_common::switches::off("IDN_GPU_SHQ") && machine().processor != Some(Processor::Cpu)
}

/// Whether a sender asks for its pictures on the GPU to encode SpeedHQ there: GPU first,
/// so whenever that is allowed — the processor Auto or
/// GPU, and `IDN_GPU_SHQ` not off. (It said "only with `IDN_GPU_SHQ=1` or GPU", but that
/// switch is on unless set off, so Auto already meant the GPU; and with the processor set
/// to the CPU it still asked for GPU pictures it wouldn't use.)
pub fn shq_on_gpu_wanted() -> bool {
    shq_on_gpu_allowed()
}

/// How H.264 and HEVC (NDI|HX, compressed IDN video) are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoding {
    /// On the CPU, moving to the GPU when the CPU falls behind, or at once for pictures
    /// larger than 1080p kept on the GPU (the default).
    Auto,
    /// On the GPU when it has a decoder.
    Gpu,
    /// On the CPU.
    Software,
}

impl Decoding {
    /// `auto`, `gpu` or `software`.
    pub fn name(self) -> &'static str {
        match self {
            Decoding::Auto => "auto",
            Decoding::Gpu => "gpu",
            Decoding::Software => "software",
        }
    }

    /// The decoding of a name.
    pub fn from_name(s: &str) -> Option<Decoding> {
        match s {
            "auto" => Some(Decoding::Auto),
            "gpu" => Some(Decoding::Gpu),
            "software" => Some(Decoding::Software),
            _ => None,
        }
    }
}

/// Every setting's name, as [`VideoSettings::set`] takes them (the panels and the Site
/// Manager list them from here).
pub const KEYS: [&str; 8] = [
    "threads",
    "parallel_decode",
    "encoding",
    "decoding",
    "ndi",
    "precision",
    "processor",
    "format",
];

/// The machine's video settings; `None`: not set on this machine (the program's default
/// applies).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoSettings {
    /// Threads for each picture's SpeedHQ work, 1 to 64 (unset: one per physical core, at
    /// most six).
    pub threads: Option<usize>,
    /// Two pictures decoding at once on inputs (4K at 50/60 fps; unset: on).
    pub parallel_decode: Option<bool>,
    /// How outputs send to IDN receivers (unset: the site option `codec.policy`, else
    /// [`Encoding::Auto`]).
    pub encoding: Option<Encoding>,
    /// How H.264/HEVC is decoded (unset: [`Decoding::Auto`]).
    pub decoding: Option<Decoding>,
    /// What outputs with an encoder send NDI receivers (§E.18.8; unset:
    /// [`NdiVideo::SpeedHq`]). An output's own [`CodecOutput::ndi`](crate::codec::CodecOutput::ndi)
    /// wins.
    pub ndi: Option<NdiVideo>,
    /// SpeedHQ decoding's inverse DCT: exact (f64, the specification's) or fast (f32;
    /// unset: exact).
    pub precision: Option<crate::shq::Precision>,
    /// Which processor encodes (unset: [`Processor::Auto`]).
    pub processor: Option<Processor>,
    /// What IDN receivers get (unset: [`Format::Auto`], [`encoding`](Self::encoding)
    /// decides).
    pub format: Option<Format>,
}

impl VideoSettings {
    /// `<video threads=… parallel_decode=… encoding=… decoding=… ndi=…/>`, with the set ones only
    /// (§N.13).
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("video");
        if let Some(n) = self.threads {
            e = e.with_attr("threads", &n.to_string());
        }
        if let Some(p) = self.parallel_decode {
            e = e.with_attr("parallel_decode", if p { "true" } else { "false" });
        }
        if let Some(x) = self.encoding {
            e = e.with_attr("encoding", x.name());
        }
        if let Some(x) = self.decoding {
            e = e.with_attr("decoding", x.name());
        }
        if let Some(x) = self.ndi {
            e = e.with_attr("ndi", x.name());
        }
        if let Some(x) = self.precision {
            e = e.with_attr("precision", x.setting_name());
        }
        if let Some(x) = self.processor {
            e = e.with_attr("processor", x.name());
        }
        if let Some(x) = self.format {
            e = e.with_attr("format", x.name());
        }
        e
    }

    /// The settings of a `<video>` element (unknown values are left unset).
    pub fn from_element(e: &Element) -> VideoSettings {
        VideoSettings {
            threads: e
                .attr("threads")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| (1..=crate::pool::MAX_THREADS).contains(n)),
            parallel_decode: match e.attr("parallel_decode") {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            },
            encoding: e.attr("encoding").and_then(Encoding::from_name),
            decoding: e.attr("decoding").and_then(Decoding::from_name),
            ndi: e.attr("ndi").and_then(NdiVideo::from_name),
            precision: e
                .attr("precision")
                .and_then(crate::shq::Precision::from_setting),
            processor: e.attr("processor").and_then(Processor::from_name),
            format: e.attr("format").and_then(Format::from_name),
        }
    }

    /// Changes one setting by its name (`threads`, `parallel_decode`, `encoding`,
    /// `decoding`, `ndi`, `precision`, `processor`, `format`) from text, as the panels, `idn-node set` and the Site Manager give it;
    /// an empty value (or `auto` for the threads) unsets it.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        match key {
            "threads" => {
                self.threads = match v {
                    "" | "auto" => None,
                    n => Some(
                        n.parse::<usize>()
                            .ok()
                            .filter(|n| (1..=crate::pool::MAX_THREADS).contains(n))
                            .ok_or_else(|| format!("threads {v:?}: 1 to 64, or auto"))?,
                    ),
                }
            }
            "parallel_decode" => {
                // (Empty: unset, the machine's choice.)
                self.parallel_decode = match v {
                    "" => None,
                    v => Some(
                        idn_common::text::parse_on_off(v)
                            .ok_or_else(|| format!("parallel_decode {v:?}: on, off or empty"))?,
                    ),
                }
            }
            "encoding" => {
                self.encoding = match v {
                    "" => None,
                    n => Some(Encoding::from_name(n).ok_or_else(|| {
                        format!("encoding {v:?}: auto, offer, cpu, force or empty")
                    })?),
                }
            }
            "decoding" => {
                self.decoding =
                    match v {
                        "" => None,
                        n => Some(Decoding::from_name(n).ok_or_else(|| {
                            format!("decoding {v:?}: auto, gpu, software or empty")
                        })?),
                    }
            }
            "ndi" => {
                self.ndi = match v {
                    "" => None,
                    n => Some(
                        NdiVideo::from_name(n)
                            .ok_or_else(|| format!("ndi {v:?}: speedhq, h264, hevc or empty"))?,
                    ),
                }
            }
            "precision" => {
                self.precision = match v {
                    "" => None,
                    n => Some(
                        crate::shq::Precision::from_setting(n)
                            .ok_or_else(|| format!("precision {v:?}: exact, fast or empty"))?,
                    ),
                }
            }
            "processor" => {
                self.processor = match v {
                    "" => None,
                    n => Some(
                        Processor::from_name(n)
                            .ok_or_else(|| format!("processor {v:?}: auto, gpu, cpu or empty"))?,
                    ),
                }
            }
            "format" => {
                self.format = match v {
                    "" => None,
                    n => Some(Format::from_name(n).ok_or_else(|| {
                        format!("format {v:?}: auto, speedhq, h264, hevc, av1 or empty")
                    })?),
                }
            }
            _ => return Err(format!("no video setting {key:?}")),
        }
        Ok(())
    }
}

static MACHINE: RwLock<VideoSettings> = RwLock::new(VideoSettings {
    threads: None,
    parallel_decode: None,
    encoding: None,
    decoding: None,
    ndi: None,
    precision: None,
    processor: None,
    format: None,
});

/// The machine's video settings as this program last heard them from the IDN Node; all
/// unset when it doesn't use the Node.
pub fn machine() -> VideoSettings {
    *MACHINE.read_ok()
}

/// Takes the settings in the Node's identity (when the program connects and at every
/// change).
pub(crate) fn heard(identity: &Element) {
    let v = identity
        .children_named("video")
        .next()
        .map(VideoSettings::from_element)
        .unwrap_or_default();
    // The pool's size set before the settings are published: whoever sees the new
    // settings sees the pool sized by them (a test that looked in between saw the old
    // size).
    if *MACHINE.read_ok() != v {
        crate::pool::set_machine_threads(v.threads);
        crate::shq::set_machine_precision(v.precision);
    }
    *MACHINE.write_ok() = v;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_reject_bad_values() {
        let mut v = VideoSettings::default();
        assert_eq!(v.to_element().to_xml(), Element::new("video").to_xml());
        v.set("threads", "8").unwrap();
        v.set("parallel_decode", "off").unwrap();
        v.set("encoding", "cpu").unwrap();
        v.set("decoding", "gpu").unwrap();
        v.set("ndi", "hevc").unwrap();
        v.set("precision", "fast").unwrap();
        v.set("processor", "cpu").unwrap();
        v.set("format", "h265").unwrap();
        assert_eq!(v.format, Some(Format::Hevc));
        assert!(v.set("processor", "npu").is_err());
        assert!(v.set("format", "vp9").is_err());
        let back = VideoSettings::from_element(&v.to_element());
        assert_eq!(back, v);
        assert_eq!(back.encoding.unwrap().policy(), None);
        assert!(v.set("threads", "0").is_err());
        assert!(v.set("threads", "65").is_err());
        assert!(v.set("encoding", "fast").is_err());
        assert!(v.set("ndi", "av1").is_err());
        assert!(v.set("precision", "half").is_err());
        assert!(v.set("speed", "1").is_err());
        v.set("threads", "auto").unwrap();
        v.set("encoding", "").unwrap();
        assert_eq!(v.threads, None);
        assert_eq!(v.encoding, None);
        assert_eq!(v.decoding, Some(Decoding::Gpu));
        assert_eq!(v.precision, Some(crate::shq::Precision::F32));
        v.set("precision", "").unwrap();
        assert_eq!(v.precision, None);
    }
}
