//! Gateways out (R.12): an output's pictures and sound streamed live to platforms
//! (RTMP(S), SRT) and recorded to files (MP4, MKV), with the GPU.
//!
//! Destinations with the same settings share one encode; a
//! destination can have an encoder of its own instead. Each destination connects,
//! writes and reconnects on its own thread, so a slow or dead server never holds up the
//! others or the output.
//!
//! ```no_run
//! # fn run(out: &idn_open_protocol::api::Output) -> std::io::Result<()> {
//! use idn_ffmpeg::gateway::{Container, Destination, Gateway};
//! use std::sync::Arc;
//!
//! let gw = Arc::new(Gateway::new(Arc::new(idn_ffmpeg::Ffmpeg::probe())));
//! gw.add(Destination::Rtmp("rtmp://a.rtmp.youtube.com/live2/KEY".into()))?;
//! gw.add(Destination::File("/srv/recordings/service.mp4".into(), Container::Mp4))?;
//! let _tap = gw.attach(out); // keep it: dropping it stops the feed
//! # Ok(()) }
//! ```

use crate::Ffmpeg;
use crate::ffi::{self, Frame, Params, Scaler};
use crate::mux::{AacEncoder, Muxer, StreamSpec, redact};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{Output, Tap, TappedFrame};
use idn_open_protocol::codec::{Codec, EncoderConfig, EncoderFactory, VideoEncoder};
use std::collections::VecDeque;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod adapt;
mod dest;
mod group;

/// Where a gateway sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A streaming platform or server: `rtmp://` or `rtmps://` URL with the stream key
    /// (YouTube, Facebook, Vimeo, Restream, any RTMP server).
    Rtmp(String),
    /// An SRT listener: `srt://host:port?…` (streamid, passphrase, latency as the
    /// platform says). Sent as MPEG-TS.
    Srt(String),
    /// A recording.
    File(PathBuf, Container),
}

/// The file format of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// Fragmented MP4: opens in every editor; a crash loses at most the last fragment.
    Mp4,
    /// Matroska.
    Mkv,
}

impl Destination {
    fn format(&self) -> &'static str {
        match self {
            Destination::Rtmp(_) => "flv",
            Destination::Srt(_) => "mpegts",
            Destination::File(_, Container::Mp4) => "mp4",
            Destination::File(_, Container::Mkv) => "matroska",
        }
    }

    fn url(&self) -> String {
        match self {
            Destination::Rtmp(u) | Destination::Srt(u) => u.clone(),
            Destination::File(p, _) => p.to_string_lossy().into_owned(),
        }
    }

    /// The muxer's and connection's options; `via`: the local address to send from.
    fn options(&self, via: Option<IpAddr>) -> Vec<(&'static str, String)> {
        match self {
            Destination::File(_, Container::Mp4) => {
                vec![(
                    "movflags",
                    "frag_keyframe+empty_moov+default_base_moof".into(),
                )]
            }
            Destination::Rtmp(_) => {
                let mut o = vec![("flvflags", "no_duration_filesize".into())];
                // FFmpeg's TCP (under RTMP and RTMPS) binds to it.
                if let Some(a) = via {
                    o.push(("local_addr", a.to_string()));
                }
                o
            }
            _ => Vec::new(),
        }
    }

    fn is_file(&self) -> bool {
        matches!(self, Destination::File(..))
    }

    /// For status and logs: the URL with stream keys hidden.
    pub fn describe(&self) -> String {
        redact(&self.url())
    }
}

/// How a gateway encodes for its destinations.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSettings {
    /// Video codec (H.264 by default: every platform takes it).
    pub codec: Codec,
    /// Fit the picture inside this size (keeping its shape); `None`: the output's size.
    pub size: Option<(usize, usize)>,
    /// Video bit rate in Mbit/s (default 6, a good 1080p30 platform rate).
    pub video_mbps: f64,
    /// AAC bit rate in kbit/s (default 160).
    pub audio_kbps: u32,
    /// Keyframe interval (default 2 s, what platforms ask for).
    pub keyframe_interval: Duration,
    /// Stereo AAC tracks to write (1–8; default 1). One: the sound's first two channels,
    /// as always. More: channels 1–2, 3–4, … each its own track (a missing channel is
    /// silence) — a recording's stems for editing. Streams take only the first track.
    pub audio_tracks: u8,
}

