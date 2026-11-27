//! The site authority (§E.10.1, §E.10.4–§E.10.8): runs inside an IDN Discovery Server,
//! issues device certificates, keeps the revocation list, and is driven by the
//! application's UI (in-process) or by admin devices (remotely).

use super::pki::{Anchor, Ca, CertInfo, DeviceCreds, KeyFile};
use super::proto::{self, b64, unb64};
use crate::secure::SiteKey;
use crate::xml::Element;
use idn_common::err::other as err;
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read};
use std::net::{IpAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod join_codes;
mod key_requests;
pub use key_requests::{KeyGrant, KeyRequest, KeyStatus};
mod alerts;
mod devices;
mod lan_certs;
mod nodes;
mod server;
mod site_options;
mod store;
mod tokens;
pub use alerts::{ALERTS, AlertEntry, LogSnapshot};
use alerts::{keep_alert, keep_report, read_alert_entries, read_alerts, safe_name};

/// Settings for [`Authority::open`].
#[derive(Debug, Clone)]
pub struct AuthorityConfig {
    /// Where the site CA key, the device list and the revocation list are kept. Back it
    /// up: losing the CA key means every device must join again.
    pub state_dir: PathBuf,
    /// The site passphrase, if devices may join automatically by knowing it ("hybrid").
    /// `None`: every device needs approval with the pairing window and code.
    pub passphrase: Option<String>,
}

/// A device waiting for approval, as shown in the UI's approve/deny dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    /// Identifies the request for [`Authority::approve`] / [`Authority::deny`].
    pub id: u64,
    /// The name the device gives, e.g. `BOOTH-PC (Camera)`. Not proof of anything: compare
    /// the code.
    pub name: String,
    /// `output`, `input` or `app`.
    pub kind: String,
    /// The device's key fingerprint (64 hex digits).
    pub fingerprint: String,
    /// The confirmation code: six digits, e.g. `482913`, shown `482 913`
    /// ([`show_code`](super::show_code)). The same code must be shown on the device; if it
    /// differs, deny, since someone may be in between.
    pub code: String,
    /// Where the request came from.
    pub address: Option<IpAddr>,
    /// The device typed a valid join code (§E.10.10, join mode `code_and_approval`):
    /// `code` is empty, the typed code having proved the site to the device; approve by
    /// name and address.
    pub by_code: bool,
    /// When it asked (Unix seconds).
    pub asked_at: u64,
}

/// A device with a certificate from this site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledDevice {
    /// Key fingerprint.
    pub fingerprint: String,
    /// Its name in the site, as its certificate carries it: the one it asked with, or the
    /// one given as it was approved ([`Authority::approve_as`]).
    pub name: String,
    /// `output`, `input` or `app`.
    pub kind: String,
    /// May administer the authority remotely.
    pub admin: bool,
    /// Revoked: refused by every device, and its joins are refused until cleared.
    pub revoked: bool,
    /// Currently connected to the authority.
    pub online: bool,
    /// A pairing relay ([`Authority::set_relay`]): it may show the site's pairing codes
    /// for clients pairing through it.
    pub relay: bool,
}

