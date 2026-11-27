//! `channel/1` (spec/extensions/idn-extensions.md §E.12): commands, requests and files
//! between IDN devices enrolled with site trust, with no video connection needed.
//!
//! ```no_run
//! use idn_open_protocol::channel::{Channel, ChannelConfig, Handler, Peer, Reply};
//! # use std::sync::Arc;
//! struct App;
//! impl Handler for App {
//!     fn on_request(&self, from: &Peer, kind: &str, payload: Vec<u8>) -> Reply {
//!         match kind {
//!             "presenter.next-slide" => Reply::Ok(Vec::new()),
//!             _ => Reply::Unknown,
//!         }
//!     }
//! }
//! # fn run(enrollment: Arc<idn_open_protocol::trust::Enrollment>) -> std::io::Result<()> {
//! let channel = Channel::start(ChannelConfig::new(enrollment), Arc::new(App))?;
//! for d in channel.devices() {
//!     println!("{} ({})", d.name, d.kind);
//! }
//! # Ok(())
//! # }
//! # fn main() {}
//! ```
//!
//! Any approved device may send any message to any other; the receiving application
//! decides what it accepts ([`Peer`] says who sent it). Received files go wherever the
//! application decides ([`FileSink`]; [`PathSink`] keeps them as plain files).

use crate::trust::{DirEntry, Enrollment};
use crate::xml::Element;
use idn_common::sync::LockOk as _;
use quinn::congestion::{Controller, ControllerFactory};
use sha2::Digest;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

mod protect;
mod streams;
pub(crate) use protect::*;
use streams::*;

mod calls;
mod conns;
mod files;
use conns::*;
use files::*;

/// ALPN of the channel (§E.12.3).
pub const ALPN: &[u8] = b"idn-channel/1";
/// Longest message type, in bytes (§E.12.7).
pub const MAX_TYPE: usize = 128;
/// Largest message payload (§E.12.7); larger data goes as a file.
pub const MAX_PAYLOAD: usize = 16 << 20;
/// Longest file id, in bytes.
pub const MAX_FILE_ID: usize = 256;
/// File transfers this device serves at once (§E.12.7).
const MAX_TRANSFERS: usize = 16;
const OP_MESSAGE: u8 = 1;
const OP_REQUEST: u8 = 2;
const OP_FETCH: u8 = 3;
/// Reply status (§E.12.4–E.12.5).
const ST_OK: u8 = 0;
const ST_ERROR: u8 = 1;
const ST_UNKNOWN: u8 = 2;
const ST_REFUSED: u8 = 3;
const ST_CHANGED: u8 = 4;
/// Connections with no stream for this long are closed (§E.12.3).
const IDLE_CLOSE: Duration = Duration::from_secs(300);
const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// An announced endpoint that didn't answer isn't tried again for this long while the
/// device's other endpoints are connected (each call waited [`CONNECT_TIMEOUT`] for it
/// first, bug run 2026-11-24).
const OPEN_RETRY: Duration = Duration::from_secs(30);
/// File data is read and sent in pieces of this size.
const CHUNK: usize = 64 << 10;
/// Bytes read from a served file per blocking read.
const READ_AHEAD: usize = 1 << 20;

/// Who a message came from, from its device certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The device's key fingerprint (§E.10.1): its identity.
    pub fingerprint: String,
    /// Its name, as enrolled.
    pub name: String,
    /// `output`, `input` or `app`.
    pub kind: String,
    /// It holds the administrator role.
    pub admin: bool,
}

/// The answer to a request (§E.12.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Done; the reply's payload.
    Ok(Vec<u8>),
    /// Failed, with an explanation.
    Error(String),
    /// No application on the device handles this type.
    Unknown,
    /// The application refused it.
    Refused,
}

/// A file's description (§E.12.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// The id the holder knows it by.
    pub id: String,
    /// Its name, for display and as a suggestion when saving.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the contents, 64 lower-case hex digits.
    pub version: String,
}

/// Where a fetched file goes. Storage is the application's (§E.12.5);
/// [`PathSink`] is a ready-made one.
pub trait FileSink: Send {
    /// What is held from an earlier attempt, if anything: (bytes, the version they are
    /// of). The transfer resumes after them.
    fn held(&mut self) -> io::Result<Option<(u64, String)>>;
    /// Drops what is held: the file changed since.
    fn restart(&mut self) -> io::Result<()>;
    /// A transfer of `info` starts (or resumes): keep its version, to resume later.
    #[allow(unused_variables)]
    fn begin(&mut self, info: &FileInfo) -> io::Result<()> {
        Ok(())
    }
    /// The next bytes, following what is held.
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// All bytes are there. Check `info.version` (SHA-256 of the whole file) and make the
    /// file available; an error means the transfer failed.
    fn finish(&mut self, info: &FileInfo) -> io::Result<()>;
}

