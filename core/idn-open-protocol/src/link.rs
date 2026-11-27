//! `link/1` (spec/extensions/idn-extensions.md §E.11): between two IDN peers, one QUIC
//! connection with real TLS 1.3 carries the control session and the media, always
//! encrypted. The receiver is the QUIC client and connects to the sender's TCP port
//! number on UDP.
//!
//! The security mode is chosen inside the TLS handshake by ALPN (§E.11.4): `s2` (device
//! certificates, `secure/2`), `s1` (the site certificate, `secure/1`) or `open`
//! (encrypted, unauthenticated, each device's own key). The control stream is bridged to
//! a loopback TCP pair, as `secure` does for TLS, so the session code works on a
//! `TcpStream` whatever carries it.

use crate::frame::{Frame, HEADER_LEN, Header, Limits};
use crate::quic::server::MediaKind;
use crate::secure::{Creds, SecureConfig, SiteKey};
use crate::transport::receiver::Sink;
use crate::trust::pki::{DeviceCreds, KeyFile};
use idn_common::sync::LockOk as _;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DistinguishedName, SignatureScheme};
use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

mod conn;
mod site;
mod tls;
pub use conn::*;
pub use site::*;
use tls::*;

mod identity;
mod media;
mod service;
mod site_listen;
mod verify;
pub use media::{announce, offered};
pub use service::{ServiceListener, ServicePending, ServiceTransport};
use verify::*;

/// The extension token (§E.11.2).
pub const TOKEN: &str = "link/1";
const ALPN: &str = "idn-link/1";
const SERVER_NAME: &str = "idn-link";
/// How long a receiver gives QUIC before also trying TCP (§E.11.3).
pub const QUIC_HEAD_START: Duration = Duration::from_millis(250);
/// Liveness (§E.11.6): a vanished peer is noticed in about the time of the TCP path.
const KEEP_ALIVE: Duration = Duration::from_secs(1);
const IDLE_TIMEOUT: Duration = Duration::from_secs(6);
/// How long the sender waits for a handshake, then for the control stream.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Handshakes in progress on one sender at once; more are refused.
const MAX_HANDSHAKES: usize = 64;
/// Trust relays (a site listener in open mode) live at once.
const MAX_TRUST_RELAYS: usize = 256;
/// Handshakes at once from one address on the site listener (it is reached from the
/// internet).
const SITE_HANDSHAKES_PER_ADDRESS: usize = 8;

/// A handshake in progress on the site listener, counted in all and per address until
/// it ends.
struct SiteHandshake {
    busy: Arc<AtomicUsize>,
    per_addr: Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>>,
    ip: std::net::IpAddr,
}

/// Bytes quinn may hold unacknowledged (as `squic`): a receiver behind skips frames.
const SEND_WINDOW: u64 = 2 << 20;
/// Media stream types (§E.11.5).
const STREAM_VIDEO: u8 = 1;
const STREAM_AUDIO: u8 = 2;
/// One video message on a stream of its own (`frames/1`, §E.13.2).
const STREAM_FRAME: u8 = 3;
/// Re-sent encrypted multicast messages (`mcast/1`, §E.17.6): each a u32 length, the
/// multicast stream byte, then the message with its trailer.
const STREAM_REPAIR: u8 = 4;
/// The `frames/1` extension token (§E.13).
pub const FRAMES_TOKEN: &str = "frames/1";

/// Application error codes in CONNECTION_CLOSE (§E.11.6).
pub mod code {
    /// Session over.
    pub const DONE: u32 = 0;
    /// Refused; the reason phrase says why.
    pub const REFUSED: u32 = 1;
    /// Protocol error.
    pub const PROTOCOL: u32 = 2;
    /// Going away (`x_idn_bye` was sent first).
    pub const GOING_AWAY: u32 = 3;
    /// A frame's stream reset because its deadline passed (`frames/1`, §E.13.2).
    pub const LATE: u32 = 4;
}

// ---------------------------------------------------------------------------------------
// Identity and key memory.

/// This device's key for links in mode `open` (§E.11.4), with its self-signed
/// certificate. Enrolled devices use their site-trust key instead (§E.10.1).
pub struct Identity {
    key: KeyFile,
    cert: CertificateDer<'static>,
}

