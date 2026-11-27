//! Files as sources (R.12): a video file played in real time as an IDN output.
//!
//! Every receiver can watch: the file is decoded (any format FFmpeg reads) and sent as
//! SpeedHQ, as chosen for codecs receivers can't take as they are. When the
//! file's video is H.264 or HEVC without B-frames and with keyframes at most about 2 s
//! apart, IDN inputs that take that codec (`codec/1`) get the file's own compressed
//! pictures, with no encode. An input that joins between the file's keyframes gets
//! encoded pictures (the GPU's, or SpeedHQ without one) until the next keyframe.
//!
//! ```no_run
//! # fn run(p: &idn_open_protocol::api::Publisher) -> std::io::Result<()> {
//! use idn_ffmpeg::player::{FilePlayer, PlayerConfig};
//! use idn_open_protocol::api::OutputConfig;
//! use std::sync::Arc;
//!
//! let ff = Arc::new(idn_ffmpeg::Ffmpeg::probe());
//! let player = FilePlayer::open(p, OutputConfig::new("Welcome video"), "/srv/media/welcome.mp4", ff, PlayerConfig::default())?;
//! player.pause();
//! player.seek(std::time::Duration::from_secs(30));
//! player.resume();
//! # Ok(()) }
//! ```

use idn_common::sync::LockOk as _;
mod list;
pub use list::{ListConfig, ListItem, ListPlayer, ListStatus};

use crate::Ffmpeg;
use crate::ffi::{
    Demuxer, FileDecoder, Frame, FrameRef, ReadPacket, Resampler, Scaler, StreamInfo,
};
use ffmpeg_sys_next as ff;
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, PlanarFrame, Publisher};
use idn_open_protocol::codec::{
    Codec, CodecOutput, CodedAudio, CodedPicture, EncoderConfig, EncoderFactory, VideoEncoder,
};
use idn_open_protocol::shq::Fourcc;
use idn_open_protocol::thin::Thin;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod convert;
mod memory;
mod passthrough;
pub use convert::*;
use memory::*;
pub(crate) use passthrough::*;

mod play_audio;
mod play_control;
mod play_video;

/// How a file plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerConfig {
    /// Start again at the end (a pre-service loop).
    pub looped: bool,
    /// Send H.264/HEVC files' own pictures to IDN inputs that take them (default on).
    pub pass_through: bool,
    /// Offer compressed video at all (the file's own, or the GPU's encode to inputs that
    /// ask for it; default on). Off: SpeedHQ to everyone (testing a SpeedHQ path with a
    /// file).
    pub compressed: bool,
    /// Start paused (on the first picture).
    pub paused: bool,
    /// Remember, per machine, that software decoding beat the hardware decoder for a
    /// codec at a picture size (a 4K download from the GPU can cost more than decoding
    /// on the CPU), and start in software next time instead of measuring again for a
    /// GOP's length (default on; kept in the library's state directory).
    pub remember_decoder: bool,
    /// Where to decode video: automatic (the GPU first, software if the GPU's download
    /// proves slow, as measured), the GPU (its pictures downloaded on helper threads, a
    /// few at once, while the next ones decode) or software. `None` (the default): the
    /// [`Ffmpeg`]'s setting ([`Ffmpeg::set_decoder`], `IDN_DECODER`, else the machine's
    /// in the IDN Node: [`Ffmpeg::decoding`]).
    pub decoder: Option<crate::DecoderChoice>,
    /// What the decoder does while nothing needs the pictures as samples (every receiver
    /// takes the file's own compressed pictures, no tap): see [`IdleDecode`].
    pub idle: IdleDecode,
    /// At most about this many pictures a second as samples (a switcher's program rate):
    /// of a file at twice that or more, the pictures between are decoded (later ones
    /// depend on them) but neither downloaded from the GPU nor converted; receivers
    /// passing the file's own pictures through still get every one. `None` (the
    /// default): every picture. A 4K60 file on a 2160p30 program spent 25 ms a picture
    /// on downloads, fell behind and paused its sound (gaps in the program's sound).
    pub picture_rate: Option<(u32, u32)>,
}

/// A player's decoding while nothing needs its pictures as samples
/// ([`Output::needs_pictures`]): every receiver passes the file's compressed pictures
/// through, and nothing taps the output (a 4K file's
/// download from the GPU and conversion cost about 2.5 cores for nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdleDecode {
    /// Keep decoding but leave the pictures on the GPU (no download, no conversion): a
    /// receiver that then needs them gets them from the next picture. Most of the cost
    /// saved with the GPU decoding; with software decoding the decode itself stays.
    #[default]
    Warm,
    /// Decode and convert every picture, needed or not (as before).
    Always,
    /// Stop decoding: a receiver that then needs pictures waits for the file's next
    /// keyframe (at most about 2.5 s with pass-through).
    Stop,
    /// As `Stop`, but go on decoding for this long after a receiver connects, so one that
    /// needs pictures mostly finds them at once.
    StopAfter(Duration),
}

impl Default for PlayerConfig {
    fn default() -> Self {
        PlayerConfig {
            looped: false,
            pass_through: true,
            compressed: true,
            paused: false,
            remember_decoder: true,
            decoder: None,
            idle: IdleDecode::Warm,
            picture_rate: None,
        }
    }
}

/// What a player is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerState {
    Playing,
    Paused,
    /// At the end (not looping): the last picture stays on.
    Ended,
    Failed(String),
}

