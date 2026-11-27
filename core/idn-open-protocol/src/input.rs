//! Receiving: open a source by name or address and read events.
//!
//! [`Input::open`] connects in the background and delivers [`InputEvent`]s:
//!
//! - decoded pictures ([`InputEvent::Video`]), for SpeedHQ sources (NDI's normal
//!   format);
//! - H.264/HEVC access units for your own decoder ([`InputEvent::Compressed`]), for
//!   NDI|HX sources such as PTZ cameras;
//! - audio, metadata, and connection state.
//!
//! It reconnects by itself when the source restarts or the network blips.
//!
//! Frames are delivered as they arrive. Each carries the sender's timestamp and its
//! local arrival time. A renderer that takes the latest frame of each input at its own
//! output rate needs nothing more to keep several sources in step.
//!
//! ```no_run
//! use idn_open_protocol::color::AlphaMode;
//! use idn_open_protocol::input::{Codec, ConnectionState, Input, InputConfig, InputEvent};
//! use std::time::Duration;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut cfg = InputConfig::new("CAM-1 (Main)");
//! cfg.rgba = Some(AlphaMode::Premultiplied); // also give me RGBA for the renderer
//! let input = Input::open(cfg)?;
//! loop {
//!     match input.recv(Duration::from_millis(100)) {
//!         Some(InputEvent::Video(v)) => {
//!             let rgba = v.rgba.as_deref(); // width * height * 4 bytes
//!             let _ = (rgba, v.picture.width, v.picture.height);
//!         }
//!         Some(InputEvent::Compressed(c)) => {
//!             // Hand c.data (Annex B) to your H.264/HEVC decoder.
//!             let _ = (c.codec == Codec::H264, c.keyframe);
//!         }
//!         Some(InputEvent::State(ConnectionState::Lost(why))) => println!("lost: {why}"),
//!         _ => {}
//!     }
//! }
//! # }
//! ```

use crate::color::{AlphaMode, ColorConfig, planes_to_rgba_into};
use crate::control::{self, parse_element};
use crate::frame::{Frame, FrameError, Limits, kind, read_frame};
use crate::hx;
use crate::media::fowt_decode;
use crate::quic::server::{Event as QEvent, MediaKind, Server};
use crate::shq;
use crate::transport::receiver::{self as trx, Endpoint, Sink};
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use crate::media::{AudioHeader, VideoHeader};
pub use crate::shq::{Format, Fourcc, Picture, Plane, Plane16};

/// Connection progress, delivered as [`InputEvent::State`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Looking up the source or connecting.
    Connecting,
    /// Connected to the source at this address.
    Connected(SocketAddr),
    /// The source is unreachable or went away, with the reason. With
    /// [`InputConfig::reconnect`], retries follow and `Connected` comes again.
    Lost(String),
    /// A routing source forwarded this input to another source.
    Routed {
        /// The name of the source now received.
        name: String,
        /// Its address.
        addr: SocketAddr,
    },
}

/// Video codec of compressed video: NDI|HX (H.264, HEVC) or IDN `codec/1` (also AV1).
pub use crate::codec::Codec;

mod worker;

mod connect;
mod messages;
mod monitor;
mod multicast;
mod rings;
mod sound;
mod video;

mod config;
mod events;
mod handle;
mod plumbing;
pub use config::{InputConfig, Transport};
pub use events::{AudioIn, CompressedAudio, CompressedVideo, InputEvent, InputStats, VideoIn};
pub use handle::weave_fields;
pub(crate) use plumbing::conversion_config;
use plumbing::*;
pub use plumbing::{resolve, resolve_within};

/// Why a new or reconnected HX input waits (§7.1.4, §7.2).
const HX_WAIT_NEW: &str = "new connection: the sender gives no video until its next keyframe";