/// Something that happened, for the UI (e.g. pop up the approve dialog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityEvent {
    /// A device asks to join; show [`JoinRequest::code`] and offer approve/deny.
    JoinRequested(JoinRequest),
    /// A pending request was withdrawn (the device went away or the window closed).
    JoinCancelled(u64),
    /// A device was approved (by the UI, an admin device or the passphrase).
    Approved(EnrolledDevice),
    /// A request was denied.
    Denied {
        /// The device's name.
        name: String,
        /// Its fingerprint.
        fingerprint: String,
    },
    /// A device was revoked.
    Revoked(EnrolledDevice),
    /// A device approved by code withdrew: its person didn't confirm the code on it
    /// (§E.10.4). Its certificate is revoked; it isn't in the site.
    Withdrawn {
        /// The device's name.
        name: String,
        /// Its fingerprint.
        fingerprint: String,
    },
    /// A denied or revoked device asked again and was refused automatically.
    Refused {
        /// The name it gave.
        name: String,
        /// Its fingerprint (clear it to let it ask again).
        fingerprint: String,
    },
    /// A machine asks for the site's newest release for it (idn-node.md §N.42): the
    /// Site Manager answers through the machine's settings (`update_available`).
    UpdateCheck {
        /// The machine's fingerprint.
        fingerprint: String,
        /// Its product (`idn-media`, …).
        product: String,
        /// The build it runs.
        version: String,
    },
    /// A named API key was used on a machine of the site (idn-node.md §N.42): its
    /// `last_used`. Told at most once a minute a key by each program checking it.
    KeyUsed {
        /// The key's id (its tokens' subject `key:<id>`).
        key: String,
        /// The machine it was used on, as enrolled.
        machine: String,
    },
    /// A device typed the join code `id` (§E.10.10): the code is used up. It joins, or
    /// waits for approval (`code_and_approval`).
    CodeUsed {
        /// The code's id ([`JoinCodeInfo::id`](super::JoinCodeInfo::id)).
        id: u64,
        /// The device's name.
        name: String,
        /// Its fingerprint.
        fingerprint: String,
    },
    /// A device typed a code that isn't one the site issued (or stopped half-way): one
    /// try of each waiting code taken.
    CodeWrong {
        /// The name it gave.
        name: String,
        /// Where it came from.
        address: Option<IpAddr>,
        /// The most tries any waiting code has left.
        left: u32,
    },
    /// A machine of the site relays a client's pairing (idn-node.md §N.42.3): the client
    /// asks a key through it. The host decides
    /// ([`Authority::key_requests`], [`Authority::answer_key_request`]); the machine only
    /// passes it on.
    KeyRequested {
        /// The request's id (for the answer).
        id: u64,
        /// The client typed the pairing code this site gave its relay
        /// ([`Authority::pairing_code_for`]), checked here: a host that pairs on
        /// the PIN alone pairs it at once. False with no code or a wrong one.
        code_ok: bool,
        /// The client's name for people.
        client: String,
        /// The machine it came through: its name.
        via: String,
        /// And its fingerprint.
        via_fingerprint: String,
        /// The client typed a PIN (in [`Authority::key_requests`], not here).
        pin: bool,
    },
    /// A relayed key request gone unanswered: its machine disconnected, or 10 minutes
    /// passed.
    KeyRequestCancelled(u64),
    /// A machine left the site by itself (idn-node.md §N.42): its certificate revoked, it
    /// isn't listed, and it may ask to join again.
    Left {
        /// The device's name.
        name: String,
        /// Its fingerprint.
        fingerprint: String,
    },
    /// The join code `id` can't be used any more: its tries ran out, or (`replaced`) a
    /// newer code took its place.
    CodeBurned {
        /// The code's id.
        id: u64,
        /// A newer code took its place (the site holds at most 4).
        replaced: bool,
    },
}

/// The site's devices: enrolled, not revoked, each once (its newest record).
fn device_count(st: &State) -> usize {
    let mut seen = std::collections::BTreeMap::new();
    for r in &st.devices {
        seen.insert(r.info.fingerprint.as_str(), r.revoked);
    }
    seen.values().filter(|revoked| !**revoked).count()
}

#[derive(Debug, Clone)]
struct Record {
    info: CertInfo,
    revoked: bool,
    /// Revoked because the device withdrew its join (its person didn't confirm the code,
    /// §E.10.4): its certificate is in the CRL, the device isn't listed, and it may ask
    /// to join again.
    withdrawn: bool,
}

#[derive(Debug)]
struct PendingJoin {
    req: JoinRequest,
    decision: Option<bool>,
    /// The name given as it was approved ([`Authority::approve_as`]): its certificate's.
    name: Option<String>,
}

/// How far a passphrase proof's time may be from the authority's, seconds (§E.10.4).
const PROOF_WINDOW: u64 = 600;

