//! Compressed video between IDN peers (`codec/1`, spec/extensions/idn-extensions.md
//! §E.18): H.264, HEVC or AV1, encoded in hardware, for inputs that want it (remote
//! rooms, Wi-Fi) and, by default, for pictures larger than 1080p (4K) to inputs that can
//! decode it. SpeedHQ on the CPU stays the fallback; NDI receivers always get it.
//!
//! The library has no codecs of its own. Encoders and decoders are adapters that the
//! application supplies through [`EncoderFactory`] and [`DecoderFactory`]; the project's
//! adapter (the `idn-ffmpeg` crate) uses the system's FFmpeg with VA-API (Linux), NVENC
//! (NVIDIA), AMF (AMD on Windows) or Quick Sync (Intel on Windows).
//!
//! ```no_run
//! # fn adapter() -> std::sync::Arc<dyn idn_open_protocol::codec::EncoderFactory> { unimplemented!() }
//! use idn_open_protocol::api::OutputConfig;
//! use idn_open_protocol::codec::{CodecOutput, CodecUse};
//!
//! let mut cfg = OutputConfig::new("Remote room");
//! cfg.codec = Some(CodecOutput::new(adapter())); // offers H.264/HEVC/AV1 to inputs that prefer it
//! ```

use crate::shq::Plane;
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// A compressed video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC: every GPU, every decoder.
    H264,
    /// H.265 / HEVC: about half the bit rate of H.264 for the same picture.
    Hevc,
    /// AV1: royalty-free; recent GPUs only.
    Av1,
}

impl Codec {
    /// The video FourCC on the wire (§7.1, §E.18.2).
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            Codec::H264 => crate::hx::FOURCC_H264,
            Codec::Hevc => crate::hx::FOURCC_HEVC,
            Codec::Av1 => FOURCC_AV1,
        }
    }

    /// The codec of a video FourCC.
    pub fn from_fourcc(f: [u8; 4]) -> Option<Self> {
        [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .find(|c| c.fourcc() == f)
    }

    /// The name used in `x_idn_codecs` / `x_idn_codec`: `h264`, `hevc` or `av1`.
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }

    /// The codec of a name.
    pub fn from_name(n: &str) -> Option<Self> {
        [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .find(|c| c.name() == n)
    }
}

/// The FourCC of AV1 video messages (§E.18.2).
pub const FOURCC_AV1: [u8; 4] = *b"AV01";
/// The token a sender with an encoder lists (§E.18.2).
#[doc(hidden)]
pub const TOKEN: &str = "codec/1";
/// The longest time between keyframes (§E.18.3).
pub const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Forced keyframes to one encoder at most this often (§E.18.3).
pub const KEYFRAME_MERGE: Duration = Duration::from_millis(250);

/// A comma-separated codec list.
#[doc(hidden)]
pub fn names(list: &[Codec]) -> String {
    list.iter().map(|c| c.name()).collect::<Vec<_>>().join(",")
}

/// Parses a comma-separated codec list, skipping unknown names.
#[doc(hidden)]
pub fn parse_names(s: &str) -> Vec<Codec> {
    s.split(',')
        .filter_map(|n| Codec::from_name(n.trim()))
        .collect()
}

/// What an encoder is opened for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// The codec to encode.
    pub codec: Codec,
    /// Picture width in pixels.
    pub width: usize,
    /// Picture height in pixels.
    pub height: usize,
    /// Frames per second, numerator and denominator.
    pub frame_rate: (u32, u32),
    /// Target bit rate, bits per second.
    pub bitrate: u64,
    /// The longest gap between keyframes, in frames.
    pub keyframe_interval: u32,
}

/// One encoded access unit (H.264/HEVC: Annex B with start codes; AV1: a temporal unit
/// of OBUs with size fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// The access unit.
    pub data: Vec<u8>,
    /// Independently decodable.
    pub keyframe: bool,
    /// The parameter sets (H.264/HEVC) or sequence header (AV1). Needed on keyframes;
    /// may be empty on other frames.
    pub extra: Vec<u8>,
}

