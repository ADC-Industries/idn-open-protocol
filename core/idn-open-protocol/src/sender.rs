//! The sender role: accepts receivers on the control port (chapter 1), honours their
//! requests, and delivers media on the transport they advertise (chapters 2, 6) or on
//! the control connection (§1.6).

use crate::control::{self, element, parse_element};
use crate::frame::{Frame, Limits, kind, read_frame};
use crate::media::{AudioHeader, VideoHeader, fowt_encode};
use crate::quic::client::Client as QuicClient;
use crate::shq::encode::{EncodeInput, EncodeOptions, encode};
use crate::shq::{Fourcc, Plane};
use crate::transport::{tcp, udp};
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read as _, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod bundle;
mod cache;
mod frames;
mod hub;
mod session;
use frames::*;
use session::*;

mod gpu;
mod start;
mod wants;

mod config;
mod dispatch;
mod quality;
mod queue;
mod receivers;
pub use config::{MediaFrame, SendTotals, SenderConfig, SenderStatus};
use dispatch::*;
pub(crate) use queue::audio_part;
use receivers::*;

/// How a receiver's session was authenticated, for what it may ask (§E.25, §E.28).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Neither: unencrypted, or with the site passphrase only.
    None,
    /// A device of this machine's site (`secure/2`).
    Site,
    /// A computer linked to this one (`pair/1`).
    Linked,
}

/// Receivers' messages arrived (any output of this process): a count, and its wake-up
/// ([`Output::wait_control`](crate::api::Output::wait_control) waits on it instead of
/// looking every few milliseconds: a game's input waited up to 10 ms).
pub(crate) static METADATA_ARRIVED: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

fn metadata_arrived() {
    *METADATA_ARRIVED.0.lock_ok() += 1;
    METADATA_ARRIVED.1.notify_all();
}

/// Encoded-payload cache key: (content id, new format, short DC, picture size, Q, field
/// id, and for a region update the content id it updates, §E.15). Receivers asking for
/// the same size (the proxy, or `adapt/1` sizes) share it.
type CacheKey = (u64, bool, bool, (usize, usize), u8, u32, Option<u64>);

/// Encoded payloads by `CacheKey`, shared by all receivers of one sender. Small: only
/// recent content is worth keeping. An entry exists from the moment its encode starts,
/// so receivers asking for the same picture at once wait for that one encode instead of
/// each encoding it (encode CPU grew with every stage display watching the output).
#[derive(Default)]
pub struct EncodeCache {
    entries: Mutex<Vec<(CacheKey, Encoded)>>,
    /// The output's hardware encoders, when it has an encoder (`codec/1`, §E.18.3).
    codec: std::sync::OnceLock<Arc<CodecHub>>,
    /// Sound that came as AAC, by content id (the latest few), for inputs that take it.
    coded_audio: Mutex<VecDeque<(u64, Arc<crate::codec::CodedAudio>)>>,
    /// Pictures scaled for receivers that asked for a size, shared between receivers of
    /// the same size: the last few frames by identity (address plus a weak reference,
    /// which rules out a reused address) and size. Scaling 1080p down costs about 5 ms,
    /// and was done once per receiver.
    scaled: Mutex<VecDeque<ScaledEntry>>,
    /// Receivers on this machine (`machine/1`, §E.26): the ring of shared memory their
    /// pictures go through, and the last pictures put in it (once however many read
    /// them), with their numbers.
    machine: Mutex<MachineRing>,
    /// While a receiver on this machine may use the rings: frames numbered as sent, so a
    /// receiver's thread that is behind never puts an older one in after a newer one
    /// (its pictures went backwards for the others).
    machine_live: std::sync::atomic::AtomicBool,
    numbers: Mutex<(u64, VecDeque<Numbered>)>,
    /// SpeedHQ pictures encoded (each picture once, however many receivers): stays
    /// still while only receivers on this machine watch (§E.26).
    shq_encodes: std::sync::atomic::AtomicU64,
    /// The application's GPU, when it gave it ([`Sender::use_gpu`], `gpu/1`, §E.27).
    #[cfg(feature = "gpu")]
    gpu: Mutex<Option<crate::gpu::GpuHandIn>>,
    /// SpeedHQ encoded there, from pictures there (made when first wanted; `Err`: this GPU
    /// can't, not tried again).
    #[cfg(feature = "gpu")]
    gpu_shq: Mutex<Option<Result<crate::shq::gpu_encode::GpuShqEncoder, ()>>>,
    /// When the GPU last couldn't encode a picture: pictures come with their samples
    /// again for a while (`Sender::samples_wanted`).
    #[cfg(feature = "gpu")]
    gpu_shq_missed: Mutex<Option<Instant>>,
    /// The GPU has encoded a picture (and the log said so): it counts as encoding from
    /// then, not from when it was made.
    #[cfg(feature = "gpu")]
    gpu_shq_said: std::sync::atomic::AtomicBool,
    /// Picture sizes the GPU's encoder was warmed for (see `warm_gpu_shq_size`).
    #[cfg(feature = "gpu")]
    gpu_shq_warmed: Mutex<Vec<(usize, usize, Fourcc)>>,
    /// Pictures on the GPU shrunk there for receivers that asked for a size (the proxy),
    /// as `scaled` keeps them, and the pass that shrinks them (by its device's key).
    #[cfg(feature = "gpu")]
    gpu_scaled: Mutex<VecDeque<GpuScaledEntry>>,
    #[cfg(feature = "gpu")]
    gpu_shrink: Mutex<Option<(Option<u64>, Arc<crate::gpu::PlaneShrink>)>>,
}

