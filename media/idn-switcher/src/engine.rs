//! The switcher engine (spec/extensions/idn-switcher.md): inputs, scenes of layers (global
//! layers in them), buses that take scenes, transitions, fade to black, the stream's sound,
//! and the program out, on its own frame clock.

use crate::compose::{self, Kind};
use crate::frame::{Frame, Ycc};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig,
};
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod frame_clock;
mod hdr_sender;
mod inputs;
mod mixes;
mod mixing;
mod program_sound;
mod slides;
mod tallies;
mod titling;
mod triggering;
mod views;
use frame_clock::*;
use inputs::*;
use mixes::*;
use mixing::*;
use program_sound::*;
use slides::*;
use tallies::*;
use titling::*;
use triggering::*;
use views::*;

mod api_state;
mod start;

mod api_inputs;
mod api_mixes;
mod api_monitor;
mod api_program;
mod api_titles;
use api_program::layer_visible;

mod impls;
mod support;
mod types;
// (For the GPU tests; the engine calls it inside support.)
#[cfg(all(feature = "gpu", test))]
pub(crate) use support::gpu_from;
#[cfg(feature = "gpu")]
use support::make_gpu;
use support::{
    bump, complete, complete_mix, data_row, effective_scenes, fed_output_config, filter_time,
    filtered, gpu_pictures, heard_lately, multiview_rate, output_config, redraw_title, resolve_bus,
    row_value, run_event, shown, slide_action_name, stinger_of, to_planes,
};
pub use types::{
    AudioSource, PictureLoader, SlideAction, Slideshow, Source, Stinger, TimerAction, TimerMode,
    Trigger, TriggerAction, TriggerEvent, View,
};

/// Pictures in a slideshow, at most.
pub const MAX_SLIDES: usize = 1000;
/// Global layers (§S.5), at most.
pub const MAX_GLOBAL_LAYERS: usize = 16;
/// The rate when nothing says otherwise.
pub const DEFAULT_RATE: u32 = 48_000;

static RATE_HZ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(DEFAULT_RATE);

