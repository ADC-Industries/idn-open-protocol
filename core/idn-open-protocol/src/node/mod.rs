//! The IDN Node: one service per machine that holds its site identity
//! (spec/extensions/idn-node.md). Programs connect to it, get their encryption settings
//! from it and send and receive media themselves, as without it; the Node signs their TLS
//! handshakes so the machine's keys never leave it.
//!
//! ```no_run
//! use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig};
//! use idn_open_protocol::node::Node;
//!
//! # fn main() -> std::io::Result<()> {
//! // Without a Node (not installed, IDN_NODE=off), configure encryption yourself instead.
//! let node = Node::connect("My presentation app")?;
//! let p = Publisher::new(PublisherConfig {
//!     secure: Some(node.secure_config()),
//!     link: Some(node.link_config()),
//!     ..Default::default()
//! })?;
//! let out = Output::new(&p, OutputConfig::new("Program"))?;
//! # Ok(())
//! # }
//! ```

use crate::link::{Identity, KeyMemory, LinkConfig};
use crate::secure::{SecureConfig, StickyStore};
use crate::trust::Enrollment;
use crate::trust::pki::{KeyFile, RemoteSigner};
use crate::trust::proto::{b64, unb64};
use crate::xml::Element;
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

pub mod bridges;
pub mod clock;
pub mod meters;
pub mod rate;
pub mod routing;
pub mod session;
pub mod video;

/// The IPC major version this library speaks (§N.4.1).
pub const MAJOR: u32 = 1;

/// The largest IPC message (§N.4.1).
pub const MAX_MESSAGE: usize = 1 << 20;

/// How long a request waits for its reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a handshake's signature may take. Shorter than other requests: rustls signs
/// inside the QUIC endpoint's driver, so while it waits, every connection of that
/// endpoint (all of a sender's receivers) waits too.
const SIGN_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a program tries to reach a Node that went away (§N.5).
const RECONNECT: Duration = Duration::from_secs(2);

/// The service's endpoint (§N.2).
pub fn system_endpoint() -> String {
    if cfg!(windows) {
        r"\\.\pipe\idn-node".to_owned()
    } else if cfg!(target_os = "macos") {
        "/var/run/idn-node/node.sock".to_owned()
    } else {
        "/run/idn-node/node.sock".to_owned()
    }
}

/// The user-mode endpoint (`idn-node run --user`, §N.2), if it can be named.
pub fn user_endpoint() -> Option<String> {
    if cfg!(windows) {
        let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
        Some(format!(r"\\.\pipe\idn-node-{user}"))
    } else {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
        Some(
            std::path::Path::new(&dir)
                .join("idn-node.sock")
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Where programs look for the Node, in order: `IDN_NODE` (a path or pipe name), else the
/// service's endpoint, else the user-mode one (§N.2). Empty when `IDN_NODE` says off
/// (`off`, `0`, `false`, `no`: `switches::says_off`).
pub fn endpoints() -> Vec<String> {
    match crate::switches::value("IDN_NODE") {
        Some(v) if crate::switches::says_off(&v) => Vec::new(),
        Some(v) if !v.trim().is_empty() => vec![v.trim().to_owned()],
        _ => std::iter::once(system_endpoint())
            .chain(user_endpoint())
            .collect(),
    }
}

/// Whether `data` is a TLS 1.3 `CertificateVerify` input (RFC 8446 §4.4.3): 64 spaces,
/// a context string, a zero byte and a SHA-256 or SHA-384 transcript hash. The Node signs
/// nothing else (§N.4.4).
pub fn is_certificate_verify_input(data: &[u8]) -> bool {
    const CONTEXTS: [&[u8]; 2] = [
        b"TLS 1.3, server CertificateVerify",
        b"TLS 1.3, client CertificateVerify",
    ];
    if data.len() < 64 || data[..64].iter().any(|&b| b != 0x20) {
        return false;
    }
    let rest = &data[64..];
    CONTEXTS.iter().any(|c| {
        rest.len() > c.len()
            && &rest[..c.len()] == *c
            && rest[c.len()] == 0
            && matches!(rest.len() - c.len() - 1, 32 | 48)
    })
}

/// Framing (§N.4.1), shared with the Node itself.
#[doc(hidden)]
pub mod wire {
    use super::MAX_MESSAGE;
    use crate::xml::Element;
    use std::io;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// The next message, or `None` at a clean end of stream.
    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Element>> {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_le_bytes(len) as usize;
        if n == 0 || n > MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("IDN Node message of {n} bytes"),
            ));
        }
        let mut b = vec![0u8; n];
        r.read_exact(&mut b).await?;
        let s = String::from_utf8(b).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "IDN Node message not UTF-8")
        })?;
        crate::xml::parse(&s)
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "IDN Node message not XML"))
    }

    /// Writes one message.
    pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, e: &Element) -> io::Result<()> {
        write_str(w, &e.to_xml()).await
    }

    /// Writes one message already serialised.
    pub async fn write_str<W: AsyncWrite + Unpin>(w: &mut W, s: &str) -> io::Result<()> {
        if s.len() > MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "message too large",
            ));
        }
        let mut b = Vec::with_capacity(4 + s.len());
        b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
        w.write_all(&b).await?;
        w.flush().await
    }
}