struct State {
    devices: Vec<Record>,
    denied: BTreeSet<String>,
    /// The machines that may relay pairings with a code (`relays.txt`).
    relays: BTreeSet<String>,
    crl_number: u64,
    crl_der: Vec<u8>,
    pairing_until: Option<Instant>,
    pending: BTreeMap<u64, PendingJoin>,
    /// Join request ids, 1, 2, 3… as the operator types them.
    next_id: u64,
    /// Passphrase proofs taken in the last [`PROOF_WINDOW`]: each is good once.
    proofs_seen: BTreeMap<[u8; 32], u64>,
    next_conn: u64,
    /// Enrolled devices on a trust connection: (fingerprint, connection id, writer).
    online: Vec<(String, u64, Arc<Mutex<TcpStream>>)>,
    /// Channel endpoints announced over trust connections (§E.12.2), by connection id.
    endpoints: BTreeMap<u64, Endpoint>,
    /// Connections that asked for the directory: its changes are pushed to them.
    directory_subs: BTreeSet<u64>,
    admin_code: Option<String>,
    /// Wrong admin codes given so far: at [`ADMIN_CODE_TRIES`] the code is used up.
    admin_tries: u32,
    /// The site's options document (§E.20), kept in `options.xml`.
    options: super::options::SiteOptions,
    /// The newest report of each IDN Node (spec/extensions/idn-node.md §N.11.2), by
    /// connection id: (fingerprint, report, when).
    node_reports: BTreeMap<u64, (String, Element, Instant, Option<IpAddr>)>,
    /// Answers to settings sent to Nodes, by request id.
    node_results: BTreeMap<u64, Element>,
    /// Node settings sent and waited for: their id and the machine they went to (only its
    /// answer is taken).
    node_pending: BTreeMap<u64, String>,
    next_set: u64,
    /// The join codes waiting to be typed, and failed tries by address (§E.10.10), kept
    /// in `join_codes.tsv`.
    codes: super::join_code::Codes,
    /// Relayed key requests waiting for the host's answer (idn-node.md §N.42.3), by id.
    key_requests: BTreeMap<u64, key_requests::Pending>,
    next_key_request: u64,
    /// How long a relayed request waits ([`key_requests::KEY_REQUEST_LIFE`]; tests shorten it).
    key_request_life: Duration,
    /// The pairing code each relay shows (made here, sent to the relay when
    /// it asks), by the relay's fingerprint.
    pairing_codes: BTreeMap<String, key_requests::PairingCode>,
    /// How long a pairing code lasts ([`key_requests::PAIRING_CODE_LIFE`]; tests shorten it).
    pairing_code_life: Duration,
    /// Keys sent to relays lately (their connection, request and key id, when), and those
    /// a relay's client took back after they were sent ([`Authority::take_unclaimed_keys`]).
    key_answered: Vec<(u64, String, String, std::time::Instant)>,
    key_unclaimed: Vec<String>,
}

/// An IDN Node's latest report (spec/extensions/idn-node.md §N.11.2).
#[derive(Debug, Clone)]
pub struct NodeReport {
    /// The machine's key fingerprint.
    pub fingerprint: String,
    /// The `idn_node_report` element as received.
    pub report: Element,
    /// How long ago it came.
    pub age: Duration,
    /// Where its trust connection comes from.
    pub address: Option<IpAddr>,
}

/// A device's channel endpoint (§E.12.2).
#[derive(Debug, Clone)]
struct Endpoint {
    info: CertInfo,
    address: IpAddr,
    port: u16,
}

impl Endpoint {
    fn element(&self) -> Element {
        Element::new("device")
            .with_attr("fp", &self.info.fingerprint)
            .with_attr("name", &display_name(&self.info.name))
            .with_attr("kind", &self.info.kind)
            .with_attr("admin", if self.info.admin { "true" } else { "false" })
            .with_attr("address", &self.address.to_string())
            .with_attr("port", &self.port.to_string())
    }
}

struct Inner {
    dir: PathBuf,
    ca: Ca,
    anchor: Arc<Anchor>,
    server: Arc<DeviceCreds>,
    site: Option<Arc<SiteKey>>,
    state: Mutex<State>,
    events: (Mutex<VecDeque<AuthorityEvent>>, Condvar),
    /// One CRL made at a time, from number to stored (R1-115); pushing is outside it.
    crl: Mutex<()>,
}

/// The site authority. Embed it in a Discovery Server
/// ([`DiscoveryServer`](crate::discovery::DiscoveryServer)) and drive it from the
/// application's UI: open the pairing window, show join requests with their codes,
/// approve, deny, revoke.
///
/// ```no_run
/// use idn_open_protocol::trust::{Authority, AuthorityConfig, AuthorityEvent};
/// use idn_open_protocol::discovery::DiscoveryServer;
/// use std::time::Duration;
///
/// # fn main() -> std::io::Result<()> {
/// let authority = Authority::open(AuthorityConfig {
///     state_dir: "/var/lib/myapp/idn-authority".into(),
///     passphrase: Some("St Mark's AV team".into()), // devices that know it join by themselves
/// })?;
/// let _server = DiscoveryServer::start(5959, Some(authority.clone()))?;
///
/// authority.open_pairing(Duration::from_secs(120)); // "Add device" button
/// while let Some(ev) = authority.recv(Duration::from_secs(1)) {
///     if let AuthorityEvent::JoinRequested(r) = ev {
///         // Show: "<r.name> wants to join. Code: <show_code(&r.code)>" (`482 913`)
///         // with Approve / Deny.
///         authority.approve(r.id)?;
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Authority(Arc<Inner>);

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Authority({})", &self.0.ca.fingerprint()[..16])
    }
}

