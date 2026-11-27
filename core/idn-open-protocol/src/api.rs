//! Sending: named outputs that other devices see as sources.
//!
//! Create one [`Publisher`] per process, then one [`Output`] per source you want to
//! offer: program, lower third, stage display and so on. Feed each output frames with
//! [`Output::send`], which never blocks. If you send faster than the output can encode,
//! the newest frame wins and older ones are dropped ([`OutputStats::replaced`]).
//! Re-sending an identical frame (a static slide) costs almost nothing, because the
//! previous encoding is reused.
//!
//! Each output appears on the network as `HOST (name)` (see [`Output::name`]) and
//! serves any number of receivers, NDI or IDN.
//!
//! ```no_run
//! use idn_open_protocol::api::{Publisher, PublisherConfig, Output, OutputConfig, VideoFrame};
//! use idn_open_protocol::color::AlphaMode;
//! let publisher = Publisher::new(PublisherConfig::default())?;
//! let program = Output::new(&publisher, OutputConfig::new("Program"))?;
//! let rgba = vec![0u8; 1920 * 1080 * 4];
//! program.send(VideoFrame::rgba(1920, 1080, rgba.into(), AlphaMode::Opaque))?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use crate::color::{AlphaMode, ColorConfig, rgba_to_planes};
use crate::discovery::responder::{HostInfo, Listing, Published, Responder};
use crate::sender::{MediaFrame, Sender, SenderConfig};
use crate::shq::{Fourcc, Plane};
use idn_common::sync::LockOk as _;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod pipeline;
mod router;
use pipeline::*;

mod output_audio;
mod output_gpu;

mod clock;
mod config;
mod frames;
mod output_send;
mod output_start;
pub use clock::timestamp_now;
use clock::*;
pub use config::{OutputConfig, PublisherConfig};
use frames::*;
pub use frames::{
    AudioBlock, OutputStats, PlanarFrame, ReceiverMessage, SendError, TappedFrame, VideoFrame,
};

struct PublisherInner {
    /// The machine name the IDN Node sets, used in `HOST (name)` instead of the host name.
    display_host: Option<String>,
    host: HostInfo,
    responder: Option<Responder>,
    /// The listing port without mDNS (the responder serves it otherwise).
    listing: Option<Listing>,
    server: Option<SocketAddr>,
    groups: Vec<String>,
    secure: Option<crate::secure::SecureConfig>,
    monitoring: bool,
    link: Option<crate::link::LinkConfig>,
    unlisted: bool,
}

/// Announces this process's outputs. Create one and pass it to every [`Output::new`].
///
/// Cheap to clone; all clones share the same announcement. Outputs keep it alive.
#[derive(Clone)]
pub struct Publisher(Arc<PublisherInner>);

/// Keeps an [`Output::tap`] running; drop it to stop.
pub struct Tap {
    sender: Arc<Sender>,
    id: u64,
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.sender.untap(self.id);
    }
}

struct Shared {
    /// The next frame for the worker, with when it was handed over and how many of its
    /// audio samples (per channel) came from frames it replaced.
    slot: Mutex<Option<(VideoFrame, Instant, usize)>>,
    ready: Condvar,
    stop: AtomicBool,
    submitted: AtomicU64,
    replaced: AtomicU64,
    repeated: AtomicU64,
    /// Size of the latest frame, for monitoring.
    size: Mutex<(usize, usize)>,
    /// The timecode following the last audio sent with [`Output::send_audio`].
    audio_timecode: Mutex<Option<AudioClock>>,
    /// Timecode zero: video and audio timecodes both count from here (see [`clock_tc`]).
    epoch: Instant,
    /// Alpha and frame rate, changeable while running.
    format: Mutex<LiveFormat>,
    /// Closed captions for the next picture ([`Output::caption_next`]).
    captions: Mutex<Option<Vec<u8>>>,
}

/// The settings of an output that can change without receivers reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiveFormat {
    alpha: bool,
    frame_rate: (u32, u32),
}