impl StreamSettings {
    /// Fills in what the site sets for this device (`options/1`, §E.20): `gateway.codec`,
    /// `gateway.video_mbps`, `gateway.audio_kbps`, `gateway.keyframe_s`. Call it first,
    /// then set the user's own values: local wins.
    pub fn apply_site(&mut self, site: &idn_open_protocol::trust::Resolved) {
        if let Some(c) = site.get("gateway.codec").and_then(Codec::from_name) {
            self.codec = c;
        }
        if let Some(m) = site.f64("gateway.video_mbps").filter(|m| *m > 0.0) {
            self.video_mbps = m;
        }
        if let Some(k) = site.f64("gateway.audio_kbps").filter(|k| *k > 0.0) {
            self.audio_kbps = k as u32;
        }
        if let Some(d) = site
            .f64("gateway.keyframe_s")
            .filter(|s| *s > 0.0)
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
        {
            self.keyframe_interval = d;
        }
    }
}

impl Default for StreamSettings {
    fn default() -> Self {
        StreamSettings {
            codec: Codec::H264,
            size: None,
            video_mbps: 6.0,
            audio_kbps: 160,
            keyframe_interval: Duration::from_secs(2),
            audio_tracks: 1,
        }
    }
}

/// How [`Gateway::add_opts`] adds a destination, besides its encode settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddOptions {
    /// An encoder of its own (else one shared by the destinations with equal settings).
    pub own_encoder: bool,
    /// A stream whose bit rate follows its connection (G4): lowered step by step while
    /// the upload can't keep up, down to this many Mbit/s, raised again after a minute of
    /// good sending, never above the settings' rate. It has an encoder of its own (a
    /// recording beside it keeps its full rate). Recordings ignore it.
    pub adaptive_floor_mbps: Option<f64>,
    /// The local address to send from (RTMP, RTMPS): a stream's own internet connection,
    /// say the backup ingest on a second line. SRT goes the system's way (FFmpeg's SRT
    /// can't choose).
    pub via: Option<IpAddr>,
}

/// A destination's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Opening the connection or file.
    Connecting,
    /// Sending (or recording).
    Live,
    /// The connection failed; trying again shortly.
    Retrying(String),
    /// A recording that couldn't be written; it won't be tried again.
    Failed(String),
    /// Removed and finished.
    Done,
}

/// One destination's status, from [`Gateway::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationStatus {
    /// The id [`Gateway::add`] returned.
    pub id: u64,
    /// The destination, stream keys hidden.
    pub target: String,
    /// What it is doing.
    pub state: State,
    /// Bytes written.
    pub bytes: u64,
    /// Times it had to connect again.
    pub reconnects: u32,
    /// Packets dropped because it was too slow (then it restarts on a keyframe).
    pub dropped: u64,
    /// Pictures its encoder skipped because it couldn't keep up (every destination of
    /// that encode misses them), and whether any were skipped in the last 10 s.
    pub skipped_pictures: u64,
    pub skipping: bool,
    /// A recording paused ([`Gateway::set_paused`]): nothing written until resumed.
    pub paused: bool,
    /// Its bit rate follows its connection ([`AddOptions::adaptive_floor_mbps`]).
    pub adaptive: bool,
    /// The video bit rate asked for, and the one encoded now (lower while adapting),
    /// kbit/s.
    pub target_kbps: u32,
    pub video_kbps: u32,
    /// What it sent over the last seconds, kbit/s (sound and pictures).
    pub sent_kbps: u32,
    /// How long the oldest packet waiting has waited, ms: over a second, the connection
    /// can't keep up.
    pub lag_ms: u32,
    /// The local address it sends from, if one was chosen.
    pub via: Option<IpAddr>,
}

