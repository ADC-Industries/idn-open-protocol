//! Discovery Server protocols (spec chapter 8): NUL-framed XML on TCP, the plain source
//! protocol, and the obfuscated receiver protocol.

use super::Source;
use super::responder::Published;
use crate::scramble::TABLE_K;
use crate::xml::{self, Element};
use idn_common::err::invalid_data as bad;
use idn_common::sync::LockOk as _;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 5959;
/// Largest message accepted (§8.1 gives no limit; this bounds memory).
pub const MAX_MESSAGE: usize = 4 << 20;

/// Resolves `host[:port]` with the default port 5959. IPv6 addresses are accepted bare
/// (`fe80::1`) or in brackets, with a port only in brackets (`[fe80::1]:5959`).
pub fn server_addr(s: &str) -> io::Result<SocketAddr> {
    let full = with_default_port(s, DEFAULT_PORT);
    let addr = full
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("cannot resolve {s}")))?;
    // A name (not an address) is remembered, so that reconnects look it up again: a
    // server whose address changes (DHCP) is found again (DS-5).
    if full.parse::<SocketAddr>().is_err() {
        let mut n = NAMES.lock_ok();
        n.retain(|(a, _)| *a != addr);
        n.push((addr, full));
    }
    Ok(addr)
}

/// Server names by the address they first resolved to.
static NAMES: std::sync::Mutex<Vec<(SocketAddr, String)>> = std::sync::Mutex::new(Vec::new());

/// Connects to the Discovery Server first resolved as `server`, looking its name up
/// again if it was given as one (its address may have changed since).
pub(crate) fn connect_server(server: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let name = NAMES
        .lock_ok()
        .iter()
        .find(|(a, _)| *a == server)
        .map(|(_, n)| n.clone());
    let addr = name
        .and_then(|n| n.to_socket_addrs().ok()?.next())
        .unwrap_or(server);
    TcpStream::connect_timeout(&addr, timeout)
}

/// `host[:port]` with `port` added when none is given, IPv6 included.
#[doc(hidden)]
pub fn with_default_port(s: &str, port: u16) -> String {
    let s = s.trim();
    if s.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{s}]:{port}")
    } else if s.starts_with('[') && s.ends_with(']') {
        format!("{s}:{port}")
    } else if s.contains(':') {
        s.to_owned()
    } else {
        format!("{s}:{port}")
    }
}

// ---------------------------------------------------------------------------------------
// Framing (§8.1) and obfuscation (§8.3.1).

/// Reads NUL-terminated messages from a stream. A message cut short by a read timeout
/// is kept and continued on the next call (callers poll with short timeouts; dropping the
/// part already read made the next message start mid-way).
pub struct MsgReader<R> {
    r: BufReader<R>,
    pending: Vec<u8>,
}

impl<R: io::Read> MsgReader<R> {
    pub fn new(r: R) -> Self {
        MsgReader {
            r: BufReader::new(r),
            pending: Vec::new(),
        }
    }

    /// The next message without its NUL; `Ok(None)` at end of stream.
    pub fn read_msg(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.read_with(MAX_MESSAGE, |_| Ok(()))
    }

    /// [`read_msg`](Self::read_msg) at most `cap` bytes long, `before` called ahead of
    /// each read of the stream.
    fn read_with(
        &mut self,
        cap: usize,
        mut before: impl FnMut(&R) -> io::Result<()>,
    ) -> io::Result<Option<Vec<u8>>> {
        loop {
            if self.r.buffer().is_empty() {
                before(self.r.get_ref())?;
            }
            let avail = match self.r.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A timeout keeps `pending` for the next call.
                Err(e) => return Err(e),
            };
            if avail.is_empty() {
                return if self.pending.is_empty() {
                    Ok(None)
                } else {
                    self.pending.clear();
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
            if let Some(p) = avail.iter().position(|&b| b == 0) {
                self.pending.extend_from_slice(&avail[..p]);
                self.r.consume(p + 1);
                return Ok(Some(std::mem::take(&mut self.pending)));
            }
            let n = avail.len();
            self.pending.extend_from_slice(avail);
            self.r.consume(n);
            if self.pending.len() > cap {
                self.pending.clear();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "discovery message too large",
                ));
            }
        }
    }
}

impl MsgReader<TcpStream> {
    /// A connection's first message, all of it within `within` and at most `cap` bytes:
    /// each read's timeout alone let a byte every few seconds hold the connection.
    /// `Ok(None)` at end of stream; a timeout is an error.
    pub fn read_first(&mut self, within: Duration, cap: usize) -> io::Result<Option<Vec<u8>>> {
        let end = Instant::now() + within;
        let m = self.read_with(cap, |s| {
            let left = end
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "first message too slow"))?;
            s.set_read_timeout(Some(left))
        });
        self.r.get_ref().set_read_timeout(None)?;
        m
    }
}

pub fn write_plain<W: Write>(w: &mut W, xml: &str) -> io::Result<()> {
    let mut b = xml.as_bytes().to_vec();
    b.push(0);
    w.write_all(&b)
}