impl LiveFormat {
    fn fourcc(self) -> Fourcc {
        if self.alpha {
            Fourcc::Shq7
        } else {
            Fourcc::Shq2
        }
    }

    /// One frame in 100 ns units, rounded.
    fn frame_dur(self) -> u64 {
        let (n, d) = self.frame_rate;
        (10_000_000u64 * d as u64 + n as u64 / 2) / n.max(1) as u64
    }
}

/// A named source on the network.
///
/// [`send`](Output::send) never blocks. Dropping the output withdraws it: receivers are
/// told, and it disappears from source lists at once. Use [`close`](Output::close) to
/// say why, e.g. for a restart.
pub struct Output {
    full_name: String,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    sender: Arc<Sender>,
    publisher: Publisher,
    _registration: Option<crate::discovery::ds::SourceRegistration>,
    /// Live status on the Discovery Server's monitoring channel, and its updater.
    advertiser: Option<crate::discovery::ds::Advertiser>,
    status: Option<JoinHandle<()>>,
    /// Listed in the IDN Node's panel while it lives, when encryption comes from the Node.
    _node: Option<crate::node::Registration>,
    /// Its levels for the Node's routing matrix, while one watches (node/meters.rs).
    meter: Option<std::sync::Arc<crate::node::meters::Meter>>,
    /// The sound channels told to the Node (0: none yet), and the labels to tell with them
    /// (the routing matrix, idn-routing.md §A.3).
    node_channels: std::sync::atomic::AtomicU32,
    audio_labels: Vec<String>,
    /// Its pictures' colour, as configured ([`OutputConfig::color_info`]): what a recorder
    /// of its tap tags its files with (an HDR program's BT.2100).
    color: crate::ext::ColorInfo,
}

/// The longest full name (`HOST (name)`) an output can have, in bytes: one DNS label
/// (§5.1.2). Longer names would be published cut short, under a name that doesn't match.
pub const MAX_FULL_NAME: usize = 63;

/// The previous RGBA frame (buffer, width, height, stride, alpha) and its planes.
type LastFrame = (
    Arc<[u8]>,
    usize,
    usize,
    usize,
    AlphaMode,
    Fourcc,
    Arc<Vec<Plane>>,
);

/// The timeline of audio sent on its own: `base` plus `samples` at `rate`.
#[derive(Debug, Clone, Copy)]
struct AudioClock {
    base: i64,
    samples: u64,
    rate: u32,
}

/// What the output worker keeps from one frame to the next.
#[derive(Default)]
struct WorkerState {
    /// The next timecode if frames keep coming at the frame rate (see `clock_tc`).
    expected: Option<i64>,
    content_id: u64,
    last: Option<LastFrame>,
}

/// Marks the output stopped if the worker thread ends by a panic, so that `send` reports
/// [`SendError::Closed`] instead of accepting frames nobody will send.
struct StopOnPanic<'a>(&'a Shared);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.stop.store(true, Ordering::Relaxed);
        }
    }
}

// Outputs and publishers are shared between threads by applications.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Output>();
    assert_send_sync::<Publisher>();
};

/// A router: a source with no media of its own whose receivers are sent to another
/// source, and follow it to the next one when the router is switched, without being
/// told anything themselves (a routing source, §1.8). IDN inputs and NDI receivers follow
/// it, so a screen showing "LOBBY (Feed)" shows whatever the feed is switched to.
///
/// ```no_run
/// # use idn_open_protocol::api::{Publisher, PublisherConfig, Router};
/// let publisher = Publisher::new(PublisherConfig::default())?;
/// let feed = Router::new(&publisher, "Feed")?;
/// feed.route(Some("BOOTH (Program)"));
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Router {
    output: Output,
    route: Arc<crate::sender::Route>,
    wanted: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    finder: Option<JoinHandle<()>>,
}

impl Drop for Router {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.finder.take() {
            // Woken from its wait, not polled for up to half a second.
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests;