/// Feeds any number of destinations from one or more shared encodes.
pub struct Gateway {
    ff: Arc<Ffmpeg>,
    groups: Arc<Mutex<Vec<Arc<Group>>>>,
    default: Mutex<StreamSettings>,
    /// Ctrl+C or a service stop closes the destinations (trailers written) before the
    /// process ends, when `shutdown::install` is in effect: a recording stopped that way
    /// lost its last second and ended mid-sample.
    _on_signal: idn_open_protocol::shutdown::Registration,
    /// The application's GPU of the output attached, looked up as pictures come: the
    /// encoders take them from there (§E.27).
    #[cfg(feature = "gpu")]
    gpu: Arc<Mutex<Option<GpuLookup>>>,
    /// The output attached sends HDR (R.24): its transfer, for 10-bit encoders.
    hdr: Arc<Mutex<Option<idn_open_protocol::color::Transfer>>>,
    /// The rate the sound came at last (0: none yet): streams and recordings share an
    /// encode at 48 kHz, and a group opens its AAC encoder at it (bug run K7-1, K7-2).
    rate: Arc<AtomicU32>,
}

#[cfg(feature = "gpu")]
type GpuLookup = Box<dyn Fn() -> Option<idn_open_protocol::gpu::GpuHandIn> + Send + Sync>;

impl Gateway {
    /// A gateway encoding with `ff`'s GPU (it has no destinations yet).
    pub fn new(ff: Arc<Ffmpeg>) -> Self {
        let groups: Arc<Mutex<Vec<Arc<Group>>>> = Arc::default();
        let on_signal = {
            let groups = groups.clone();
            // Within the shutdown hooks' time (2 s): a recording's queue and trailer take
            // well under a second; a stalled network destination is not waited for.
            idn_open_protocol::shutdown::register(move || {
                close_groups(&groups, Duration::from_millis(1500));
            })
        };
        Gateway {
            ff,
            groups,
            default: Mutex::default(),
            _on_signal: on_signal,
            #[cfg(feature = "gpu")]
            gpu: Arc::default(),
            hdr: Arc::default(),
            rate: Arc::default(),
        }
    }

    /// The program's sound rate, when it is known before its first sound (else the
    /// machine's, until sound comes).
    pub fn set_rate(&self, hz: u32) {
        self.rate.store(hz, Ordering::Relaxed);
    }

    /// The sound's rate: as it last came, else as set; `None` before either.
    fn sound_rate(&self) -> Option<u32> {
        Some(self.rate.load(Ordering::Relaxed)).filter(|&hz| hz > 0)
    }

    /// The settings [`add`](Self::add) uses.
    pub fn set_default_settings(&self, s: StreamSettings) {
        *self.default.lock_ok() = s;
    }

    /// The settings [`add`](Self::add) uses (for [`add_opts`](Self::add_opts) with them).
    pub fn default_settings(&self) -> StreamSettings {
        self.default.lock_ok().clone()
    }

    /// Adds a destination with the default settings, sharing their encode. Returns its id.
    pub fn add(&self, dest: Destination) -> io::Result<u64> {
        let s = self.default.lock_ok().clone();
        self.add_with(dest, s, false)
    }

    /// Adds a destination with its own settings. Destinations with equal settings share
    /// one encode, unless `own_encoder` (then it gets an encoder of its own).
    pub fn add_with(
        &self,
        dest: Destination,
        settings: StreamSettings,
        own_encoder: bool,
    ) -> io::Result<u64> {
        self.add_opts(
            dest,
            settings,
            AddOptions {
                own_encoder,
                ..AddOptions::default()
            },
        )
    }