/// A hardware video encoder, fed pictures in order. Low latency: no B-frames, one access
/// unit out per picture in (it may return none while it starts).
pub trait VideoEncoder: Send {
    /// Encodes one picture: Y'CbCr planes, limited range, laid out as `layout` says
    /// (4:2:2 for SHQ2, 4:2:0 for SHQ0, 4:4:4 for SHQ4; alpha planes ignored).
    /// `force_keyframe` asks for a keyframe now.
    fn encode(
        &mut self,
        planes: &[Plane],
        layout: crate::shq::Fourcc,
        pts: i64,
        force_keyframe: bool,
    ) -> io::Result<Vec<Encoded>>;

    /// Encodes one picture above 8 bits: 16-bit Y'CbCr 4:2:2 planes (an 8-bit value v as
    /// v × 256), for an encoder opened for 10 bits (an HDR program's recording, R.24).
    /// Others refuse.
    fn encode16(
        &mut self,
        _planes: &[crate::shq::Plane16],
        _pts: i64,
        _force_keyframe: bool,
    ) -> io::Result<Vec<Encoded>> {
        Err(io::Error::other("this encoder takes 8-bit pictures"))
    }

    /// Closed captions for the next picture encoded: A/53 `cc_data`
    /// ([`crate::cea608`]), put in the video by encoders that can (H.264 and HEVC SEI);
    /// others leave them out.
    fn captions_next(&mut self, _cc: &[u8]) {}

    /// Encodes a picture on the application's GPU `hand` (§E.27), scaled on the way to the
    /// encoder's size, never coming to this memory: `None` when this encoder can't (the
    /// caller then gives it samples with [`encode`](Self::encode)).
    #[cfg(feature = "gpu")]
    fn encode_gpu(
        &mut self,
        _picture: &crate::gpu::GpuPicture,
        _hand: &crate::gpu::GpuHandIn,
        _pts: i64,
        _force_keyframe: bool,
    ) -> Option<io::Result<Vec<Encoded>>> {
        None
    }
}

/// Opens encoders (§E.18.1): hardware only.
pub trait EncoderFactory: Send + Sync {
    /// The codecs this machine encodes in hardware; empty without a usable encoder.
    fn codecs(&self) -> Vec<Codec>;
    /// Opens an encoder; an error leaves that receiver on SpeedHQ.
    fn open(&self, cfg: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>>;
}

/// A decoded picture: 4:2:0 Y'CbCr planes (Y, Cb, Cr), 8 bits, limited range; a picture
/// kept on the GPU ([`VideoDecoder::take_gpu`]) may be 4:2:2 (Cb and Cr every row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Y (`width` × `height`), then Cb and Cr (half width, half height, rounded up).
    pub planes: Vec<Plane>,
    /// Sample range the stream declares, when it does: `Some(true)` for full range
    /// (0–255), `Some(false)` for limited (16–235). Without `x_idn_color` the input
    /// passes this on to the application (NDI|HX streams carry it in the H.264/HEVC VUI).
    pub full_range: Option<bool>,
    /// Y'CbCr matrix the stream declares, when it does: `bt_601`, `bt_709` or `bt_2020`.
    pub matrix: Option<&'static str>,
    /// The `pts` of the access unit this picture came from, for a decoder that hands
    /// pictures over later than their access unit (a pipelined one); `None`: the access
    /// unit just decoded.
    pub pts: Option<i64>,
}

/// A video decoder, fed access units in order.
pub trait VideoDecoder: Send {
    /// Decodes one access unit; `None` if the decoder holds it back. A pipelined decoder
    /// may return an earlier access unit's picture ([`Decoded::pts`] says which).
    fn decode(&mut self, data: &[u8], keyframe: bool, pts: i64) -> io::Result<Option<Decoded>>;

    /// A picture that became ready since the last call, without new input (a pipelined
    /// decoder finishing earlier access units). The input asks every few milliseconds
    /// while it has a decoder.
    fn poll(&mut self) -> io::Result<Option<Decoded>> {
        Ok(None)
    }

    /// The picture just returned by [`decode`](Self::decode) or [`poll`](Self::poll), on
    /// the GPU, from a decoder opened with [`DecoderFactory::open_gpu`] that kept it there
    /// (§E.27): then that [`Decoded`]'s planes have their sizes and no samples. `None`: the
    /// planes have them.
    #[cfg(feature = "gpu")]
    fn take_gpu(&mut self) -> Option<crate::gpu::GpuPicture> {
        None
    }
}