/// One reply, awaited by a request.
#[derive(Default)]
struct Slot {
    value: Mutex<Option<io::Result<Element>>>,
    cv: Condvar,
}

/// The connection to the Node: requests from any thread, replies matched by id, events to
/// the enrollment's thread, and reconnection in the background (§N.5).
#[doc(hidden)]
pub struct Conn {
    endpoint: String,
    app: String,
    /// The current connection's outgoing messages, while connected.
    tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    pending: Mutex<HashMap<u64, Arc<Slot>>>,
    next: AtomicU64,
    /// Events (messages without a known id) and `node_connected` / `node_lost` markers.
    events: Mutex<Option<mpsc::Sender<Element>>>,
    stop: AtomicBool,
    /// A session is up (since its welcome, until it is lost).
    up: AtomicBool,
    /// The Node's `node_welcome` on the current connection.
    welcome: Mutex<Option<Element>>,
    /// This program's registrations by handle, sent again after a reconnect (the Node may
    /// have restarted).
    regs: Mutex<BTreeMap<String, Element>>,
    next_handle: AtomicU64,
    /// Who hears each routable receiver's `routes` (idn-routing.md §A.3), by its name.
    route_watch: Mutex<HashMap<String, RouteWatcher>>,
}

type RouteWatcher = Box<dyn Fn(&Element) + Send>;

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node::Conn({})", self.endpoint)
    }
}