/// A [`FileSink`] writing to a file: `path.part` (and `path.part.version`) until
/// complete and checked, then renamed to `path`. An interrupted transfer resumes from
/// the `.part` file.
pub struct PathSink {
    path: PathBuf,
    part: Option<std::fs::File>,
}

/// Opens `path` without following a link at its end, refusing anything but a plain file
/// (and, on Unix, one of this account's when `ours`): in a folder others may write in (the
/// Node's shared media folder), a planted link made the service write or read through it.
/// A file's identity (device and inode) on Unix: the same file, whatever its path leads
/// to later. `None` elsewhere (and when it can't be read).
type FileIdentity = (u64, u64);

/// A file offered with [`Channel::push`]: where, since when, and which file it was.
type Offer = (PathBuf, Instant, Option<FileIdentity>);

/// What the application does with incoming messages, requests and files. Every method
/// has a default that ignores or refuses.
pub trait Handler: Send + Sync + 'static {
    /// A one-way message.
    #[allow(unused_variables)]
    fn on_message(&self, from: &Peer, kind: &str, payload: Vec<u8>) {}

    /// A request; the answer goes back to the sender.
    #[allow(unused_variables)]
    fn on_request(&self, from: &Peer, kind: &str, payload: Vec<u8>) -> Reply {
        Reply::Unknown
    }

    /// The file a peer fetches by `id`, if this device offers it (files offered with
    /// [`Channel::push`] are served without asking).
    #[allow(unused_variables)]
    fn file(&self, from: &Peer, id: &str) -> Option<PathBuf> {
        None
    }

    /// A peer offers a file (a push). Return where to keep it to accept: the library
    /// fetches it into that sink and reports the result to [`Handler::on_received`].
    #[allow(unused_variables)]
    fn on_push(&self, from: &Peer, offer: &FileInfo) -> Option<Box<dyn FileSink>> {
        None
    }

    /// A pushed file arrived (or the transfer failed).
    #[allow(unused_variables)]
    fn on_received(&self, from: &Peer, offer: &FileInfo, result: io::Result<()>) {}

    /// How a fetch of file `id` that this device serves (offered with [`Channel::push`],
    /// or [`Handler::file`]'s) goes for peer `to`: progress as the bytes go, then whether
    /// the peer took the whole file (§E.12.5). Called on a blocking thread; keep it short.
    #[allow(unused_variables)]
    fn on_served(&self, to: &Peer, id: &str, served: Served) {}
}

/// A served file's fetch, as [`Handler::on_served`] hears of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// `sent` of the file's `size` bytes are on their way (counted from the file's start:
    /// a resumed fetch starts where it resumed).
    Progress {
        /// Bytes on their way so far.
        sent: u64,
        /// The file's size.
        size: u64,
    },
    /// The peer read the whole file (`size` bytes) to its end, acknowledged.
    Done {
        /// The file's size.
        size: u64,
    },
    /// Serving it failed or the peer stopped it: why.
    Failed(String),
}

/// How long a file offered with [`Channel::push`] stays fetchable (resuming an interrupted
/// transfer included).
pub const OFFER_LIFE: Duration = Duration::from_secs(6 * 3600);

/// Settings for [`Channel::start`].
#[derive(Clone)]
pub struct ChannelConfig {
    /// This device's site membership: the channel works only once it is enrolled.
    pub enrollment: Arc<Enrollment>,
    /// UDP port to listen on; 0 (the default) picks one (the directory tells peers).
    pub port: u16,
    /// Cap on the file data this device sends, in bytes per second, all transfers
    /// together: 50 MB/s is `Some(50_000_000)`. `None` (the default): as fast as the
    /// network allows, still backing off for live video (§E.12.5). Can be changed later
    /// with [`Channel::set_max_rate`].
    pub max_rate: Option<u64>,
    /// How transfers react when live video reports trouble. Default:
    /// [`TroubleResponse::PerDevice`]. Can be changed later with
    /// [`Channel::set_trouble_response`].
    pub on_video_trouble: TroubleResponse,
    /// Where the status (§E.12.6) is advertised as a `metadata` receiver entry with
    /// `x-idn-*` properties: `None` (the default) is the trust authority's Discovery
    /// Server; a site that discovers through another server (an NDI 6.3 one) names it
    /// here so that its monitors see the status too.
    pub status_server: Option<String>,
    /// Only reaches out: this endpoint isn't announced in the directory, so peers can't
    /// open a connection to it (a program on a machine whose IDN Node holds the machine's
    /// endpoint, idn-node.md §N.25; the site's media library, §E.12.11). Requests,
    /// messages and fetches it starts work as usual.
    pub client_only: bool,
}

/// `channel.max_rate_mbps`: `off` → no cap; a number → bytes per second.
fn site_max_rate(site: &crate::trust::Resolved) -> Option<Option<u64>> {
    match site.get("channel.max_rate_mbps")? {
        "off" => Some(None),
        v => v
            .parse::<f64>()
            .ok()
            .filter(|m| m.is_finite() && *m >= 0.0)
            .map(|m| Some((m * 1e6 / 8.0) as u64)),
    }
}