    /// Adds a destination with its own settings and options (an adaptive bit rate, the
    /// connection to send from). Returns its id.
    pub fn add_opts(
        &self,
        dest: Destination,
        settings: StreamSettings,
        opts: AddOptions,
    ) -> io::Result<u64> {
        let adaptive = opts.adaptive_floor_mbps.filter(|_| !dest.is_file());
        let own_encoder = opts.own_encoder || adaptive.is_some();
        let target = (settings.video_mbps * 1e6) as u64;
        if !EncoderFactory::codecs(&*self.ff).contains(&settings.codec) {
            return Err(io::Error::other(format!(
                "no hardware {} encoder on this machine",
                settings.codec.name()
            )));
        }
        // An H.264 stream over RTMP stays H.264 (8 bits, SDR) for an HDR output: FLV
        // ingests (most platforms) don't take the HEVC an HDR program would become.
        let sdr_only = settings.codec == Codec::H264 && matches!(dest, Destination::Rtmp(_));
        let streams = !dest.is_file();
        // Streams and recordings share one encode at 48 kHz; at another rate the streams'
        // sound is converted to 48 kHz and the recordings' isn't, so they don't (bug run
        // K7-1: J2-3 split them always, two video encodes for one setting).
        // (Apart too while the rate isn't known: bug run L4-2.)
        let apart = self.sound_rate() != Some(48_000);
        let d = Dest::new(dest, adaptive, opts.via, target);
        let id = d.id;
        let mut groups = self.groups.lock_ok();
        groups.retain(|g| !g.stop.load(Ordering::Relaxed));
        let group = loop {
            let found = groups
                .iter()
                .find(|g| {
                    !g.own
                        && !own_encoder
                        && g.settings == settings
                        && g.sdr_only == sdr_only
                        && (!apart
                            || if streams {
                                !g.files.load(Ordering::Relaxed)
                            } else {
                                !g.streams.load(Ordering::Relaxed)
                            })
                })
                .cloned();
            match found {
                Some(g) => {
                    // Joined under its `dests` lock: an idle group ends only under that
                    // lock too, so it either sees this destination or is skipped here.
                    let mut ds = g.dests.lock_ok();
                    if g.stop.load(Ordering::Relaxed) {
                        drop(ds);
                        groups.retain(|x| !Arc::ptr_eq(x, &g));
                        continue;
                    }
                    ds.push(d.clone());
                    if streams {
                        g.streams.store(true, Ordering::Relaxed);
                    } else {
                        g.files.store(true, Ordering::Relaxed);
                    }
                    break g.clone();
                }
                None => {
                    let g = Arc::new(Group {
                        settings: settings.clone(),
                        own: own_encoder,
                        streams: AtomicBool::new(streams),
                        files: AtomicBool::new(!streams),
                        rate: self.rate.clone(),
                        sdr_only,
                        bitrate: AtomicU64::new(target),
                        input: Mutex::default(),
                        ready: Condvar::new(),
                        dests: Mutex::new(vec![d.clone()]),
                        params: Mutex::default(),
                        keyframe: AtomicBool::new(true),
                        stop: AtomicBool::new(false),
                        skipped: AtomicU64::new(0),
                        last_skip: Mutex::new(None),
                        started: Instant::now(),
                        heard_sound: AtomicBool::new(false),
                        #[cfg(feature = "gpu")]
                        gpu: self.gpu.clone(),
                        hdr: self.hdr.clone(),
                    });
                    groups.push(g.clone());
                    let (ff, g2) = (self.ff.clone(), g.clone());
                    std::thread::Builder::new()
                        .name("idn-gateway-encode".into())
                        .spawn(move || g2.encode_loop(ff))?;
                    break g;
                }
            }
        };
        drop(groups);
        let g2 = group.clone();
        std::thread::Builder::new()
            .name("idn-gateway-write".into())
            .spawn(move || d.write_loop(&g2))?;
        Ok(id)
    }

