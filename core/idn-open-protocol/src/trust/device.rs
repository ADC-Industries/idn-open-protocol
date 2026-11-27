//! The device side of site trust (§E.10.2–§E.10.7): identity, joining, the pinned CA,
//! the cached CRL, and closing sessions with revoked peers.

use super::pki::{Anchor, DeviceCreds, KeyFile};
use super::proto::{self, b64, unb64};
use crate::secure::SiteKey;
use crate::xml::Element;
use idn_common::err::other as err;
use idn_common::sync::LockOk as _;
use idn_common::sync::RwLockOk as _;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

mod connect;
mod directory;
mod join;
mod keys;
mod lan;
mod node;
mod pairs;
mod state;
pub use pairs::PairLease;

/// What this device is, as shown in the authority's UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceRole {
    /// An application with outputs and/or inputs (the usual case).
    #[default]
    App,
    /// Only sends.
    Output,
    /// Only receives.
    Input,
}

impl DeviceRole {
    fn as_str(self) -> &'static str {
        match self {
            DeviceRole::App => "app",
            DeviceRole::Output => "output",
            DeviceRole::Input => "input",
        }
    }
}

/// Settings for [`Enrollment::start`].
#[derive(Debug, Clone)]
pub struct EnrollmentConfig {
    /// The IDN Discovery Server that hosts the site authority (`host` or `host:port`).
    pub discovery_server: String,
    /// Where this device keeps its key, certificate, the site CA and revocation list.
    pub state_dir: PathBuf,
    /// The name the authority shows, e.g. `BOOTH-PC (Presentation)`.
    pub name: String,
    /// App, output or input.
    pub role: DeviceRole,
    /// The site passphrase: if set, the device joins without anyone approving it (and can
    /// still be revoked on its own). Also enables `secure/1` with devices outside the site.
    pub passphrase: Option<String>,
    /// The authority's one-time admin code, to become an administrator.
    pub admin_code: Option<String>,
}

/// A site audio transmitter as its authority lists it (§E.12.12).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteTx {
    pub name: String,
    pub channels: u16,
    /// The machine sending it.
    pub machine: String,
    /// Where its sound comes from, as the sending program said (§A.16); `None`: not said.
    pub origin: Option<crate::node::routing::AudioOrigin>,
}

/// A device's channel endpoint, from the authority's directory (§E.12.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The device's key fingerprint (§E.10.1).
    pub fingerprint: String,
    /// Its name, as enrolled.
    pub name: String,
    /// `output`, `input` or `app`.
    pub kind: String,
    /// It holds the administrator role.
    pub admin: bool,
    /// Where its channel endpoint listens.
    pub address: SocketAddr,
}

impl DirEntry {
    fn parse(e: &Element) -> Option<Self> {
        let ip: std::net::IpAddr = e.attr("address")?.parse().ok()?;
        let port: u16 = e.attr("port")?.parse().ok()?;
        Some(DirEntry {
            fingerprint: e.attr("fp")?.to_owned(),
            name: e.attr("name").unwrap_or("").to_owned(),
            kind: e.attr("kind").unwrap_or("app").to_owned(),
            admin: e.attr("admin") == Some("true"),
            address: SocketAddr::new(ip, port),
        })
    }
}

/// The machine settings held by the IDN Node (see [`Enrollment::node_machine`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeMachine {
    /// The device name, the `HOST` in `HOST (name)`.
    pub name: Option<String>,
    /// The Discovery Server.
    pub discovery_server: Option<String>,
    /// The network interface (an IPv4 address of the machine).
    pub interface: Option<std::net::Ipv4Addr>,
}

