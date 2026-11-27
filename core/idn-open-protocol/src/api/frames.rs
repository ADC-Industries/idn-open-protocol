//! What goes in and out of an output: frames, tapped frames, sound blocks, errors,
//! receivers' messages and statistics.

use super::*;

/// A Y'CbCr picture for [`Output::send_planes`].
#[derive(Debug, Clone)]
pub struct PlanarFrame {
    /// The plane layout: [`Fourcc::Shq2`] (4:2:2), `Shq0` (4:2:0) or `Shq4` (4:4:4).
    pub layout: Fourcc,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Y, Cb, Cr: 8 bits, limited range (Y 16–235, Cb/Cr 16–240), BT.709.
    pub planes: Arc<Vec<crate::shq::Plane>>,
    /// As [`VideoFrame::timestamp`].
    pub timestamp: Option<i64>,
    /// As [`VideoFrame::timecode`].
    pub timecode: Option<i64>,
    /// Audio to send with this picture.
    pub audio: Option<AudioBlock>,
    /// The same picture, already compressed (from a file), for IDN inputs that take its
    /// codec (`codec/1`); needs [`OutputConfig::codec`] offering that codec.
    pub coded: Option<crate::codec::CodedPicture>,
}

/// A picture (or an audio block alone: no planes) as an output sent it, for
/// [`Output::tap`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TappedFrame {
    /// The plane layout (SHQ2 4:2:2, SHQ7 4:2:2 with alpha, SHQ0, SHQ4).
    pub layout: Fourcc,
    /// Width in pixels (0 for audio alone).
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Y, Cb, Cr (and alpha): 8 bits, limited range. Empty for audio alone.
    pub planes: Arc<Vec<crate::shq::Plane>>,
    /// The output's frame rate.
    pub frame_rate: (u32, u32),
    /// Timecode, 100 ns units.
    pub timecode: i64,
    /// Timestamp, 100 ns since the Unix epoch.
    pub timestamp: i64,
    /// Planar float audio sent with it (may be empty).
    pub audio: Vec<Vec<f32>>,
    /// Audio sample rate.
    pub sample_rate: u32,
    /// Closed captions carried with the picture: A/53 `cc_data`
    /// ([`Output::caption_next`], [`crate::cea608`]); empty: none.
    pub captions: Vec<u8>,
    /// The picture on the application's GPU too, for a tap made with
    /// [`Output::tap_gpu_while`] (§E.27): then `planes` may have their sizes and no
    /// samples. The tap's own copy.
    #[cfg(feature = "gpu")]
    pub gpu: Option<crate::gpu::GpuPicture>,
    /// The picture above 8 bits, when the output sent one ([`VideoFrame::p216`]: HDR or
    /// 10-bit): 16-bit Y, Cb, Cr (4:2:2, an 8-bit value v as v × 256), beside `planes`.
    /// What a recorder of an HDR program encodes in 10 bits (`Output::color_info` says its
    /// transfer).
    pub precise: Option<Arc<Vec<crate::shq::Plane16>>>,
}

impl TappedFrame {
    /// A picture built outside an output, for whatever takes tapped frames (a recorder such
    /// as `idn-ffmpeg`'s gateway): e.g. a switcher input recorded on its own. Planes are
    /// 8-bit limited-range Y'CbCr in `layout` (SHQ2 4:2:2, SHQ7 with alpha, SHQ0, SHQ4).
    /// No sound; add it with [`with_audio`](Self::with_audio).
    pub fn picture(
        layout: Fourcc,
        width: usize,
        height: usize,
        planes: Arc<Vec<crate::shq::Plane>>,
        frame_rate: (u32, u32),
        timecode: i64,
        timestamp: i64,
    ) -> TappedFrame {
        TappedFrame {
            layout,
            width,
            height,
            planes,
            frame_rate,
            timecode,
            timestamp,
            audio: Vec::new(),
            sample_rate: 48_000,
            captions: Vec::new(),
            #[cfg(feature = "gpu")]
            gpu: None,
            precise: None,
        }
    }