pub use idn_common::text::{base64 as base64_encode, unbase64 as base64_decode};

fn xor_s(b: &mut [u8]) {
    for (i, x) in b.iter_mut().enumerate() {
        *x ^= TABLE_K[i % 125];
    }
}

/// Obfuscates one receiver-protocol message (without the NUL).
pub fn obfuscate(xml: &str) -> String {
    let mut b = xml.as_bytes().to_vec();
    xor_s(&mut b);
    base64_encode(&b)
}

pub fn deobfuscate(wire: &[u8]) -> Option<String> {
    let mut b = base64_decode(wire)?;
    xor_s(&mut b);
    String::from_utf8(b).ok()
}

// ---------------------------------------------------------------------------------------
// Source protocol (§8.2).

/// `metadata` is the `<metadata>` text: empty, or the IDN discovery hint (§E.2.2), which
/// must be text, not a child element (the NDI server drops those).
pub fn source_element(
    name: &str,
    address: &str,
    port: u16,
    groups: &[String],
    metadata: &str,
) -> Element {
    let mut g = Element::new("groups");
    for x in groups {
        g.children.push(Element::new("group").with_text(x));
    }
    Element::new("source")
        .with_child(Element::new("name").with_text(name))
        .with_child(Element::new("metadata").with_text(metadata))
        .with_child(Element::new("address").with_text(address))
        .with_child(Element::new("port").with_text(&port.to_string()))
        .with_child(g)
}

pub fn source_from_element(e: &Element) -> Option<Source> {
    let port = e.child_text("port").trim().parse().ok()?;
    let addr: Option<IpAddr> = e.child_text("address").trim().parse().ok();
    let groups = e.child("groups").map_or_else(Vec::new, |g| {
        g.children_named("group").map(|x| x.text.clone()).collect()
    });
    Some(Source {
        name: e.child_text("name").to_owned(),
        addr: addr.filter(|a| !a.is_unspecified()),
        port,
        host: String::new(),
        groups,
        routing: false,
        listing_port: None,
        idn_ext: crate::ext::parse_hint(e.child_text("metadata")),
    })
}

/// Registers a source (§8.2.1). The registration lasts while the returned stream is open.
pub fn register_source(server: SocketAddr, p: &Published) -> io::Result<TcpStream> {
    let mut s = connect_server(server, Duration::from_secs(5))?;
    // A server host that vanishes is noticed, so the caller re-registers (§8.3.9).
    crate::transport::keepalive(&s, Duration::from_secs(5));
    s.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    write_plain(
        &mut s,
        &source_element(
            &p.name,
            "0.0.0.0",
            p.port,
            &p.groups,
            &p.idn_ext
                .as_deref()
                .map(crate::ext::hint)
                .unwrap_or_default(),
        )
        .to_xml(),
    )?;
    Ok(s)
}

/// A change pushed to a finder (§8.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceChange {
    All(Vec<Source>),
    Added(Source),
    Removed(Source),
}

pub fn parse_source_message(m: &[u8]) -> Option<SourceChange> {
    let e = xml::parse(std::str::from_utf8(m).ok()?)?;
    let first = || e.child("source").and_then(source_from_element);
    match e.name.as_str() {
        "sources" => Some(SourceChange::All(
            e.children_named("source")
                .filter_map(source_from_element)
                .collect(),
        )),
        "add_source" => first().map(SourceChange::Added),
        "remove_source" => first().map(SourceChange::Removed),
        _ => None,
    }
}

/// A finder connection: sends `<query/>`, then yields the list and later changes.
pub struct SourceWatcher {
    r: MsgReader<TcpStream>,
}

impl SourceWatcher {
    pub fn connect(server: SocketAddr) -> io::Result<Self> {
        Self::connect_within(server, Duration::from_secs(5))
    }

    /// As [`connect`](Self::connect), with this TCP connect timeout.
    pub fn connect_within(server: SocketAddr, timeout: Duration) -> io::Result<Self> {
        let mut s = connect_server(server, timeout)?;
        crate::transport::keepalive(&s, Duration::from_secs(5));
        s.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
        write_plain(&mut s, "<query/>")?;
        Ok(SourceWatcher {
            r: MsgReader::new(s),
        })
    }

    pub fn set_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.r.r.get_ref().set_read_timeout(t)
    }

    /// The next recognised change; `Ok(None)` when the server closes the connection.
    pub fn next_change(&mut self) -> io::Result<Option<SourceChange>> {
        loop {
            match self.r.read_msg()? {
                None => return Ok(None),
                Some(m) => {
                    if let Some(c) = parse_source_message(&m) {
                        return Ok(Some(c));
                    }
                }
            }
        }
    }
}