/// Wrong admin codes before the code is used up (bug run 2026-11-24: any machine of the
/// site could try codes without end).
const ADMIN_CODE_TRIES: u32 = 5;

/// A one-time admin code: 8 characters of [`crate::random::CODE_ALPHABET`] (no 0/O or
/// 1/I to misread).
fn new_admin_code() -> String {
    crate::random::code_base32(8)
}

const PAIRING_DEFAULT: Duration = Duration::from_secs(120);

impl Authority {
    /// The site CA fingerprint (64 hex digits).
    pub fn ca_fingerprint(&self) -> String {
        self.0.ca.fingerprint()
    }

    /// The one-time admin code: a device that joins with it (and is approved) becomes an
    /// administrator. `None` once used. A new code is made at each start.
    pub fn admin_code(&self) -> Option<String> {
        self.0.state.lock_ok().admin_code.clone()
    }

    /// The next event, waiting up to `timeout`.
    pub fn recv(&self, timeout: Duration) -> Option<AuthorityEvent> {
        let (q, cv) = &self.0.events;
        let mut q = q.lock_ok();
        if q.is_empty() {
            q = cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        q.pop_front()
    }

    /// The CA private key (PKCS#8), for a backup. Keep it secret: whoever has it can
    /// enrol devices for this site.
    pub fn export_ca_key(&self) -> Vec<u8> {
        self.0.ca.key().pkcs8().to_vec()
    }

    /// The authority's own TLS identity (issued by the site CA, kind `authority`), for a
    /// server of the site's own (§I.4).
    pub fn server_creds(&self) -> Arc<DeviceCreds> {
        self.0.server.clone()
    }

    /// A TLS client for site machine `machine`'s HTTPS ports (its device fingerprint:
    /// `DeviceHealth.fingerprint`, its Node's): [`Anchor::lan_client_config`] with this
    /// site's CA. Reached at any address, only that machine's certificate passes.
    pub fn lan_client_config(&self, machine: &str) -> io::Result<rustls::ClientConfig> {
        self.0.server.anchor.lan_client_config(machine)
    }

    /// The site CA certificate (DER).
    pub fn ca_der(&self) -> Vec<u8> {
        self.0.ca.cert_der()
    }

    /// The site CA certificate as PEM: what a client pins to trust the site's machines'
    /// HTTPS ports (§E.10.11).
    pub fn ca_pem(&self) -> String {
        super::pki::pem("CERTIFICATE", &self.ca_der())
    }

    /// The current revocation list (DER), as pushed to devices (§E.10.6).
    pub fn crl_der(&self) -> Vec<u8> {
        self.0.state.lock_ok().crl_der.clone()
    }

    fn event(&self, e: AuthorityEvent) {
        let (q, cv) = &self.0.events;
        let mut q = q.lock_ok();
        // An application that never reads events, and a peer repeating refused joins, must
        // not grow memory without end (R1-167): the oldest unread go.
        while q.len() >= MAX_EVENTS {
            q.pop_front();
        }
        q.push_back(e);
        cv.notify_all();
    }
}

/// Longest device name kept, in characters.
const MAX_NAME: usize = 128;
/// Unread events kept, at most.
const MAX_EVENTS: usize = 256;
/// The longest pairing window.
const MAX_PAIRING: Duration = Duration::from_secs(24 * 3600);

/// A device's name as the authority keeps and shows it. The name comes from whoever asks
/// to join, and is shown to the operator (a console, an app's UI): control characters
/// (terminal escapes, line breaks) and bidirectional overrides, which could fake or hide
/// what the operator reads, become spaces; the length is capped.
fn display_name(raw: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    raw.chars()
        .take(MAX_NAME)
        .map(|c| if c.is_control() || bidi(c) { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests;