fn site_trouble(site: &crate::trust::Resolved) -> Option<TroubleResponse> {
    Some(match site.get("channel.trouble")? {
        "all" => TroubleResponse::AllTransfers,
        "off" => TroubleResponse::Off,
        "per-device" => TroubleResponse::PerDevice,
        _ => return None,
    })
}

/// How file transfers react when a live video session reports trouble: lost packets, or
/// frames skipped because the receiver is behind (§E.12.5). A slowed transfer runs at
/// half the speed it had, until 5 s after the last trouble report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TroubleResponse {
    /// Slow only the transfers with the device whose video is in trouble.
    #[default]
    PerDevice,
    /// Slow all of this device's transfers, whichever device the trouble is with (they
    /// often share the same network links).
    AllTransfers,
    /// Never slow transfers for video trouble. The background congestion control still
    /// backs off when the network gets busy.
    Off,
}

impl TroubleResponse {
    fn to_u8(self) -> u8 {
        match self {
            TroubleResponse::PerDevice => 0,
            TroubleResponse::AllTransfers => 1,
            TroubleResponse::Off => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => TroubleResponse::AllTransfers,
            2 => TroubleResponse::Off,
            _ => TroubleResponse::PerDevice,
        }
    }
}

impl ChannelConfig {
    /// Fills in what the site sets for this device (`options/1`, §E.20):
    /// `channel.max_rate_mbps` and `channel.trouble`. Call it first, then set the
    /// user's own values: local wins.
    pub fn apply_site(&mut self, site: &crate::trust::Resolved) {
        if let Some(r) = site_max_rate(site) {
            self.max_rate = r;
        }
        if let Some(t) = site_trouble(site) {
            self.on_video_trouble = t;
        }
    }

    /// Defaults for `enrollment`.
    pub fn new(enrollment: Arc<Enrollment>) -> Self {
        ChannelConfig {
            enrollment,
            port: 0,
            max_rate: None,
            on_video_trouble: TroubleResponse::PerDevice,
            status_server: None,
            client_only: false,
        }
    }
}

struct Conn {
    conn: quinn::Connection,
    /// The peer's certificate, checked again as new CRLs arrive.
    cert: Vec<u8>,
    /// Opened by this device (for the duplicate rule, §E.12.3).
    outgoing: bool,
    /// The peer endpoint's UDP port. A device may run more than one endpoint (its IDN
    /// Node's, announced, and a program's that only reaches out): connections to
    /// different ones are kept side by side, and the duplicate rule applies only between
    /// connections to the same one (§E.12.3).
    port: u16,
    last_used: Arc<Mutex<Instant>>,
}

/// At most this many endpoints of one device are kept connected; more: the least
/// recently used connection goes.
const MAX_ENDPOINTS: usize = 8;

/// The status this device reports (§E.12.6).
#[derive(Default)]
struct Status {
    app: String,
    activity: String,
    fields: Vec<(String, String)>,
    advertiser: Option<crate::discovery::ds::Advertiser>,
    /// The field names the advertiser declared.
    declared: Vec<String>,
}

/// A served file as last hashed: (path, size, modification time).
type FileKey = (PathBuf, u64, Option<std::time::SystemTime>);

/// This device's channel endpoint (§E.12). Runs until dropped.
pub struct Channel {
    /// This channel, for the tasks it starts.
    me: Weak<Channel>,
    endpoint: quinn::Endpoint,
    enrollment: Arc<Enrollment>,
    /// The port it announced to the site's directory (not a client-only one).
    announced: Option<u16>,
    my_fp: String,
    handler: Arc<dyn Handler>,
    /// Live connections by device fingerprint: one per endpoint of that device.
    conns: Mutex<BTreeMap<String, Vec<Conn>>>,
    /// Files offered with `push`, by id.
    /// Files offered with [`Channel::push`], and since when: withdrawn after
    /// [`OFFER_LIFE`] (they stayed for the channel's life).
    offers: Mutex<BTreeMap<String, Offer>>,
    /// Announced endpoints that didn't answer, by device fingerprint, and when.
    open_failed: Mutex<BTreeMap<String, Instant>>,
    /// The shared cap (bytes per second, 0: none) and its bucket.
    max_rate: AtomicU64,
    cap: Arc<TokenBucket>,
    /// [`TroubleResponse`], as stored by `to_u8`.
    trouble: std::sync::atomic::AtomicU8,
    serving: Arc<tokio::sync::Semaphore>,
    status: Mutex<Status>,
    status_server: Option<String>,
    /// Hashes of files served, by (path, size, modification time).
    hashes: Mutex<BTreeMap<FileKey, String>>,
}

#[cfg(test)]
mod tests;