/// A frame as sent (a weak reference and its address, which rule out a reused address)
/// and its number.
type Numbered = (std::sync::Weak<MediaFrame>, usize, u64);

#[derive(Default)]
struct MachineRing {
    /// The control page (the heads and the doorbell), kept while the rings change.
    control: Option<Arc<crate::machine::Control>>,
    pic: Option<Arc<crate::machine::Ring>>,
    sound: Option<Arc<crate::machine::Ring>>,
    /// Changes when a ring is replaced (receivers reopen).
    generation: u64,
    pic_seq: u64,
    sound_seq: u64,
    /// The frames last put in (their numbers as sent, and their place in the ring), so a
    /// frame several receivers take is written once, and none older than the newest.
    pictures_written: VecDeque<(u64, u64)>,
    sound_written: VecDeque<(u64, u64)>,
    /// Receivers that may use the rings (offered and not refused): when the last one goes,
    /// so do the rings (their memory).
    users: usize,
    /// Receivers reading the rings now: those taking the pictures' samples, and those
    /// taking them on the GPU (§E.27). With none of the first, the samples are left out.
    cpu_takers: usize,
    gpu_takers: usize,
    /// The GPU ring (§E.27), its last number, the pictures put in it (their numbers as
    /// sent, slot, number there), and whether this GPU can't share (not tried again).
    #[cfg(feature = "gpu")]
    gpu: Option<Arc<crate::gpu::GpuRing>>,
    #[cfg(feature = "gpu")]
    gpu_seq: u64,
    #[cfg(feature = "gpu")]
    gpu_written: VecDeque<(u64, (u32, u64))>,
    #[cfg(feature = "gpu")]
    gpu_failed: bool,
}

/// A receiver on this machine that may use the output's rings (§E.26), for as long as it
/// is held (dropped when it refuses them or goes): the rings go with the last.
struct RingUse<'a>(&'a EncodeCache);

/// A receiver reading the rings, counted as taking the samples or the GPU's pictures
/// (§E.27), for as long as it is held.
struct Take<'a> {
    cache: &'a EncodeCache,
    gpu: bool,
}

/// Where a frame's picture or sound is in the rings, for one receiver's thread.
#[derive(Clone, Copy, PartialEq, Debug)]
enum InRing {
    /// Written (now or by another receiver's thread) as this item.
    At(u64),
    /// Older than what is already there: those reading the rings had it, or are past it.
    Stale,
    /// Not in the rings (too large, no shared memory): over the connection.
    No,
}

type ScaledEntry = (
    std::sync::Weak<MediaFrame>,
    usize,
    (usize, usize),
    Arc<std::sync::OnceLock<Arc<Vec<Plane>>>>,
);

#[cfg(feature = "gpu")]
type GpuScaledEntry = (
    std::sync::Weak<MediaFrame>,
    usize,
    (usize, usize),
    Arc<std::sync::OnceLock<Option<crate::gpu::GpuPicture>>>,
);

/// A gateway's view of the frames a sender sends (R.12).
pub type FrameTap = Arc<dyn Fn(&Arc<MediaFrame>) + Send + Sync>;