    /// Records what destination `of` (a stream) is sent, as its encoder makes it: the same
    /// packets, no encode of its own — every one of them, also what the stream skipped to
    /// stay live, and across its changes of rate (the new parameter sets in band). From
    /// the next keyframe (asked for at once). Returns the recording's id; it stops when
    /// removed, or when its stream's encoder ends.
    pub fn record(&self, of: u64, path: PathBuf, container: Container) -> io::Result<u64> {
        let g = self
            .groups
            .lock_ok()
            .iter()
            .find(|g| {
                !g.stop.load(Ordering::Relaxed) && g.dests.lock_ok().iter().any(|d| d.id == of)
            })
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such stream"))?;
        let d = Dest::new(
            Destination::File(path, container),
            None,
            None,
            g.bitrate.load(Ordering::Relaxed),
        );
        let id = d.id;
        {
            // As in `add_opts`: an encode that ended meanwhile takes nobody.
            let mut ds = g.dests.lock_ok();
            if g.stop.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "the stream ended"));
            }
            ds.push(d.clone());
        }
        g.keyframe.store(true, Ordering::Relaxed);
        std::thread::Builder::new()
            .name("idn-gateway-write".into())
            .spawn(move || d.write_loop(&g))?;
        Ok(id)
    }

    /// Auto-split a file destination into parts: a new file (`name-2.mp4`, `name-3.mp4`, …)
    /// every `every_s` seconds and/or every `every_mb` megabytes (0 = off for that limit).
    /// Each part starts at the next keyframe, so a part can run a little past the limit and
    /// the boundary is at a keyframe. Streams (RTMP/SRT) ignore this.
    pub fn set_split(&self, id: u64, every_s: u64, every_mb: u64) {
        for g in self.groups.lock_ok().iter() {
            for d in g.dests.lock_ok().iter() {
                if d.id == id {
                    d.split_s.store(every_s, Ordering::Relaxed);
                    d.split_mb.store(every_mb, Ordering::Relaxed);
                }
            }
        }
    }

    /// Pauses a file destination (a recording) or resumes it. Paused, nothing is written;
    /// resumed, it goes on in the same file from the next keyframe (asked for at once), its
    /// time carrying on from where it stopped — the file has no gap, the paused part is
    /// simply not in it (sound and pictures resume together). Streams ignore this.
    pub fn set_paused(&self, id: u64, paused: bool) {
        for g in self.groups.lock_ok().iter() {
            for d in g.dests.lock_ok().iter() {
                if d.id == id && d.target.is_file() {
                    d.paused.store(paused, Ordering::Relaxed);
                    d.status.lock_ok().paused = paused;
                    if !paused {
                        g.keyframe.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Stops a destination: a recording is finished properly (its last packets written
    /// and the file closed).
    pub fn remove(&self, id: u64) {
        for g in self.groups.lock_ok().iter() {
            let mut ds = g.dests.lock_ok();
            if let Some(i) = ds.iter().position(|d| d.id == id) {
                let d = ds.remove(i);
                d.stop.store(true, Ordering::Relaxed);
                d.ready.notify_all();
            }
        }
    }

    /// Encodes running: one per distinct settings (plus each own-encoder destination).
    pub fn encodes(&self) -> usize {
        self.groups
            .lock_ok()
            .iter()
            .filter(|g| !g.stop.load(Ordering::Relaxed) && !g.dests.lock_ok().is_empty())
            .count()
    }

    /// All destinations' status.
    pub fn status(&self) -> Vec<DestinationStatus> {
        let mut out: Vec<DestinationStatus> = self
            .groups
            .lock_ok()
            .iter()
            .flat_map(|g| {
                let skipped = g.skipped.load(Ordering::Relaxed);
                let skipping = g
                    .last_skip
                    .lock_ok()
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
                g.dests
                    .lock_ok()
                    .iter()
                    .map(|d| DestinationStatus {
                        skipped_pictures: skipped,
                        skipping,
                        ..d.status()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        out.sort_by_key(|s| s.id);
        out
    }

    /// Takes one frame (from [`Output::tap`], or pictures from an input to record it).
    /// Returns at once.
    pub fn send(&self, f: &TappedFrame) {
        if !f.audio.is_empty() && f.sample_rate >= 8000 {
            self.rate.store(f.sample_rate, Ordering::Relaxed);
        }
        for g in self.groups.lock_ok().iter() {
            g.push(f.clone());
        }
    }

    /// Feeds this gateway everything `out` sends, until the returned tap is dropped.
    pub fn attach(self: &Arc<Self>, out: &Output) -> Tap {
        let me = Arc::downgrade(self);
        // An HDR output (R.24): recorded and streamed in 10 bits, BT.2100.
        let t = idn_open_protocol::color::Transfer::from_name(&out.color_info().transfer);
        *self.hdr.lock_ok() = (t != idn_open_protocol::color::Transfer::Sdr).then_some(t);
        // Pictures wanted only while something is recorded or streamed: meanwhile a
        // program composed on the GPU stays there for receivers that take it there
        // (the switcher read every 2160p picture back for nothing).
        let groups = Arc::downgrade(&self.groups);
        let wanted = move || {
            groups
                .upgrade()
                .is_some_and(|g| g.lock_ok().iter().any(|g| !g.dests.lock_ok().is_empty()))
        };
        let send = move |f: &TappedFrame| {
            if let Some(g) = me.upgrade() {
                g.send(f);
            }
        };
        // Pictures on the application's GPU, where the output has them there: drawn into
        // the encoder there (read back only where the encoder can't take them). Its GPU
        // is looked up as pictures come: it may be given after this.
        #[cfg(feature = "gpu")]
        {
            *self.gpu.lock_ok() = Some(Box::new(out.gpu_lookup()));
            out.tap_gpu_while(wanted, send)
        }
        #[cfg(not(feature = "gpu"))]
        out.tap_while(wanted, send)
    }
}

impl Gateway {
    /// Stops every destination and waits (at most `timeout`) for each to finish: queued
    /// packets written, recordings closed with their trailer. Dropping the gateway does
    /// the same with a 3 s wait.
    pub fn close(&self, timeout: Duration) {
        close_groups(&self.groups, timeout);
    }
}

/// Stops every destination of `groups` and waits up to `timeout` for their files and
/// streams to be finished (trailers written).
fn close_groups(groups: &Mutex<Vec<Arc<Group>>>, timeout: Duration) {
    let dests: Vec<Arc<Dest>> = {
        let groups = groups.lock_ok();
        let mut all = Vec::new();
        for g in groups.iter() {
            all.extend(g.dests.lock_ok().drain(..));
            g.stop.store(true, Ordering::Relaxed);
            g.ready.notify_all();
        }
        all
    };
    for d in &dests {
        d.stop.store(true, Ordering::Relaxed);
        d.ready.notify_all();
    }
    let end = Instant::now() + timeout;
    while Instant::now() < end
        && dests
            .iter()
            .any(|d| !matches!(d.status.lock_ok().state, State::Done | State::Failed(_)))
    {
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.close(Duration::from_secs(3));
    }
}

/// An encoded packet for the destinations: video (90 kHz) or audio (samples) times.
struct Pkt {
    /// When it was made: its wait in a destination's queue tells how that one keeps up.
    made: Instant,
    video: bool,
    /// Sound: its track (0 the first).
    track: usize,
    data: Vec<u8>,
    pts: i64,
    key: bool,
}

/// What the muxers need, made when the encoders open; `version` changes with them.
struct Streams {
    video: Params,
    audio: Params,
    /// The second and later sound tracks' parameters.
    extra_audio: Vec<Params>,
    audio_rate: u32,
    version: u64,
}

/// One encode (video and audio) and the destinations it feeds.
struct Group {
    settings: StreamSettings,
    own: bool,
    /// It has streams (sound at 48 kHz, §N.40) and/or recordings (sound at the session's
    /// rate): both only while the sound is at 48 kHz (bug run J2-3: a stream starting
    /// during a recording flipped the group's rate and split the file; K7-1).
    streams: AtomicBool,
    files: AtomicBool,
    /// The gateway's sound rate (as it last came).
    rate: Arc<AtomicU32>,
    /// The video bit rate to encode at, bit/s: the settings', or lower while an adaptive
    /// stream's connection can't keep up.
    bitrate: AtomicU64,
    input: Mutex<VecDeque<TappedFrame>>,
    ready: Condvar,
    dests: Mutex<Vec<Arc<Dest>>>,
    params: Mutex<Option<Arc<Streams>>>,
    keyframe: AtomicBool,
    stop: AtomicBool,
    /// Pictures dropped from `input` because the encoder fell behind, and the last time.
    skipped: AtomicU64,
    last_skip: Mutex<Option<Instant>>,
    /// When it started, and whether the source's own sound has come (not silence put in).
    started: Instant,
    heard_sound: AtomicBool,
    /// The application's GPU, when the output tapped has its pictures there (§E.27).
    #[cfg(feature = "gpu")]
    gpu: Arc<Mutex<Option<GpuLookup>>>,
    /// The output's HDR transfer, when it sends HDR (R.24).
    hdr: Arc<Mutex<Option<idn_open_protocol::color::Transfer>>>,
    /// 8 bits (SDR) even for an HDR output: an H.264 stream over RTMP.
    sdr_only: bool,
}

const VIDEO_TB: (i32, i32) = (1, 90_000);
/// A group's video encoder, its size and its bit rate.
type OpenEncoder = (Box<dyn VideoEncoder>, (usize, usize), u64);
/// Frames waiting to be encoded: the newest pictures win; audio is never dropped
/// while the encoder keeps up.
const MAX_WAITING_PICTURES: usize = 3;
/// Frames of any kind waiting, at most: an encoder that can't open must not let audio
/// pile up without end.
const MAX_WAITING: usize = 256;
/// Packets waiting for one destination before it is called too slow.
const MAX_QUEUED: usize = 600;
/// The longest a stream's packet may wait before the stream skips to the next keyframe.
const STREAM_MAX_WAIT: Duration = Duration::from_secs(3);

/// An AAC-LC frame's samples (each sound packet's length).
const AAC_FRAME: i64 = 1024;

/// H.264/HEVC parameter sets with start codes (what goes in band); not AV1's `av1C`.
fn annex_b(x: &[u8]) -> bool {
    x.starts_with(&[0, 0, 0, 1]) || x.starts_with(&[0, 0, 1])
}

/// `w` × `h` fitted inside `max` (shape kept, even, never larger).
fn fit(w: usize, h: usize, max: Option<(usize, usize)>) -> (usize, usize) {
    let Some((mw, mh)) = max else {
        return (w & !1, h & !1);
    };
    let s = (mw as f64 / w as f64).min(mh as f64 / h as f64).min(1.0);
    (
        ((w as f64 * s).round() as usize).max(16) & !1,
        ((h as f64 * s).round() as usize).max(16) & !1,
    )
}

/// `f` with the samples of `pic`, its picture on the GPU, read back.
#[cfg(feature = "gpu")]
fn read_back(
    f: &TappedFrame,
    pic: &idn_open_protocol::gpu::GpuPicture,
    hand: &idn_open_protocol::gpu::GpuHandIn,
) -> io::Result<TappedFrame> {
    let data = pic.read_planes(hand).map_err(io::Error::other)?;
    let planes: Vec<idn_open_protocol::shq::Plane> = f
        .planes
        .iter()
        .zip(data)
        .map(|(p, data)| idn_open_protocol::shq::Plane {
            width: p.width,
            height: p.height,
            data,
        })
        .collect();
    let mut out = f.clone();
    out.planes = Arc::new(planes);
    Ok(out)
}

/// Scales a picture's planes (swscale), keeping the layout.
fn scale(
    f: &TappedFrame,
    w: usize,
    h: usize,
    scaler: &mut Option<Scaler>,
    scaled: &mut Option<Frame>,
) -> io::Result<Vec<idn_open_protocol::shq::Plane>> {
    let fmt = ffi::planar_format(f.layout);
    let src = Frame::view(&f.planes, fmt)?;
    // The destination buffers are kept between pictures (a 4 MB allocation per frame
    // otherwise).
    if scaled
        .as_ref()
        .is_none_or(|d| d.size() != (w, h) || d.format() != fmt)
    {
        *scaled = Some(Frame::alloc(w, h, fmt)?);
    }
    let dst = scaled.as_ref().unwrap();
    Scaler::run(scaler, src.as_ref(), dst)?;
    let (cw, ch) = match f.layout {
        idn_open_protocol::shq::Fourcc::Shq0 => (w.div_ceil(2), h.div_ceil(2)),
        idn_open_protocol::shq::Fourcc::Shq4 | idn_open_protocol::shq::Fourcc::Shq9 => (w, h),
        _ => (w.div_ceil(2), h),
    };
    Ok(vec![
        dst.to_plane(0, w, h),
        dst.to_plane(1, cw, ch),
        dst.to_plane(2, cw, ch),
    ])
}

fn log(m: &str) {
    idn_open_protocol::log::log(m);
}

/// One destination: its packet queue and the thread writing it.
struct Dest {
    id: u64,
    target: Destination,
    queue: Mutex<VecDeque<Arc<Pkt>>>,
    ready: Condvar,
    status: Mutex<DestinationStatus>,
    stop: AtomicBool,
    /// Packets were dropped: nothing more until a keyframe (a decoder would show
    /// garbage from the gap on).
    need_key: AtomicBool,
    /// Recordings: the part number (a new file per encoder change, R12 review).
    part: AtomicU64,
    /// Auto-split a recording into parts: a new part every `split_s` seconds and/or every
    /// `split_mb` megabytes (0 = off). The new part starts at the next keyframe.
    split_s: AtomicU64,
    split_mb: AtomicU64,
    /// A recording paused: packets dropped until resumed, then one file goes on.
    paused: AtomicBool,
    /// The local address to send from.
    via: Option<IpAddr>,
    /// Packets were dropped for slowness since the writer last looked (adaptive rate).
    slow: AtomicBool,
    /// The send meter: since when, the bytes written then, and when it last wrote.
    meter: Mutex<(Instant, u64, Instant)>,
    /// An adaptive stream's rate control (the writer's).
    adapt: Option<Mutex<Adapt>>,
}

/// The wait before a stream's next try after `failures` failed ones: 1, 2, 3, then 5 s
/// (a platform's viewers wait; 30 s was too long, G4).
fn retry_after(failures: usize) -> Duration {
    Duration::from_secs([1, 2, 3, 5][failures.min(3)])
}

/// An adaptive stream's rate control (G4): down a quarter when packets have waited over
/// [`Adapt::HIGH`] for [`Adapt::HOLD`] (and the last change has had [`Adapt::SETTLE`] to
/// show), halved when the queue overflowed; up a quarter (not past the rate asked) after
/// [`Adapt::UP_AFTER`] of waits under [`Adapt::LOW`]. Never under the floor.
#[derive(Debug)]
struct Adapt {
    target: u64,
    floor: u64,
    now: u64,
    high_since: Option<Instant>,
    good_since: Instant,
    changed: Instant,
}

/// `c` resampled to `m` samples, linearly (a block eased a thousandth in length).
fn stretch(c: &[f32], m: usize) -> Vec<f32> {
    let n = c.len();
    if n < 2 || m < 2 {
        return c.iter().copied().cycle().take(m).collect();
    }
    let step = (n - 1) as f64 / (m - 1) as f64;
    (0..m)
        .map(|j| {
            let x = j as f64 * step;
            let k = (x as usize).min(n - 2);
            let t = (x - k as f64) as f32;
            c[k] + (c[k + 1] - c[k]) * t
        })
        .collect()
}

#[cfg(test)]
mod stretch_tests;

#[cfg(test)]
mod adapt_tests;