/// A player's status, from [`FilePlayer::status`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerStatus {
    pub state: PlayerState,
    /// Where it is in the file.
    pub position: Duration,
    /// The file's length, if it says.
    pub duration: Option<Duration>,
    /// The video, e.g. `"h264 1920x1080"`; `None` for sound only.
    pub video: Option<String>,
    /// IDN inputs can get the file's own pictures; if not, why.
    pub pass_through: Result<Codec, String>,
    /// The video decoder in use, once a picture came out: codec and the GPU decoder
    /// ("VA-API", "NVDEC", "D3D11VA") or "software".
    pub decoder: Option<String>,
    /// Average time to decode and convert a picture, in milliseconds.
    pub decode_ms: f64,
    /// How far presentation was pushed back to keep sound and picture together when
    /// pictures came out of the decoder after their time (the decoder's own latency).
    pub shifted: Duration,
    /// Pictures of a file with sound sent without their planes (their coded data still
    /// to receivers passing it through) because they came over four frames late: the
    /// sound kept its time.
    pub late_pictures: u64,
    /// Times the decoder rested until a keyframe to keep up with the sound (a file with
    /// sound whose pictures came out over four frames late).
    pub caught_up: u64,
}

enum Cmd {
    Pause,
    Resume,
    Seek(Duration),
}

struct Control {
    cmds: Mutex<VecDeque<Cmd>>,
    status: Mutex<PlayerStatus>,
    stop: AtomicBool,
    /// The application's GPU ([`FilePlayer::use_gpu`]): pictures the GPU decodes kept
    /// there while only this process's inputs on that GPU take them (§E.27).
    #[cfg(feature = "gpu")]
    gpu: Mutex<Option<idn_open_protocol::gpu::GpuHandIn>>,
}

