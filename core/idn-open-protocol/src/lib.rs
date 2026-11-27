//! # IDN Open Protocol
//!
//! Send and receive live video, audio and metadata on a local network: NDI-Compatible
//! sources and receivers that work with NDI® 6 senders and receivers. **IDN** stands for
//! Intelligent Device Network.
//!
//! This crate is the video I/O layer for an application such as a presentation server.
//! It gives you:
//!
//! - **Outputs**: named sources (program, lower third, stage display…) that you feed
//!   RGBA or 16-bit Y'CbCr frames without ever blocking your render loop. See [`api`].
//! - **Inputs**: cameras and other software, delivered as decoded pictures, or as
//!   H.264/HEVC access units for your own decoder, with automatic reconnection. See
//!   [`input`].
//! - **A source list** for your UI, live and with add/remove events. See [`discovery`].
//! - **Optional encryption** between IDN devices that share a site passphrase. See
//!   [`secure`].
//! - **Clean shutdown** so sources disappear from other apps' lists at once. See
//!   [`shutdown`].
//!
//! NDI devices work with all of it unchanged. Between IDN devices, extra features switch
//! on automatically: encryption, faster reconnects, health statistics, lossless audio,
//! and 10-bit/HDR signalling ([`ext`]).
//!
//! ## Quick start
//!
//! Publish an output and watch it from an input:
//!
//! ```no_run
//! use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
//! use idn_open_protocol::color::AlphaMode;
//! use idn_open_protocol::input::{Input, InputConfig, InputEvent};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! # fn main() -> std::io::Result<()> {
//! // Sending: one publisher per process, one output per named source.
//! let publisher = Publisher::new(PublisherConfig::default())?;
//! let program = Output::new(&publisher, OutputConfig::new("Program"))?;
//! let (w, h) = (1920, 1080);
//! let pixels: Arc<[u8]> = vec![0u8; w * h * 4].into();
//! program.send(VideoFrame::rgba(w, h, pixels, AlphaMode::Opaque)).ok();
//!
//! // Receiving: by full source name ("HOST (name)") or "ip:port".
//! let input = Input::open(InputConfig::new("STUDIO-PC (Camera 1)"))?;
//! while let Some(event) = input.recv(Duration::from_millis(100)) {
//!     match event {
//!         InputEvent::Video(v) => println!("{}x{} picture", v.picture.width, v.picture.height),
//!         InputEvent::State(s) => println!("connection: {s:?}"),
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The integration guide (`docs/INTEGRATION.md` in the repository) covers threading,
//! performance, encryption, 10-bit/HDR and a network checklist. Its code is built as
//! `examples/integration.rs`.
//!
//! ## Threads and errors
//!
//! Every output, input and browser runs on its own background threads. Nothing in the
//! library blocks the caller's loop, panics on bad network input, or prints: errors are
//! returned as values or delivered as events. Install a logger with [`log::set_logger`]
//! to see diagnostics.
//!
//! ## Stability
//!
//! The modules listed here are the public API. Other modules exist for the bundled tools
//! (`idn-send`, `idn-recv`, `idn-doctor`…) and tests; they are hidden from this
//! documentation and may change without notice.
//!
//! NDI® is a registered trademark of Vizrt NDI AB. This project is not affiliated with
//! or endorsed by Vizrt. It was written clean-room from a specification, without vendor
//! code.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

/// This library's version (`major.minor.patch`), e.g. for an "about" box or logs. The
/// NDI SDK version announced to other devices is separate:
/// [`control::SDK_VERSION`].
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build's tag, set by whoever packages it (`IDN_BUILD=hub2 cargo build …`), so a
/// person can tell builds apart: "dev" when unset.
pub const BUILD: &str = match option_env!("IDN_BUILD") {
    Some(b) => b,
    None => "dev",
};

/// When the build was packaged (`IDN_BUILD_DATE`, e.g. `2026-10-24 16:43`), if given.
pub const BUILD_DATE: Option<&str> = option_env!("IDN_BUILD_DATE");

/// `Build hub2 (2026-10-24 16:43)`: the line tools, the plugin and the Node show for
/// their version.
pub fn build_line() -> String {
    match BUILD_DATE {
        Some(d) => format!("Build {BUILD} ({d})"),
        None => format!("Build {BUILD}"),
    }
}

/// How many threads encode, convert and decode each picture, the calling thread included.
/// One pool serves every output and input in the process.
pub fn worker_threads() -> usize {
    pool::threads()
}

/// Sets how many threads encode, convert and decode each picture (at most 64). Takes
/// effect from the next picture; can be changed at any time, e.g. from a settings page.
/// More threads make each large picture faster (a 4K picture needs about six) at the cost
/// of a little more CPU time in total. 0 hands the choice back: the environment variable
/// `IDN_WORKER_THREADS` if set, else the machine's setting in the IDN Node
/// ([`node::video`]), else [`default_worker_threads`].
pub fn set_worker_threads(n: usize) {
    pool::set_threads(n)
}

/// The automatic number of threads: one per physical core, at most six.
pub fn default_worker_threads() -> usize {
    pool::default_threads()
}

/// `f()` and the CPU time it took, on this thread and on the worker threads it handed
/// work to: what a piece of work really costs, however many threads shared it.
pub fn metered_cpu<T>(f: impl FnOnce() -> T) -> (T, std::time::Duration) {
    pool::metered(f)
}

/// CPU time the calling thread has used so far (user and system).
pub fn thread_cpu_time() -> std::time::Duration {
    pool::thread_cpu()
}

/// CPU time the whole process has used so far (user and system): for work done on
/// threads that aren't the library's (FFmpeg's decoders).
pub fn process_cpu_time() -> std::time::Duration {
    pool::process_cpu()
}

pub mod api;
pub mod audio;
pub mod backup;
pub mod cea608;
pub mod channel;
pub mod codec;
pub mod color;
pub mod diag;
pub mod discovery;
pub mod ext;
pub mod hash;
pub mod input;
pub mod library;
pub mod link;
pub mod localtime;
pub mod log;
pub mod mcast;
pub mod node;
pub mod osc;
pub mod ptp;
pub mod random;
pub mod release;
pub mod remote;
pub mod secure;
pub mod shutdown;
pub mod simple;
pub mod sync;
pub mod text;
pub mod trust;
pub mod update;

// Internals used by the bundled tools and tests; not part of the public API.
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod control;
#[doc(hidden)]
pub mod frame;
/// Every environment variable the programs read ([`idn_common::switches`]).
pub use idn_common::switches;
/// Pictures on the GPU between programs on one machine (`gpu/1`, §E.27).
#[cfg(feature = "gpu")]
pub mod gpu;
#[doc(hidden)]
pub mod hx;
mod local;
#[doc(hidden)]
pub mod machine;
#[doc(hidden)]
pub mod media;
pub mod mem;
#[doc(hidden)]
pub mod pool;
#[doc(hidden)]
pub mod quic;
#[doc(hidden)]
pub mod regions;
/// RGB pictures (a screen's) made planar Y'CbCr on the GPU (§E.27).
#[cfg(feature = "gpu")]
pub mod rgb_gpu;
pub mod scale;
#[doc(hidden)]
pub mod scramble;
#[doc(hidden)]
pub mod sender;
#[doc(hidden)]
pub mod shq;
#[doc(hidden)]
pub mod squic;
pub mod thin;
#[doc(hidden)]
pub mod transport;
#[doc(hidden)]
pub mod web;
#[doc(hidden)]
pub mod xml;