impl Conn {
    /// Connects (failing at once if the first attempt fails) and keeps the connection up.
    fn start(endpoint: &str, app: &str) -> io::Result<(Arc<Conn>, mpsc::Receiver<Element>)> {
        let (etx, erx) = mpsc::channel();
        let c = Arc::new(Conn {
            endpoint: endpoint.to_owned(),
            app: app.to_owned(),
            tx: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            events: Mutex::new(Some(etx)),
            stop: AtomicBool::new(false),
            up: AtomicBool::new(false),
            welcome: Mutex::new(None),
            regs: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
            route_watch: Mutex::new(HashMap::new()),
        });
        let (first_tx, first_rx) = mpsc::channel::<io::Result<()>>();
        let weak = Arc::downgrade(&c);
        std::thread::Builder::new()
            .name("idn-node-client".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = first_tx.send(Err(e));
                        return;
                    }
                };
                let mut first = Some(first_tx);
                loop {
                    // The session holds the connection weakly: when the program drops
                    // its last handle, the connection ends (and the Node forgets it).
                    if weak
                        .upgrade()
                        .is_none_or(|c| c.stop.load(Ordering::Relaxed))
                    {
                        return;
                    }
                    let r = rt.block_on(Conn::session(weak.clone(), &mut first));
                    let Some(c) = weak.upgrade() else { return };
                    if let Some(f) = first.take() {
                        let _ = f.send(r.map(|_| ()));
                        if !c.connected_once() {
                            return;
                        }
                    }
                    c.lost();
                    drop(c);
                    let end = Instant::now() + RECONNECT;
                    while Instant::now() < end {
                        std::thread::sleep(Duration::from_millis(100));
                        if weak
                            .upgrade()
                            .is_none_or(|c| c.stop.load(Ordering::Relaxed))
                        {
                            return;
                        }
                    }
                }
            })?;
        first_rx
            .recv()
            .map_err(|_| io::Error::other("IDN Node client thread ended"))??;
        Ok((c, erx))
    }

    fn connected_once(&self) -> bool {
        self.welcome.lock_ok().is_some()
    }

    /// One connection: hello, welcome, then messages both ways until it ends.
    async fn session(
        weak: std::sync::Weak<Self>,
        first: &mut Option<mpsc::Sender<io::Result<()>>>,
    ) -> io::Result<()> {
        let gone = || io::Error::other("IDN Node connection dropped");
        let me = weak.upgrade().ok_or_else(gone)?;
        let (mut rd, mut wr, mut rx) = me.start_session(first).await?;
        drop(me);
        let writer = async {
            while let Some(m) = rx.recv().await {
                wire::write_str(&mut wr, &m).await?;
            }
            Ok::<_, io::Error>(())
        };
        let reader = async {
            loop {
                let Some(m) = wire::read(&mut rd).await? else {
                    return Ok::<_, io::Error>(());
                };
                let Some(me) = weak.upgrade() else {
                    return Ok(());
                };
                me.dispatch(m);
                if me.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
        };
        let r = tokio::select! {
            r = writer => r,
            r = reader => r,
        };
        if let Some(me) = weak.upgrade() {
            *me.tx.lock_ok() = None;
        }
        r
    }

    /// Hello and welcome; the registrations again; the outgoing queue in place.
    #[allow(clippy::type_complexity)]
    async fn start_session(
        &self,
        first: &mut Option<mpsc::Sender<io::Result<()>>>,
    ) -> io::Result<(
        tokio::io::ReadHalf<Stream>,
        tokio::io::WriteHalf<Stream>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    )> {
        let stream = open(&self.endpoint).await?;
        let (mut rd, mut wr) = tokio::io::split(stream);
        let hello = Element::new("node_hello")
            .with_attr("v", &MAJOR.to_string())
            .with_attr("app", &self.app)
            .with_attr("pid", &std::process::id().to_string());
        wire::write(&mut wr, &hello).await?;
        let w = tokio::time::timeout(Duration::from_secs(5), wire::read(&mut rd))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IDN Node did not answer"))??
            .ok_or_else(|| io::Error::other("IDN Node closed the connection"))?;
        match w.name.as_str() {
            "node_welcome" => {}
            "node_refused" => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "IDN Node refused: {}",
                        w.attr("reason").unwrap_or("no reason given")
                    ),
                ));
            }
            other => return Err(io::Error::other(format!("IDN Node sent {other}"))),
        }
        *self.welcome.lock_ok() = Some(w);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // Registrations again: the Node knows none of them on a new connection.
        for e in self.regs.lock_ok().values() {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(e.clone().with_attr("id", &id.to_string()).to_xml());
        }
        *self.tx.lock_ok() = Some(tx);
        self.up.store(true, Ordering::Relaxed);
        if let Some(f) = first.take() {
            let _ = f.send(Ok(()));
        } else {
            self.event(Element::new("node_connected"));
        }
        Ok((rd, wr, rx))
    }

    /// A reply to the request with its id, else an event.
    fn dispatch(&self, m: Element) {
        let id = m.attr("id").and_then(|i| i.parse::<u64>().ok());
        if let Some(slot) = id.and_then(|i| self.pending.lock_ok().remove(&i)) {
            let v = if m.name == "error" {
                Err(io::Error::other(format!(
                    "IDN Node: {} ({})",
                    m.attr("text").unwrap_or("error"),
                    m.attr("code").unwrap_or("failed")
                )))
            } else {
                Ok(m)
            };
            *slot.value.lock_ok() = Some(v);
            slot.cv.notify_all();
        } else if id.is_none() {
            // A matrix watches this machine's levels (meters.rs).
            if m.name == "meters" {
                meters::wanted_for(m.attr("secs").and_then(|s| s.parse().ok()).unwrap_or(5));
                return;
            }
            if m.name == "routes" {
                let w = self.route_watch.lock_ok();
                if let Some(f) = m.attr("receiver").and_then(|r| w.get(r)) {
                    f(&m);
                }
                return;
            }
            self.event(m);
        }
    }

    /// Calls `f` with every `routes` message for receiver `name`.
    /// Watches `name`'s routes; false if something here watches that name already (a
    /// second registration under it would be refused, and its clean-up took the live one's
    /// watch away).
    fn watch_routes(&self, name: &str, f: impl Fn(&Element) + Send + 'static) -> bool {
        let mut w = self.route_watch.lock_ok();
        if w.contains_key(name) {
            return false;
        }
        w.insert(name.to_owned(), Box::new(f));
        true
    }

    fn unwatch_routes(&self, name: &str) {
        self.route_watch.lock_ok().remove(name);
    }

    fn event(&self, m: Element) {
        if let Some(tx) = self.events.lock_ok().as_ref() {
            let _ = tx.send(m);
        }
    }

    /// The connection ended: every waiting request fails, and the enrollment hears of it.
    fn lost(&self) {
        // Up until now (a session had started): a failed reconnect isn't another loss
        // (an event and a log line every 2 s while the Node was down).
        *self.tx.lock_ok() = None;
        let was_up = self.up.swap(false, Ordering::Relaxed);
        // Taken out first, `pending` let go before any slot is locked: a request timing out
        // holds its slot and then locks `pending` (a deadlock otherwise).
        let waiting: Vec<_> = self.pending.lock_ok().drain().collect();
        for (_, slot) in waiting {
            *slot.value.lock_ok() = Some(Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "IDN Node connection lost",
            )));
            slot.cv.notify_all();
        }
        if was_up {
            self.event(Element::new("node_lost"));
        }
    }

    /// Sends `e` with a fresh id and waits for its reply.
    pub fn request(&self, e: Element, timeout: Duration) -> io::Result<Element> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(Slot::default());
        self.pending.lock_ok().insert(id, slot.clone());
        let msg = e.with_attr("id", &id.to_string()).to_xml();
        let sent = self
            .tx
            .lock_ok()
            .as_ref()
            .is_some_and(|tx| tx.send(msg).is_ok());
        if !sent {
            self.pending.lock_ok().remove(&id);
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not connected to the IDN Node",
            ));
        }
        let end = Instant::now() + timeout;
        let mut v = slot.value.lock_ok();
        loop {
            if let Some(r) = v.take() {
                return r;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.pending.lock_ok().remove(&id);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the IDN Node did not answer in time",
                ));
            }
            v = slot
                .cv
                .wait_timeout(v, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Sends `e` with a fresh id, not waiting for the reply.
    pub fn send(&self, e: Element) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if let Some(tx) = self.tx.lock_ok().as_ref() {
            let _ = tx.send(e.with_attr("id", &id.to_string()).to_xml());
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        *self.tx.lock_ok() = None;
    }
}