/// What one shared encoder makes: a codec at a receiver's size (§E.18.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EncKey {
    codec: crate::codec::Codec,
    size: Option<(u32, u32)>,
    low: bool,
}

/// Width, height and frame rate an encoder was opened for.
type EncShape = (usize, usize, (u32, u32));

/// One shared encoder: its input, the receivers it feeds, and keyframe requests.
struct EncSlot {
    key: EncKey,
    input: Arc<FrameQueue>,
    subscribers: Mutex<Vec<Arc<FrameQueue>>>,
    keyframe: AtomicBool,
    /// Passing pictures that came compressed straight through, not encoding: its
    /// receivers need no decoded pictures ([`Sender::needs_pictures`]).
    passing: AtomicBool,
}

/// An output's hardware encoders (`codec/1`, §E.18.3): one per [`EncKey`] in use, each
/// on its own thread, fed every picture, sending HX frames to its receivers' queues.
struct CodecHub {
    out: crate::codec::CodecOutput,
    /// The codecs the encoder has, in the order offered.
    offered: Vec<crate::codec::Codec>,
    slots: Mutex<Vec<Arc<EncSlot>>>,
    /// Pictures that came already compressed, by content id (the latest few).
    coded: Mutex<VecDeque<(u64, Arc<crate::codec::CodedPicture>)>>,
    /// Access units sent as they came, and encoded here.
    passed: std::sync::atomic::AtomicU64,
    encoded: std::sync::atomic::AtomicU64,
    /// The application's GPU ([`Sender::use_gpu`]): pictures there are encoded from
    /// there where the encoder takes them (`VideoEncoder::encode_gpu`, §E.27).
    #[cfg(feature = "gpu")]
    gpu: Mutex<Option<crate::gpu::GpuHandIn>>,
    /// An encoder took a picture from the GPU (its receivers need no samples), and when
    /// one last couldn't (samples again for a while).
    #[cfg(feature = "gpu")]
    gpu_ok: AtomicBool,
    #[cfg(feature = "gpu")]
    gpu_missed: Mutex<Option<Instant>>,
}

/// One cached payload, set once its encode finishes.
type Encoded = Arc<std::sync::OnceLock<Arc<Vec<u8>>>>;

/// Where a routing source (§1.8) sends its receivers: the target's full name and
/// address, with a count of changes that its sessions wait on.
#[derive(Debug, Default)]
pub struct Route {
    now: Mutex<(Option<(String, SocketAddr)>, u64)>,
    changed: Condvar,
}

/// What an output's multicast offers receivers (§6.4, §E.17.1).
struct McOffer {
    /// The `ndi_multicast_v2` announcement (with `x_idn_mc="1"` when encrypted).
    xml: String,
    mode: crate::mcast::Mode,
    /// The group key state, when encrypted.
    group: Option<Arc<crate::mcast::Group>>,
}

/// A session that arrived over the IDN link (§E.11).
struct LinkSession {
    conn: crate::link::LinkConn,
    mode: crate::link::Mode,
    peer_fingerprint: String,
    peer_cert: Vec<u8>,
    keys: Arc<crate::link::KeyMemory>,
}

/// A receiver's `regions/1` state (§E.15.3): the picture it was last sent, which is the
/// one it shows.
#[derive(Default)]
struct Regions {
    sent: Option<RegionRef>,
    /// The picture of the frame being sent, which becomes `sent` once it is.
    pending: Option<RegionRef>,
    last_whole: Option<Instant>,
    /// The receiver asked for a whole frame (`<x_idn_refresh/>`, §E.15.4).
    refresh: Arc<AtomicBool>,
}

struct RegionRef {
    planes: Arc<Vec<Plane>>,
    content_id: Option<u64>,
    fourcc: Fourcc,
    size: (usize, usize),
    new_format: bool,
    whole: bool,
}

/// A receiver's size limit (`adapt/1`, §E.16.2); `None` = the full picture.
type SizeRequest = Option<(u32, u32)>;

/// `adapt/1` (§E.16.1): quality down 4 steps on trouble (skipped or abandoned frames,
/// new losses or late frames at the receiver), up 1 step after 3 clean seconds; never
/// below the floor nor above the output's quality.
struct Adapt {
    q: f64,
    max: f64,
    floor: f64,
    checked: Instant,
    clean_since: Instant,
    seen: (u64, u64, u64), // skipped, v_lost, v_late
}