    /// Sound alone (no picture), as an output sends its sound apart from its pictures:
    /// planar float, one `Vec` per channel.
    pub fn sound(
        audio: Vec<Vec<f32>>,
        sample_rate: u32,
        frame_rate: (u32, u32),
        timecode: i64,
        timestamp: i64,
    ) -> TappedFrame {
        TappedFrame::picture(
            Fourcc::Shq2,
            0,
            0,
            Arc::new(Vec::new()),
            frame_rate,
            timecode,
            timestamp,
        )
        .with_audio(audio, sample_rate)
    }

    /// The same frame with sound: planar float, one `Vec` per channel.
    pub fn with_audio(mut self, audio: Vec<Vec<f32>>, sample_rate: u32) -> TappedFrame {
        self.audio = audio;
        self.sample_rate = sample_rate;
        self
    }
}

/// Audio: planar 32-bit float, nominally −1.0..1.0.
///
/// Either attach it to video frames ([`VideoFrame::audio`], about one frame's duration
/// with each, e.g. 1601 or 1602 samples per channel at 48 kHz and 29.97 fps), or send it
/// on its own with [`Output::send_audio`], in blocks of any length, e.g. from an audio
/// callback.
///
/// Timing of attached audio: the block's **first sample** gets the frame's timecode, so
/// attach the samples that start with that picture. Samples that belong to an earlier
/// picture, attached to a later frame, come out late by the difference (a frame's worth
/// of queued audio on the next frame is about 33 ms late at 30 fps). Audio from a frame
/// that was replaced before sending keeps its own time. When audio and video come from
/// separate clocks or queues, prefer [`Output::send_audio`] with the media timecode.
#[derive(Debug, Clone)]
pub struct AudioBlock {
    /// Samples per second, e.g. 48000.
    pub sample_rate: u32,
    /// One `Vec` per channel, all the same length. Up to 256 channels.
    pub channels: Vec<Vec<f32>>,
}

/// A frame to send: RGBA (the usual case) or 16-bit Y'CbCr ([`VideoFrame::p216`]).
///
/// Pixel buffers are `Arc`s, so sending the same buffer again (a static slide) copies
/// nothing and is detected as a repeat.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Width in pixels, 1–8192.
    pub width: usize,
    /// Height in pixels, 1–8192.
    pub height: usize,
    /// Bytes from one RGBA row to the next, at least `4 * width`. Unused for P216.
    pub stride: usize,
    /// RGBA, 8 bits per channel, sRGB-encoded (as rendered). Unused for P216.
    pub rgba: Arc<[u8]>,
    /// How the RGBA's colour relates to its alpha: [`AlphaMode::Opaque`] (alpha
    /// ignored), `Straight` or `Premultiplied` (as most GPU renderers produce).
    pub alpha: AlphaMode,
    /// When the frame was made, in 100 ns units since the Unix epoch. Receivers get it
    /// unchanged and can use it to line up sources. `None`: the time of `send`.
    pub timestamp: Option<i64>,
    /// The frame's timecode in 100 ns units, on your own media timeline (use the same
    /// timeline for [`Output::send_audio`]). `None` (the usual case): the output's clock
    /// (see [`Output::timecode_now`]), which `send_audio` also uses by default.
    pub timecode: Option<i64>,
    /// Audio to send with this frame, if any.
    pub audio: Option<AudioBlock>,
    /// A 16-bit Y'CbCr 4:2:2 picture instead of RGBA, in the P216 layout: first
    /// `width * height` Y samples, then `height` rows of `width` interleaved Cb, Cr
    /// samples. Limited range; an 8-bit value `v` is `v << 8`. Width must be even. Use
    /// [`VideoFrame::p216`] to build one.
    pub p216: Option<Arc<[u16]>>,
}

impl VideoFrame {
    /// An RGBA frame with rows packed tightly (`stride = 4 * width`).
    pub fn rgba(width: usize, height: usize, rgba: Arc<[u8]>, alpha: AlphaMode) -> Self {
        VideoFrame {
            width,
            height,
            stride: width * 4,
            rgba,
            alpha,
            timestamp: None,
            timecode: None,
            audio: None,
            p216: None,
        }
    }