impl RemoteSigner for Conn {
    fn sign(&self, key: &str, message: &[u8]) -> io::Result<Vec<u8>> {
        let r = self.request(
            Element::new("sign")
                .with_attr("key", key)
                .with_attr("data", &b64(message)),
            SIGN_TIMEOUT,
        )?;
        r.attr("sig")
            .and_then(unb64)
            .filter(|s| s.len() == 64)
            .ok_or_else(|| io::Error::other("IDN Node: bad signature"))
    }
}

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
async fn open(endpoint: &str) -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(endpoint).await
}

#[cfg(windows)]
async fn open(endpoint: &str) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint) {
            Ok(c) => {
                // Served by the Node (SYSTEM, or this user's own): another user's program
                // could take the name while the Node restarts.
                use std::os::windows::io::AsRawHandle;
                if !pipe_check::trusted(c.as_raw_handle()) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "the IDN Node's pipe is served by another account's program",
                    ));
                }
                return Ok(c);
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Who serves a named pipe (Windows): its process's account.
#[cfg(windows)]
mod pipe_check {
    use std::ffi::c_void;
    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(h: Handle) -> i32;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            info: *mut c_void,
            len: u32,
            got: *mut u32,
        ) -> i32;
        fn IsWellKnownSid(sid: *mut c_void, kind: u32) -> i32;
        fn EqualSid(a: *mut c_void, b: *mut c_void) -> i32;
        fn GetSecurityInfo(
            handle: Handle,
            object_type: u32,
            info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            sd: *mut *mut c_void,
        ) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }
    const SE_KERNEL_OBJECT: u32 = 6;
    const OWNER_SECURITY_INFORMATION: u32 = 1;

    /// Whether the pipe itself is SYSTEM's: the Node's service makes it so (`O:SY`), which
    /// only SYSTEM (or an administrator) can. Read from the pipe, which every client may:
    /// a standard user can't read the service's process token (lab: the tray couldn't
    /// see the service, 2026-11-04).
    fn owned_by_system(pipe: Handle) -> bool {
        // SAFETY: GetSecurityInfo fills `owner` (inside `sd`), freed with LocalFree.
        unsafe {
            let mut owner: *mut c_void = std::ptr::null_mut();
            let mut sd: *mut c_void = std::ptr::null_mut();
            let r = GetSecurityInfo(
                pipe,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            );
            let yes =
                r == 0 && !owner.is_null() && IsWellKnownSid(owner, WIN_LOCAL_SYSTEM_SID) != 0;
            if !sd.is_null() {
                LocalFree(sd);
            }
            yes
        }
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER: u32 = 1;
    const WIN_LOCAL_SYSTEM_SID: u32 = 22;

    /// A process's TOKEN_USER (its first field points at the account's SID), 8-aligned.
    fn user_of(process: Handle) -> Option<Vec<u64>> {
        // SAFETY: Win32 calls with buffers of the sizes they report; handles closed.
        unsafe {
            let mut token: Handle = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut need = 0u32;
            GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut need);
            let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(
                token,
                TOKEN_USER,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
                &mut need,
            ) != 0;
            CloseHandle(token);
            ok.then_some(buf)
        }
    }

    /// The pipe is SYSTEM's (the Node's service), or its server runs as this process's
    /// user (a Node at log-on, or one a test started: its own token, which it may read).
    pub fn trusted(pipe: std::os::windows::io::RawHandle) -> bool {
        if owned_by_system(pipe as Handle) {
            return true;
        }
        // SAFETY: as above; the SID pointers point into the buffers, alive meanwhile.
        unsafe {
            let mut pid = 0u32;
            if GetNamedPipeServerProcessId(pipe as Handle, &mut pid) == 0 {
                return false;
            }
            let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if p.is_null() {
                return false;
            }
            let theirs = user_of(p);
            CloseHandle(p);
            let (Some(theirs), Some(mine)) = (theirs, user_of(GetCurrentProcess())) else {
                return false;
            };
            let (t, m) = (
                *(theirs.as_ptr() as *const *mut c_void),
                *(mine.as_ptr() as *const *mut c_void),
            );
            IsWellKnownSid(t, WIN_LOCAL_SYSTEM_SID) != 0 || EqualSid(t, m) != 0
        }
    }
}