/// Per-receiver quality control: moves Q so that video message sizes approach a target.
struct RateControl {
    q: f64,
    max: f64,
}

/// What a connected receiver asked for (§1.5).
#[derive(Debug, Clone, Default)]
struct Wants {
    video: bool,
    audio: bool,
    /// Sound in packets of at least this many ms (`x_idn_audio_ms`, §E.6: a receiver on
    /// Wi-Fi or through a tunnel).
    audio_ms: Option<u32>,
    new_format: bool,
    /// Receiver allows short DC (`shq_short_dc` with `shq_skip_block`, §3.1.1).
    short_dc: bool,
    low: bool,
    quic: Option<(u16, u32, u32)>,
    udp: Option<((u16, u32), (u16, u32))>,
    tcp: Option<(u16, u16)>,
    multicast: bool,
    /// The receiver asked for lossless float audio (audio-f32/1, §E.6).
    audio_f32: bool,
    /// The receiver's secure QUIC offer (secure/1, §E.8.4); sent only inside TLS.
    squic: Option<crate::squic::Offer>,
    /// The receiver asked for one stream per video frame (`frames/1`, §E.13.1).
    frames: bool,
    /// The largest picture the receiver wants (`adapt/1`, §E.16.2).
    size: Option<(u32, u32)>,
    /// One picture in this many is enough for the receiver (`adapt/1`, §E.16.2: a
    /// preview): the others aren't made for it at all.
    every: Option<u32>,
    /// The receiver takes changed regions (`regions/1`, §E.15.1).
    regions: bool,
    /// The receiver takes encrypted multicast (`mcast/1`, §E.17.2).
    mc_encrypted: bool,
    /// The receiver decodes AAC (§E.19.2); it gets it while it gets compressed video.
    aac: bool,
    /// The source channels the receiver wants, in its order (`channels/1`, §E.21).
    channels: Option<Vec<u16>>,
    /// The codecs the receiver decodes, preferred first, and whether it prefers or only
    /// allows them (`codec/1`, §E.18.2).
    codecs: Vec<crate::codec::Codec>,
    codec_use: Option<String>,
    /// Frames unscrambled: the multicast stream (§6.4.2).
    plain: bool,
}

enum Path {
    Inline,
    /// secure/1 media over QUIC with real TLS (§E.8.4).
    Squic(crate::squic::SquicSender),
    /// The IDN link's media streams (§E.11.5).
    Link(crate::link::LinkMedia),
    /// Messages for the QUIC driver thread: (is_video, frame bytes).
    Quic {
        tx: SyncSender<QuicFrame>,
        failed: Arc<Mutex<Option<String>>>,
        stop: Arc<AtomicBool>,
    },
    Udp {
        sock: UdpSocket,
        video: (SocketAddr, u32, u16),
        audio: (SocketAddr, u32, u16),
    },
    /// Multi-TCP (§6.2): per media type, two connections and the chunk sequence.
    Tcp {
        video: ([TcpStream; 2], u32),
        audio: ([TcpStream; 2], u32),
    },
    /// The shared multicast stream carries the media (§6.4).
    Multicast,
}

pub(crate) fn fourcc_bytes(f: Fourcc) -> [u8; 4] {
    match f {
        Fourcc::Shq0 => *b"SHQ0",
        Fourcc::Shq2 => *b"SHQ2",
        Fourcc::Shq4 => *b"SHQ4",
        Fourcc::Shq7 => *b"SHQ7",
        Fourcc::Shq9 => *b"SHQ9",
    }
}

/// Per-frame metadata travels in the video extension header, which receivers limit to
/// 4096 bytes (§2.4; ours treat more as a broken stream): larger metadata is left off
/// that frame, and logged once. Send long data with `send_metadata` instead.
const MAX_FRAME_METADATA: usize = 4096 - 56 - 64;