/// Events waiting for the application. Media is bounded; state is never dropped.
struct Queue {
    q: Mutex<VecDeque<InputEvent>>,
    ready: Condvar,
    cap: usize,
    stats: Mutex<InputStats>,
    /// An H.264/HEVC frame was dropped here: the worker must wait for a keyframe.
    hx_broken: AtomicBool,
    /// Encrypted multicast datagrams and messages refused (§E.17).
    mc_rejected: Arc<AtomicUsize>,
    /// RGBA buffers handed back with [`Input::recycle_rgba`], for the next pictures.
    spare_rgba: Mutex<Vec<Vec<u8>>>,
}

/// RGBA buffers kept for reuse, at most.
const SPARE_RGBA: usize = 3;

/// Metadata, Error and Log events kept unread, at most, and at most this much text: older
/// ones are dropped ([`InputStats::events_dropped`]). State events are always kept.
const OTHER_KEPT: usize = 256;
const OTHER_BYTES_KEPT: usize = 4 << 20;

impl Queue {
    fn push(&self, mut e: InputEvent) {
        let mut q = self.q.lock_ok();
        if let Some(len) = e.text_len() {
            let (mut n, mut bytes) = q
                .iter()
                .filter_map(InputEvent::text_len)
                .fold((0, 0), |(n, b), l| (n + 1, b + l));
            while n > 0 && (n >= OTHER_KEPT || bytes + len > OTHER_BYTES_KEPT) {
                let Some(i) = q.iter().position(|x| x.text_len().is_some()) else {
                    break;
                };
                bytes -= q.remove(i).and_then(|x| x.text_len()).unwrap_or(0);
                n -= 1;
                self.stats.lock_ok().events_dropped += 1;
            }
        }
        if e.is_media()
            && q.iter().filter(|x| x.is_media()).count() >= self.cap
            && let Some(i) = q.iter().position(InputEvent::is_media)
        {
            let dropped = q.remove(i).unwrap();
            let mut n = 1;
            let video = matches!(dropped, InputEvent::Video(_) | InputEvent::Compressed(_));
            if matches!(dropped, InputEvent::Compressed(_)) {
                // The frames after it can't be decoded without it: drop the waiting
                // non-keyframes too (and this one, unless it is a keyframe), and have the
                // worker hold back until a keyframe, so the decoder never sees a broken
                // sequence.
                let before = q.len();
                q.retain(|x| !matches!(x, InputEvent::Compressed(c) if !c.keyframe));
                n += before - q.len();
                if matches!(&e, InputEvent::Compressed(c) if !c.keyframe) {
                    n += 1;
                    e = InputEvent::Log(
                        "dropped an H.264/HEVC frame: waiting for a keyframe".into(),
                    );
                }
                self.hx_broken.store(true, Ordering::Relaxed);
            }
            let mut st = self.stats.lock_ok();
            st.dropped += n;
            if video {
                st.video_dropped += n;
            }
        }
        q.push_back(e);
        self.ready.notify_one();
    }
}

pub(crate) enum Command {
    Tally(bool, bool),
    Metadata(String),
    LowBandwidth(bool),
    MaxSize(Option<(u32, u32)>),
    ParallelDecode(bool),
    Coefficients(bool),
    AudioChannels(Option<Vec<u16>>),
    #[cfg(feature = "gpu")]
    GpuOff,
}

/// A source being received. Dropping it disconnects.
///
/// All methods are cheap and non-blocking, except [`recv`](Input::recv), which waits
/// up to its timeout.
pub struct Input {
    queue: Arc<Queue>,
    cmd: Sender<Command>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Listed in the IDN Node's panel while it lives, when encryption comes from the Node.
    _node: Option<crate::node::Registration>,
}

// ---------------------------------------------------------------------------------------