/// Opens decoders (hardware or software).
pub trait DecoderFactory: Send + Sync {
    /// The codecs this machine decodes.
    fn codecs(&self) -> Vec<Codec>;
    /// Opens a decoder for `codec`.
    fn open(&self, codec: Codec) -> io::Result<Box<dyn VideoDecoder>>;

    /// Opens a decoder for `codec` whose pictures may stay on `gpu`, the application's GPU
    /// ([`VideoDecoder::take_gpu`]), where the decoder decodes on that GPU and can hand them
    /// over there. By default, [`open`](Self::open).
    #[cfg(feature = "gpu")]
    fn open_gpu(
        &self,
        codec: Codec,
        gpu: &crate::gpu::GpuHandIn,
    ) -> io::Result<Box<dyn VideoDecoder>> {
        let _ = gpu;
        self.open(codec)
    }

    /// Whether [`open_aac`](Self::open_aac) works: then the input takes AAC sound from
    /// outputs that have it (§E.19.2).
    fn decodes_aac(&self) -> bool {
        false
    }

    /// Opens an AAC decoder for this AudioSpecificConfig.
    fn open_aac(&self, config: &[u8]) -> io::Result<Box<dyn AudioDecoder>> {
        let _ = config;
        Err(io::Error::other("no AAC decoder"))
    }
}

/// A picture that is already compressed (e.g. read from a file), sent as it is to IDN
/// inputs that take its codec at full size instead of encoding it again (R.12). No
/// B-frames (decode order = presentation order), keyframes at most ~2 s apart, the
/// parameter sets in `extra` on keyframes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodedPicture {
    /// Its codec.
    pub codec: Codec,
    /// Independently decodable.
    pub keyframe: bool,
    /// The access unit (as [`Encoded::data`]).
    pub data: Vec<u8>,
    /// Parameter sets or sequence header; needed on keyframes.
    pub extra: Vec<u8>,
}

/// Sound that is already AAC (a file's), sent as it is to inputs that get compressed
/// video and decode AAC (§E.19.2), instead of float audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodedAudio {
    /// One raw AAC frame (no ADTS header).
    pub data: Vec<u8>,
    /// The AudioSpecificConfig.
    pub config: Vec<u8>,
    /// Samples per channel in the frame (1024 for AAC-LC).
    pub samples: u32,
    /// Channels.
    pub channels: u32,
    /// Sample rate.
    pub sample_rate: u32,
}

/// An AAC decoder: one raw frame in, planar float samples out.
pub trait AudioDecoder: Send {
    /// Decodes one frame; empty while the decoder holds it back.
    fn decode(&mut self, frame: &[u8]) -> io::Result<Vec<Vec<f32>>>;
}

/// Whether an output sends compressed video to inputs that only allow it (§E.18.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecPolicy {
    /// Compressed video to inputs that prefer it; SpeedHQ to the others.
    Offer,
    /// As `Offer`, and also to inputs that allow it when the picture they get is larger
    /// than [`LARGE_PICTURE`] (4K): the GPU encodes large pictures, SpeedHQ on the CPU is
    /// the fallback (the default).
    #[default]
    Auto,
    /// Compressed video to every input that can decode it.
    Force,
}

impl CodecPolicy {
    /// The name used in the site option `codec.policy`: `offer`, `auto` or `force`.
    pub fn name(self) -> &'static str {
        match self {
            CodecPolicy::Offer => "offer",
            CodecPolicy::Auto => "auto",
            CodecPolicy::Force => "force",
        }
    }

    /// The policy of a name.
    pub fn from_name(n: &str) -> Option<Self> {
        [CodecPolicy::Offer, CodecPolicy::Auto, CodecPolicy::Force]
            .into_iter()
            .find(|p| p.name() == n)
    }
}

/// Pictures with more pixels than this (1920 × 1080) are "large" for
/// [`CodecPolicy::Auto`].
pub const LARGE_PICTURE: usize = 1920 * 1080;