/// Keys of peers seen in mode `open`, per peer name (trust on first use, §E.11.4). A
/// changed key is reported, never refused.
#[derive(Debug)]
pub struct KeyMemory {
    path: Option<PathBuf>,
    keys: Mutex<BTreeMap<String, String>>,
}

/// The IDN link of a device (§E.11): its key for mode `open` and the keys of the peers
/// it has seen. Encryption with a passphrase or site trust comes from the device's
/// [`SecureConfig`].
#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// This device's key for mode `open`.
    pub identity: Arc<Identity>,
    /// The keys of peers seen in mode `open`.
    pub keys: Arc<KeyMemory>,
}

// ---------------------------------------------------------------------------------------
// Modes and TLS.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Site,
    /// Linked computers (§E.28).
    Pair,
    Passphrase,
    Open,
}

/// The mode of one link (§E.11.4).
#[derive(Debug, Clone)]
pub enum Mode {
    /// Encrypted, not authenticated: each side's own key.
    Open,
    /// Authenticated: `secure/1` (passphrase), `secure/2` (device certificates) or
    /// `pair/1` (linked computers, §E.28).
    Secure(Creds),
}

/// What one side can use right now, and what it offers, in preference order.
#[derive(Debug)]
struct Local {
    site: Option<(Arc<DeviceCreds>, Arc<crate::trust::Enrollment>)>,
    pass: Option<Arc<SiteKey>>,
    identity: Arc<Identity>,
    /// The linked computers' fingerprints (§E.28): none in a site.
    pairs: Arc<Vec<String>>,
    /// The membership the links are of (a session's hold on a link is counted there).
    enrollment: Option<Arc<crate::trust::Enrollment>>,
    offers: Vec<(Kind, Vec<u8>)>,
}

/// Checks the peer's certificate: it must belong to a mode this side offers (the exact
/// mode is checked after the handshake, `negotiated`).
#[derive(Debug)]
struct Verifier {
    local: Arc<Local>,
    algs: WebPkiSupportedAlgorithms,
}

const KINDS: [Kind; 4] = [Kind::Site, Kind::Pair, Kind::Passphrase, Kind::Open];

/// The server's check of the client certificate, and the mode hint in its request.
#[derive(Debug)]
struct ClientVerifier {
    inner: Verifier,
    /// The mode the server's certificate resolver chose for this connection.
    chosen: Arc<Mutex<Option<Kind>>>,
    hints: [Vec<DistinguishedName>; 4],
}

/// The client's certificate: the one of the mode the server names in its request.
#[derive(Debug)]
struct ClientCert {
    local: Arc<Local>,
    provider: Arc<CryptoProvider>,
}

/// The server's certificate: the one of the first mode in its own order that the client
/// offered, as rustls selects the ALPN.
#[derive(Debug)]
struct ServerCert {
    local: Arc<Local>,
    provider: Arc<CryptoProvider>,
    chosen: Arc<Mutex<Option<Kind>>>,
}

// ---------------------------------------------------------------------------------------
// The control stream bridge.

// ---------------------------------------------------------------------------------------
// Sender side.

/// A link session accepted by a [`LinkServer`].
pub struct Accepted {
    /// The control stream (plaintext end of the bridge).
    pub control: TcpStream,
    /// The receiver's address.
    pub peer: SocketAddr,
    /// The negotiated mode.
    pub mode: Mode,
    /// Fingerprint of the receiver's key (for the key memory in mode `open`).
    pub peer_fingerprint: String,
    /// The receiver's certificate (DER).
    pub peer_cert: Vec<u8>,
    /// The connection, for the media streams.
    pub conn: LinkConn,
}

/// The sender's handle on one link: opens the media streams, closes with a code.
#[derive(Clone)]
pub struct LinkConn(quinn::Connection);