/// The rate the switcher's sound runs at (44.1, 48 or 96 kHz; `idn-node.md` §N.40): set
/// once at its start ([`set_rate`]). Its streams still go out at 48 kHz (converted by
/// their encoders).
pub fn rate() -> u32 {
    RATE_HZ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets the switcher's rate (one of `node::rate::RATES`; others are left as they were).
pub fn set_rate(hz: u32) {
    if idn_open_protocol::node::rate::valid(hz) {
        RATE_HZ.store(hz, std::sync::atomic::Ordering::Relaxed);
    }
}
/// The program sound's buffer against network timing, ms (plus the user's delay): the
/// low-latency mode's, so that sound leaves with the picture, which comes straight from
/// its input (at 60 ms the sound was a frame late, at 30 still
/// 20 ms). Its lanes widen it by themselves where the network needs more (2 ms per
/// underrun).
const SOUND_BUFFER_MS: u32 = 10;

/// How to start.
#[derive(Clone)]
pub struct SwitcherConfig {
    /// The program's source name (`HOST (name)` on the network). Default "Switcher".
    pub name: String,
    /// The program format (§S.2): 1920 × 1080 at 30/1 by default.
    pub width: usize,
    pub height: usize,
    pub fps: (u32, u32),
    /// Also send the preview bus, as `name (Preview)`.
    pub preview_output: bool,
    /// Send the multiview from the start (§S.7), as `name (Multiview)`, 1920 × 1080 at
    /// about 15 fps; [`Switcher::multiview_output`] turns it on and off later.
    pub multiview_output: bool,
    /// How long an input no longer in use keeps receiving before it disconnects (§S.3):
    /// a scene taken off preview and put back soon doesn't reconnect its cameras.
    pub idle_after: Duration,
    /// Encryption with IDN sources and receivers (the machine's Node's, usually).
    pub secure: Option<idn_open_protocol::secure::SecureConfig>,
    /// Announce on mDNS (default on; off in tests).
    pub mdns: Option<bool>,
    /// Find sources (and announce) through this Discovery Server.
    pub discovery_server: Option<String>,
    /// A decoder for compressed video (NDI|HX, `codec/1`), from `idn-ffmpeg`.
    pub codec: Option<idn_open_protocol::codec::CodecInput>,
    /// The program's and mixes' compressed video for IDN receivers, when one format is
    /// forced (the machine's video settings, §N.13); `None`: SpeedHQ, as always.
    pub out_codec: Option<idn_open_protocol::codec::CodecOutput>,
    /// The machine's IDN Node: the switcher then offers itself as a 2-channel routable
    /// receiver named like the program (R.14), for [`AudioSource::Routed`].
    pub node: Option<Arc<idn_open_protocol::node::Node>>,
    /// Composite on the GPU when there is one (§S.9, stage 3: 2160p60); off, or without
    /// one, on the CPU. Default on.
    pub gpu: bool,
    /// A wgpu device and queue to composite on instead of opening one (feature `gpu`). Set
    /// it to keep a single wgpu device in the process when the host already has one; the
    /// device must meet the compositor's limits (see [`crate::gpu::Gpu::with_device`]).
    /// `None` opens the best GPU, as `gpu` alone does.
    #[cfg(feature = "gpu")]
    pub gpu_device: Option<(wgpu::Device, wgpu::Queue)>,
    /// The program in HDR (roadmap R.24 stage 1): `Pq` or `Hlg`, BT.2100, 16-bit with its
    /// colour described; SDR inputs and graphics mapped in as BT.2408 does (white at 203
    /// nits). `Sdr` (the default): BT.709 as always. The preview, the multiview, the
    /// monitor and the mixes stay SDR.
    pub hdr: idn_open_protocol::color::Transfer,
}

/// A virtual shot's move under way.
#[derive(Clone, Copy)]
struct ViewRun {
    from: View,
    to: View,
    start: Instant,
    secs: f32,
}

/// A camera's control (§S.21) and the preset it was last sent to.
struct Camera {
    control: crate::ptz::Control,
    visca: Option<Arc<crate::ptz::ViscaLink>>,
    last_preset: Option<u8>,
    error: Option<String>,
}

/// A slideshow's place, shared with its worker.
#[derive(Default)]
struct SlideCtl {
    run: Mutex<SlideRun>,
    wake: Condvar,
}

#[derive(Default)]
struct SlideRun {
    /// The place in `order` shown (or being moved to).
    index: usize,
    /// The pictures' order: the list's, or shuffled.
    order: Vec<usize>,
    playing: bool,
    /// The last picture of a slideshow that doesn't loop was reached while playing.
    ended: bool,
    error: Option<String>,
}

/// A title's data as read (§S.20): its rows and the one showing.
struct DataRun {
    source: crate::data::Source,
    rows: Vec<crate::data::Row>,
    row: usize,
    /// Moving on by itself (its `advance`), unless paused.
    advancing: bool,
    error: Option<String>,
    /// When it was last read (none yet: read soon), and when the row last changed.
    read_at: Option<Instant>,
    moved_at: Instant,
}

/// A timer's count.
struct Timer {
    mode: TimerMode,
    since: Option<Instant>,
    banked: Duration,
    ended: bool,
}

/// Stinger slots.
pub const STINGERS: usize = 8;

/// Mixes besides the program, at most (§S.15).
pub const MIXES: usize = 4;
/// The routable receiver's channels (§S.26): the program's pair, then a pair for each mix
/// slot.
pub const FEED_CHANNELS: usize = 2 + 2 * MIXES;

/// An independent mix's buses and transition (§S.15); its output is in `Shared`.
#[derive(Debug, Clone)]
struct MixState {
    name: String,
    /// Its place on the routable receiver (§S.26): channels `2 + 2 × slot` and the next.
    slot: usize,
    width: usize,
    height: usize,
    program: Option<u32>,
    preview: Option<u32>,
    kind: Kind,
    duration: f32,
    position: f32,
    running: bool,
    /// It follows the program (its scenes and transitions), its pictures the program's
    /// centre-cropped to its shape: a vertical version of the program ("also make a
    /// vertical version"). Its own buses are not set.
    follow: bool,
}

/// One program frame's hand-off to an ISO tap: the input's picture, its sound since the
/// last (and the rate), and the program frame's timecode and timestamp.
type IsoItem = (Arc<Frame>, Vec<Vec<f32>>, u32, i64, i64);

/// Keeps a [`Switcher::tap_input`] running; drop it to stop (its worker finishes the
/// pictures it holds, then ends).
pub struct InputTap {
    shared: std::sync::Weak<Shared>,
    input: u32,
    id: u64,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// One sound-track recording's registration: a mixer per chosen input (its own sound
/// through a lane at the program sound's latency) and the channel to its worker.
struct TracksReg {
    id: u64,
    mixers: Vec<Arc<Mixer>>,
    tx: std::sync::mpsc::SyncSender<idn_open_protocol::api::TappedFrame>,
}

/// Keeps a [`Switcher::tap_sound_tracks`] running; drop it to stop.
pub struct TracksTap {
    shared: std::sync::Weak<Shared>,
    id: u64,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// A scene layer's fade under way (§S.12): from one level to the other over `secs`.
#[derive(Debug, Clone, Copy)]
struct LayerFade {
    from: f32,
    to: f32,
    start: Instant,
    secs: f32,
}

#[derive(Debug, Clone)]
struct State {
    version: u64,
    program: Option<u32>,
    preview: Option<u32>,
    kind: Kind,
    duration: f32,
    position: f32,
    running: bool,
    ftb: bool,
    ftb_level: f32,
    audio: AudioSource,
    gain_db: f32,
    delay_ms: u32,
    /// Noise suppression on the stream's sound (`idn_dsp::denoise`).
    audio_denoise: bool,
    /// The program takes a desk's feed by itself while one is routed to its pair (an
    /// option; mixes always do), whatever its own source says.
    take_feed: bool,
    triggers: Vec<Trigger>,
    /// Stinger slots 1–8 (§S.14).
    stingers: Vec<Option<Stinger>>,
    /// A stinger under way has passed its cut point (program and preview swapped).
    cut_done: bool,
    /// The independent mixes, by id (§S.15).
    mixes: BTreeMap<u32, MixState>,
    /// What the operator hears (§S.24).
    monitor: Monitor,
}

/// The switcher's monitor (§S.24): what the operator hears, as streamed. The stream's
/// sound, or preview's input's sound, or the soloed inputs' instead of either.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Preview's input's sound instead of the stream's.
    pub preview: bool,
    /// Inputs heard instead (summed), by id; empty: none soloed.
    pub solo: Vec<u32>,
    /// The monitor's volume, dB (−60…+12).
    pub volume_db: f32,
    /// The monitor silent (the stream untouched).
    pub mute: bool,
    /// On a sound card of this machine: its device (`Some("")` the default); `None`: none.
    pub card: Option<String>,
}

/// A peak meter: the loudest sample of each side over the last 150–300 ms.
#[derive(Default)]
struct Meter {
    now: [f32; 2],
    before: [f32; 2],
    since: Option<Instant>,
}

/// What a switcher output with sound announces in discovery (spec/extensions/
/// idn-switcher.md §S.25), so a mixing desk lists it by itself.
pub const ROLE: &str = "switcher/1";

/// What a switcher output that takes its own sound from a desk announces (§S.26).
pub const FEED_ROLE: &str = "switcher-feed/1";

/// How long the browser's Listen keeps the monitor's sound coming after its last ask.
const LISTEN_KEEP: Duration = Duration::from_secs(3);
/// The monitor's sound kept for Listen, frames (2 s).
fn listen_ring() -> usize {
    rate() as usize * 2
}

/// The monitor's sound and meters, made each frame.
#[derive(Default)]
struct MonitorOut {
    stream: Meter,
    monitor: Meter,
    /// For the browser's Listen: interleaved 16-bit stereo, the newest [`listen_ring`]
    /// frames, and the frame count at its end; when Listen last asked.
    ring: std::collections::VecDeque<i16>,
    end: u64,
    listened: Option<Instant>,
    /// When a client last read the meters: the monitor runs for its meter meanwhile.
    metered: Option<Instant>,
}

/// An input's picture fitted to its multiview tile: (picture count, tile size, picture).
type TileCache = (u64, (usize, usize), Arc<Frame>);

/// What a source was last told: on program, on preview, the size asked for (`None`: full).
type Told = (bool, bool, Option<(u32, u32)>);

/// An ISO recording's tap: its id, where its pictures go, the sound it couldn't take yet.
type IsoTap = (u64, std::sync::mpsc::SyncSender<IsoItem>, Vec<Vec<f32>>);

/// A desk on the network: the connection its feeds come on, and when it was last heard.
type DeskLink = (Arc<idn_open_protocol::input::Input>, Option<Instant>);

/// One input.
struct Slot {
    id: u32,
    label: Mutex<String>,
    source: Source,
    input: Mutex<Option<Arc<Input>>>,
    /// The newest picture as shown (through its filters), with a count of pictures.
    latest: Mutex<Option<Arc<Frame>>>,
    /// The newest picture as it came, and when (filtered again when the filters change,
    /// and each frame for a scroll on a still).
    raw: Mutex<Option<(Arc<Frame>, Instant)>>,
    /// Its filters (§S.17), when it has any on.
    filters: Mutex<Option<Arc<crate::filter::Chain>>>,
    /// The source says it takes PTZ commands (its `ndi_capabilities`).
    ptz_capable: AtomicBool,
    /// How its pictures are decoded (lab 521: nothing showed it): 0 not yet, 1 on the
    /// CPU, 2 kept on the GPU (decoded there), 3 coefficients reconstructed on the GPU.
    decode_path: std::sync::atomic::AtomicU8,
    /// The receiver's last error and when (lab 521-A: an 8K picture was refused with
    /// nothing said), in the state for 30 s.
    last_error: Mutex<Option<(Instant, String)>>,
    /// Why the source was lost, while it is (cleared when it connects again): the state's
    /// `lost`, the service's typed events (§S.37).
    lost: Mutex<Option<String>>,
    seq: AtomicU64,
    /// Pictures a second arriving (×100), measured by the frame clock: an input decoding
    /// slower than it is sent shows here.
    fps: std::sync::atomic::AtomicU32,
    /// The newest picture fitted to the program size, the count it was made from and its
    /// timestamp.
    fitted: Mutex<Option<(u64, Arc<Frame>, i64)>>,
    connected: AtomicBool,
    /// Pictures arrive in this session (cleared at each connection change).
    running: AtomicBool,
    /// Its sound into the program, while the stream hears it, and at what level (an
    /// `f32`'s bits: 1 alone, its items' level in a scene, §S.12).
    lane: Mutex<Option<Lane>>,
    gain: std::sync::atomic::AtomicU32,
    /// The newest picture's timestamp, and the time just past the last sound block
    /// pushed into the lane (the source's, 100 ns): the program's sound is stamped from
    /// them (locked after `lane`).
    picture_ts: std::sync::atomic::AtomicI64,
    audio_tail: Mutex<Option<i64>>,
    /// The timestamp of the picture the frame being made took from this input (0: none
    /// taken this frame): the sound is stamped against the picture shown, not one that
    /// arrived while the frame was made (after a media restart moved the
    /// pictures' arrival onto the frame clock's tick, the sound was stamped a frame early
    /// for the rest of the run, 17-38 ms).
    shown_ts: std::sync::atomic::AtomicI64,
    /// (on program, on preview, the size asked for: `None` full) as last told to the
    /// source.
    told: Mutex<Option<Told>>,
    /// A network input's connection settings (opened again when it comes into use, §S.3),
    /// and the connection while it is open.
    input_cfg: Option<InputConfig>,
    /// Kept receiving while not in use (§S.3).
    keep_active: AtomicBool,
    /// A scene: the desk snapshot recalled when it goes to program (desk source name,
    /// snapshot name; "recall a desk snapshot when live").
    on_air_snapshot: Mutex<Option<(String, String)>>,
    /// When it was last in use (the frame clock marks it each frame it is): it receives
    /// until [`SwitcherConfig::idle_after`] past that.
    used_at: Mutex<Option<Instant>>,
    /// It receives now (a network input: its connection is open).
    active: AtomicBool,
    /// Its newest picture shrunk for the multiview's scene tiles, for the count and size
    /// it was made for.
    tile_small: Mutex<Option<TileCache>>,
    /// The newest picture fitted to its multiview tile, for the count and size it was
    /// made for.
    tile: Mutex<Option<TileCache>>,
    /// Its sound's sync offset (ms, 0–500): added to the stream sound's delay while its
    /// sound is the one used, to put it back with its picture.
    audio_offset_ms: std::sync::atomic::AtomicU32,
    /// ISO recording: the taps its pictures go to each program frame ([`Switcher::tap_input`]),
    /// and its own sound since the last frame (with its rate).
    /// Each tap with the sound it couldn't take yet (its queue was full), sent with its
    /// next picture.
    iso: Mutex<Vec<IsoTap>>,
    iso_audio: Mutex<Vec<Vec<f32>>>,
    iso_rate: std::sync::atomic::AtomicU32,
    /// Its sound into sound-track recordings ([`Switcher::tap_sound_tracks`]): per tap, a
    /// lane on that track's mixer and the latency it was last set to.
    track_lanes: Mutex<Vec<(u64, Lane, u32)>>,
    /// Its sound into the monitor while soloed or on preview (§S.24), with its latency.
    monitor_lane: Mutex<Option<(Lane, u32)>>,
    /// Its sound into the Preview output while it is on preview (§S.25).
    preview_lane: Mutex<Option<(Lane, u32)>>,
    /// Its own sound's level.
    meter: Mutex<Meter>,
    stop: AtomicBool,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Told when a stinger starts (its slot, 1–8).
type StingerHook = Arc<dyn Fn(u32) + Send + Sync>;

struct Shared {
    cfg: SwitcherConfig,
    state: Mutex<State>,
    changed: Condvar,
    inputs: Mutex<BTreeMap<u32, Arc<Slot>>>,
    next_id: AtomicU64,
    program_out: Arc<Output>,
    /// An HDR program's sender (its pictures made HDR off the frame clock); `None`: SDR.
    program_hdr: Option<hdr_sender::HdrSender>,
    preview_out: Option<Output>,
    /// The multiview's output while it is on (§S.7).
    multiview_out: Mutex<Option<Arc<Output>>>,
    /// The multiview's layout; `None`: the default (§S.7).
    multiview_layout: Mutex<Option<Vec<crate::multiview::TileSpec>>>,
    /// The newest multiview, its count, and until when someone looks at it (an API client).
    multiview: Mutex<(u64, Option<Arc<Frame>>)>,
    multiview_wanted: Mutex<Instant>,
    /// Scenes whose thumbnail an API client asked for, and until when: their inputs are in
    /// use (at a thumbnail's size) meanwhile.
    thumbs_wanted: Mutex<BTreeMap<u32, Instant>>,
    /// The JPEG of it for API clients, for the count it was made from.
    multiview_jpeg: Mutex<Option<(u64, Arc<Vec<u8>>)>>,
    mixer: Arc<Mixer>,
    /// Sound-track recordings: per tap, a mixer per chosen input and where its frames go.
    tracks: Mutex<Vec<TracksReg>>,
    /// Scenes' layers, by the scene input's id (§S.12).
    scenes: Mutex<BTreeMap<u32, Vec<crate::scene::Item>>>,
    /// The global layers' definitions, by id (§S.5), and the next id.
    globals: Mutex<BTreeMap<u32, crate::scene::Item>>,
    next_global: AtomicU64,
    /// Scene layers fading in or out, by (scene, layer id).
    layer_fades: Mutex<BTreeMap<(u32, u32), LayerFade>>,
    /// Titles' layers, by the title input's id (§S.16).
    titles: Mutex<BTreeMap<u32, crate::title::Title>>,
    /// Each title's texts as last drawn (drawn again when a clock or timer changes them).
    title_texts: Mutex<BTreeMap<u32, Vec<String>>>,
    /// Pictures titles show, read once (§S.19).
    title_pictures: Mutex<BTreeMap<String, Arc<Frame>>>,
    /// Timers, by name (§S.19).
    timers: Mutex<BTreeMap<String, Timer>>,
    /// Titles' data, by the title's id (§S.20).
    title_data: Mutex<BTreeMap<u32, DataRun>>,
    /// Cameras' control, by the camera input's id (§S.21).
    cameras: Mutex<BTreeMap<u32, Camera>>,
    /// Virtual shots' moves, by input id.
    views: Mutex<BTreeMap<u32, ViewRun>>,
    /// The compositor's frame clock: when its frame 0 was due, and the period. Frame k is
    /// due at start + k × period.
    frame_clock: Mutex<Option<(Instant, Duration)>>,
    /// Virtual shots are drawn by the compositor's scene drawer on the GPU, each frame
    /// (their workers make no pictures then).
    views_on_gpu: AtomicBool,
    /// The GPU compositor, made at the start (so inputs opened from then on can keep
    /// their pictures on its GPU: §E.27) and taken by the frame clock; and its GPU.
    #[cfg(feature = "gpu")]
    gpu_made: Mutex<Option<Result<crate::gpu::Gpu, String>>>,
    #[cfg(feature = "gpu")]
    gpu_hand: Mutex<Option<idn_open_protocol::gpu::GpuHandIn>>,
    /// The first compositor's device, for one made again (the frame clock restarted after
    /// a panic, lab 452(b)): the inputs' GPU pictures are on it. `Err`: no GPU (none at
    /// the start, or it failed and was given up): a restarted clock stays on the CPU rather
    /// than open a device the inputs' pictures aren't on.
    #[cfg(feature = "gpu")]
    gpu_seed: Mutex<Option<Result<crate::gpu::GpuSeed, String>>>,
    /// The frame clock's runs so far (one more after each restart): its scene worker
    /// stops when a later run starts its own.
    clock_runs: std::sync::atomic::AtomicU64,
    /// The mixes (§S.15) are drawn by the scene drawer on the GPU, virtual shots cropped
    /// there from their cameras too.
    mixes_on_gpu: AtomicBool,
    /// An HDR program's last picture: 1 composed in HDR on the GPU, 2 made HDR on the CPU
    /// (§S.34); 0 none yet.
    hdr_path: std::sync::atomic::AtomicU8,
    /// The fonts, loaded with the first title.
    fonts: Mutex<Option<crate::title::Fonts>>,
    /// Slideshows' places, by input id.
    slides: Mutex<BTreeMap<u32, Arc<SlideCtl>>>,
    loader: Mutex<Option<PictureLoader>>,
    /// Called with a stinger's clip input as the stinger starts (to play a file from its
    /// start), see [`Switcher::on_stinger`].
    on_stinger: Mutex<Option<StingerHook>>,
    /// The announcement every output is made on (the mixes' too, added later).
    publisher: Publisher,
    /// Each mix's output, by the mix's id (§S.15).
    /// (Behind Arcs: the frame clock takes a list and sends without the lock, which
    /// displays and the API wait on.)
    mix_outs: Mutex<BTreeMap<u32, Arc<Output>>>,
    next_mix: AtomicU64,
    stop: AtomicBool,
    /// Routed sound: a lane per transmitter while it is the audio source.
    routed: Arc<Mutex<BTreeMap<String, Lane>>>,
    routed_on: Arc<AtomicBool>,
    /// The stream's delay (ms) its routed lanes are made with: with one, they hold it
    /// rather than adapting (an adapting lane came back down from it, a minute at a time).
    routed_delay: Arc<std::sync::atomic::AtomicU32>,
    router: Mutex<Option<idn_open_protocol::node::routing::Router>>,
    /// The routable receiver (its routes say which mixes have a feed of their own), and
    /// its sound: the program's pair, then each mix slot's (§S.26).
    receiver: Mutex<Option<Arc<idn_open_protocol::node::routing::AudioReceiver>>>,
    routed_mixer: Arc<Mixer>,
    /// The desks' feeds as last applied, by this switcher's output (§S.26).
    feeds: Mutex<BTreeMap<String, Feed>>,
    /// The desks on the network (`desk/1`), by name: the connection the feeds come on,
    /// which also carries a cue's scene to the desk (§S.27), and when it was last heard.
    desks: Mutex<BTreeMap<String, DeskLink>>,
    /// The text-only sources on the network (IDN Data's feeds, `text/1`), by name.
    text_sources: Mutex<Vec<String>>,
    /// The other sources on the network (cameras, computers, desks), by name: the
    /// choices for a network input.
    other_sources: Mutex<Vec<String>>,
    /// Every source listed on the network with its address (`IP:port`, "" unknown), and
    /// whether the list has been read yet: a network input's source gone or renamed
    /// (§S.37).
    listed: Mutex<Option<Vec<(String, String)>>>,
    /// Closed captions on the program (§S.29).
    captions: Mutex<Captions>,
    /// The sound card, while it is the sound.
    #[cfg(feature = "card")]
    card: Mutex<Option<crate::card::CardIn>>,
    /// The monitor (§S.24): the soloed or preview inputs' lanes, its sound and meters,
    /// told to Listen as it comes, and its sound card.
    monitor_mixer: Arc<Mixer>,
    /// The monitor as a source with sound only (§S.25).
    monitor_net: Output,
    /// Preview's input's sound, for the Preview output (§S.25).
    preview_mixer: Arc<Mixer>,
    monitor_out: Mutex<MonitorOut>,
    monitor_ready: Condvar,
    #[cfg(feature = "card")]
    monitor_card: Mutex<Option<crate::card::CardOut>>,
    /// What composites: "GPU: name" or "CPU" (with why not the GPU).
    compositor: Mutex<String>,
    /// The last frame's compositing time, µs, and frames late.
    frame_us: AtomicU64,
    /// Its parts, µs: composing, the multiview, sending.
    parts: [AtomicU64; 3],
    /// On the GPU, composing's parts, µs: writing the pictures up, waiting for the GPU,
    /// reading the program back.
    gpu_parts: [AtomicU64; 3],
    /// Scenes drawn on the GPU this frame, µs, summed: making their operations and writing
    /// their pictures up, waiting for the GPU, reading them back.
    scene_parts: [AtomicU64; 3],
    /// The inputs the stream hears now and at what level, loudest first (§S.6, §S.12).
    heard: Mutex<Vec<(u32, f32)>>,
    late: AtomicU64,
    /// The frame clock's frames (marked each one) and its restarts after a panic: the
    /// status's `clock_alive` and `clock_restarts`.
    frame_beat: idn_common::heartbeat::Heartbeat,
    /// The worst frame time and the worst lateness of a frame's start in the last full
    /// 10 s, µs (for finding stalls), and the window being measured.
    worst: Mutex<Worst>,
}

/// Worst frame times, by 10 s windows.
#[derive(Default)]
struct Worst {
    since: Option<Instant>,
    frame: u64,
    stall: u64,
    /// The last full window's.
    last: (u64, u64),
}

/// The switcher; stops when dropped.
pub struct Switcher {
    shared: Arc<Shared>,
    _publisher: Publisher,
    clock: Option<std::thread::JoinHandle<()>>,
    triggers: Option<std::thread::JoinHandle<()>>,
    /// Takes desks' feeds (§S.26), with a Node.
    desks: Option<std::thread::JoinHandle<()>>,
    filters: Option<std::thread::JoinHandle<()>>,
    titles: Option<std::thread::JoinHandle<()>>,
    data: Option<std::thread::JoinHandle<()>>,
}

impl Switcher {
    /// The program output (to attach streams and recordings, `idn-ffmpeg`'s gateway).
    /// The GPU compositor's GPU (its wgpu device), when there is one: for what plays into
    /// the switcher to keep its pictures there (file players, §E.27).
    #[cfg(feature = "gpu")]
    pub fn gpu(&self) -> Option<idn_open_protocol::gpu::GpuHandIn> {
        self.shared.gpu_hand.lock_ok().clone()
    }

    /// How slideshows read their pictures (set it before adding one; without it a
    /// slideshow shows its error).
    pub fn set_picture_loader(&self, loader: PictureLoader) {
        *self.shared.loader.lock_ok() = Some(loader);
    }

    /// Called with a stinger's clip input as each stinger starts — a program plays a file
    /// clip from its start there (the engine doesn't own file players).
    pub fn on_stinger(&self, f: impl Fn(u32) + Send + Sync + 'static) {
        *self.shared.on_stinger.lock_ok() = Some(Arc::new(f));
    }

    /// The state's version: it grows with every change.
    pub fn version(&self) -> u64 {
        self.shared.state.lock_ok().version
    }
}

impl Drop for Switcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the triggers watcher so it sees `stop` promptly.
        self.shared.changed.notify_all();
        if let Some(t) = self.clock.take() {
            let _ = t.join();
        }
        if let Some(t) = self.triggers.take() {
            let _ = t.join();
        }
        if let Some(t) = self.desks.take() {
            let _ = t.join();
        }
        if let Some(t) = self.filters.take() {
            let _ = t.join();
        }
        if let Some(t) = self.titles.take() {
            let _ = t.join();
        }
        if let Some(t) = self.data.take() {
            let _ = t.join();
        }
        // Not under the inputs' lock: a delay input's, a preset input's or a shot's worker
        // takes it each turn, and would wait for it while this waited for the worker.
        let slots: Vec<Arc<Slot>> = self.shared.inputs.lock_ok().values().cloned().collect();
        for s in &slots {
            s.stop.store(true, Ordering::Relaxed);
        }
        for s in slots {
            let t = s.thread.lock_ok().take();
            if let Some(t) = t {
                let _ = t.join();
            }
        }
    }
}

/// The newest picture received and not yet made the input's (with an HDR source's
/// full-precision planes, R.24), and whether receiving ended.
#[allow(clippy::type_complexity)]
type Mailbox = (
    Mutex<(
        Option<(
            idn_open_protocol::shq::Picture,
            i64,
            Option<Arc<crate::frame::Hdr16>>,
        )>,
        bool,
    )>,
    Condvar,
);

/// Closed captions on the program (§S.29).
#[derive(Debug)]
struct Captions {
    on: bool,
    /// A captions source whose final lines become these captions (G2: "HOST (Captions)").
    from: Option<String>,
    /// Seconds with no new line before the captions clear (0: never).
    clear_after: f64,
    captioner: idn_open_protocol::cea608::Captioner,
    /// The last lines said, newest last.
    recent: std::collections::VecDeque<String>,
}

/// A desk's feed for one of this switcher's outputs (§S.26): the desk's source and its
/// two channels (from 0) that carry the mix for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub desk: String,
    pub ch: [u16; 2],
}