/// What an output with an encoder sends NDI receivers (§E.18.8): SpeedHQ, as NDI
/// receivers have always had from it, or NDI|HX from the GPU encoder (chapter 7), which
/// NDI receivers of SDK 5 and later decode: a fraction of the CPU and of the network for
/// 4K, with the compressed picture's quality and a little more latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NdiVideo {
    /// SpeedHQ from the CPU (the default).
    SpeedHq,
    /// NDI|HX H.264.
    H264,
    /// NDI|HX HEVC to receivers that take it (SDK 4.2 and later, §7.1.3), H.264 to the
    /// others.
    Hevc,
}

impl NdiVideo {
    /// `speedhq`, `h264` or `hevc`.
    pub fn name(self) -> &'static str {
        match self {
            NdiVideo::SpeedHq => "speedhq",
            NdiVideo::H264 => "h264",
            NdiVideo::Hevc => "hevc",
        }
    }

    /// The setting of a name (`speedhq`, `h264`, `hevc`).
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "speedhq" | "shq" | "cpu" => Some(NdiVideo::SpeedHq),
            "h264" | "hx" | "hx-h264" => Some(NdiVideo::H264),
            "hevc" | "h265" | "hx-hevc" => Some(NdiVideo::Hevc),
            _ => None,
        }
    }
}

/// The codec an NDI receiver gets from `output` (§E.18.8): `None`, SpeedHQ. `hevc_ok`:
/// the receiver declared an SDK of 4.2 or later. Only H.264 and HEVC: NDI|HX has no AV1.
pub fn choose_ndi(ndi: NdiVideo, offered: &[Codec], hevc_ok: bool) -> Option<Codec> {
    match ndi {
        NdiVideo::SpeedHq => None,
        NdiVideo::Hevc if hevc_ok && offered.contains(&Codec::Hevc) => Some(Codec::Hevc),
        NdiVideo::Hevc | NdiVideo::H264 => offered.contains(&Codec::H264).then_some(Codec::H264),
    }
}

/// The running encoder an IDN receiver uses instead of its own choice (§E.18.9), so that an
/// output encodes each picture once, e.g. HEVC that is already going to NDI receivers.
/// `wanted`: the codecs the receiver decodes (it allows or prefers compressed video);
/// `running`: each running encoder at the receiver's size, by codec, with its number of
/// receivers (this one included where it is one); `current`: the codec it gets now.
/// `None` when no encoder of a codec it decodes feeds another receiver: its own choice
/// stands. Between two such encoders the one with more receivers wins, then a fixed
/// codec order, so that two receivers never swap.
pub fn join_running(
    wanted: &[Codec],
    running: &[(Codec, usize)],
    current: Option<Codec>,
) -> Option<Codec> {
    let count = |c: Codec| running.iter().find(|r| r.0 == c).map_or(0, |r| r.1);
    let others = |c: Codec| count(c).saturating_sub(usize::from(current == Some(c)));
    let rank = |c: Codec| (count(c), c as u8);
    let best = running
        .iter()
        .map(|r| r.0)
        .filter(|&c| wanted.contains(&c) && others(c) > 0)
        .max_by_key(|&c| rank(c))?;
    match current {
        Some(c) if wanted.contains(&c) && count(c) > 0 && rank(c) >= rank(best) => Some(c),
        _ => Some(best),
    }
}

/// An output's compressed video (§E.18.1).
#[derive(Clone)]
pub struct CodecOutput {
    /// The encoder adapter.
    pub encoder: Arc<dyn EncoderFactory>,
    /// Offer, automatic (the default: also for large pictures) or force compressed video.
    pub policy: CodecPolicy,
    /// Bit rate for 1080p in Mbit/s, scaled by area for other sizes. Default: 8.
    pub mbps_1080p: f64,
    /// What NDI receivers get (§E.18.8); `None`: the machine's setting (the IDN Node's
    /// `video.ndi`, §N.13), else SpeedHQ.
    pub ndi: Option<NdiVideo>,
    /// The policy chosen for this output (one format forced, §N.13): the site's
    /// `codec.policy` doesn't change it.
    pub fixed: bool,
}

impl CodecOutput {
    /// Offers the encoder's codecs at 8 Mbit/s for 1080p, and uses them for large
    /// pictures ([`CodecPolicy::Auto`]).
    pub fn new(encoder: Arc<dyn EncoderFactory>) -> Self {
        CodecOutput {
            encoder,
            policy: CodecPolicy::Auto,
            mbps_1080p: 8.0,
            ndi: None,
            fixed: false,
        }
    }