/// The 32-byte public key in a base64 attribute.
pub(crate) fn public_attr(e: &Element, name: &str) -> Option<[u8; 32]> {
    e.attr(name)
        .and_then(unb64)
        .and_then(|k| k.as_slice().try_into().ok())
}

/// This program's connection to the IDN Node (§N.5).
pub struct Node {
    conn: Arc<Conn>,
    enrollment: Arc<Enrollment>,
    link: LinkConfig,
    name: String,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node({}, {})", self.name, self.conn.endpoint)
    }
}

/// The Node's refusal as a person reads it: "wrong PIN", not "IDN Node: wrong PIN
/// (denied)".
fn plain(e: io::Error) -> io::Error {
    let t = e.to_string();
    let t = t.strip_prefix("IDN Node: ").unwrap_or(&t);
    let t = t.rsplit_once(" (").map_or(t, |x| x.0);
    io::Error::new(e.kind(), t.to_owned())
}

/// Tells the Node where this program's log is and hands it the crash reports not yet
/// handed on ([`crate::diag`]).
fn report_diag(conn: &Conn) {
    let Some((app, log)) = crate::diag::current() else {
        return;
    };
    let _ = conn.request(
        Element::new("log_file")
            .with_attr("app", &app)
            .with_attr("path", &log.display().to_string()),
        REQUEST_TIMEOUT,
    );
    for p in crate::diag::crash_reports() {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let text = crate::diag::tail(&p, 256 << 10)
            .map(|t| t.0)
            .unwrap_or_default();
        let e = Element::new("crash")
            .with_attr("app", &app)
            .with_attr("name", &name)
            .with_text(&text);
        match conn.request(e, REQUEST_TIMEOUT) {
            Ok(r) if r.name == "ok" => crate::diag::mark_reported(&p),
            _ => break,
        }
    }
}

