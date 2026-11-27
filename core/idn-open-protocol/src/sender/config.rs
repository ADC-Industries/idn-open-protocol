//! The public shapes: a frame given to a sender, its settings, and its totals and status.

use super::*;

/// One uncompressed picture plus its audio, shared by all receivers.
pub struct MediaFrame {
    /// Closed captions to carry with this picture: A/53 `cc_data` (the encoder of a
    /// stream or recording puts them in the video; [`crate::cea608`]). Empty: none.
    pub captions: Vec<u8>,
    pub fourcc: Fourcc,
    pub width: usize,
    pub height: usize,
    /// Shared, so a repeated picture costs no copy.
    pub planes: Arc<Vec<Plane>>,
    pub frame_rate: (u32, u32),
    /// Planar float audio for this frame (may be empty).
    pub audio: Vec<Vec<f32>>,
    pub sample_rate: u32,
    pub timecode: i64,
    pub timestamp: i64,
    /// Per-frame metadata XML, if any.
    pub metadata: Option<String>,
    /// Send as two fields (even lines, then odd lines) instead of one progressive frame.
    pub interlaced: bool,
    /// Pre-encoded NDI|HX video (chapter 7): (FourCC, keyframe, compressed packet). When
    /// set, `planes` is ignored.
    pub hx_video: Option<([u8; 4], bool, Vec<u8>)>,
    /// Pre-encoded AAC audio (§7.2): (compressed packet, samples per channel, channels).
    /// When set, `audio` is ignored.
    pub hx_audio: Option<(Vec<u8>, u32, u32)>,
    /// Frames with the same id have identical pictures: their encoded payload is reused
    /// (static slides re-sent at the frame rate cost almost nothing). `None` = unique.
    pub content_id: Option<u64>,
    /// The picture above 8 bits (P216 convention), coded with high precision for
    /// full-quality, progressive, new-format receivers (§3.9.5, §E.7).
    pub precise: Option<Arc<Vec<crate::shq::Plane16>>>,
    /// The picture on the GPU instead (§E.27), for this process's inputs that take it
    /// there ([`Output::send_planes_gpu_only`](crate::api::Output::send_planes_gpu_only)):
    /// then `planes` have their sizes and no samples, and receivers elsewhere get no picture.
    #[cfg(feature = "gpu")]
    pub gpu: Option<crate::gpu::GpuPicture>,
}

impl MediaFrame {
    /// A picture whose planes have their sizes and no samples: on the GPU only (§E.27).
    /// It goes to receivers that take it there, and nowhere else.
    pub fn samples_missing(&self) -> bool {
        self.hx_video.is_none() && self.planes.first().is_some_and(|p| p.data.is_empty())
    }
}

/// No picture and no sound (a starting point for `..Default::default()`).
impl Default for MediaFrame {
    fn default() -> Self {
        MediaFrame {
            captions: Vec::new(),
            fourcc: Fourcc::Shq2,
            width: 0,
            height: 0,
            planes: Arc::new(Vec::new()),
            frame_rate: (30, 1),
            audio: Vec::new(),
            sample_rate: 48_000,
            timecode: 0,
            timestamp: 0,
            metadata: None,
            interlaced: false,
            hx_video: None,
            hx_audio: None,
            content_id: None,
            precise: None,
            #[cfg(feature = "gpu")]
            gpu: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Full NDI name, `HOST (name)`.
    pub name: String,
    pub quality: u8,
    /// `ndi_capabilities` attributes to announce, if any.
    pub capabilities: Vec<(String, String)>,
    /// Offer multicast to receivers that accept it (§6.4; off by default in NDI).
    pub multicast: bool,
    /// The source is NDI|HX: announce `q_change_slow` as kind 3 (§7.1.2).
    pub hx: bool,
    /// Target video bit rate per receiver in Mbit/s; Q adapts each frame (never above
    /// `quality`, never below 50). `None` = fixed `quality`.
    pub target_mbps: Option<f64>,
    /// Colour description announced to receivers with color/1 active (§E.7).
    pub color: crate::ext::ColorInfo,
    /// Encryption with receivers that share the site passphrase (secure/1, §E.8).
    pub secure: Option<crate::secure::SecureConfig>,
    /// The IDN link (`link/1`, §E.11): IDN receivers connect over one encrypted QUIC
    /// connection on UDP at this sender's port. `None`: off.
    pub link: Option<crate::link::LinkConfig>,
    /// One stream per video frame on links (`frames/1`, §E.13), for receivers that ask.
    pub frame_streams: bool,
    /// Quality follows each IDN receiver (`adapt/1`, §E.16.1), never below this floor.
    /// `None`: fixed quality.
    pub adapt_floor: Option<u8>,
    /// Changed regions only (`regions/1`, §E.15), for receivers that ask.
    pub regions: bool,
    /// Who gets the multicast stream, signatures and key changes (`mcast/1`, §E.17),
    /// when `multicast` is on.
    pub mc: crate::mcast::McConfig,
    /// Compressed video for IDN receivers that want it (`codec/1`, §E.18).
    pub codec: Option<crate::codec::CodecOutput>,
    /// A routing source (§1.8): no media of its own; receivers are sent to the source
    /// this names, and to the next one when it changes. See [`crate::api::Router`].
    pub route: Option<Arc<Route>>,
}

/// Everything a sender has sent, to all receivers together, since it started
/// (NDI's `*-sent` monitor counters, §8.3.6.1).
#[derive(Debug, Default)]
pub struct SendTotals {
    /// Video messages sent.
    pub video_frames: std::sync::atomic::AtomicU64,
    /// Video bytes sent.
    pub video_bytes: std::sync::atomic::AtomicU64,
    /// Audio messages sent.
    pub audio_frames: std::sync::atomic::AtomicU64,
    /// Audio bytes sent.
    pub audio_bytes: std::sync::atomic::AtomicU64,
    /// Metadata messages sent (kind 3, counted once per receiver).
    pub metadata_frames: std::sync::atomic::AtomicU64,
    /// Metadata bytes sent.
    pub metadata_bytes: std::sync::atomic::AtomicU64,
}

/// A sender's state for monitoring (§E.5.1), from [`Sender::status`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderStatus {
    /// Receivers in an accepted session.
    pub receivers: usize,
    /// Of those, encrypted.
    pub encrypted: usize,
    /// Frames skipped for receivers that were behind, summed.
    pub skipped: u64,
    /// Some receiver has this source on program / on preview.
    pub on_program: bool,
    /// See `on_program`.
    pub on_preview: bool,
    /// Totals since the sender started: video frames, video bytes, audio frames, audio bytes.
    pub sent: (u64, u64, u64, u64),
    /// Metadata messages and bytes sent since the sender started.
    pub metadata_sent: (u64, u64),
    /// Each receiver: name, current encode quality, frames skipped and audio messages
    /// dropped for it.
    pub per_receiver: Vec<(String, u8, u64, u64)>,
    /// Receivers on this machine taking the pictures from the ring (§E.26).
    pub machine_readers: usize,
    /// Of those, the ones taking them GPU to GPU (§E.27).
    pub gpu_readers: usize,
    /// SpeedHQ pictures encoded since the start (still while only receivers on this
    /// machine watch).
    pub shq_encodes: u64,
    /// Compressed-video encoders running now (H.264/HEVC/AV1, `codec/1`), and access
    /// units they encoded since the start.
    pub codec_encoders: usize,
    pub codec_encoded: u64,
}