/// The repair stream of one link (§E.17.6).
pub struct LinkRepairs {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// The media streams of one link: queue messages with [`LinkMedia::send`].
pub struct LinkMedia {
    video: tokio::sync::mpsc::Sender<Vec<u8>>,
    audio: tokio::sync::mpsc::Sender<Vec<u8>>,
    failed: Arc<Mutex<Option<String>>>,
    /// Bytes queued and not yet taken by quinn.
    pending: Arc<AtomicU64>,
    /// Audio bytes queued and not yet written to the connection.
    pending_audio: Arc<AtomicU64>,
    conn: quinn::Connection,
    /// The lowest smoothed round trip seen on this connection (µs): the path's delay
    /// without queueing, which [`queueing`] measures against.
    min_rtt_us: AtomicU64,
    /// Video skipping: the accumulated share of frames to skip, and when the last video
    /// message was accepted.
    pace: Mutex<(f64, Instant)>,
    /// Per-frame streams: the deadline (µs) and the frames abandoned at it.
    deadline_us: Arc<AtomicU64>,
    resets: Arc<AtomicU64>,
}

/// The sender's link endpoint: QUIC on UDP at the sender's TCP port number (§E.11.1).
pub struct LinkServer {
    endpoint: quinn::Endpoint,
    port: u16,
}

// ---------------------------------------------------------------------------------------
// Receiver side.

/// A link session the receiver opened.
pub struct LinkClient {
    /// The control stream (plaintext end of the bridge).
    pub control: TcpStream,
    /// The negotiated mode.
    pub mode: Mode,
    /// Fingerprint of the sender's key (for the key memory in mode `open`).
    pub peer_fingerprint: String,
    /// The sender's certificate (DER).
    pub peer_cert: Vec<u8>,
    conn: quinn::Connection,
    /// Per-frame streams: frames that completed after a newer one was delivered.
    late: Arc<AtomicU64>,
    /// Why the connection ended, as first seen.
    first_close: Arc<std::sync::OnceLock<quinn::ConnectionError>>,
    /// Set by the receiver when the session this link carried has ended or been replaced:
    /// what its reader still hands over belongs to no session (an input's own flag, see
    /// `Input::link_sink`).
    pub ended: Arc<std::sync::atomic::AtomicBool>,
}

/// Makes every link server ignore incoming connections, as if UDP were blocked. For
/// testing the fallback.
#[doc(hidden)]
pub static SIMULATE_BLOCKED: AtomicBool = AtomicBool::new(false);

/// Makes every link server wait this long before answering a handshake (0: no wait), as
/// on a slow or lossy network. For testing the move from TCP to the link.
#[doc(hidden)]
pub static SIMULATE_SLOW_MS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------------------
// The site link (roadmap R.18, spec/extensions/idn-remote.md): the same QUIC and the same
// trust as the IDN link, for a connection between two sites of one site trust. Only site
// certificates (secure/2) are offered and taken; ALPN `idn-site/1`. What it carries is
// the caller's: messages on streams, blocking calls for code outside the runtime.

const SITE_ALPN: &str = "idn-site/1";

/// A site link: an authenticated QUIC connection to a machine of the same site.
#[derive(Clone)]
pub struct SiteConn {
    conn: quinn::Connection,
    /// The peer's name, from its site certificate.
    pub peer_name: String,
    /// The peer's key fingerprint.
    pub peer_fingerprint: String,
    /// The peer's address as seen here.
    pub peer: SocketAddr,
    cert: Arc<Vec<u8>>,
    enrollment: Option<Arc<crate::trust::Enrollment>>,
}

/// The sending end of a stream, carrying messages (a 4-byte length, then the bytes).
pub struct SiteSend(quinn::SendStream);
/// The receiving end of a stream of messages.
pub struct SiteRecv(quinn::RecvStream);

/// The largest message on a site link (a 4K keyframe fits).
const SITE_MAX_MESSAGE: usize = 64 << 20;

/// The ALPN of trust connections relayed through a site gateway (X.12.2).
const TRUST_ALPN: &str = "idn-trust/1";

/// Trust connections relayed at once, from one remote address and in all. Relayed
/// connections reach the authority's Discovery Server from this machine's address, where
/// they share its per-address limit (128) with this machine's own programs: without these
/// caps anyone who can reach the gateway (open mode, port-forwarded) could keep it full,
/// shutting out the remote site's machines and this machine's programs alike.
const RELAYS_PER_ADDRESS: usize = 48;
const RELAYS: usize = 96;

static RELAYING: Mutex<BTreeMap<std::net::IpAddr, usize>> = Mutex::new(BTreeMap::new());

/// One relayed trust connection, counted while it lasts.
struct Relaying(std::net::IpAddr);

/// The main site's end: accepts site links on a UDP port.
pub struct SiteListener {
    endpoint: quinn::Endpoint,
    port: u16,
}

#[cfg(test)]
mod tests;