    /// Bits per second for a `w` × `h` picture.
    #[doc(hidden)]
    pub fn bitrate(&self, w: usize, h: usize) -> u64 {
        let area = (w * h) as f64 / (1920.0 * 1080.0);
        (self.mbps_1080p * 1e6 * area).max(500_000.0) as u64
    }
}

impl std::fmt::Debug for CodecOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodecOutput")
            .field("codecs", &self.encoder.codecs())
            .field("policy", &self.policy)
            .field("mbps_1080p", &self.mbps_1080p)
            .finish()
    }
}

/// Whether an input takes compressed video (§E.18.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecUse {
    /// Always SpeedHQ; nothing is announced (the default).
    #[default]
    Never,
    /// Can decode it, but takes SpeedHQ unless the output forces compressed video, or
    /// sends it automatically for a picture larger than 1080p ([`CodecPolicy::Auto`]).
    Allow,
    /// Wants compressed video (e.g. a remote room or Wi-Fi).
    Prefer,
}

impl CodecUse {
    #[doc(hidden)]
    pub fn name(self) -> Option<&'static str> {
        match self {
            CodecUse::Never => None,
            CodecUse::Allow => Some("allow"),
            CodecUse::Prefer => Some("prefer"),
        }
    }
}

/// An input's compressed video (§E.18.1, §E.18.4).
#[derive(Clone)]
pub struct CodecInput {
    /// The decoder adapter.
    pub decoder: Arc<dyn DecoderFactory>,
    /// Whether to ask for compressed video.
    pub use_: CodecUse,
    /// Codecs in order of preference; empty = the decoder's, H.264 first.
    pub prefer: Vec<Codec>,
}

impl CodecInput {
    /// Prefers compressed video, in the decoder's codecs.
    pub fn new(decoder: Arc<dyn DecoderFactory>, use_: CodecUse) -> Self {
        CodecInput {
            decoder,
            use_,
            prefer: Vec::new(),
        }
    }

    /// The codecs to ask for, preferred first, that the decoder has.
    #[doc(hidden)]
    pub fn wanted(&self) -> Vec<Codec> {
        let have = self.decoder.codecs();
        let order = if self.prefer.is_empty() {
            vec![Codec::H264, Codec::Hevc, Codec::Av1]
        } else {
            self.prefer.clone()
        };
        order.into_iter().filter(|c| have.contains(c)).collect()
    }
}

impl std::fmt::Debug for CodecInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodecInput")
            .field("codecs", &self.decoder.codecs())
            .field("use", &self.use_)
            .field("prefer", &self.prefer)
            .finish()
    }
}