/// Where a device stands with the site authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// Contacting the authority.
    Starting,
    /// The Discovery Server isn't an IDN site authority (an NDI server, or trust not
    /// enabled). The device keeps using the passphrase (`secure/1`) if one is set.
    NoAuthority,
    /// The authority's pairing window is closed: open it there ("Add device"). The device
    /// keeps asking every few seconds.
    WaitingForPairing,
    /// Waiting for approval. Show `code` ([`show_code`](super::show_code)): it must match
    /// the code the authority shows.
    Pending {
        /// The confirmation code: six digits, e.g. `482913` (shown `482 913`).
        code: String,
    },
    /// Approved by the authority, waiting for this device's person to confirm that the
    /// authority shows `code` too ([`Enrollment::confirm_code`]; only when
    /// [`Enrollment::confirm_codes`] is on): only then is the site trusted (else a rogue
    /// authority could approve on its own).
    ConfirmCode {
        /// The confirmation code: six digits, e.g. `482913` (shown `482 913`).
        code: String,
    },
    /// Enrolled: sessions with other enrolled devices use `secure/2`. `connected` tells
    /// whether the authority is reachable right now (not needed for sessions).
    Enrolled {
        /// The authority is reachable.
        connected: bool,
    },
    /// The site's join mode needs a code typed on this device (`code_and_approval`,
    /// §E.10.10): one from its Site Manager ([`Enrollment::join_with_code`]).
    CodeNeeded,
    /// The typed code was right; waiting for an operator to approve (`code_and_approval`).
    CodeAccepted,
    /// The typed code wasn't one the site issued. `left`: the most tries the site's codes
    /// have left (0: used up, ask for a new one). Type it again, or Try again to ask for
    /// approval where the site allows that.
    WrongCode {
        /// Tries left.
        left: u32,
    },
    /// The site has no code waiting: the typed code expired, was used, or burned. Ask
    /// the Site Manager for a new one.
    CodeExpired,
    /// The authority denied this device.
    Denied,
    /// The authority revoked this device; it no longer has a certificate.
    Revoked,
    /// Something went wrong (shown to the operator); retried.
    Error(String),
}

/// Live sessions with enrolled peers, by certificate serial, closed when revoked.
type Sessions = Mutex<Vec<(Vec<u8>, TcpStream)>>;

/// A relay's pairing codes from the site, by request number: (code, until, seconds left).
/// Pairing codes answered, by request: the code, until when and the seconds left; or why
/// none (this machine isn't a pairing relay).
type PairingAnswers = std::collections::BTreeMap<u64, Result<(String, u64, u64), String>>;

