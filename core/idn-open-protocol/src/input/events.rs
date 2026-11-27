//! What an input delivers: pictures, sound, compressed media, events, and its statistics.

use super::*;

/// A decoded picture: a whole frame, or one field of an interlaced source
/// (`header.field_id` 1 or 2; combine them with [`weave_fields`]).
#[derive(Debug, Clone)]
pub struct VideoIn {
    /// The picture's colour description: colour space, range, bit depth. The defaults
    /// (BT.709 SDR, limited range, 8 bits) apply where the source says nothing.
    pub color: crate::ext::ColorInfo,
    /// Frame information: size, frame rate, timestamps, per-frame metadata.
    pub header: VideoHeader,
    /// The decoded Y'CbCr planes (and alpha, for sources with alpha).
    pub picture: Picture,
    /// The picture as RGBA, `width * height * 4` bytes, if [`InputConfig::rgba`] asked
    /// for it. Give the buffer back with [`Input::recycle_rgba`] when done with it.
    pub rgba: Option<Vec<u8>>,
    /// How this picture's Y'CbCr becomes RGB (matrix, range, transfer), as the RGBA
    /// conversion does it: for converting elsewhere, in a GPU's shader (idn-display).
    pub rgb: ColorConfig,
    /// How the frame arrived: `squic` (encrypted QUIC), `quic`, `tcp`, `udp`,
    /// `multicast` or `control`.
    pub via: &'static str,
    /// When the frame arrived here.
    pub received: Instant,
    /// The CPU time decoding this SpeedHQ picture took, on every thread that worked on it
    /// (to coefficients only, with [`InputConfig::coefficients`]); zero for pictures
    /// decoded otherwise. For choosing between decoders by what they cost.
    pub decode_cpu: Duration,
    /// The picture on the GPU (`gpu/1`, §E.27; [`InputConfig::gpu`]): then
    /// [`picture`](Self::picture)'s planes have their sizes and no samples.
    #[cfg(feature = "gpu")]
    pub gpu: Option<crate::gpu::GpuPicture>,
}

/// One H.264/HEVC access unit from an NDI|HX source, for your decoder. Delivered in
/// order starting at a keyframe. After a loss, frames are held back until the next
/// keyframe (one is requested), so your decoder never sees a broken sequence.
#[derive(Debug, Clone)]
pub struct CompressedVideo {
    /// Frame information: size, frame rate, timestamps.
    pub header: VideoHeader,
    /// H.264 or HEVC.
    pub codec: Codec,
    /// A keyframe (IDR): decoding can start here.
    pub keyframe: bool,
    /// One access unit, Annex B, ready for the decoder as is: on keyframes it always
    /// includes the parameter sets (SPS/PPS, or VPS/SPS/PPS), also from senders that send
    /// them only separately.
    pub data: Vec<u8>,
    /// The parameter sets as the sender also sent them separately, on keyframes (e.g. for
    /// a decoder that wants them up front). Usually not needed: `data` has them.
    pub extra: Vec<u8>,
    /// Presentation time, in 100 ns units.
    pub pts: i64,
    /// Decoding time, in 100 ns units.
    pub dts: i64,
    /// When it arrived here.
    pub received: Instant,
}

/// Decoded audio.
#[derive(Debug, Clone)]
pub struct AudioIn {
    /// Sample rate, channel count, samples, timestamps.
    pub header: AudioHeader,
    /// Planar 32-bit float samples, one `Vec` per channel, nominally −1.0..1.0.
    pub channels: Vec<Vec<f32>>,
    /// How it arrived (as [`VideoIn::via`]).
    pub via: &'static str,
    /// When it arrived here.
    pub received: Instant,
}

impl AudioIn {
    /// The source channels this block holds, in order, when the source sent only some
    /// (`channels/1`, §E.21.3); `None`: every channel of the source, in order.
    pub fn source_channels(&self) -> Option<Vec<u16>> {
        crate::ext::parse_channels_metadata(self.header.metadata.as_deref()).map(|(l, _)| l)
    }
}

/// One AAC frame from an NDI|HX source, for your decoder.
#[derive(Debug, Clone)]
pub struct CompressedAudio {
    /// Sample rate, channel count, samples, timestamps.
    pub header: AudioHeader,
    /// AudioSpecificConfig.
    pub config: Vec<u8>,
    /// One raw AAC frame.
    pub data: Vec<u8>,
    /// When it arrived here.
    pub received: Instant,
}

/// What an [`Input`] delivers, in arrival order.
// Pictures are most events: boxing them would add an allocation to each.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Connection progress.
    State(ConnectionState),
    /// A decoded picture.
    Video(VideoIn),
    /// An H.264/HEVC access unit for your decoder (NDI|HX sources).
    Compressed(CompressedVideo),
    /// Decoded audio.
    Audio(AudioIn),
    /// An AAC frame for your decoder (NDI|HX sources).
    CompressedAudio(CompressedAudio),
    /// An XML message from the source: tally, PTZ capabilities, application metadata…
    Metadata {
        /// 1 for control messages, 3 for application metadata.
        kind: u16,
        /// The XML text.
        xml: String,
    },
    /// Something went wrong but the input carries on, e.g. a frame that failed to
    /// decode, or encrypted media falling back to the control connection.
    Error(String),
    /// Diagnostics: what was offered, which transport was chosen, retries. Usually only
    /// logged.
    Log(String),
}