/// A running sender: listens for receivers and fans frames out to them.
/// What a sender knows about one connected receiver.
#[derive(Default)]
pub struct ClientShared {
    /// The receiver's full name (`ndi_identify`).
    pub name: Mutex<String>,
    /// Extensions active with this receiver (§E.2).
    pub ext: Mutex<crate::ext::Active>,
    /// Frames skipped because this receiver was behind (§E.5 `v_skipped`).
    pub skipped: std::sync::atomic::AtomicU64,
    /// The quality its pictures are encoded at now (rate control, `adapt/1`).
    pub quality: std::sync::atomic::AtomicU8,
    /// Audio messages dropped because this receiver was far behind.
    pub audio_dropped: std::sync::atomic::AtomicU64,
    /// The receiver's latest `x_idn_stats` report (§E.5).
    pub report: Mutex<Option<crate::ext::ReceiverReport>>,
    /// The session with this receiver is encrypted (secure/1).
    pub encrypted: AtomicBool,
    /// The receiver's latest tally: (on program, on preview).
    pub tally: Mutex<(bool, bool)>,
    /// It opened this output's ring and takes its pictures from there (§E.26): the ring's
    /// name it said yes to.
    machine_ok: Mutex<Option<String>>,
    /// It said it can't take the ring (another account): the connection, as from afar.
    machine_refused: AtomicBool,
    /// It takes the pictures from the GPU ring (`gpu/1`, §E.27).
    machine_gpu: AtomicBool,
    /// Its SpeedHQ can be encoded on the application's GPU, from pictures only there
    /// (the whole picture, the new format, no changed regions: §E.27); set by its thread.
    gpu_shq: AtomicBool,
    /// Its last picture went through this machine's picture ring (§E.26); set by its
    /// thread.
    ring_pictures: AtomicBool,
    /// The sender's receivers' combined tally, echoed to all of them.
    tally_hub: Arc<TallyHub>,
    /// The sender's totals, shared by all its receivers.
    totals: Arc<SendTotals>,
    writer: Mutex<Option<Arc<Mutex<TcpStream>>>>,
    /// Its link (§E.11), when it came over one: closed with `GOING_AWAY` on departure.
    link: Mutex<Option<crate::link::LinkConn>>,
    /// The receiver turned metadata off (`ndi_enabled_streams text="false"`, §1.5).
    no_text: AtomicBool,
    /// The receiver's video comes from a shared encoder (§E.18.3), not from each picture.
    coded: AtomicBool,
    /// It wants video, and the shared encoder its video comes from (if any).
    video: AtomicBool,
    slot: Mutex<Option<Arc<EncSlot>>>,
    /// The receiver asked for streams (`ndi_enabled_streams`): a receiver, not a probe or a
    /// connection still negotiating.
    active: AtomicBool,
    /// Its `ndi_version` named no IDN extensions: an NDI receiver, which gets NDI|HX when
    /// the output sends it that (§E.18.8).
    plain_ndi: AtomicBool,
    /// Its `ndi_version` declared an SDK of 4.2 or later: it takes HEVC (§7.1.3).
    hevc_ok: AtomicBool,
}

struct ClientSlot {
    tx: Arc<FrameQueue>,
    shared: Arc<ClientShared>,
}

/// One receiver's frames waiting to be encoded and sent. The newest video wins: with
/// [`FrameQueue::VIDEO`] video frames already waiting, the oldest one is dropped (its
/// audio is kept), so a receiver that is behind gets current pictures at a lower rate
/// instead of old ones late. A slow receiver or an overloaded machine used to build a
/// backlog of 16 frames, over a second of delay. NDI|HX frames aren't dropped one by one
/// (a gap breaks the picture until the next keyframe): when [`FrameQueue::MAX`] are
/// waiting, all the queued HX pictures go (their audio stays) and the queue takes HX
/// again from the next keyframe.
#[derive(Default)]
struct FrameQueue {
    /// (frames, closed, waiting for an HX keyframe)
    q: Mutex<(VecDeque<Arc<MediaFrame>>, bool, bool)>,
    ready: Condvar,
}

/// The receiving end of a [`FrameQueue`]: marks it closed when the receiver's thread ends.
struct FrameRx(Arc<FrameQueue>);