enum Incoming {
    Tcp(u32, io::Result<Option<Frame>>),
    Quic(QEvent),
    /// A media message, with the session it came in under ([`Worker::media_epoch`]):
    /// those queued for a session that ended are dropped (F5-1).
    Media(MediaKind, &'static str, Result<Frame, FrameError>, u64),
    /// A link connected in the background to the sender of a TCP session (§E.11.3).
    Upgrade(SocketAddr, Box<io::Result<crate::link::LinkClient>>),
    /// Result of a background name resolution: (address, how it was found, whether the
    /// discovery hint offers the link, if discovery said) or an error.
    Resolved(Result<(SocketAddr, String, Option<bool>), String>),
    /// A picture decoded on the side is ready ([`Ahead`]).
    Decoded,
    /// A look for the routing source that left (§1.8): where it answers, if it does.
    RouterLook(Option<SocketAddr>),
    /// A picture (`false`) or block of sound (`true`) in the source's ring, by its number
    /// (§E.26), from the watcher of the rings taken `epoch`-th (a watcher of rings since
    /// let go may still pass on what it was woken for: ignored).
    Machine(u64, bool, u64),
}

/// The rings of a source on this machine (§E.26), and their watcher (stopped when this
/// goes).
struct MachineIn {
    /// The control page's name and the generation: the same from no other source, or
    /// this one restarted (generations count alike from 1).
    control: String,
    generation: String,
    epoch: u64,
    /// The picture ring imported as a GPU buffer, tried once; before `pic`, so
    /// it goes first (the ring's memory must outlive it).
    #[cfg(feature = "gpu")]
    host: Option<Option<wgpu::Buffer>>,
    pic: Option<crate::machine::Ring>,
    sound: Option<crate::machine::Ring>,
    /// Where pictures and sound left the connection (`x_idn_machine_from`): items before,
    /// or before it is told, came over the connection and are passed over here.
    from: (Option<u64>, Option<u64>),
    /// The newest numbers the watcher passed on.
    seen: (u64, u64),
    stop: Arc<AtomicBool>,
    /// The source's GPU ring, imported (§E.27), with the control page its stamps are in.
    #[cfg(feature = "gpu")]
    gpu: Option<(crate::gpu::GpuSlots, crate::machine::Control)>,
}

/// How often a routing source that left is looked for again (§1.8).
const ROUTER_LOOK: Duration = Duration::from_secs(2);

/// A routing source that drops the connection again this soon after being followed
/// closes its connections itself: it isn't followed again.
const ROUTER_STEADY: Duration = Duration::from_secs(10);

/// How often a running TCP session may try to move to the link (§E.11.3).
const UPGRADE_INTERVAL: Duration = Duration::from_secs(30);

/// After a link attempt to an address failed while TCP to it worked, how long TCP is
/// used straight away (§E.11.3).
const LINK_FAILURE_MEMORY: Duration = Duration::from_secs(600);
/// A session that ends within this time without delivering media was refused: senders
/// hold a refused connection about 2 s before closing it (§E.8.5).
const REFUSAL_WINDOW: Duration = Duration::from_secs(3);

/// One way of connecting to a sender, in the race of `connect_control`.
enum Attempt {
    Link(io::Result<crate::link::LinkClient>),
    Tcp(io::Result<TcpStream>),
}

/// Video messages waiting for the worker, at most: more are dropped on arrival (counted
/// as [`InputStats::late`]). Only reached when decoding can't keep up and late pictures
/// aren't skipped ([`InputConfig::skip_late`] off); about 2.5 s at 25 fps.
const MAX_VIDEO_WAITING: usize = 64;
/// Sound messages waiting, at most (about 2.7 s of a sender's 32-sample blocks): more are
/// dropped on arrival (unbounded, a worker held up kept every block; bug round F, F5-8).
const MAX_AUDIO_WAITING: usize = 4096;

/// The worker's inbox, counting the video messages waiting in it: when a newer picture
/// is already queued, decoding the older one would only add latency (this machine is too
/// busy), so it is skipped. Drop frames, never stall.
#[derive(Clone)]
struct Inbox {
    tx: Sender<Incoming>,
    video_waiting: Arc<AtomicUsize>,
    audio_waiting: Arc<AtomicUsize>,
    /// A video message was dropped on arrival: the reference picture is stale (§E.15.4).
    video_lost: Arc<AtomicBool>,
    queue: Arc<Queue>,
}

/// Pictures decoded at most this many at once ([`InputConfig::parallel_decode`]).
const AHEAD: usize = 2;

/// SpeedHQ pictures decoding on threads of their own, two at once
/// ([`InputConfig::parallel_decode`]); handed over in arrival order.
struct Ahead {
    /// Jobs for the two decoding threads, started with the first picture.
    lanes: Option<Sender<Box<dyn FnOnce() + Send>>>,
    results: (Sender<Numbered>, Receiver<Numbered>),
    /// Dispatched pictures, oldest first, and those of them already done.
    in_flight: VecDeque<(u64, AheadPicture)>,
    done: std::collections::BTreeMap<u64, AheadDone>,
    /// The newest picture that arrived while both threads were busy.
    next: Option<(Frame, VideoHeader, &'static str, Instant)>,
    seq: u64,
}

/// A decoded picture with its place in arrival order.
type Numbered = (u64, AheadDone);

/// What the worker keeps of a picture while it decodes.
struct AheadPicture {
    vh: VideoHeader,
    color: crate::ext::ColorInfo,
    rgb: ColorConfig,
    via: &'static str,
    received: Instant,
    bytes: usize,
    b0: Option<u8>,
}

/// A decoded picture: `Err(())` where the decoder or the RGBA conversion panicked.
struct AheadDone {
    picture: Result<Result<Picture, shq::DecodeError>, ()>,
    rgba: Option<Result<Vec<u8>, ()>>,
    secs: f64,
    /// CPU time of the decode, every thread's ([`VideoIn::decode_cpu`]).
    cpu: Duration,
}

struct Worker {
    cfg: InputConfig,
    queue: Arc<Queue>,
    stop: Arc<AtomicBool>,
    tx: Inbox,
    rx: Receiver<Incoming>,
    sink: Sink,
    endpoints: Vec<Endpoint>,
    /// Bumped at each new control session: UDP streams start afresh (R1-76).
    udp_session: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// The current session's sender: the only host multi-TCP media is taken from.
    sender_ip: trx::Allowed,
    /// The session media now belongs to: moved on when a session ends and when one
    /// starts (media queued before is dropped; multi-TCP pairs of the last one close).
    media_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// The multicast groups of the current session, replaced by each announcement (each
    /// one used to add another endpoint, so frames arrived several times).
    multicast: Option<Endpoint>,
    /// The tally last set by the application, sent again on every new session.
    tally: (bool, bool),
    /// Set when a routed connection failed: the next `handle` reports the source lost.
    pending_lost: Option<String>,
    /// The name the session's source announced (`ndi_identify`), and after a fast
    /// reconnect the one it must announce again, until when: since IDN's ports are one
    /// block, the last address could be another source of that machine after its program
    /// restarted (bug round F, F1-1).
    peer_name: Option<String>,
    expect_name: Option<(String, Instant)>,
    /// The routing source left while its target stays (§1.8): when to look for it again.
    /// A router that ends and starts again (its machine's Node restarted) is followed
    /// again.
    router_gone: Option<Instant>,
    /// When the routing source last routed this input, and whether that was a follow
    /// after it left: one that drops the connection again within [`ROUTER_STEADY`] of
    /// being followed closes its connections itself, and following it again would loop. (A
    /// first fix judged that by its address, and a router started again on the same port
    /// was never followed.)
    routed_at: Option<Instant>,
    followed_again: bool,
    threads: Vec<JoinHandle<()>>,
    /// `ndi_enabled_streams` with this input's transports; the UDP and QUIC identifiers
    /// are renewed for each session.
    streams: control::EnabledStreams,
    stream_ids: [Option<crate::quic::server::StreamIds>; 2],
    /// `ndi_enabled_streams` without transports: media on the (encrypted) control
    /// connection (§E.8.4).
    inline_streams_xml: String,
    /// The secure QUIC media endpoint (secure/1, §E.8.4), when a site key is configured.
    squic: Option<crate::squic::SquicServer>,
    /// The current session is encrypted, and whether it offered secure QUIC.
    secure_session: bool,
    /// When `secure.require` was last looked at for an unencrypted session.
    require_checked: Instant,
    squic_offered: bool,
    fallback_reported: bool,
    full_name: String,
    quality: &'static str,
    conns: BTreeMap<u32, TcpStream>,
    next_conn: u32,
    active: u32,
    /// The current session's QUIC media connection (the sender's address), if any.
    quic_peer: Option<SocketAddr>,
    routing: Option<u32>,
    ever_connected: bool,
    /// Why the last attempt to connect failed, as logged (each reason once, not every
    /// retry).
    last_failure: Option<String>,
    hx_need_key: bool,
    hx_last_counter: Option<u64>,
    /// Why the input is waiting for a keyframe; logged with the first request it sends.
    hx_wait: Option<&'static str>,
    // IDN extensions (§E.2–E.5).
    ext: crate::ext::Active,
    peer_instance: Option<String>,
    /// Instance of the sender before the last reconnect, for `x_idn_resume`.
    resume_from: Option<String>,
    last_addr: Option<SocketAddr>,
    bye: Option<crate::ext::Departure>,
    fast_until: Option<Instant>,
    next_fast: Instant,
    /// When the source was last lost (cleared on reconnecting): for 30 s after, it is
    /// looked for every second rather than every 2 s.
    lost_at: Option<Instant>,
    last_report: Instant,
    /// Counters at the start of the current connection: x_idn_stats is per connection (§E.5).
    report_base: [u64; 5],
    next_ping: Instant,
    ping_id: u64,
    started: Instant,
    dec_time: f64,
    dec_n: u64,
    /// The CPU time of the picture being handed over ([`VideoIn::decode_cpu`]).
    decode_cpu: Duration,
    /// The picture being handed over, on the GPU ([`VideoIn::gpu`]).
    #[cfg(feature = "gpu")]
    next_gpu: Option<crate::gpu::GpuPicture>,
    /// Pictures from shared memory on their way to the GPU (for an input given one).
    #[cfg(feature = "gpu")]
    uploads: Option<crate::gpu::Uploads>,
    /// Headers of access units given to the decoder, until their pictures come out.
    codec_meta: VecDeque<(i64, VideoHeader, &'static str, Instant)>,
    /// SpeedHQ pictures decoding on the side ([`InputConfig::parallel_decode`]).
    ahead: Ahead,
    /// Range and matrix the decoder reported for the compressed picture about to be
    /// shown (taken by `show`), and the last pair logged.
    dec_color: Option<(Option<bool>, Option<&'static str>)>,
    dec_color_logged: Option<(Option<bool>, Option<&'static str>)>,
    /// The source's rings of shared memory, when it is on this machine (§E.26).
    machine: Option<MachineIn>,
    /// Rings taken so far (each set's watcher tagged with its number).
    machine_epoch: u64,
    last_via: &'static str,
    color: Option<crate::ext::ColorInfo>,
    /// The source has shown more than 8 bits or an HDR transfer: with
    /// [`InputConfig::precise`] its pictures are decoded precisely (samples), others still
    /// as coefficients where asked (R.24 stage 2: an HDR switcher keeps ordinary cameras on
    /// the GPU).
    hdr_seen: bool,
    /// The source is in `precise_hdr`'s transfer.
    hdr_match: bool,
    /// A background resolution is in progress.
    resolving: bool,
    /// The last first field was skipped as late: skip its second field too.
    skip_second_field: bool,
    /// With [`InputConfig::picture_rate`]: the SpeedHQ pictures decoded.
    thin: Option<crate::thin::Thin>,
    /// The source announced `regions/1` (§E.15).
    regions_peer: bool,
    /// The picture region updates apply to: the last one handed over (§E.15.4).
    reference: Option<Picture>,
    /// When a whole picture was last asked for.
    refresh_asked: Option<Instant>,
    /// The session's multicast announcement, to rejoin for another quality (R1-37).
    mc_announcement: Option<String>,
    /// Encrypted multicast keys from this session's link (§E.17.2).
    mc_keys: Option<Arc<crate::mcast::Keyring>>,
    /// Encrypted multicast messages put back in order, per stream (§E.17.6).
    mc_seq: [crate::mcast::Sequencer<Frame>; crate::mcast::STREAMS],
    /// The open AAC decoder and its AudioSpecificConfig (§E.19.2).
    aac: Option<(Vec<u8>, Box<dyn crate::codec::AudioDecoder>)>,
    /// The open video decoder, if compressed video is being decoded (§E.18.4).
    decoder: Option<(Codec, Box<dyn crate::codec::VideoDecoder>)>,
    /// The IDN link (§E.11), when [`Transport::Link`] is on.
    link_cfg: Option<crate::link::LinkConfig>,
    /// Whether the source's discovery hint offers the link (`None`: unknown).
    link_hint: Option<bool>,
    /// Addresses whose link attempt failed, and when: TCP straight away for a while.
    link_failed: BTreeMap<SocketAddr, Instant>,
    /// Senders whose link timed out once while TCP worked: a second time within the
    /// memory period, and UDP is taken as blocked (`link_failed`).
    link_suspect: BTreeMap<SocketAddr, Instant>,
    /// The current session's link, if it runs over one.
    link: Option<crate::link::LinkClient>,
    /// Mode open: the source's link key, checked once it announces its name.
    link_peer_key: Option<String>,
    /// When a running TCP session last tried to move to a link (§E.11.3).
    last_upgrade: Option<Instant>,
    /// After moving to a link: the TCP session to close, and when at the latest.
    retiring: Option<(u32, Instant)>,
    /// Frame-accurate presentation (§E.14), when on.
    sync: Option<crate::sync::Receiver>,
    /// Per-frame streams: the link's late count already added to the statistics.
    link_late_seen: u64,
    /// A link attempt still running after TCP won the race.
    link_probe: Option<(SocketAddr, mpsc::Receiver<Attempt>)>,
    /// `ndi_enabled_streams` for a link session (§E.11.5).
    link_streams_xml: String,
    /// The audio channels this session's stream request asked for (§E.21.1): a change
    /// made before the sender's extensions were known is sent once they are.
    channels_requested: Option<Vec<u16>>,
    /// The same for the picture size (§E.16.2): a switcher asking for full size the moment
    /// it adds an input, as the session starts.
    size_requested: Option<(u32, u32)>,
    /// Live status on the Discovery Server's monitoring channel (§E.5.1).
    monitor: Option<crate::discovery::ds::Advertiser>,
    monitor_at: Instant,
    monitor_video: usize,
    last_video: Option<VideoHeader>,
    last_audio: Option<AudioHeader>,
    /// Site enrolment seen; a new enrolment re-opens a session not yet using it.
    site_seen_enrolled: bool,
    /// When the current session started, and how many sessions in a row ended within a
    /// second (a refusal): those back off instead of retrying at once (lab X-t4/X-t7).
    session_started: Instant,
    /// Whether the current session delivered any video or audio.
    session_media: bool,
    /// The last picture handed over: its timestamp and the way it came. A picture from
    /// another way that is older goes (switching to this machine's ring, a picture still on
    /// its way over the connection came after a newer one from the ring).
    delivered: Option<(i64, &'static str)>,
    quick_drops: u32,
}

// Inputs are shared between threads by applications.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Input>();
};

#[cfg(test)]
mod tests;