/// This device's membership of a site (§E.10). Start one per application and pass it to
/// [`SecureConfig::with_site`](crate::secure::SecureConfig::with_site); sessions with
/// other enrolled devices are then encrypted with per-device certificates.
///
/// ```no_run
/// use idn_open_protocol::trust::{Enrollment, EnrollmentConfig, DeviceRole, TrustState, show_code};
/// use std::time::Duration;
///
/// # fn main() -> std::io::Result<()> {
/// let me = Enrollment::start(EnrollmentConfig {
///     discovery_server: "192.168.1.10".into(),
///     state_dir: "/var/lib/myapp/idn-device".into(),
///     name: "BOOTH-PC (Presentation)".into(),
///     role: DeviceRole::App,
///     passphrase: Some("St Mark's AV team".into()), // joins by itself
///     admin_code: None,
/// })?;
/// while let Some(state) = me.recv(Duration::from_secs(1)) {
///     if let TrustState::Pending { code } = &state {
///         println!("Approve this device on the server. Code: {}", show_code(code));
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct Enrollment {
    /// Talking to the authority itself, or through the machine's IDN Node.
    node: Option<Arc<crate::node::Conn>>,
    /// The machine's network interface as the Node has it set, if any.
    node_interface: Option<std::net::Ipv4Addr>,
    /// The identity for `link/1` mode `open` built from this device's key (§E.11.4).
    open_identity: std::sync::OnceLock<Arc<crate::link::Identity>>,
    /// The channel endpoint port to announce to the authority (§E.12.2), once set.
    endpoint: Mutex<Option<u16>>,
    /// The directory of channel endpoints, by fingerprint; `None` until asked for.
    directory: Mutex<Option<BTreeMap<String, DirEntry>>>,
    /// The site's device count, as its authority said with the directory (§E.12.2).
    site_devices: Mutex<Option<usize>>,
    /// The site's audio transmitters `(name, channels, machine)` as its authority last
    /// said (§E.12.12), and when they were asked for.
    site_txs: Mutex<(Vec<SiteTx>, Option<Instant>)>,
    /// The trust connection's writer, while connected.
    trust_writer: Mutex<Option<TcpStream>>,
    cfg: EnrollmentConfig,
    key: KeyFile,
    site: RwLock<Option<Arc<SiteKey>>>,
    creds: RwLock<Option<Arc<DeviceCreds>>>,
    state: Mutex<TrustState>,
    events: (Mutex<VecDeque<TrustState>>, Condvar),
    sessions: Sessions,
    stop: AtomicBool,
    /// This enrollment, weakly: when the background thread holds the only strong
    /// reference, the application has let it go (TR-4).
    me: std::sync::OnceLock<std::sync::Weak<Enrollment>>,
    /// The site's options (§E.20), the latest document received (or kept from the last
    /// run), and a wake-up for waiters.
    options: Mutex<Arc<super::options::SiteOptions>>,
    /// The CA fingerprint of the site the options came from (`options.ca`): another
    /// site's options give way to the new site's whatever their version (bug run
    /// 2026-11-25: a previous site's higher version kept its options in force).
    options_ca: Mutex<String>,
    /// The admin code given was refused: not sent again at the next connections (bug run
    /// 2026-11-25: a mistyped code went out at each reconnect and used up the site's
    /// tries).
    admin_code_refused: AtomicBool,
    options_cv: Condvar,
    /// Replies to this device's administration requests, in order.
    /// Answers to remote-admin requests (numbered: `req`), and the next number.
    admin_replies: (Mutex<VecDeque<crate::xml::Element>>, Condvar),
    admin_next: std::sync::atomic::AtomicU64,
    /// Pairings relayed for clients (idn-node.md §N.42.3): the next ticket, and each
    /// ticket's answer once it came.
    key_answers: Mutex<(
        u64,
        std::collections::BTreeMap<u64, Option<super::KeyGrant>>,
    )>,
    /// Relayed pairings whose typed code the site refused (still waiting), by ticket.
    key_code_wrong: Mutex<std::collections::BTreeSet<u64>>,
    /// The site's answers to this relay's pairing-code requests, by request number.
    pairing_codes: (Mutex<PairingAnswers>, Condvar),
    /// Ends the current wait early ([`Enrollment::retry_now`]).
    kick: AtomicBool,
    /// The IDN Node's "require encryption" for the machine, as it last said (its
    /// `identity`, sent again on every change): read at each connection.
    node_require: AtomicBool,
    /// Settings the authority sent for the IDN Node holding this membership
    /// (idn-node.md §N.11.2), in order.
    node_sets: (Mutex<VecDeque<Element>>, Condvar),
    /// The machine's linked computers, as the IDN Node last gave them (§E.28): empty
    /// while the machine is in a site, or without a Node.
    pairs: RwLock<Arc<Vec<super::pair::Linked>>>,
    /// Sessions using each linked computer now (by fingerprint), told to the IDN Node: a
    /// link for this time only ends with the last (§E.28).
    pair_uses: Mutex<std::collections::HashMap<String, usize>>,
    /// Live `pair/1` sessions (the link's fingerprint, the socket): closed when that link
    /// is removed — an unlinked computer in control went on until it chose to leave (bug
    /// round E, E3-2).
    pair_sessions: Mutex<Vec<(String, TcpStream)>>,
    /// Joins by code wait for this device's person to confirm the code (the IDN Node turns
    /// it on), and their answer.
    confirm_codes: AtomicBool,
    confirmation: (Mutex<Option<bool>>, Condvar),
    /// A join code typed on this device (§E.10.10), for the next join: used up by the
    /// site's answer (a connection lost before one keeps it for the next try).
    typed_code: Mutex<Option<String>>,
    /// The code the join on the wire carries (`None`: a join without one). Another typed
    /// while it waits: that join gives way to one with it.
    code_in_use: Mutex<Option<String>>,
    /// The site's join mode as its authority said at the last hello (`None`: not known
    /// yet, or an authority from before join modes).
    site_mode: Mutex<Option<super::JoinMode>>,
    /// The machine's certificate for its HTTPS ports (§E.10.11), and when to look at it
    /// again.
    lan: lan::Held,
}