/// A file playing as an output. Dropping it stops the output.
pub struct FilePlayer {
    out: Arc<Output>,
    ctl: Arc<Control>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FilePlayer {
    /// Opens `path` and starts an output for it (`out_cfg`, whose frame rate becomes
    /// the file's). With pass-through, the output offers the file's codec besides the
    /// GPU's (`out_cfg.codec`'s policy is kept).
    pub fn open(
        publisher: &Publisher,
        mut out_cfg: OutputConfig,
        path: impl AsRef<Path>,
        ff: Arc<Ffmpeg>,
        cfg: PlayerConfig,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_string_lossy().into_owned();
        let mut demux = Demuxer::open(&path)?;
        let video = demux.streams.iter().position(|s| s.video);
        let audio = demux.streams.iter().position(|s| !s.video);
        if video.is_none() && audio.is_none() {
            return Err(io::Error::other(format!("{path}: no video or audio")));
        }
        let pass = match video {
            Some(v) if cfg.pass_through && cfg.compressed => pass_through(&mut demux, v),
            Some(_) => Err("off".to_owned()),
            None => Err("no video".to_owned()),
        };
        demux.rewind()?;
        if let Some(v) = video {
            let (n, d) = demux.streams[v].frame_rate;
            if n > 0 && d > 0 {
                out_cfg.frame_rate = (n as u32, d as u32);
            }
        }
        // The file's colour, so receivers convert (and tone-map HDR) as it was made.
        if let Some(v) = video {
            stream_colour(&demux.streams[v], &mut out_cfg.color);
        }
        let policy = out_cfg.codec.as_ref().map(|c| c.policy).unwrap_or_default();
        let mbps = out_cfg.codec.as_ref().map_or(8.0, |c| c.mbps_1080p);
        out_cfg.codec = cfg.compressed.then(|| CodecOutput {
            encoder: Arc::new(WithFileCodec {
                gpu: ff.clone(),
                file: pass.as_ref().ok().map(|p| p.codec),
            }),
            policy,
            mbps_1080p: mbps,
            ndi: None,
            fixed: false,
        });
        let out = Arc::new(Output::new(publisher, out_cfg)?);
        let vinfo = video.map(|v| {
            let s = &demux.streams[v];
            format!("{} {}x{}", codec_name(s.codec_id), s.width, s.height)
        });
        let ctl = Arc::new(Control {
            cmds: Mutex::default(),
            status: Mutex::new(PlayerStatus {
                state: if cfg.paused {
                    PlayerState::Paused
                } else {
                    PlayerState::Playing
                },
                position: Duration::ZERO,
                duration: demux.duration_us.map(|us| Duration::from_micros(us as u64)),
                video: vinfo,
                pass_through: pass.as_ref().map(|p| p.codec).map_err(Clone::clone),
                decoder: None,
                decode_ms: 0.0,
                shifted: Duration::ZERO,
                late_pictures: 0,
                caught_up: 0,
            }),
            stop: AtomicBool::new(false),
            #[cfg(feature = "gpu")]
            gpu: Mutex::new(None),
        });
        let (o, c) = (out.clone(), ctl.clone());
        let thread = std::thread::Builder::new()
            .name("idn-file-player".into())
            .spawn(move || {
                let mut p = Play::new(demux, video, audio, pass.ok(), o, c.clone(), cfg, ff);
                if let Err(e) = p.run() {
                    c.status.lock_ok().state = PlayerState::Failed(e.to_string());
                }
            })?;
        Ok(FilePlayer {
            out,
            ctl,
            thread: Some(thread),
        })
    }

    /// The output the file plays on.
    pub fn output(&self) -> &Arc<Output> {
        &self.out
    }

    /// The application's GPU (its wgpu device): while only this process's inputs on it
    /// take the output's pictures ([`Output::gpu_only`]), pictures decoded on the GPU
    /// (VA-API) stay there, never downloaded (§E.27). `None` stops it.
    #[cfg(feature = "gpu")]
    pub fn use_gpu(&self, gpu: Option<idn_open_protocol::gpu::GpuHandIn>) {
        *self.ctl.gpu.lock_ok() = gpu;
    }

    pub fn pause(&self) {
        self.ctl.cmds.lock_ok().push_back(Cmd::Pause);
    }

    pub fn resume(&self) {
        self.ctl.cmds.lock_ok().push_back(Cmd::Resume);
    }

    /// Jumps to `to` (from the keyframe before it, decoding up to it).
    pub fn seek(&self, to: Duration) {
        self.ctl.cmds.lock_ok().push_back(Cmd::Seek(to));
    }

    pub fn status(&self) -> PlayerStatus {
        self.ctl.status.lock_ok().clone()
    }
}

impl Drop for FilePlayer {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The GPU's codecs, plus the file's own (passed through, never encoded here).
struct WithFileCodec {
    gpu: Arc<Ffmpeg>,
    file: Option<Codec>,
}

/// How a file's compressed pictures become Annex B access units.
pub(crate) struct PassThrough {
    pub(crate) codec: Codec,
    /// NAL length size (MP4/MKV), or `None` when the file is already Annex B (TS).
    pub(crate) length_size: Option<usize>,
    /// The parameter sets, Annex B.
    pub(crate) params: Vec<u8>,
}

/// Something to hand over at its time.
enum Item {
    Video(PlanarFrame),
    /// A picture kept on the GPU (§E.27): its sizes, and it there.
    #[cfg(all(feature = "gpu", target_os = "linux"))]
    VideoGpu(PlanarFrame, idn_open_protocol::gpu::GpuPicture),
    /// Decoded sound, with the file's own AAC frame when it has one (§E.19.2).
    Audio(AudioBlock, Option<CodedAudio>),
}

/// The playing state (on the player's thread).
struct Play {
    demux: Demuxer,
    vstream: Option<StreamInfo>,
    astream: Option<StreamInfo>,
    vdec: Option<FileDecoder>,
    /// The FFmpeg handle (its hardware device), to reopen the video decoder.
    ff: Arc<Ffmpeg>,
    /// Choosing between the hardware and the software decoder by measured cost.
    trial: Trial,
    /// Pictures decoded since the decoder was (re)opened.
    decoded: u32,
    /// Pictures decoded since the file started playing, whatever decoder: a switch of
    /// decoders (the decoder trial) doesn't make the file start again. (`decoded` alone
    /// decided it: after the trial's switch back, the new decoder's first pictures pushed
    /// the whole input's clock back, sound and pictures gone for about 3 s, lab 451.)
    played: u64,
    /// The decoder trial's window: pictures decoded and the clock's shift at its start;
    /// the share hardware came late by.
    window: (u32, Duration),
    window_idle: bool,
    late_once: bool,
    hw_late: f64,
    /// The process's CPU at the window's start, and hardware's per picture (ms).
    window_cpu: Duration,
    hw_cpu: f64,
    /// After a switch: the previous decoder, kept until its output reaches the switch
    /// keyframe's time, so the pictures before it (the leading pictures of an open GOP,
    /// which the new decoder skips) still come out. Lab item 62: ~5 frames were lost at
    /// the switch without this.
    retiring: Option<(FileDecoder, i64)>,
    /// The trial's hardware decoder, kept (flushed) while software is measured, for the
    /// switch back: a new one set up its GPU context on this thread (the program's
    /// sound stopped for up to a second, lab 453-B).
    parked_hw: Option<FileDecoder>,
    adec: Option<FileDecoder>,
    pass: Option<PassThrough>,
    out: Arc<Output>,
    ctl: Arc<Control>,
    cfg: PlayerConfig,
    scaler: Option<Scaler>,
    /// The 4:2:2 picture the decoded frame is converted into (kept between frames).
    scaled: Option<Frame>,
    /// Download target for GPU frames converted on this thread.
    sw: Frame,
    /// Pictures the GPU decodes converted there (made when first wanted; `Err`: this GPU or
    /// file can't, not tried again).
    #[cfg(all(feature = "gpu", target_os = "linux"))]
    va: Option<Result<crate::va_gpu::VaToGpu, ()>>,
    /// What was last said about keeping pictures on the GPU.
    #[cfg(all(feature = "gpu", target_os = "linux"))]
    va_said: Option<bool>,
    /// With the GPU chosen: downloads and conversions on helper threads.
    downloads: Option<Downloads>,
    resampler: Option<Resampler>,
    /// Decoded media by media time (µs), and compressed pictures waiting for theirs.
    queue: BTreeMap<(i64, u8), Item>,
    coded: VecDeque<(i64, CodedPicture)>,
    /// Pictures above `PlayerConfig::picture_rate`, left out.
    thin: Option<Thin>,
    /// Late pictures in a row sent without their planes (the sound keeping its time).
    late_run: u32,
    /// AAC frames waiting for their decoded sound, by time (µs), when the file's sound
    /// is AAC with its AudioSpecificConfig (§E.19.2).
    coded_audio: VecDeque<(i64, CodedAudio)>,
    aac_config: Option<Vec<u8>>,
    /// The clock: media time `media0` (µs) is shown at `wall0`, once started.
    wall0: Instant,
    media0: i64,
    clock_started: bool,
    /// Looping: the file's times are continued, each pass offset by its duration, so
    /// the clock never restarts and the seam shows no gap.
    loop_offset: i64,
    /// The latest video time queued (µs, offset included): the pass's end when the
    /// file has no duration.
    last_video_t: i64,
    /// Timecode = tc0 + media time (µs) × 10, kept increasing across loops and seeks.
    tc0: i64,
    last_tc: i64,
    /// After a seek: drop media before this time (µs).
    skip_until: i64,
    paused: bool,
    /// Paused and moved (a seek): the picture at the new place is shown once.
    show_next: bool,
    at_end: bool,
    /// Whether anything needs the pictures as samples, checked at most every 100 ms;
    /// receivers last seen (a new one starts `StopAfter`'s grace) and until when.
    needs: (bool, Instant),
    receivers: usize,
    grace_until: Option<Instant>,
    /// Not decoding (`IdleDecode::Stop`/`StopAfter`): starts again at a keyframe.
    stopped: bool,
    /// Resting to catch up with the sound ([`Play::picture_late`]).
    catch_up: Catch,
    /// How late pictures came out in all, the clock pushed back for them or not: the
    /// decoders' trials judge by it.
    behind: Duration,
    /// This pass's pictures, and the last one; a file whose pass is one picture and no
    /// sound (a still) keeps it and sends it again each period, not decoded again.
    pass_pictures: usize,
    last_picture: Option<PlanarFrame>,
    still: Option<PlanarFrame>,
}

/// Decoding behind a file's sound ([`Play::picture_late`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Catch {
    No,
    /// Rest from the next packet (the decoder's held pictures dropped).
    Wanted,
    /// Not decoding until a keyframe; the compressed pictures go as they are.
    Resting,
}

/// Hardware decoding of a big picture can be slower than software here: the picture
/// has to come back from the GPU (a 4K download took 17 of 25 ms per frame), while
/// software decoding with a few threads took 5 ms. The player watches the
/// decoder it started with and, when its pictures come late (over 5% of the time), moves
/// to the other one at a keyframe; if that comes late as much it moves back and stays.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Trial {
    /// Measuring the first decoder (hardware when there is one).
    First,
    /// Switch to software at the next keyframe.
    WantSoftware,
    /// Measuring software, with hardware's lateness and CPU to beat (`Play::hw_late`, `hw_cpu`).
    Software,
    /// Switch back to hardware at the next keyframe.
    WantHardware,
    /// The choice remembered for this machine, watched: late in two windows running, it
    /// is measured again (hardware VP9 4K60, remembered from an earlier
    /// run, fell behind next to the switcher's GPU work and held the file to 3–7 fps).
    Remembered,
    /// Switch to hardware at the next keyframe, then measure it as at the start.
    WantHardwareFirst,
    Settled,
}