impl Node {
    /// Connects to the machine's Node (§N.2: `IDN_NODE`, else the service, else user mode)
    /// as `app`, the name the Node's panel shows. Fails if none is running, it refuses this
    /// version, or `IDN_NODE=off`.
    pub fn connect(app: &str) -> io::Result<Node> {
        let eps = endpoints();
        if eps.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "IDN Node disabled (IDN_NODE=off)",
            ));
        }
        let mut last = None;
        let mut denied = None;
        for ep in eps {
            match Node::connect_to(&ep, app) {
                Ok(h) => return Ok(h),
                // A Node there that this account may not use says more than one not there.
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => denied = Some(e),
                Err(e) => last = Some(e),
            }
        }
        Err(denied
            .or(last)
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no IDN Node")))
    }

    /// Connects to the Node at `endpoint` (a socket path, or a pipe name on Windows).
    pub fn connect_to(endpoint: &str, app: &str) -> io::Result<Node> {
        let (conn, events) = Conn::start(endpoint, app)?;
        let id = conn.request(Element::new("identity"), REQUEST_TIMEOUT)?;
        let link_pub = public_attr(&id, "link_key")
            .ok_or_else(|| io::Error::other("IDN Node sent no link key"))?;
        let link_cert = id
            .attr("link_cert")
            .and_then(unb64)
            .ok_or_else(|| io::Error::other("IDN Node sent no link certificate"))?;
        let signer: Arc<dyn RemoteSigner> = conn.clone();
        let link = LinkConfig {
            identity: Identity::remote(KeyFile::remote(link_pub, signer, "link"), link_cert),
            keys: KeyMemory::in_memory(),
        };
        let enrollment = Enrollment::via_node(conn.clone(), &id, events)?;
        // This program's network ports present the site's certificate from it (§E.10.11).
        crate::web::lan::use_membership(&enrollment);
        // This program's log and crash reports, for the Site Manager (§N.16).
        report_diag(&conn);
        // The site clock, when the Node keeps one (any other answer: no clock).
        if let Ok(c) = conn.request(Element::new("clock"), REQUEST_TIMEOUT)
            && c.name == "clock"
        {
            clock::heard(&c);
        }
        Ok(Node {
            name: id.attr("name").unwrap_or("").to_owned(),
            conn,
            enrollment,
            link,
        })
    }

    /// The encryption settings for outputs, inputs and channels: the machine's site
    /// membership and passphrase, signed for by the Node. Use it wherever a
    /// [`SecureConfig`] goes.
    pub fn secure_config(&self) -> SecureConfig {
        SecureConfig {
            key: None,
            site: Some(self.enrollment.clone()),
            // The Node's setting is read from the membership at each connection, so a
            // change applies to running programs ([`SecureConfig::requires`]).
            require: false,
            sticky: StickyStore::in_memory(),
            input_scope: None,
        }
    }

    /// The machine's `link/1` identity (§E.11.4).
    pub fn link_config(&self) -> LinkConfig {
        self.link.clone()
    }

    /// The machine's site membership, as the Node reports it.
    pub fn enrollment(&self) -> &Arc<Enrollment> {
        &self.enrollment
    }

    /// The machine's name (default: the host name).
    pub fn device_name(&self) -> &str {
        &self.name
    }

    /// The Node holds this program back until the operator allows it in its panel
    /// (§N.8.1): it signs nothing for it meanwhile.
    pub fn awaiting_approval(&self) -> bool {
        self.conn
            .welcome
            .lock_ok()
            .as_ref()
            .and_then(|w| w.attr("approval"))
            == Some("pending")
    }

    /// The Node's endpoint.
    pub fn endpoint(&self) -> &str {
        &self.conn.endpoint
    }

    /// Sends a request (§N.4.2) and waits for its reply.
    pub fn request(&self, e: Element) -> io::Result<Element> {
        self.conn.request(e, REQUEST_TIMEOUT)
    }

    /// As [`request`](Node::request), waiting up to `timeout` (what takes the Node a
    /// while: linking with another computer, §N.32).
    pub fn request_for(&self, e: Element, timeout: Duration) -> io::Result<Element> {
        self.conn.request(e, timeout)
    }

    /// The Node's status (§N.4.2): settings, enrolment, programs, checks.
    pub fn status(&self) -> io::Result<Element> {
        self.request(Element::new("status"))
    }

    /// This machine's transmitters' levels for a routing matrix's meters
    /// (idn-routing.md §A.15): `<levels><tx name db/>…</levels>`; asking counts as watching
    /// (the programs report while asked, every 2 s at least).
    pub fn levels(&self) -> io::Result<Element> {
        self.request(Element::new("get_levels"))
    }

    /// This machine's media folder (idn-node.md §N.25): where programs keep and play the
    /// site's videos, and where a file name without a folder of its own is looked for.
    pub fn media_dir(&self) -> io::Result<std::path::PathBuf> {
        self.request(Element::new("media"))?
            .attr("dir")
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .ok_or_else(|| io::Error::other("the IDN Node has no media folder"))
    }

    /// The panels' simple-mode PIN (§N.20): whether there is one, and whether only the Site
    /// Manager may change it. Any other answer than the PIN's state is an error.
    pub fn panel_lock(&self) -> io::Result<(bool, bool)> {
        let r = self.request(Element::new("panel_lock"))?;
        if r.name != "panel" {
            return Err(io::Error::other("this IDN Node keeps no simple-mode PIN"));
        }
        Ok((
            r.attr("pin") == Some("true"),
            r.attr("site_only") == Some("true"),
        ))
    }

    /// Whether `pin` leaves simple mode (any does with no PIN set); `Err` with why not.
    pub fn panel_unlock(&self, pin: &str) -> io::Result<()> {
        self.request(Element::new("panel_unlock").with_attr("pin", pin))
            .map(drop)
            .map_err(plain)
    }

    /// Sets the simple-mode PIN (`None` wipes it); refused while the site holds it.
    pub fn set_panel_pin(&self, pin: Option<&str>) -> io::Result<()> {
        self.request(Element::new("panel_pin").with_attr("pin", pin.unwrap_or("")))
            .map(drop)
            .map_err(plain)
    }

    /// An alert for the site's operators (§N.18): `level` `"error"`, `"warning"` or
    /// `"ok"` (a problem over), `text` what happened. The Node logs it and passes it to the
    /// Site Manager, when it can.
    pub fn alert(&self, level: &str, text: &str) -> io::Result<()> {
        self.request(
            Element::new("alert")
                .with_attr("app", &self.conn.app)
                .with_attr("level", level)
                .with_text(text),
        )
        .map(drop)
    }

    /// A typed failure event for the Site Manager's log (§N.41): an alert (§N.18) that
    /// carries the event's name and fields (`detail`, JSON). The Node puts it on its own
    /// push channel too, and passes it on once a minute at most per event and subject.
    /// Engines send these through [`web::problems::LogForward`](crate::web::problems::LogForward),
    /// rate-limited already.
    pub fn event(
        &self,
        level: &str,
        event: &str,
        text: &str,
        detail: &serde_json::Value,
    ) -> io::Result<()> {
        let mut d = detail.to_string();
        if d.len() > 2048 {
            d = "{}".into();
        }
        self.request(
            Element::new("alert")
                .with_attr("app", &self.conn.app)
                .with_attr("level", level)
                .with_attr("event", event)
                .with_attr("detail", &d)
                .with_text(text),
        )
        .map(drop)
    }

    /// Tells the Node this program runs an output or an input named `name` (shown in its
    /// panel). Unregistered when the returned value is dropped or the program ends.
    pub fn register(&self, kind: &str, name: &str, port: Option<u16>) -> io::Result<Registration> {
        register_on(&self.conn, kind, name, port)
    }
}