impl InputEvent {
    /// For Metadata, Error and Log events (the ones kept within a limit): their size.
    pub(super) fn text_len(&self) -> Option<usize> {
        match self {
            InputEvent::Metadata { xml, .. } => Some(xml.len()),
            InputEvent::Error(t) | InputEvent::Log(t) => Some(t.len()),
            _ => None,
        }
    }

    pub(super) fn is_media(&self) -> bool {
        matches!(
            self,
            InputEvent::Video(_)
                | InputEvent::Compressed(_)
                | InputEvent::Audio(_)
                | InputEvent::CompressedAudio(_)
        )
    }
}

/// Counters and connection details, from [`Input::stats`]. Counters run for the life
/// of the input, across reconnects.
#[derive(Debug, Clone, Default)]
pub struct InputStats {
    /// Video frames received (decoded or compressed).
    pub video: usize,
    /// Video frames that failed to decode.
    pub video_errors: usize,
    /// Audio blocks received.
    pub audio: usize,
    /// Audio blocks that failed to decode.
    pub audio_errors: usize,
    /// Control and metadata messages received.
    pub metadata: usize,
    /// Times the input reconnected after losing the source.
    pub reconnects: usize,
    /// Attempts to reach the source (looking it up and connecting) that failed since it
    /// was last connected; 0 while connected. A display gives up after so many (its
    /// reconnect cap, idn-display.md §D.8).
    pub retries: usize,
    /// NDI|HX keyframes received.
    pub hx_keyframes: usize,
    /// NDI|HX keyframe requests sent to the sender (one per discarded frame, as an NDI
    /// 6.3 receiver sends them; §7.1.4). The log says why each wait started.
    pub hx_key_requests: usize,
    /// NDI|HX frames held back after a loss, until the next keyframe.
    pub hx_discarded: usize,
    /// Media events dropped because the application didn't read them in time (see
    /// [`InputConfig::queue`]).
    pub dropped: usize,
    /// Of those, pictures (the rest is audio).
    pub video_dropped: usize,
    /// Pictures not decoded because a newer one had already arrived: this machine (or
    /// this input's thread) couldn't keep up, so the input stays current instead of
    /// falling behind.
    pub late: usize,
    /// Pictures left out above [`InputConfig::picture_rate`].
    pub thinned: usize,
    /// Whole pictures asked for because a region update had no reference to apply to
    /// (`regions/1`: just joined, or a picture was lost).
    pub region_refreshes: usize,
    /// Encrypted multicast (`mcast/1`): datagrams that didn't open and messages whose
    /// signature didn't verify or that were replayed. Above zero, someone on the network
    /// is sending packets that aren't the source's.
    pub mc_rejected: usize,
    /// Encrypted multicast messages lost on the network and re-sent over the link
    /// (`mcast/1`).
    pub mc_repaired: usize,
    /// Metadata, error and log events dropped because the application didn't read them
    /// (more than 256 or 4 MB waiting).
    pub events_dropped: usize,
    /// Compressed video bytes received.
    pub video_bytes: u64,
    /// Compressed audio bytes received.
    pub audio_bytes: u64,
    /// IDN features active with the current source, e.g. `["resume/1", "secure/1"]`;
    /// empty for NDI sources.
    pub extensions: Vec<String>,
    /// Network round-trip time to the source in milliseconds (IDN sources only).
    pub rtt_ms: Option<f64>,
    /// The source's latest report on what it sends this input (IDN sources only).
    pub sender_report: Option<crate::ext::SenderReport>,
    /// The current session is encrypted ([`secure`](crate::secure)). Show a lock in the
    /// UI.
    pub encrypted: bool,
    /// The encryption uses site trust (per-device certificates, [`trust`](crate::trust))
    /// rather than the shared passphrase.
    pub site_trust: bool,
    /// The encryption is between computers linked to each other (`pair/1`, §E.28).
    pub linked: bool,
    /// The session's peer, authenticated: its key's fingerprint (site trust, a linked
    /// computer, or the IDN link's peer). For a viewer that must know whom it sends its
    /// keyboard to.
    pub peer_fp: Option<String>,
    /// Encrypted, but media travels on the control connection because UDP seems blocked.
    /// Still encrypted and working; worth a warning ("allow UDP for full performance").
    pub secure_fallback: bool,
    /// The session runs over the IDN link: `open` (encrypted, not authenticated),
    /// `s1` (passphrase) or `s2` (site trust). `None` otherwise.
    pub link_mode: Option<&'static str>,
    /// Sync figures (mode, delay, clock offset, late frames), when sync is on.
    pub sync: Option<crate::sync::SyncStats>,
    /// The source's link key differs from the one it used before (mode `open`): a
    /// reinstalled machine, or another device using its name. Worth showing.
    pub key_changed: bool,
    /// Messages counted by kind and transport, e.g. `"video via quic"`, for
    /// diagnostics.
    pub via: BTreeMap<String, usize>,
}