/// The codec a sender uses for one receiver (§E.18.2): the receiver's first codec the
/// sender encodes, if the receiver prefers compressed video, or allows it and the
/// output forces it, or is automatic and the receiver's picture (`pixels`, after any
/// size request) is large.
#[doc(hidden)]
pub fn choose(
    output: &CodecOutput,
    offered: &[Codec],
    wanted: &[Codec],
    use_: Option<&str>,
    pixels: usize,
) -> Option<Codec> {
    let go = match use_ {
        Some("prefer") => true,
        Some("allow") => match output.policy {
            CodecPolicy::Force => true,
            CodecPolicy::Auto => pixels > LARGE_PICTURE,
            CodecPolicy::Offer => false,
        },
        _ => false,
    };
    go.then(|| wanted.iter().copied().find(|c| offered.contains(c)))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoEncoder;
    impl EncoderFactory for NoEncoder {
        fn codecs(&self) -> Vec<Codec> {
            vec![Codec::H264, Codec::Av1]
        }
        fn open(&self, _: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>> {
            Err(io::Error::other("test"))
        }
    }

    #[test]
    fn idn_receivers_join_an_encoder_that_is_running_anyway() {
        use Codec::*;
        let both = [H264, Hevc];
        // HEVC going to an NDI receiver: an IDN receiver on SpeedHQ joins it.
        assert_eq!(join_running(&both, &[(Hevc, 1)], None), Some(Hevc));
        // Nothing it decodes is running, or only for itself: its own choice stands.
        assert_eq!(join_running(&[H264], &[(Hevc, 1)], None), None);
        assert_eq!(join_running(&both, &[(Hevc, 1)], Some(Hevc)), None);
        // On its own H.264 encoder while HEVC feeds another: one encode instead of two.
        assert_eq!(
            join_running(&both, &[(H264, 1), (Hevc, 1)], Some(H264)),
            Some(Hevc)
        );
        // The other receiver on that HEVC encoder, seeing the H.264 one: it stays, so
        // the two never swap.
        assert_eq!(
            join_running(&both, &[(H264, 1), (Hevc, 1)], Some(Hevc)),
            Some(Hevc)
        );
        // More receivers win over the codec order.
        assert_eq!(
            join_running(&both, &[(H264, 3), (Hevc, 1)], Some(Hevc)),
            Some(H264)
        );
        assert_eq!(
            join_running(&both, &[(H264, 2), (Hevc, 1)], None),
            Some(H264)
        );
    }

    #[test]
    fn ndi_receivers_get_hx_when_asked() {
        let both = [Codec::H264, Codec::Hevc];
        assert_eq!(choose_ndi(NdiVideo::SpeedHq, &both, true), None);
        assert_eq!(choose_ndi(NdiVideo::H264, &both, true), Some(Codec::H264));
        assert_eq!(choose_ndi(NdiVideo::Hevc, &both, true), Some(Codec::Hevc));
        assert_eq!(
            choose_ndi(NdiVideo::Hevc, &both, false),
            Some(Codec::H264),
            "an old receiver: H.264"
        );
        assert_eq!(
            choose_ndi(NdiVideo::Hevc, &[Codec::Hevc], false),
            None,
            "nothing it takes: SpeedHQ"
        );
        assert_eq!(
            choose_ndi(NdiVideo::H264, &[Codec::Av1], true),
            None,
            "no AV1 in NDI|HX"
        );
        for n in [NdiVideo::SpeedHq, NdiVideo::H264, NdiVideo::Hevc] {
            assert_eq!(NdiVideo::from_name(n.name()), Some(n));
        }
    }

    #[test]
    fn the_receivers_choice_and_the_outputs_force() {
        let mut out = CodecOutput::new(Arc::new(NoEncoder));
        let offered = out.encoder.codecs();
        let w = [Codec::Hevc, Codec::Av1, Codec::H264];
        let (hd, uhd) = (1920 * 1080, 3840 * 2160);
        assert_eq!(out.policy, CodecPolicy::Auto);
        assert_eq!(
            choose(&out, &offered, &w, Some("prefer"), hd),
            Some(Codec::Av1)
        );
        assert_eq!(choose(&out, &offered, &w, Some("allow"), hd), None);
        assert_eq!(
            choose(&out, &offered, &w, Some("allow"), uhd),
            Some(Codec::Av1)
        );
        assert_eq!(choose(&out, &offered, &w, None, uhd), None);
        assert_eq!(
            choose(&out, &offered, &[Codec::Hevc], Some("prefer"), hd),
            None
        );
        out.policy = CodecPolicy::Offer;
        assert_eq!(choose(&out, &offered, &w, Some("allow"), uhd), None);
        assert_eq!(
            choose(&out, &offered, &w, Some("prefer"), hd),
            Some(Codec::Av1)
        );
        out.policy = CodecPolicy::Force;
        assert_eq!(
            choose(&out, &offered, &w, Some("allow"), hd),
            Some(Codec::Av1)
        );
        assert_eq!(
            choose(&out, &offered, &w, None, uhd),
            None,
            "never stays never"
        );
        assert_eq!(CodecPolicy::from_name("auto"), Some(CodecPolicy::Auto));
        assert_eq!(parse_names("h264, av1,vp9"), [Codec::H264, Codec::Av1]);
        assert_eq!(names(&[Codec::Hevc, Codec::H264]), "hevc,h264");
        assert_eq!(out.bitrate(1920, 1080), 8_000_000);
        assert_eq!(out.bitrate(64, 36), 500_000);
        assert_eq!(Codec::from_fourcc(*b"AV01"), Some(Codec::Av1));
    }
}