/// Registers an output or input on `conn` (§N.4.2).
pub(crate) fn register_on(
    conn: &Arc<Conn>,
    kind: &str,
    name: &str,
    port: Option<u16>,
) -> io::Result<Registration> {
    let mut e = Element::new("register")
        .with_attr("kind", kind)
        .with_attr("name", name);
    if let Some(p) = port {
        e = e.with_attr("port", &p.to_string());
    }
    register_element(conn, e).map(|(r, _)| r)
}

/// Sends a `register` request with a handle of this program's choosing, kept so it is
/// sent again after a reconnect; the registration and the Node's reply.
pub(crate) fn register_element(
    conn: &Arc<Conn>,
    e: Element,
) -> io::Result<(Registration, Element)> {
    let handle = format!("c{}", conn.next_handle.fetch_add(1, Ordering::Relaxed));
    let e = e.with_attr("handle", &handle);
    conn.regs.lock_ok().insert(handle.clone(), e.clone());
    match conn.request(e, Duration::from_secs(2)) {
        Ok(reply) => Ok((
            Registration {
                conn: Arc::downgrade(conn),
                handle,
            },
            reply,
        )),
        Err(err) => {
            conn.regs.lock_ok().remove(&handle);
            Err(err)
        }
    }
}

/// An output or input registered with the Node; unregistered when dropped.
#[derive(Debug)]
pub struct Registration {
    conn: std::sync::Weak<Conn>,
    handle: String,
}