impl std::fmt::Debug for Enrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Enrollment({}, {:?})",
            &self.key.fingerprint()[..16],
            self.state()
        )
    }
}

enum JoinOutcome {
    Enrolled,
    Closed,
    Denied,
    NoAuthority,
    /// Approved by code, but this device's person didn't confirm the code (§E.10.4): the
    /// join withdrawn, and not asked for again until the person does.
    Unconfirmed,
    /// The typed code was wrong: tries left (§E.10.10).
    WrongCode(u32),
    /// No code waits at the site.
    CodeExpired,
    /// The site needs a typed code.
    CodeNeeded,
    /// Too many wrong codes from this address: seconds to wait.
    CodeLimited(u64),
}

/// Whether a tracked session's socket is still open, without disturbing the session
/// using it. The socket is a dup of the live one, and O_NONBLOCK is shared by every dup:
/// setting it to peek made the session's own blocking reads fail now and then, cutting
/// live sessions. A per-call non-blocking peek (MSG_DONTWAIT) touches no shared state.
#[cfg(unix)]
fn session_alive(t: &TcpStream) -> bool {
    let mut b = [std::mem::MaybeUninit::<u8>::uninit(); 1];
    match socket2::SockRef::from(t).recv_with_flags(&mut b, libc::MSG_PEEK | libc::MSG_DONTWAIT) {
        Ok(n) => n > 0, // 0: the peer closed
        Err(e) => e.kind() == io::ErrorKind::WouldBlock,
    }
}

#[cfg(not(unix))]
fn session_alive(t: &TcpStream) -> bool {
    t.peer_addr().is_ok()
}

/// A connection to the authority. A trust connection can stay quiet for days (a pending
/// join waits for the operator, an enrolled session for the next CRL), so TCP keepalive
/// notices an authority host that vanished without closing (power loss, cable pull) and
/// the device reconnects instead of waiting forever; a write timeout bounds a stalled one.
fn trust_socket(server: SocketAddr) -> io::Result<TcpStream> {
    let s = crate::discovery::ds::connect_server(server, Duration::from_secs(2))?;
    crate::transport::keepalive(&s, Duration::from_secs(5));
    s.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    Ok(s)
}

impl Drop for Enrollment {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sleeps `d` unless the enrollment goes away meanwhile.
/// Sleeps `d` unless the enrollment goes away (false) or is kicked (returns early).
fn pause(weak: &std::sync::Weak<Enrollment>, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        match weak.upgrade() {
            Some(e) if !e.stop.load(Ordering::Relaxed) => {
                if e.kick.swap(false, Ordering::Relaxed) {
                    return true;
                }
            }
            _ => return false,
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// A code typed while a join waited (`recv_while_wanted`'s error): that join gives way.
#[derive(Debug)]
pub(super) struct CodeTyped;

impl std::fmt::Display for CodeTyped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a code was typed")
    }
}

impl std::error::Error for CodeTyped {}

/// Site credentials issued without the network (spec/extensions/idn-node.md §N.11.7: an IDN
/// Appliance making its own site, its Node issuing its own certificate from the new
/// site's authority before the Site Manager runs): `ca_der` the site's CA and `cert_der`
/// the certificate for the key in `state_dir`, checked (signed by that CA, for that key)
/// and kept where an [`Enrollment`] on `state_dir` finds them at its start.
pub fn install_issued(
    state_dir: &std::path::Path,
    ca_der: &[u8],
    cert_der: &[u8],
) -> io::Result<()> {
    let anchor = Anchor::new(ca_der.to_vec())?;
    let info = anchor.verify(cert_der)?;
    let key = KeyFile::load_or_create(&state_dir.join("device.key"))?;
    if info.fingerprint != key.fingerprint() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the certificate is not for this device",
        ));
    }
    super::pki::write_private(&state_dir.join("ca.crt"), ca_der)?;
    super::pki::write_private(&state_dir.join("device.crt"), cert_der)?;
    Ok(())
}

#[cfg(test)]
mod tests;