/// One-shot query (§8.2.2): the current source list.
pub fn query_sources(server: SocketAddr, timeout: Duration) -> io::Result<Vec<Source>> {
    let mut w = SourceWatcher::connect(server)?;
    w.set_timeout(Some(timeout))?;
    loop {
        match w.next_change()? {
            Some(SourceChange::All(v)) => return Ok(v),
            Some(_) => {}
            None => return Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Receiver protocol (§8.3).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Advertiser,
    Listener,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Receiver,
    Source,
}

impl Subject {
    pub fn tag(self) -> &'static str {
        match self {
            Subject::Receiver => "receiver",
            Subject::Source => "source",
        }
    }
}

/// An obfuscated receiver-protocol connection.
pub struct Session {
    w: TcpStream,
    r: MsgReader<TcpStream>,
    /// The listener identifier the server assigned (listeners only).
    pub listener_id: Option<String>,
}

impl Session {
    /// Connects and exchanges the opening hello (§8.3.2).
    pub fn connect(
        server: SocketAddr,
        subject: Subject,
        role: Role,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::connect_with(server, subject, role, Duration::from_secs(5), timeout)
    }

    /// As [`Session::connect`], with a separate TCP connect timeout.
    pub fn connect_with(
        server: SocketAddr,
        subject: Subject,
        role: Role,
        connect: Duration,
        timeout: Duration,
    ) -> io::Result<Self> {
        let mut w = connect_server(server, connect)?;
        crate::transport::keepalive(&w, Duration::from_secs(5));
        w.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
        let r = MsgReader::new(w.try_clone()?);
        let role_tag = match role {
            Role::Advertiser => "advertiser",
            Role::Listener => "listener",
        };
        write_plain(
            &mut w,
            &format!("<query/><{}/><{role_tag}/>", subject.tag()),
        )?;
        let mut s = Session {
            w,
            r,
            listener_id: None,
        };
        s.w.set_read_timeout(Some(timeout))?;
        let reply = s.recv_raw()?.ok_or(io::ErrorKind::UnexpectedEof)?;
        let parts = xml::parse_fragment(&reply).ok_or_else(|| bad("unparsable hello reply"))?;
        if !parts.iter().any(|e| e.name == "compatible") {
            return Err(bad(format!("server not compatible: {reply}")));
        }
        s.listener_id = parts
            .iter()
            .find(|e| e.name == "listener_id")
            .map(|e| e.text.clone());
        s.w.set_read_timeout(None)?;
        Ok(s)
    }

    pub fn set_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.w.set_read_timeout(t)
    }

    pub fn send(&mut self, e: &Element) -> io::Result<()> {
        write_plain(&mut self.w, &obfuscate(&e.to_xml()))
    }

    fn recv_raw(&mut self) -> io::Result<Option<String>> {
        match self.r.read_msg()? {
            None => Ok(None),
            Some(m) => deobfuscate(&m)
                .map(Some)
                .ok_or_else(|| bad("bad obfuscated message")),
        }
    }

    /// The next message as XML elements (a message may be a fragment).
    pub fn recv(&mut self) -> io::Result<Option<Vec<Element>>> {
        match self.recv_raw()? {
            None => Ok(None),
            Some(s) => xml::parse_fragment(&s)
                .map(Some)
                .ok_or_else(|| bad("unparsable message")),
        }
    }
}

/// Monitor properties NDI 6.3 receivers declare (§8.3.3), with their types.
pub const RECEIVER_PROPERTIES: &[(&str, &str)] = &[
    ("sdk-version", "string"),
    ("source-name", "string"),
    ("source-url", "string"),
    ("connection-state", "string"),
    ("audio-present", "bool"),
    ("audio-codec", "string"),
    ("audio-channels", "int"),
    ("audio-sample-rate", "int"),
    ("audio-receive-mode", "string"),
    ("video-present", "bool"),
    ("video-codec", "string"),
    ("video-resolution", "string"),
    ("video-frame-rate", "string"),
    ("video-frame-type", "string"),
    ("video-color-primaries", "string"),
    ("video-transfer-function", "string"),
    ("video-matrix-coefficients", "string"),
    ("video-has-alpha", "bool"),
    ("video-receive-mode", "string"),
];

/// A receiver advertisement (§8.3.3) that accepts `connect` and declares the NDI properties.
pub fn receiver_element(
    id: &str,
    name: &str,
    typ: &str,
    input_id: &str,
    input_name: &str,
) -> Element {
    receiver_element_with(id, name, typ, input_id, input_name, &[], true)
}

fn monitor_capabilities(props: &[(&str, &str)], extras: &[(&str, &str)]) -> Element {
    let mut mon = Element::new("monitor_capabilities");
    for (n, t) in props.iter().chain(extras) {
        mon.children.push(
            Element::new("property")
                .with_attr("name", n)
                .with_attr("type", t)
                .with_attr("autosub", "false"),
        );
    }
    mon
}