impl Registration {
    /// Changes attributes of the registration (an output's sound, once known): the Node
    /// replaces it, and the new form is what a reconnect sends.
    pub fn update(&self, attrs: &[(&str, &str)]) {
        let Some(c) = self.conn.upgrade() else { return };
        let e = {
            let mut regs = c.regs.lock_ok();
            let Some(e) = regs.get_mut(&self.handle) else {
                return;
            };
            for (k, v) in attrs {
                e.attrs.retain(|(n, _)| n != k);
                e.attrs.push(((*k).to_owned(), (*v).to_owned()));
            }
            e.clone()
        };
        c.send(e);
    }

    /// A meter for this registration's levels (an output's: meters.rs).
    pub(crate) fn meter(&self) -> Arc<meters::Meter> {
        meters::Meter::new(self.conn.clone(), self.handle.clone())
    }

    /// Sends a message about this registration, not waiting for the reply.
    pub(crate) fn send(&self, e: Element) {
        if let Some(c) = self.conn.upgrade() {
            c.send(e);
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(c) = self.conn.upgrade() {
            c.regs.lock_ok().remove(&self.handle);
            c.send(Element::new("unregister").with_attr("handle", &self.handle));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_certificate_verify_inputs_are_signed() {
        let mut ok = vec![0x20u8; 64];
        ok.extend_from_slice(b"TLS 1.3, client CertificateVerify\0");
        ok.extend_from_slice(&[7u8; 32]);
        assert!(is_certificate_verify_input(&ok));
        let mut ok48 = vec![0x20u8; 64];
        ok48.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
        ok48.extend_from_slice(&[7u8; 48]);
        assert!(is_certificate_verify_input(&ok48));
        // Wrong context, wrong padding, wrong hash length, no zero byte, arbitrary data.
        let mut bad = ok.clone();
        bad[70] ^= 1;
        assert!(!is_certificate_verify_input(&bad));
        let mut bad = ok.clone();
        bad[3] = b'x';
        assert!(!is_certificate_verify_input(&bad));
        let mut bad = ok.clone();
        bad.push(1);
        assert!(!is_certificate_verify_input(&bad));
        let mut bad = vec![0x20u8; 64];
        bad.extend_from_slice(b"TLS 1.3, client CertificateVerify");
        bad.extend_from_slice(&[7u8; 33]);
        assert!(!is_certificate_verify_input(&bad));
        assert!(!is_certificate_verify_input(b"<idn_admin op=\"revoke\"/>"));
    }

    #[test]
    fn endpoints_follow_the_environment() {
        // One test, so the variable isn't raced by another.
        // SAFETY: tests in this module don't read IDN_NODE concurrently.
        for off in ["off", "OFF", "0", "false", "no"] {
            unsafe { std::env::set_var("IDN_NODE", off) };
            assert!(endpoints().is_empty(), "{off}");
        }
        unsafe { std::env::set_var("IDN_NODE", "/tmp/x.sock") };
        assert_eq!(endpoints(), vec!["/tmp/x.sock".to_owned()]);
        // SAFETY: as above; and under nextest each test runs in a process of its own, while std's
        // own readers take its environment lock.
        unsafe { std::env::remove_var("IDN_NODE") };
        assert_eq!(endpoints()[0], system_endpoint());
    }
}