pub struct Sender {
    port: u16,
    /// Its listener's thread, joined when it is dropped: the listener let go at once, so
    /// a sender started again on the same fixed port can bind it (bug round F, F5-12).
    accepting: Mutex<Option<std::thread::JoinHandle<()>>>,
    totals: Arc<SendTotals>,
    /// The multicast stream's queue (it drops old pictures like a receiver's, never
    /// audio on its own).
    multicast: Option<Arc<FrameQueue>>,
    /// The encrypted multicast group, if any (§E.17).
    mc_group: Option<Arc<crate::mcast::Group>>,
    /// The hardware encoders, if any (§E.18.3).
    codec_hub: Option<Arc<CodecHub>>,
    /// The encode cache shared by the receivers (also holds sound that came as AAC).
    cache: Arc<EncodeCache>,
    /// Gateways watching what this sender sends (R.12), by id.
    taps: Mutex<Vec<(u64, FrameTap)>>,
    /// Taps that say when they want pictures ([`tap_while`](Self::tap_while)): the
    /// others always do.
    tap_wanted: Mutex<std::collections::HashMap<u64, Arc<dyn Fn() -> bool + Send + Sync>>>,
    /// Buffers of pictures copied for taps on the GPU ([`gpu_copy`](Self::gpu_copy)).
    #[cfg(feature = "gpu")]
    gpu_copies: crate::gpu::BufferPool,
    /// The taps that take pictures on the GPU (this process's inputs given a GPU, §E.27).
    #[cfg(feature = "gpu")]
    gpu_taps: Mutex<std::collections::HashSet<u64>>,
    /// What last needed the pictures ([`Sender::needs_pictures`]), logged on change.
    needs_why: Mutex<Option<String>>,
    clients: Arc<Mutex<Vec<ClientSlot>>>,
    stop: Arc<AtomicBool>,
    /// The byes are being sent (§E.4): set before `stop`.
    departing: Arc<AtomicBool>,
    /// `ndi_color_info` for every video frame, if the source is not BT.709 SDR (§E.7).
    color_info: Option<String>,
    /// Metadata messages received from receivers (kind 3), with the receiver's name and
    /// how its session was authenticated (a device of our site, `secure/2`; a linked
    /// computer, `pair/1`); the latest [`Sender::METADATA_KEPT`] until taken.
    pub metadata: Arc<Mutex<Vec<(String, String, Trust)>>>,
    /// Queue to the thread that sends application metadata, started on first use.
    metadata_out: std::sync::OnceLock<SyncSender<String>>,
    /// Departure notices on SIGINT/SIGTERM, when `shutdown::install` is in effect.
    _on_signal: crate::shutdown::Registration,
    /// The IDN link endpoint, while listening.
    link: Option<crate::link::LinkServer>,
}

/// Senders write timecode 0 in control and metadata frames (§1.2).
/// The source's combined tally and its echo. NDI's `ndi_tally_echo` is the OR of every
/// receiver's tally, sent to every connected receiver when it changes, a receiver that
/// sets none included, and to one that connects while something is on (§1.4, measured
/// against the NDI 6.3 SDK; not an echo of its own for each receiver).
#[derive(Default)]
pub struct TallyHub {
    members: Mutex<Vec<std::sync::Weak<ClientShared>>>,
    /// The combined tally last echoed, and its change's number.
    sent: Mutex<((bool, bool), u64)>,
    /// The number of the change last written: the echo threads write in order, a
    /// change overtaken by a newer one is not written (two quick changes could otherwise
    /// arrive the wrong way round and leave every receiver on the older state).
    written: Mutex<u64>,
    /// An echo thread is writing: one at a time, taking the latest change each round (a
    /// thread per change, each waiting on the last, piled up behind a receiver that
    /// doesn't read; bug round F, F5-4).
    echoing: std::sync::atomic::AtomicBool,
}

/// Receiver sessions at once, in all and from one address; more are refused.
const MAX_SESSIONS: usize = 64;
const MAX_SESSIONS_PER_ADDRESS: usize = 16;

/// How long a new receiver may take to send its first message.
const FIRST_MESSAGE: Duration = Duration::from_secs(10);

/// How long UDP sends may keep failing before the session ends.
const UDP_FAILING: Duration = Duration::from_secs(2);

/// Limits for what receivers send on the control connection: text only (control and
/// metadata), never media, so far below the media limits. A larger frame ends the
/// session. Up to 1 MB of XML, like [`crate::api::Output::send_metadata`] allows.
const CONTROL_LIMITS: Limits = Limits {
    max_hdr_len: 64,
    max_data_len: (1 << 20) + 1,
};

/// One frame's messages for the QUIC driver, and whether its video is an HX keyframe
/// (`None`: not HX).
struct QuicFrame {
    msgs: Vec<(bool, Vec<u8>)>,
    hx_key: Option<bool>,
}

#[cfg(test)]
mod tests;