/// A receiver advertisement with extra monitor properties; without `control` it
/// declares no commands (monitoring only).
pub fn receiver_element_with(
    id: &str,
    name: &str,
    typ: &str,
    input_id: &str,
    input_name: &str,
    extras: &[(&str, &str)],
    control: bool,
) -> Element {
    let mut ctl = Element::new("control_capabilities");
    if control {
        ctl.children.push(
            Element::new("command")
                .with_attr("name", "connect")
                .with_child(
                    Element::new("arg")
                        .with_attr("type", "string")
                        .with_attr("name", "source-name"),
                ),
        );
    }
    let mon = monitor_capabilities(RECEIVER_PROPERTIES, extras);
    Element::new("receiver")
        .with_child(Element::new("receiver_id").with_text(id))
        .with_child(Element::new("name").with_text(name))
        .with_child(Element::new("type").with_text(typ))
        .with_child(Element::new("input_id").with_text(input_id))
        .with_child(Element::new("input_name").with_text(input_name))
        .with_child(Element::new("metadata"))
        .with_child(Element::new("address").with_text("0.0.0.0"))
        .with_child(Element::new("port").with_text("0"))
        .with_child(Element::new("capabilities").with_child(ctl).with_child(mon))
}

/// Monitor properties NDI 6.3 sources declare (§8.3.7).
pub const SOURCE_PROPERTIES: &[(&str, &str)] = &[
    ("sdk-version", "string"),
    ("source-name", "string"),
    ("source-url", "string"),
    ("connection-state", "string"),
    ("connected-receivers", "int"),
    ("audio-present", "bool"),
    ("audio-codec", "string"),
    ("audio-channels", "int"),
    ("audio-sample-rate", "int"),
    ("video-present", "bool"),
    ("video-codec", "string"),
    ("video-resolution", "string"),
    ("video-frame-rate", "string"),
    ("video-frame-type", "string"),
    ("video-has-alpha", "bool"),
    ("video-color-primaries", "string"),
    ("video-transfer-function", "string"),
    ("video-matrix-coefficients", "string"),
    ("tally-on-program", "bool"),
    ("tally-on-preview", "bool"),
];

/// A source advertisement in the receiver protocol (§8.3.7). It carries the same groups
/// as the source-protocol registration, after the capabilities; without them NDI 6.3
/// finders list the source twice.
pub fn source_advert_element(id: &str, name: &str, port: u16, groups: &[String]) -> Element {
    source_advert_element_with(id, name, port, groups, &[])
}

/// A source advertisement with extra monitor properties.
pub fn source_advert_element_with(
    id: &str,
    name: &str,
    port: u16,
    groups: &[String],
    extras: &[(&str, &str)],
) -> Element {
    let mut g = Element::new("groups");
    for x in groups {
        g.children.push(Element::new("group").with_text(x));
    }
    let mon = monitor_capabilities(SOURCE_PROPERTIES, extras);
    Element::new("source")
        .with_child(Element::new("source_id").with_text(id))
        .with_child(Element::new("name").with_text(name))
        .with_child(Element::new("metadata"))
        .with_child(Element::new("address").with_text("0.0.0.0"))
        .with_child(Element::new("port").with_text(&port.to_string()))
        .with_child(Element::new("capabilities").with_child(mon))
        .with_child(g)
}

/// A `control_receiver` connect command (§8.3.5).
pub fn connect_command(receiver_id: &str, source: &str) -> Element {
    Element::new("control_receiver")
        .with_child(Element::new("receiver_id").with_text(receiver_id))
        .with_child(
            Element::new("command")
                .with_attr("name", "connect")
                .with_child(
                    Element::new("arg")
                        .with_attr("name", "source-name")
                        .with_attr("type", "string")
                        .with_text(source),
                ),
        )
}

/// The source name of a connect command, if `e` is one for `receiver_id`.
pub fn parse_connect_command<'a>(e: &'a Element, receiver_id: &str) -> Option<&'a str> {
    if e.name != "control_receiver" || e.child_text("receiver_id") != receiver_id {
        return None;
    }
    let c = e
        .child("command")
        .filter(|c| c.attr("name") == Some("connect"))?;
    c.children_named("arg")
        .find(|a| a.attr("name") == Some("source-name"))
        .map(|a| a.text.as_str())
}

/// A `monitor_events` report for one subject (§8.3.6).
pub fn monitor_events(
    subject: Subject,
    listener_id: &str,
    id: &str,
    props: &[(&str, String)],
) -> Element {
    let mut r = Element::new(subject.tag()).with_attr("id", id);
    for (n, v) in props {
        r.children.push(
            Element::new("property")
                .with_attr("name", n)
                .with_attr("value", v),
        );
    }
    Element::new("monitor_events")
        .with_child(Element::new("events_type").with_text(subject.tag()))
        .with_child(
            Element::new("listener")
                .with_attr("id", listener_id)
                .with_child(r),
        )
}

/// A random version-4 UUID string.
pub fn uuid4() -> String {
    let mut b = crate::random::secure_random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = idn_common::hex::hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &x[..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..]
    )
}