struct Remembered(bool);

/// GPU pictures downloaded and converted ([`to_planar`]) on helper threads, a few at once,
/// while the next ones decode ([`crate::DecoderChoice::Gpu`]). Results go into the
/// queue by time; a picture isn't handed over while an earlier one is still on its way.
struct Downloads {
    jobs: std::sync::mpsc::Sender<(u64, Frame, i64, Option<CodedPicture>)>,
    /// (generation, time, picture, seconds the helper spent on it).
    done: std::sync::mpsc::Receiver<(u64, i64, io::Result<PlanarFrame>, f64)>,
    /// Times of the pictures on their way.
    pending: std::collections::BTreeSet<i64>,
    /// Bumped by a seek or loop restart: results from before are dropped.
    generation: u64,
}

impl Play {
    #[allow(clippy::too_many_arguments)]
    fn new(
        demux: Demuxer,
        video: Option<usize>,
        audio: Option<usize>,
        pass: Option<PassThrough>,
        out: Arc<Output>,
        ctl: Arc<Control>,
        cfg: PlayerConfig,
        ff: Arc<Ffmpeg>,
    ) -> Self {
        let mut demux = demux;
        let take = |i: Option<usize>, d: &mut Demuxer| i.map(|i| d.streams.swap_remove(i));
        // Indices shift with swap_remove: take the higher one first.
        let (vstream, astream) = match (video, audio) {
            (Some(v), Some(a)) if v > a => {
                let vs = take(Some(v), &mut demux);
                (vs, take(Some(a), &mut demux))
            }
            (v, a) => {
                let a2 = take(a, &mut demux);
                (take(v, &mut demux), a2)
            }
        };
        let rotation = vstream.as_ref().map_or(0, |s| s.rotation);
        let range = vstream
            .as_ref()
            .map_or(ff::AVColorRange::AVCOL_RANGE_UNSPECIFIED, |s| s.color_range);
        let choice = cfg.decoder.unwrap_or_else(|| ff.decoding());
        if choice == crate::DecoderChoice::Gpu && ff.hw_decode.is_none() {
            idn_open_protocol::log::log(
                "player: GPU decoding asked for, but no GPU decoder here; software decoding",
            );
        }
        let device = ff
            .hw_decode
            .as_deref()
            .filter(|_| choice != crate::DecoderChoice::Software);
        let memory = vstream
            .as_ref()
            .filter(|_| choice == crate::DecoderChoice::Auto && cfg.remember_decoder)
            .and_then(|s| remembered(&decoder_key(s)));
        let remembered = memory.is_some();
        let vdec = vstream.as_ref().and_then(|s| {
            let device = match memory {
                Some(software) if device.is_some() => {
                    idn_open_protocol::log::log(&format!(
                        "player: {} decoding for {}, as measured before on this machine",
                        if software { "software" } else { "hardware" },
                        decoder_key(s)
                    ));
                    device.filter(|_| !software)
                }
                _ => device,
            };
            FileDecoder::open(s, device)
                .or_else(|_| FileDecoder::open(s, None))
                .map_err(|e| idn_open_protocol::log::log(&format!("player: video decoder: {e}")))
                .ok()
        });
        let adec = astream
            .as_ref()
            .and_then(|s| FileDecoder::open(s, None).ok());
        // AAC with its configuration (MP4, MKV): passed through too (§E.19.2).
        let aac_config = astream
            .as_ref()
            .filter(|s| {
                cfg.pass_through
                    && s.codec_id == ff::AVCodecID::AV_CODEC_ID_AAC
                    && !s.extradata.is_empty()
            })
            .map(|s| s.extradata.clone());
        let paused = cfg.paused;
        // Download helpers whenever the GPU decodes, the automatic choice too (without
        // them its 4K60 pictures came 51% late and it moved to software decoding).
        let gpu_downloads = device.is_some();
        let thin = cfg.picture_rate.and_then(Thin::new);
        Play {
            demux,
            vstream,
            astream,
            vdec,
            ff,
            // Only the automatic choice measures and switches.
            trial: if choice != crate::DecoderChoice::Auto {
                Trial::Settled
            } else if remembered {
                Trial::Remembered
            } else {
                Trial::First
            },
            decoded: 0,
            played: 0,
            window: (0, Duration::ZERO),
            window_idle: false,
            late_once: false,
            hw_late: 0.0,
            window_cpu: Duration::ZERO,
            hw_cpu: 0.0,
            retiring: None,
            parked_hw: None,
            adec,
            pass,
            tc0: out.timecode_now(),
            out,
            ctl,
            cfg,
            scaler: None,
            scaled: None,
            sw: Frame::new().expect("an AVFrame"),
            #[cfg(all(feature = "gpu", target_os = "linux"))]
            va: None,
            #[cfg(all(feature = "gpu", target_os = "linux"))]
            va_said: None,
            downloads: gpu_downloads.then(|| Downloads::start(rotation, range)),
            resampler: None,
            queue: BTreeMap::new(),
            coded: VecDeque::new(),
            thin,
            late_run: 0,
            coded_audio: VecDeque::new(),
            aac_config,
            wall0: Instant::now(),
            media0: 0,
            clock_started: false,
            loop_offset: 0,
            last_video_t: 0,
            last_tc: 0,
            skip_until: 0,
            paused,
            show_next: false,
            at_end: false,
            needs: (true, Instant::now()),
            receivers: 0,
            grace_until: None,
            stopped: false,
            catch_up: Catch::No,
            behind: Duration::ZERO,
            pass_pictures: 0,
            last_picture: None,
            still: None,
        }
    }