/// An input's lane into a mixer, with the latency it was made for.
type InputLane = Mutex<Option<(Lane, u32)>>;

/// The frame clock: every frame period, advance what moves, composite, send.
/// The multiview's preview tile: (input, its picture's number, tile size, the tile).
type PreviewTile = (u32, u64, (usize, usize), Arc<Frame>);

/// The program on the CPU (§S.9 stage 1): the program bus with the transition under way,
/// a stinger's clip over it, fade to black.
/// A frame the CPU composed: the program bus's own picture untouched (shared: its planes
/// are made once per picture, not copied every frame), or a new one.
enum Composed {
    Shared(Arc<Frame>),
    Owned(Frame),
}

/// A picture of a mix drawn on the GPU: with a crop (x0, y0, x1, y1 of it) a virtual
/// shot, fitted to the mix as a picture of the given shape.
#[derive(Clone)]
// (Used by the GPU path only.)
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
struct MixLayer {
    pic: Option<Arc<Frame>>,
    opacity: f32,
    crop: Option<Crop>,
}

/// A virtual shot's crop: its rectangle (x0, y0, x1, y1 of the picture) and the shape
/// it is fitted as.
type Crop = ((f32, f32, f32, f32), (usize, usize));

/// Draws scenes: on the GPU when the compositor has one (§S.12 stage 1e), else — or once
/// that fails — on the CPU.
#[derive(Default)]
struct SceneDrawer {
    #[cfg(feature = "gpu")]
    gpu: Option<crate::gpu_scene::SceneGpu>,
    /// Why the GPU stopped drawing them, until the state is told.
    failed: Option<String>,
    /// The GPU's parts of the scenes drawn since this was zeroed, ms.
    timing: [f64; 3],
    /// The next mix's picture kept on the GPU ([`layers`](Self::layers)): for an output
    /// that sends it from there (§E.27). Taken by the drawing it is for.
    keep_mix: bool,
    /// The scene being drawn kept so (set from `keep_mix` for one drawing).
    // (Used by the GPU path only.)
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    keep: bool,
    /// Opaque scenes left on the GPU (the compositor takes them there; what needs their
    /// samples reads them back): set by the scene worker while the compositor is on the
    /// GPU. A 2160p scene read back each frame and uploaded again was two copies of 12 MB
    /// for nothing (lab 350's rig).
    // (Used by the GPU path only.)
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    keep_scenes: bool,
}

#[cfg(test)]
mod tests;