/// Monitor properties IDN sources add to the NDI set (§E.5.1). NDI listeners pass them
/// through unchanged (§8.3.6.1).
pub const IDN_SOURCE_PROPERTIES: &[(&str, &str)] = &[
    ("x-idn-fps", "string"),
    ("x-idn-receivers-encrypted", "int"),
    ("x-idn-machine-receivers", "int"),
    ("x-idn-gpu-receivers", "int"),
    ("x-idn-shq-encodes", "int"),
    ("x-idn-codec-encoders", "int"),
    ("x-idn-frames-skipped", "int"),
    ("x-idn-extensions", "string"),
];

/// Monitor properties IDN receivers add (§E.5.1): NDI receivers report no counters.
pub const IDN_RECEIVER_PROPERTIES: &[(&str, &str)] = &[
    ("x-idn-fps", "string"),
    ("x-idn-frames-received", "int"),
    ("x-idn-frames-dropped", "int"),
    ("x-idn-frames-lost", "int"),
    ("x-idn-decode-errors", "int"),
    ("x-idn-rtt-ms", "string"),
    ("x-idn-encrypted", "bool"),
    ("x-idn-media-fallback", "bool"),
    ("x-idn-authenticated", "bool"),
    ("x-idn-key-changed", "bool"),
    ("x-idn-sync", "string"),
    ("x-idn-sync-delay-ms", "string"),
    ("x-idn-sync-late", "int"),
    ("x-idn-transport", "string"),
    ("x-idn-extensions", "string"),
];

/// Source counters that NDI reports only when requested by name, then every 5 s
/// (§8.3.6.1).
pub const SOURCE_COUNTERS: &[&str] = &[
    "video-frames-sent",
    "video-bytes-sent",
    "audio-frames-sent",
    "audio-bytes-sent",
    "metadata-frames-sent",
    "metadata-bytes-sent",
];

const COUNTER_PERIOD: Duration = Duration::from_secs(5);
/// Delay between attempts to reach the Discovery Server.
pub const RETRY: Duration = Duration::from_secs(2);