    fn us(tb: (i32, i32), t: i64) -> i64 {
        (t as i128 * 1_000_000 * tb.0 as i128 / tb.1.max(1) as i128) as i64
    }

    fn run(&mut self) -> io::Result<()> {
        let mut first_frame = true;
        while !self.ctl.stop.load(Ordering::Relaxed) {
            self.commands()?;
            // Read ahead: about 300 ms of media decoded and waiting, and a few pictures on
            // their way to the download helpers at most (each holds a GPU surface). Never
            // a wait in here: this thread also hands the pictures over, and while it
            // waited they went out late in a burst, which receivers' connections thin
            // out.
            // Warm and idle: less read ahead, so a receiver that comes to need pictures
            // waits behind fewer queued without them (300 ms of them was the 0.5 s a
            // SpeedHQ receiver waited for its first picture).
            let read_ahead = if self.warm_idle() { 80_000 } else { 300_000 };
            while self.ahead() < read_ahead
                && self.queue.len() < 64
                && !self.downloads_full()
                && !self.due_soon()
            {
                if self.at_end && (!self.cfg.looped || !self.loop_around()?) {
                    break;
                }
                self.read_one()?;
            }
            if self.paused {
                // Show the first picture when starting paused, or the one a seek moved to.
                if (first_frame || self.show_next)
                    && let Some(k) = self.queue.keys().find(|k| k.1 == 0).copied()
                {
                    self.wall0 = Instant::now();
                    self.media0 = k.0;
                    self.clock_started = true;
                    self.hand_over(k);
                    first_frame = false;
                    self.show_next = false;
                    // Handing it over said "playing"; it isn't.
                    self.ctl.status.lock_ok().state = PlayerState::Paused;
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            self.collect_downloads(None);
            let Some((&k, _)) = self.queue.iter().next() else {
                if self
                    .downloads
                    .as_ref()
                    .is_some_and(|d| !d.pending.is_empty())
                {
                    self.collect_downloads(Some(Duration::from_millis(20)));
                    continue;
                }
                if self.at_end {
                    if self.cfg.looped && self.loop_around()? {
                        continue;
                    }
                    self.ctl.status.lock_ok().state = PlayerState::Ended;
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                continue;
            };
            if first_frame {
                self.wall0 = Instant::now();
                self.media0 = k.0;
                self.clock_started = true;
                first_frame = false;
            }
            let due = self.wall0 + Duration::from_micros((k.0 - self.media0).max(0) as u64);
            let now = Instant::now();
            if due > now {
                std::thread::sleep((due - now).min(Duration::from_millis(20)));
                continue;
            }
            // An earlier picture still downloading goes first; sound goes at its time (held
            // behind a slow download it left gaps in a switcher program's sound).
            if k.1 == 0 && self.downloading_before(k.0) {
                self.collect_downloads(Some(Duration::from_millis(20)));
                continue;
            }
            // Late by four frames or more (a real stall, not this thread's usual work): the
            // clock waits rather than the late pictures going out together. Smaller
            // lateness is left alone: correcting it each time made the clock drift
            // (12 ms a second in software decoding).
            let period = self.vstream.as_ref().map_or(33_333, |s| {
                1_000_000 * s.frame_rate.1.max(1) as i64 / s.frame_rate.0.max(1) as i64
            });
            let now_media = self.media0 + self.wall0.elapsed().as_micros() as i64;
            if now_media - k.0 > 4 * period {
                // With sound, the sound keeps time: a picture this late goes without its
                // planes (the clock waiting for it left a gap as long in the sound) —
                // unless pictures have been late for a second running (decoding
                // slower than the file): then the clock waits after all.
                if k.1 == 0
                    && self.astream.is_some()
                    && (self.late_run as i64) < 1_000_000 / period.max(1)
                {
                    self.late_run += 1;
                    self.ctl.status.lock_ok().late_pictures += 1;
                    let keep = match self.queue.get_mut(&k) {
                        Some(Item::Video(f)) => {
                            f.planes = Arc::default();
                            f.coded.is_some()
                        }
                        _ => true,
                    };
                    if keep {
                        self.hand_over(k);
                    } else {
                        self.queue.remove(&k);
                    }
                    continue;
                }
                let late = Duration::from_micros((now_media - k.0) as u64);
                self.wall0 += late;
                self.ctl.status.lock_ok().shifted += late;
            }
            if k.1 == 0 {
                self.late_run = 0;
            }
            self.hand_over(k);
        }
        Ok(())
    }

    fn read_one(&mut self) -> io::Result<()> {
        let Some(p) = self.demux.read()? else {
            self.at_end = true;
            return Ok(());
        };
        let is_video = self.vstream.as_ref().is_some_and(|s| s.index == p.stream);
        let is_audio = self.astream.as_ref().is_some_and(|s| s.index == p.stream);
        if is_video {
            self.on_video_packet(&p);
        } else if is_audio {
            self.on_audio_packet(&p);
        }
        Ok(())
    }

    /// Whether anything needs the pictures as samples (see [`IdleDecode`]); `true` with
    /// `Always`, without pass-through, and within `StopAfter`'s grace after a receiver
    /// connects.
    fn needed(&mut self) -> bool {
        if self.cfg.idle == IdleDecode::Always || self.pass.is_none() {
            return true;
        }
        if self.needs.1.elapsed() >= Duration::from_millis(100) {
            let n = self.out.receivers();
            if n > self.receivers
                && let IdleDecode::StopAfter(d) = self.cfg.idle
            {
                self.grace_until = Some(Instant::now() + d);
            }
            self.receivers = n;
            self.needs = (self.out.needs_pictures(), Instant::now());
        }
        self.needs.0 || self.grace_until.is_some_and(|g| Instant::now() < g)
    }

    /// Warm and nothing needing the pictures (as last checked).
    fn warm_idle(&self) -> bool {
        self.cfg.idle == IdleDecode::Warm && self.pass.is_some() && !self.needs.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quarter turns: exact on a small plane, four of them the identity, and 4:2:2 and
    /// 4:2:0 shapes after a quarter turn (checked against FFmpeg's own turned decode by
    /// hand).
    #[test]
    fn pictures_turn_as_their_display_matrix_says() {
        use idn_open_protocol::shq::Plane;
        let p = Plane {
            width: 3,
            height: 2,
            data: vec![1, 2, 3, 4, 5, 6],
        };
        // 1 2 3      counter-clockwise:  3 6
        // 4 5 6                          2 5
        //                                1 4
        assert_eq!(turn(&p, 1).data, [3, 6, 2, 5, 1, 4]);
        assert_eq!(turn(&p, 3).data, [4, 1, 5, 2, 6, 3]);
        assert_eq!(turn(&p, 2).data, [6, 5, 4, 3, 2, 1]);
        let big = Plane {
            width: 37,
            height: 70,
            data: (0..37 * 70).map(|i| (i * 7) as u8).collect(),
        };
        let mut t = turn(&big, 1);
        for _ in 0..3 {
            t = turn(&t, 1);
        }
        assert_eq!(t, big);
        let pf = PlanarFrame {
            layout: Fourcc::Shq2,
            width: 37,
            height: 20,
            planes: Arc::new(vec![
                Plane {
                    width: 37,
                    height: 20,
                    data: vec![16; 37 * 20],
                },
                Plane {
                    width: 19,
                    height: 20,
                    data: vec![128; 19 * 20],
                },
                Plane {
                    width: 19,
                    height: 20,
                    data: vec![128; 19 * 20],
                },
            ]),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: None,
        };
        let r = rotated(pf, 270);
        assert_eq!((r.width, r.height), (20, 37));
        let shapes: Vec<_> = r
            .planes
            .iter()
            .map(|p| (p.width, p.height, p.data.len()))
            .collect();
        assert_eq!(shapes, [(20, 37, 740), (10, 37, 370), (10, 37, 370)]);
        // 4:2:0, odd both ways: 37x21 (chroma 19x11) turned is 21x37 (chroma 11x19).
        let plane = |w: usize, h: usize| Plane {
            width: w,
            height: h,
            data: (0..w * h).map(|i| i as u8).collect(),
        };
        let pf = PlanarFrame {
            layout: Fourcc::Shq0,
            width: 37,
            height: 21,
            planes: Arc::new(vec![plane(37, 21), plane(19, 11), plane(19, 11)]),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: None,
        };
        let r = rotated(pf, 90);
        assert_eq!((r.layout, r.width, r.height), (Fourcc::Shq0, 21, 37));
        let shapes: Vec<_> = r.planes.iter().map(|p| (p.width, p.height)).collect();
        assert_eq!(shapes, [(21, 37), (11, 19), (11, 19)]);
        assert_eq!(r.planes[1], turn(&plane(19, 11), 1));
    }

    /// Above 1080p, 4:2:0 pictures go out 4:2:0 as NDI's senders send them, the same
    /// samples swscale gives (NV12 and I420 straight, P010 through swscale); 1080p and
    /// 4:2:2 sources stay 4:2:2.
    #[test]
    fn big_420_pictures_go_out_420() {
        use ff::AVPixelFormat::*;
        // A picture of `fmt` with every byte of every plane set from its position.
        let picture = |w: usize, h: usize, fmt| {
            let f = Frame::alloc(w, h, fmt).unwrap();
            for i in 0..3 {
                let (p, stride) = f.plane(i);
                if p.is_null() {
                    break;
                }
                let rows = if i == 0 || fmt == AV_PIX_FMT_YUV422P {
                    h
                } else {
                    h.div_ceil(2)
                };
                for r in 0..rows {
                    for x in 0..stride {
                        // SAFETY: inside plane i: `rows` rows of `stride` bytes, as allocated.
                        unsafe { *p.add(r * stride + x) = (x * 7 + r * 3 + i * 50) as u8 };
                    }
                }
            }
            f
        };
        for (w, h, fmt, layout) in [
            (3840, 2160, AV_PIX_FMT_NV12, Fourcc::Shq0),
            (2562, 1442, AV_PIX_FMT_NV12, Fourcc::Shq0),
            (3840, 2160, AV_PIX_FMT_YUV420P, Fourcc::Shq0),
            (3840, 2160, AV_PIX_FMT_P010LE, Fourcc::Shq0),
            (1920, 1080, AV_PIX_FMT_NV12, Fourcc::Shq2),
            (3840, 2160, AV_PIX_FMT_YUV422P, Fourcc::Shq2),
        ] {
            let src = picture(w, h, fmt);
            let (mut sc, mut sd) = (None, None);
            let p = to_planar(src.as_ref(), &mut sc, &mut sd, None).unwrap();
            assert_eq!(p.layout, layout, "{fmt:?} {w}x{h}");
            let ch = if layout == Fourcc::Shq0 {
                h.div_ceil(2)
            } else {
                h
            };
            let shapes: Vec<_> = p.planes.iter().map(|q| (q.width, q.height)).collect();
            assert_eq!(shapes, [(w, h), (w.div_ceil(2), ch), (w.div_ceil(2), ch)]);
            // The direct conversions against swscale's I420: 8-bit the same, P010 at most a
            // step apart (rounded here; swscale's own rounding differs).
            if layout == Fourcc::Shq0 {
                let dst = Frame::alloc(w, h, AV_PIX_FMT_YUV420P).unwrap();
                Scaler::run(&mut None, src.as_ref(), &dst).unwrap();
                let want = [
                    dst.to_plane(0, w, h),
                    dst.to_plane(1, w.div_ceil(2), ch),
                    dst.to_plane(2, w.div_ceil(2), ch),
                ];
                let step = if fmt == AV_PIX_FMT_P010LE { 1 } else { 0 };
                for (got, want) in p.planes.iter().zip(&want) {
                    let worst = got.data.iter().zip(&want.data).map(|(a, b)| a.abs_diff(*b));
                    assert!(worst.max().unwrap() <= step, "{fmt:?} {w}x{h}");
                }
            }
        }
    }

    /// Turning a 2160p picture (cargo test --release -p idn-ffmpeg --lib turn_speed -- --ignored --nocapture).
    #[test]
    #[ignore = "timing"]
    fn turn_speed() {
        use idn_open_protocol::shq::Plane;
        let (w, h) = (3840usize, 2160usize);
        let pl = |pw: usize| Plane {
            width: pw,
            height: h,
            data: (0..pw * h).map(|i| i as u8).collect(),
        };
        let pf = PlanarFrame {
            layout: Fourcc::Shq2,
            width: w,
            height: h,
            planes: Arc::new(vec![pl(w), pl(w / 2), pl(w / 2)]),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: None,
        };
        for q in [90u16, 180] {
            let t = Instant::now();
            for _ in 0..5 {
                std::hint::black_box(rotated(pf.clone(), q));
            }
            println!(
                "turning 2160p by {q}: {:.2} ms",
                t.elapsed().as_secs_f64() * 200.0
            );
        }
    }

    /// Picture files made by the ffmpeg program (skipped without it): BT.709 limited
    /// range out, alpha kept.
    #[test]
    fn stills_are_bt709_with_their_alpha() {
        let dir = idn_common::testing::TempDir::new("still");
        let make = |name: &str, colour: &str, fmt: &str| {
            let out = dir.join(name);
            let ok = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("color=c={colour}:s=32x16,format={fmt}"))
                .args(["-frames:v", "1"])
                .arg(&out)
                .status()
                .is_ok_and(|s| s.success());
            ok.then_some(out)
        };
        let Some(red) = make("red.png", "0xFD0000", "rgb24") else {
            eprintln!("no ffmpeg program: skipped");
            return;
        };
        let at = |p: &PlanarFrame| {
            (
                p.planes[0].data[40],
                p.planes[1].data[20],
                p.planes[2].data[20],
            )
        };
        let near = |got: (u8, u8, u8), want: (u8, u8, u8)| {
            [(got.0, want.0), (got.1, want.1), (got.2, want.2)]
                .iter()
                .all(|&(g, w)| g.abs_diff(w) <= 1)
        };
        for (file, want) in [
            (red, (62, 103, 239)),
            (
                make("lime.png", "0x00FD00", "rgb24").unwrap(),
                (171, 42, 27),
            ),
            (
                make("blue.png", "0x0000FE", "rgb24").unwrap(),
                (32, 240, 118),
            ),
            // JPEG: BT.601 full range in the file.
            (
                make("red.jpg", "0xFD0000", "yuvj444p").unwrap(),
                (62, 103, 239),
            ),
        ] {
            let p = still(&file).unwrap();
            assert_eq!((p.layout, p.planes.len()), (Fourcc::Shq2, 3), "{file:?}");
            assert!(near(at(&p), want), "{file:?}: {:?}, want {want:?}", at(&p));
        }
        let p = still(make("bar.png", "0x2040C0BF", "rgba").unwrap()).unwrap();
        assert_eq!((p.layout, p.planes.len()), (Fourcc::Shq7, 4));
        assert!(
            p.planes[3].data.iter().all(|&a| a.abs_diff(191) <= 1),
            "{:?}",
            &p.planes[3].data[..4]
        );
    }

    /// A moving picture with alpha (ProRes 4444's 12-bit 4:4:4 and alpha) keeps it: SHQ7
    /// (a stinger covered the whole picture).
    #[test]
    fn moving_pictures_keep_their_alpha() {
        let (w, h) = (64, 32);
        let src = Frame::alloc(w, h, ff::AVPixelFormat::AV_PIX_FMT_YUVA444P12LE).unwrap();
        // Mid grey, alpha 100 of 255 (12 bits: 100 × 16 + 8).
        for (k, v) in [(0, 2048u16), (1, 2048), (2, 2048), (3, 1608)] {
            let (p, stride) = src.plane(k);
            for r in 0..h {
                for x in 0..w {
                    // SAFETY: inside plane k: w 16-bit samples in each of h rows.
                    unsafe { *(p.add(r * stride) as *mut u16).add(x) = v };
                }
            }
        }
        let (mut sc, mut sd) = (None, None);
        let pf = to_planar(src.as_ref(), &mut sc, &mut sd, None).unwrap();
        assert_eq!((pf.layout, pf.planes.len()), (Fourcc::Shq7, 4));
        assert_eq!((pf.planes[3].width, pf.planes[3].height), (w, h));
        assert!(
            pf.planes[3].data.iter().all(|&a| a.abs_diff(100) <= 1),
            "{:?}",
            &pf.planes[3].data[..4]
        );
        assert_eq!(pf.planes[1].width, w / 2);
    }

    /// The conversion every decoded or captured picture goes through (a screen's BGR0 to
    /// 4:2:2 planes; a file's NV12): BT.709 for RGB, and its time a picture.
    ///   cargo test --release -p idn-ffmpeg --lib -- --ignored --nocapture to_planar_time
    #[test]
    #[ignore]
    fn to_planar_time() {
        use ff::AVPixelFormat::*;
        for (fmt, name) in [
            (AV_PIX_FMT_BGR0, "BGR0 (screen)"),
            (AV_PIX_FMT_NV12, "NV12 (GPU decode)"),
            (AV_PIX_FMT_YUV420P, "I420 (software decode)"),
            (AV_PIX_FMT_P010LE, "P010 (10-bit GPU decode)"),
            (AV_PIX_FMT_YUYV422, "YUYV (webcam)"),
        ] {
            for (w, h) in [(1920, 1080), (3840, 2160)] {
                let src = Frame::alloc(w, h, fmt).unwrap();
                if fmt == AV_PIX_FMT_BGR0 {
                    // Red, for the matrix.
                    let (p, stride) = src.plane(0);
                    for r in 0..h {
                        for x in 0..w {
                            // SAFETY: pixel x of row r of the BGR0 plane (4 bytes each).
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    [0u8, 0, 253, 0].as_ptr(),
                                    p.add(r * stride + x * 4),
                                    4,
                                )
                            };
                        }
                    }
                }
                let (mut sc, mut sd) = (None, None);
                let p = to_planar(src.as_ref(), &mut sc, &mut sd, None).unwrap();
                if fmt == AV_PIX_FMT_BGR0 {
                    let got = (
                        p.planes[0].data[0],
                        p.planes[1].data[0],
                        p.planes[2].data[0],
                    );
                    assert!(
                        got.0.abs_diff(62) <= 1
                            && got.1.abs_diff(103) <= 1
                            && got.2.abs_diff(239) <= 1,
                        "{got:?}"
                    );
                }
                // The process's CPU time (this test runs alone), not only the wall time:
                // the library's converter uses every core.
                let cpu = || {
                    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
                    // After the name: state, …, utime and stime (fields 14 and 15).
                    let f: Vec<&str> = s
                        .rsplit(')')
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .collect();
                    let n = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
                    (n(11) + n(12)) * 10.0
                };
                let n = 60;
                let (t, c) = (Instant::now(), cpu());
                for _ in 0..n {
                    to_planar(src.as_ref(), &mut sc, &mut sd, None).unwrap();
                }
                println!(
                    "{name} {w}x{h}: {:.2} ms a picture, {:.2} ms of CPU",
                    t.elapsed().as_secs_f64() * 1e3 / n as f64,
                    (cpu() - c) / n as f64
                );
            }
        }
    }

    #[test]
    fn mp4_configuration_and_nal_units_become_annex_b() {
        // avcC: version 1, profile/compat/level, length size 4, one SPS, one PPS.
        let avcc = [
            1, 0x64, 0, 0x28, 0xff, 0xe1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9,
        ];
        let (len, params) = parse_config(Codec::H264, &avcc).unwrap();
        assert_eq!(len, Some(4));
        assert_eq!(params, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9]);
        let au = [0, 0, 0, 2, 0x65, 7, 0, 0, 0, 1, 0x06];
        assert_eq!(
            to_annex_b(&au, Some(4)).unwrap(),
            [0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 0x06]
        );
        assert_eq!(to_annex_b(&au[..5], Some(4)), None, "truncated");
        assert_eq!(
            parse_config(Codec::H264, &[0, 0, 0, 1, 0x67]).unwrap().0,
            None
        );
    }
}