    /// A 16-bit Y'CbCr frame (see [`VideoFrame::p216`](field@VideoFrame::p216) for the
    /// layout), for 10-bit and HDR sources such as capture cards. It is coded with
    /// extra precision, so smooth gradients keep about 11 bits. Describe the source
    /// with [`OutputConfig::color_info`].
    pub fn p216(width: usize, height: usize, data: Arc<[u16]>) -> Self {
        VideoFrame {
            width,
            height,
            stride: 0,
            rgba: Arc::from(&[][..]),
            alpha: AlphaMode::Opaque,
            timestamp: None,
            timecode: None,
            audio: None,
            p216: Some(data),
        }
    }
}

/// Y, Cb, Cr planes from a P216 buffer.
pub(super) fn p216_planes(d: &[u16], w: usize, h: usize) -> Vec<crate::shq::Plane16> {
    let y = crate::shq::Plane16 {
        width: w,
        height: h,
        data: d[..w * h].to_vec(),
    };
    let c = &d[w * h..2 * w * h];
    let plane = |o: usize| crate::shq::Plane16 {
        width: w / 2,
        height: h,
        data: c.iter().skip(o).step_by(2).copied().collect(),
    };
    vec![y, plane(0), plane(1)]
}

/// Why [`Output::send`] or [`Output::send_audio`] refused a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendError {
    /// The frame's size, stride or buffer length are inconsistent.
    BadFrame(String),
    /// The audio's channels, length or sample rate are unusable.
    BadAudio(String),
    /// The metadata isn't sendable, or too many messages are waiting.
    BadMetadata(String),
    /// The output has stopped (it is being dropped or closed).
    Closed,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::BadFrame(m) => write!(f, "bad frame: {m}"),
            SendError::BadAudio(m) => write!(f, "bad audio: {m}"),
            SendError::BadMetadata(m) => write!(f, "bad metadata: {m}"),
            SendError::Closed => write!(f, "output closed"),
        }
    }
}

impl std::error::Error for SendError {}

/// A metadata message a receiver sent to an output ([`Output::recv_messages`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceiverMessage {
    /// The receiver's name, as it gave it.
    pub receiver: String,
    /// The message (XML).
    pub xml: String,
    /// Whether it came over a session authenticated as a device of this machine's site
    /// (`secure/2`).
    pub site: bool,
    /// Whether it came from a computer linked to this one (`pair/1`, §E.28).
    pub linked: bool,
}

/// Counters since the output was created, for monitoring.
///
/// Returned by [`Output::stats`]; you never build one. Marked `#[non_exhaustive]` so new
/// counters can be added without breaking you — match with a trailing `..`, and read the
/// fields you use rather than destructuring the whole struct.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutputStats {
    /// Frames passed to `send`.
    pub submitted: u64,
    /// Frames replaced in the mailbox before the worker took them (the host was faster
    /// than the output could process).
    pub replaced: u64,
    /// Frames sent that were identical to the previous one (encoding reused).
    pub repeated: u64,
    /// Encrypted multicast (`mcast/1`): group key changes so far.
    pub mc_key_changes: u64,
    /// Encrypted multicast: receivers on it now.
    pub mc_receivers: usize,
    /// Encrypted multicast: messages re-sent to receivers that lost them.
    pub mc_resent: u64,
    /// Compressed video (`codec/1`): pictures encoded on the GPU so far.
    pub codec_encoded: u64,
    /// Compressed video: pictures sent as they came (a file's own, [`PlanarFrame::coded`]).
    pub codec_passed: u64,
    /// Receivers on this machine taking the pictures from shared memory (§E.26).
    pub machine_readers: usize,
    /// Of those, the ones taking them GPU to GPU (§E.27); while all do, the pictures'
    /// samples aren't copied into shared memory.
    pub gpu_readers: usize,
    /// SpeedHQ pictures encoded since the start: still while only receivers on this
    /// machine watch.
    pub shq_encodes: u64,
    /// Compressed-video encoders running now (`codec/1`).
    pub codec_encoders: usize,
}