/// Sleeps up to `d`, returning early (true) when `stop` is set.
pub(crate) fn sleep_or_stop(stop: &std::sync::atomic::AtomicBool, d: Duration) -> bool {
    let end = std::time::Instant::now() + d;
    while std::time::Instant::now() < end {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.load(std::sync::atomic::Ordering::Relaxed)
}

/// An advertiser running in the background (§8.3.3–§8.3.7). It keeps the advertisement
/// alive across Discovery Server restarts (same id, retried every [`RETRY`]), delivers
/// `connect` commands, and answers monitor subscriptions from a shared property table:
/// a snapshot of the requested properties, then changes (counters every 5 s).
pub struct Advertiser {
    /// The advertised id (`receiver_id` or `source_id`).
    pub receiver_id: String,
    /// Source names from `connect` commands, in arrival order, if the advertisement
    /// declares the command; the latest [`Advertiser::CONNECT_KEPT`] while unread. (In a
    /// mutex so that the advertiser, and outputs holding one, stay `Sync`.)
    pub connect: std::sync::Mutex<std::sync::mpsc::Receiver<String>>,
    /// Current monitor property values; update them and changes are reported.
    pub props: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
    /// Whether the advertisement is currently registered with the server.
    pub connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Advertiser {
    /// `connect` commands kept while the app hasn't read them; more are dropped.
    pub const CONNECT_KEPT: usize = 16;
    /// Monitor subscribers served at once; more are ignored.
    pub const MAX_SUBSCRIBERS: usize = 64;

    /// Advertises a receiver that accepts `connect` commands, with the NDI properties.
    pub fn start(server: SocketAddr, name: &str, typ: &str) -> io::Result<Self> {
        Ok(Self::receiver(server, name, typ, &[], true))
    }

    /// Advertises a source with its control port and groups, with the NDI properties.
    pub fn start_source(
        server: SocketAddr,
        name: &str,
        port: u16,
        groups: &[String],
    ) -> io::Result<Self> {
        Ok(Self::source(server, name, port, groups, &[]))
    }

    /// Advertises a receiver declaring the NDI properties plus `extras`. Without
    /// `control`, it declares no commands: monitoring only, it can't be switched remotely.
    pub fn receiver(
        server: SocketAddr,
        name: &str,
        typ: &str,
        extras: &[(&str, &str)],
        control: bool,
    ) -> Self {
        let id = uuid4();
        let e = receiver_element_with(&id, name, typ, &uuid4(), "IDN", extras, control);
        Self::start_with(server, Subject::Receiver, id, e)
    }

    /// Advertises a source declaring the NDI properties plus `extras` (§8.3.7).
    pub fn source(
        server: SocketAddr,
        name: &str,
        port: u16,
        groups: &[String],
        extras: &[(&str, &str)],
    ) -> Self {
        let id = uuid4();
        let e = source_advert_element_with(&id, name, port, groups, extras);
        Self::start_with(server, Subject::Source, id, e)
    }

    fn start_with(
        server: SocketAddr,
        subject: Subject,
        receiver_id: String,
        advert: Element,
    ) -> Self {
        use std::sync::atomic::Ordering;
        // Bounded: any client of the server can send commands for an advertised id, and
        // advertisers that declare no `connect` (monitoring only, sources) never read them.
        let (tx, connect) = std::sync::mpsc::sync_channel(Self::CONNECT_KEPT);
        let accepts_connect = advert
            .child("capabilities")
            .and_then(|c| c.child("control_capabilities"))
            .is_some_and(|c| {
                c.children_named("command")
                    .any(|k| k.attr("name") == Some("connect"))
            });
        let props: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>> =
            Default::default();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let (rid, props, stop, connected) = (
                receiver_id.clone(),
                props.clone(),
                stop.clone(),
                connected.clone(),
            );
            let id_tag = format!("{}_id", subject.tag());
            let declared: std::collections::BTreeSet<String> = advert
                .child("capabilities")
                .and_then(|c| c.child("monitor_capabilities"))
                .map(|m| {
                    m.children_named("property")
                        .filter_map(|p| p.attr("name"))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut s = match Session::connect_with(
                        server,
                        subject,
                        Role::Advertiser,
                        Duration::from_secs(1),
                        Duration::from_secs(3),
                    ) {
                        Ok(s) => s,
                        Err(_) => {
                            if sleep_or_stop(&stop, RETRY) {
                                return;
                            }
                            continue;
                        }
                    };
                    if s.send(&advert).is_err()
                        || s.set_timeout(Some(Duration::from_millis(250))).is_err()
                    {
                        // A server that accepts and then drops us: wait, don't hammer it.
                        if sleep_or_stop(&stop, RETRY) {
                            return;
                        }
                        continue;
                    }
                    connected.store(true, Ordering::Relaxed);
                    // Each subscriber with the properties it asked for (§8.3.6.1).
                    let mut subs: Vec<(String, std::collections::BTreeSet<String>)> = Vec::new();
                    let mut sent: std::collections::BTreeMap<String, String> = Default::default();
                    let mut counters_at = std::time::Instant::now();
                    'session: while !stop.load(Ordering::Relaxed) {
                        match s.recv() {
                            Ok(Some(msgs)) => {
                                for e in msgs {
                                    if let Some(src) = parse_connect_command(&e, &rid) {
                                        if accepts_connect {
                                            // Full: the app isn't reading; drop it.
                                            let _ = tx.try_send(src.to_owned());
                                        }
                                    } else if e.name == "monitor_request"
                                        && e.child_text(&id_tag) == rid
                                    {
                                        let lid = e.child_text("listener_id").to_owned();
                                        subs.retain(|(l, _)| *l != lid);
                                        // Bounded: each subscriber gets its own copy of
                                        // every change. Monitors already subscribed stay.
                                        if e.attr("unsubscribe") != Some("true")
                                            && subs.len() < Self::MAX_SUBSCRIBERS
                                        {
                                            let want: std::collections::BTreeSet<String> = e
                                                .child("monitor_capabilities")
                                                .map(|m| {
                                                    m.children_named("property")
                                                        .filter_map(|p| p.attr("name"))
                                                        .map(str::to_owned)
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                            // Declared but not yet known: "" (as NDI 6.3, lab X-m1).
                                            let all = props.lock_ok().clone();
                                            let snapshot: Vec<(&str, String)> = want
                                                .iter()
                                                .filter_map(|k| match all.get(k) {
                                                    Some(v) => Some((k.as_str(), v.clone())),
                                                    None if declared.contains(k) => {
                                                        Some((k.as_str(), String::new()))
                                                    }
                                                    None => None,
                                                })
                                                .collect();
                                            if !snapshot.is_empty()
                                                && s.send(&monitor_events(
                                                    subject, &lid, &rid, &snapshot,
                                                ))
                                                .is_err()
                                            {
                                                break 'session;
                                            }
                                            subs.push((lid, want));
                                        }
                                    } else if e.name == "remove_listener" {
                                        subs.retain(|(l, _)| *l != e.text);
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) if crate::transport::is_poll_timeout(&e) => {}
                            Err(_) => break,
                        }
                        let now = props.lock_ok().clone();
                        let counters_due = counters_at.elapsed() >= COUNTER_PERIOD;
                        if counters_due {
                            counters_at = std::time::Instant::now();
                        }
                        let changed: Vec<(&str, String)> = now
                            .iter()
                            .filter(|(k, v)| {
                                sent.get(*k) != Some(*v)
                                    && (counters_due || !SOURCE_COUNTERS.contains(&k.as_str()))
                            })
                            .map(|(k, v)| (k.as_str(), v.clone()))
                            .collect();
                        for (lid, want) in &subs {
                            let mine: Vec<(&str, String)> = changed
                                .iter()
                                .filter(|(k, _)| want.contains(*k))
                                .cloned()
                                .collect();
                            if !mine.is_empty()
                                && s.send(&monitor_events(subject, lid, &rid, &mine)).is_err()
                            {
                                break 'session;
                            }
                        }
                        for (k, v) in changed {
                            sent.insert(k.to_owned(), v);
                        }
                    }
                    connected.store(false, Ordering::Relaxed);
                    if sleep_or_stop(&stop, RETRY) {
                        return;
                    }
                }
            })
        };
        Advertiser {
            receiver_id,
            connect: std::sync::Mutex::new(connect),
            props,
            connected,
            stop,
            thread: Some(thread),
        }
    }

    /// Sets a property; subscribers get the change within about 250 ms.
    pub fn set(&self, k: &str, v: impl ToString) {
        self.props.lock_ok().insert(k.to_owned(), v.to_string());
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Not stalled by a half-dead server (M-3).
        if let Some(t) = self.thread.take() {
            crate::pool::join_within(t, Duration::from_millis(500));
        }
    }
}

/// A source registration (§8.2.1) kept alive in the background: re-registered whenever
/// the connection to the Discovery Server drops (server restart), every [`RETRY`].
/// Dropping it withdraws the source.
pub struct SourceRegistration {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SourceRegistration {
    /// Starts registering `p` with `server`. Never fails: an unreachable server is retried.
    pub fn start(server: SocketAddr, p: &Published) -> Self {
        use std::io::Read;
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let msg = source_element(
            &p.name,
            "0.0.0.0",
            p.port,
            &p.groups,
            &p.idn_ext
                .as_deref()
                .map(crate::ext::hint)
                .unwrap_or_default(),
        )
        .to_xml();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(mut s) = connect_server(server, Duration::from_secs(1))
                        // A server host that vanished breaks the read below with an error,
                        // so the registration is renewed with the server when it is back.
                        && {
                            crate::transport::keepalive(&s, Duration::from_secs(5));
                            true
                        }
                        && s.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT)).is_ok()
                        && write_plain(&mut s, &msg).is_ok()
                        && s.set_read_timeout(Some(Duration::from_millis(250))).is_ok()
                    {
                        let mut buf = [0u8; 4096];
                        // The server sends nothing back on a registration; EOF means it went away.
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                            match s.read(&mut buf) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(e) if crate::transport::is_poll_timeout(&e) => {}
                                Err(_) => break,
                            }
                        }
                    }
                    if sleep_or_stop(&stop, RETRY) {
                        return;
                    }
                }
            })
        };
        SourceRegistration {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for SourceRegistration {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Not stalled by a half-dead server (M-3).
        if let Some(t) = self.thread.take() {
            crate::pool::join_within(t, Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_are_looked_up_again_on_connect() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let addr = server_addr(&format!("localhost:{port}")).unwrap();
        assert!(
            NAMES
                .lock_ok()
                .iter()
                .any(|(a, n)| *a == addr && n.starts_with("localhost"))
        );
        // An address given as one isn't remembered.
        server_addr(&format!("127.0.0.1:{port}")).unwrap();
        assert!(!NAMES.lock_ok().iter().any(|(_, n)| n.starts_with("127.")));
        assert!(connect_server(addr, Duration::from_secs(2)).is_ok());
    }

    /// A reader that delivers its data in pieces, with a timeout after each piece.
    struct Stutter(Vec<Vec<u8>>);
    impl io::Read for Stutter {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.first_mut() {
                None => Ok(0),
                Some(p) if p.is_empty() => {
                    self.0.remove(0);
                    Err(io::ErrorKind::WouldBlock.into())
                }
                Some(p) => {
                    let n = p.len().min(buf.len());
                    buf[..n].copy_from_slice(&p[..n]);
                    p.drain(..n);
                    Ok(n)
                }
            }
        }
    }

    #[test]
    fn a_timeout_mid_message_loses_nothing() {
        // "<a/>" then "<bc/>", split mid-message by a timeout.
        let mut r = MsgReader::new(Stutter(vec![b"<a/>\0<b".to_vec(), b"c/>\0".to_vec()]));
        assert_eq!(r.read_msg().unwrap().unwrap(), b"<a/>");
        assert_eq!(r.read_msg().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(r.read_msg().unwrap().unwrap(), b"<bc/>");
        assert_eq!(r.read_msg().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(
            r.read_msg().unwrap().is_none(),
            "then the end of the stream"
        );
    }

    #[test]
    fn base64_roundtrip_and_rfc_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(i.as_bytes()), o);
            assert_eq!(base64_decode(o.as_bytes()).unwrap(), i.as_bytes());
        }
        assert!(base64_decode(b"Zg=").is_none());
        assert!(base64_decode(b"Zg==Zg==").is_none());
    }

    #[test]
    fn obfuscation_restarts_and_uses_125_bytes() {
        let long = "x".repeat(300);
        let enc = obfuscate(&long);
        let raw = base64_decode(enc.as_bytes()).unwrap();
        assert_eq!(raw[0] ^ b'x', TABLE_K[0]);
        assert_eq!(raw[125] ^ b'x', TABLE_K[0]);
        assert_eq!(deobfuscate(enc.as_bytes()).unwrap(), long);
        assert_eq!(
            deobfuscate(obfuscate("<compatible/>").as_bytes()).unwrap(),
            "<compatible/>"
        );
    }

    #[test]
    fn source_messages() {
        let e = source_element("SND (LAB)", "10.0.0.5", 5961, &["public".into()], "");
        assert_eq!(
            e.to_xml(),
            "<source><name>SND (LAB)</name><metadata/><address>10.0.0.5</address><port>5961</port><groups><group>public</group></groups></source>"
        );
        let add = Element::new("add_source").with_child(e).to_xml();
        let SourceChange::Added(s) = parse_source_message(add.as_bytes()).unwrap() else {
            panic!()
        };
        assert_eq!(s.socket_addr(), Some("10.0.0.5:5961".parse().unwrap()));
    }

    #[test]
    fn connect_command_roundtrip() {
        let c = connect_command("abc", "SND (LAB)");
        let p = xml::parse(&c.to_xml()).unwrap();
        assert_eq!(parse_connect_command(&p, "abc"), Some("SND (LAB)"));
        assert_eq!(parse_connect_command(&p, "other"), None);
    }

    #[test]
    fn msg_reader_splits() {
        let data = b"<a/>\0<b/>\0".to_vec();
        let mut r = MsgReader::new(&data[..]);
        assert_eq!(r.read_msg().unwrap().unwrap(), b"<a/>");
        assert_eq!(r.read_msg().unwrap().unwrap(), b"<b/>");
        assert!(r.read_msg().unwrap().is_none());
    }

    #[test]
    fn connect_commands_are_bounded_and_only_kept_when_declared() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || super::super::ds_server::serve(l));
        let control = Advertiser::receiver(addr, "RCV (CONTROL)", "video", &[], true);
        let monitor_only = Advertiser::receiver(addr, "RCV (MONITOR)", "video", &[], false);
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !(control.connected.load(std::sync::atomic::Ordering::Relaxed)
            && monitor_only
                .connected
                .load(std::sync::atomic::Ordering::Relaxed))
            && std::time::Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut l = Session::connect(
            addr,
            Subject::Receiver,
            Role::Listener,
            Duration::from_secs(3),
        )
        .unwrap();
        for i in 0..50 {
            for a in [&control, &monitor_only] {
                l.send(&connect_command(&a.receiver_id, &format!("SRC ({i})")))
                    .unwrap();
            }
        }
        std::thread::sleep(Duration::from_millis(800));
        let drain = |a: &Advertiser| -> Vec<String> {
            let rx = a.connect.lock_ok();
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        };
        let kept = drain(&control);
        assert_eq!(kept.len(), Advertiser::CONNECT_KEPT);
        assert_eq!(kept[0], "SRC (0)");
        assert!(
            drain(&monitor_only).is_empty(),
            "no connect declared, nothing kept"
        );
    }

    #[test]
    fn server_addresses_with_and_without_ports() {
        for (given, want) in [
            ("10.0.0.5", "10.0.0.5:5959"),
            ("10.0.0.5:6000", "10.0.0.5:6000"),
            ("::1", "[::1]:5959"),
            ("[::1]", "[::1]:5959"),
            ("[::1]:6000", "[::1]:6000"),
        ] {
            assert_eq!(
                server_addr(given).unwrap(),
                want.parse().unwrap(),
                "{given}"
            );
        }
    }

    #[test]
    fn monitor_subscribers_are_capped() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || super::super::ds_server::serve(l));
        let a = Advertiser::receiver(addr, "RCV (WATCHED)", "video", &[], false);
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !a.connected.load(std::sync::atomic::Ordering::Relaxed)
            && std::time::Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        a.set("connection-state", "connected");
        // Each listener connection gets its own listener id from the server.
        let mut listeners: Vec<Session> = (0..Advertiser::MAX_SUBSCRIBERS + 6)
            .map(|_| {
                let mut l = Session::connect(
                    addr,
                    Subject::Receiver,
                    Role::Listener,
                    Duration::from_secs(3),
                )
                .unwrap();
                l.send(
                    &Element::new("monitor_request")
                        .with_attr("unsubscribe", "false")
                        .with_child(Element::new("receiver_id").with_text(&a.receiver_id))
                        .with_child(Element::new("listener_id"))
                        .with_child(Element::new("monitor_capabilities").with_child(
                            Element::new("property").with_attr("name", "connection-state"),
                        )),
                )
                .unwrap();
                l
            })
            .collect();
        std::thread::sleep(Duration::from_millis(800));
        let mut served = 0;
        for l in &mut listeners {
            l.set_timeout(Some(Duration::from_millis(100))).unwrap();
            if std::iter::from_fn(|| l.recv().ok().flatten())
                .flatten()
                .any(|m| m.name == "monitor_events")
            {
                served += 1;
            }
        }
        assert_eq!(served, Advertiser::MAX_SUBSCRIBERS);
    }
}
