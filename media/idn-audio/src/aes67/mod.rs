//! AES67 interoperation (spec/extensions/idn-aes67.md): IDN sound as AES67 streams and
//! AES67 streams as IDN sound. RTP (RFC 3550) carrying L24/L16 PCM (RFC 3190) at 44.1, 48
//! or 96 kHz in 48-frame packets (1 ms at 48 kHz), announced with SAP (RFC 2974) and SDP (RFC 4566, clock lines RFC 7273),
//! timed by the site clock (PTP, idn-clock.md) where this machine follows one.

use idn_common::sync::LockOk as _;
use idn_open_protocol::api::AudioBlock;
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::ptp::{Ns, UTC_OFFSET_S};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// SAP's group and port (RFC 2974, the global-scope address AES67 equipment uses).
pub const SAP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);
pub const SAP_PORT: u16 = 9875;
/// The RTP port AES67 streams use by default.
pub const RTP_PORT: u16 = 5004;
/// How often a stream sent here is announced.
pub const SAP_INTERVAL: Duration = Duration::from_secs(30);
/// An announced stream not heard again for this long is gone.
const SAP_TIMEOUT: Duration = Duration::from_secs(300);
/// AF41 (34), AES67's recommendation for media, as the IP TOS byte.
const DSCP_AF41: u32 = 34 << 2;

/// The samples' format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// 24-bit big-endian PCM.
    L24,
    /// 16-bit big-endian PCM.
    L16,
}

impl Encoding {
    /// Its samples' layout (big-endian on the wire, RFC 3190).
    fn pcm(self) -> idn_common::pcm::Pcm {
        match self {
            Encoding::L24 => idn_common::pcm::Pcm::I24,
            Encoding::L16 => idn_common::pcm::Pcm::I16,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Encoding::L24 => 3,
            Encoding::L16 => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoding::L24 => "L24",
            Encoding::L16 => "L16",
        }
    }
}

/// One stream's session description: what a receiver needs to join and decode it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sdp {
    /// `s=`: the stream's name.
    pub name: String,
    /// `o=`: session id and version, and the address of the machine that made it.
    pub session_id: u64,
    pub version: u64,
    pub origin: Ipv4Addr,
    /// `c=`: the multicast group (or a unicast address) and its TTL.
    pub group: Ipv4Addr,
    pub ttl: u8,
    /// `m=`: the port and payload type.
    pub port: u16,
    pub payload: u8,
    /// `a=rtpmap`: encoding, rate, channels.
    pub encoding: Encoding,
    pub rate: u32,
    pub channels: u16,
    /// `a=ptime`, in microseconds (1000: 1 ms).
    pub ptime_us: u32,
    /// `a=ts-refclk:ptp=IEEE1588-2008:GMID:DOMAIN`: the grandmaster (`aa-bb-…`, lower
    /// case) and domain the media clock follows; `None`: not PTP (or not said).
    pub ptp: Option<(String, u8)>,
    /// `a=mediaclk:direct=OFFSET`: the RTP timestamp is the media clock plus this.
    pub offset: u32,
    /// `a=source-filter: incl …`: the sender's address, where given.
    pub source: Option<Ipv4Addr>,
}

/// The most samples a packet can carry: a UDP datagram's payload less RTP's header.
const MAX_PAYLOAD: usize = 65_507 - 12;

impl Sdp {
    /// Reads a session description; the first audio `m=` line is the stream.
    pub fn parse(text: &str) -> Result<Sdp, String> {
        let mut sdp = Sdp {
            name: String::new(),
            session_id: 0,
            version: 0,
            origin: Ipv4Addr::UNSPECIFIED,
            group: Ipv4Addr::UNSPECIFIED,
            ttl: 32,
            port: 0,
            payload: 0,
            encoding: Encoding::L24,
            rate: 48_000,
            channels: 2,
            ptime_us: 1000,
            ptp: None,
            offset: 0,
            source: None,
        };
        let (mut media, mut rtpmap) = (false, false);
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "s" => sdp.name = v.trim().to_owned(),
                "o" => {
                    let f: Vec<&str> = v.split_whitespace().collect();
                    if f.len() >= 6 {
                        sdp.session_id = f[1].parse().unwrap_or(0);
                        sdp.version = f[2].parse().unwrap_or(0);
                        sdp.origin = f[5].parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
                    }
                }
                "c" => {
                    // c=IN IP4 239.69.1.2/32 (a media-level c= wins over the session's).
                    if let Some(addr) = v.split_whitespace().nth(2) {
                        let (a, ttl) = addr.split_once('/').unwrap_or((addr, ""));
                        if let Ok(a) = a.parse() {
                            sdp.group = a;
                        }
                        if let Ok(t) = ttl.split('/').next().unwrap_or("").parse() {
                            sdp.ttl = t;
                        }
                    }
                }
                "m" if !media => {
                    let f: Vec<&str> = v.split_whitespace().collect();
                    if f.first() == Some(&"audio") && f.len() >= 4 {
                        sdp.port = f[1]
                            .split('/')
                            .next()
                            .unwrap_or("")
                            .parse()
                            .map_err(|_| "a bad m= port".to_owned())?;
                        sdp.payload = f[3]
                            .parse()
                            .map_err(|_| "a bad m= payload type".to_owned())?;
                        media = true;
                    }
                }
                "m" => break, // a second stream: only the first is taken
                "a" => {
                    let (name, value) = v.split_once(':').unwrap_or((v, ""));
                    match name {
                        "rtpmap" => {
                            // rtpmap:98 L24/48000/2
                            let mut f = value.split_whitespace();
                            let pt: Option<u8> = f.next().and_then(|p| p.parse().ok());
                            if media && pt == Some(sdp.payload) {
                                let mut e = f.next().unwrap_or("").split('/');
                                sdp.encoding =
                                    match e.next().unwrap_or("").to_ascii_uppercase().as_str() {
                                        "L24" => Encoding::L24,
                                        "L16" => Encoding::L16,
                                        other => return Err(format!("{other}: not L24 or L16")),
                                    };
                                sdp.rate = e
                                    .next()
                                    .and_then(|r| r.parse().ok())
                                    .ok_or("a bad rtpmap rate")?;
                                sdp.channels = e.next().and_then(|c| c.parse().ok()).unwrap_or(1);
                                rtpmap = true;
                            }
                        }
                        "ptime" => {
                            if let Ok(ms) = value.trim().parse::<f64>() {
                                sdp.ptime_us = (ms * 1000.0).round() as u32;
                            }
                        }
                        "ts-refclk" => {
                            // ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0
                            if let Some(p) = value.strip_prefix("ptp=") {
                                let f: Vec<&str> = p.split(':').collect();
                                if f.len() >= 2 && f[1] != "traceable" {
                                    let domain = f.get(2).and_then(|d| d.parse().ok()).unwrap_or(0);
                                    sdp.ptp = Some((f[1].to_ascii_lowercase(), domain));
                                }
                            }
                        }
                        "mediaclk" => {
                            if let Some(o) = value.trim().strip_prefix("direct=") {
                                sdp.offset = o
                                    .split_whitespace()
                                    .next()
                                    .unwrap_or("0")
                                    .parse()
                                    .unwrap_or(0);
                            }
                        }
                        "source-filter" => {
                            // source-filter: incl IN IP4 239.69.1.2 192.168.1.10
                            let f: Vec<&str> = value.split_whitespace().collect();
                            if f.first() == Some(&"incl") && f.len() >= 5 {
                                sdp.source = f[4].parse().ok();
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if !media {
            return Err("no audio stream (m=audio) in it".into());
        }
        if !rtpmap {
            return Err(format!("no rtpmap for payload type {}", sdp.payload));
        }
        if sdp.group.is_unspecified() {
            return Err("no address (c=) in it".into());
        }
        if !(1..=64).contains(&sdp.channels) || sdp.rate == 0 || sdp.ptime_us == 0 {
            return Err("unusable channels, rate or packet time".into());
        }
        // Heard from anyone on the network: a rate or packet time no stream has made the
        // receiver ask for gigabytes a block (and abort) at every restart.
        if !(8_000..=384_000).contains(&sdp.rate) || sdp.payload_bytes() > MAX_PAYLOAD {
            return Err(format!(
                "{} Hz, {} µs packets: not a stream this takes",
                sdp.rate, sdp.ptime_us
            ));
        }
        Ok(sdp)
    }

    /// The description as sent (CRLF lines, AES67's attributes).
    pub fn to_text(&self) -> String {
        let ms = if self.ptime_us.is_multiple_of(1000) {
            format!("{}", self.ptime_us / 1000)
        } else {
            format!("{}", self.ptime_us as f64 / 1000.0)
        };
        let mut s = format!(
            "v=0\r\no=- {} {} IN IP4 {}\r\ns={}\r\nc=IN IP4 {}/{}\r\nt=0 0\r\nm=audio {} RTP/AVP {}\r\na=rtpmap:{} {}/{}/{}\r\na=ptime:{}\r\n",
            self.session_id,
            self.version,
            self.origin,
            self.name,
            self.group,
            self.ttl,
            self.port,
            self.payload,
            self.payload,
            self.encoding.name(),
            self.rate,
            self.channels,
            ms
        );
        match &self.ptp {
            Some((gm, domain)) => {
                s += &format!(
                    "a=ts-refclk:ptp=IEEE1588-2008:{}:{domain}\r\n",
                    gm.to_ascii_uppercase()
                )
            }
            // This machine's own clock (RFC 7273 §5.2): no PTP to name.
            None => s += "a=ts-refclk:local\r\n",
        }
        s += &format!("a=mediaclk:direct={}\r\na=recvonly\r\n", self.offset);
        s
    }

    /// Bytes of samples a packet carries.
    pub fn payload_bytes(&self) -> usize {
        self.frames_per_packet() * self.channels as usize * self.encoding.bytes()
    }

    /// Sample frames a packet carries.
    pub fn frames_per_packet(&self) -> usize {
        // Rounded to the nearest frame: 1.088 ms at 44.1 kHz is 47.98, meant as 48.
        ((self.rate as u64 * self.ptime_us as u64 + 500_000) / 1_000_000) as usize
    }
}

// ---- SAP ----

/// A SAP packet (RFC 2974): an announcement, or with `delete` its withdrawal.
pub fn sap_packet(origin: Ipv4Addr, hash: u16, sdp: &str, delete: bool) -> Vec<u8> {
    let mut p = vec![0x20 | if delete { 0x04 } else { 0 }, 0];
    p.extend_from_slice(&hash.to_be_bytes());
    p.extend_from_slice(&origin.octets());
    p.extend_from_slice(b"application/sdp\0");
    p.extend_from_slice(sdp.as_bytes());
    p
}

/// A heard SAP packet: (withdrawal, message hash, origin, the SDP), or `None` when it isn't
/// one this reads (IPv6 origins are read; encrypted or compressed ones aren't).
pub fn parse_sap(b: &[u8]) -> Option<(bool, u16, std::net::IpAddr, String)> {
    let h = *b.first()?;
    if h >> 5 != 1 || h & 0x03 != 0 {
        return None; // not version 1, or encrypted / compressed
    }
    let delete = h & 0x04 != 0;
    let auth = *b.get(1)? as usize * 4;
    let hash = u16::from_be_bytes([*b.get(2)?, *b.get(3)?]);
    let (origin, mut i): (std::net::IpAddr, usize) = if h & 0x10 != 0 {
        let o: [u8; 16] = b.get(4..20)?.try_into().ok()?;
        (o.into(), 20)
    } else {
        let o: [u8; 4] = b.get(4..8)?.try_into().ok()?;
        (o.into(), 8)
    };
    i += auth;
    let mut rest = b.get(i..)?;
    // An optional payload type ("application/sdp\0") before the SDP itself.
    if !rest.starts_with(b"v=0") {
        let nul = rest.iter().position(|&c| c == 0)?;
        if &rest[..nul] != b"application/sdp" {
            return None;
        }
        rest = &rest[nul + 1..];
    }
    Some((
        delete,
        hash,
        origin,
        String::from_utf8_lossy(rest).into_owned(),
    ))
}

/// This machine's address towards the multicast network: the interface the default route
/// (or `interface`) leaves by.
pub fn local_address(interface: Option<Ipv4Addr>) -> Ipv4Addr {
    if let Some(i) = interface {
        return i;
    }
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| {
            s.connect((Ipv4Addr::new(239, 69, 0, 1), RTP_PORT))?;
            s.local_addr()
        })
        .ok()
        .and_then(|a| match a.ip() {
            std::net::IpAddr::V4(v) if !v.is_unspecified() => Some(v),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

/// A UDP socket that sends multicast out of `interface` with `ttl` (0: this machine only),
/// marked AF41.
fn sender_socket(interface: Ipv4Addr, ttl: u32) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    // The interface's address; on loopback (no network) the system's choice.
    if !interface.is_loopback() {
        s.set_multicast_if_v4(&interface)?;
    }
    s.set_multicast_ttl_v4(ttl)?;
    s.set_multicast_loop_v4(true)?;
    let _ = s.set_tos_v4(DSCP_AF41);
    Ok(s.into())
}

/// A UDP socket receiving `group`:`port` (shared with other programs on this machine),
/// joined on `interface`.
fn receiver_socket(group: Ipv4Addr, port: u16, interface: Ipv4Addr) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    // Bound to the group (Unix), so other groups on the same port don't arrive here.
    #[cfg(unix)]
    let bind = if group.is_multicast() {
        group
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    #[cfg(not(unix))]
    let bind = Ipv4Addr::UNSPECIFIED;
    s.bind(&SocketAddr::from((bind, port)).into())?;
    if group.is_multicast() {
        let iface = if interface.is_loopback() {
            Ipv4Addr::UNSPECIFIED
        } else {
            interface
        };
        s.join_multicast_v4(&group, &iface)?;
    }
    s.set_read_timeout(Some(Duration::from_millis(100)))?;
    let _ = s.set_recv_buffer_size(1 << 20);
    Ok(s.into())
}

/// Announces a stream every 30 s until dropped (or the program is stopped with Ctrl+C
/// or SIGTERM), then withdraws it.
pub struct SapAnnouncer {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    _on_stop: idn_open_protocol::shutdown::Registration,
    /// What is announced: (message id hash, the description).
    current: Arc<Mutex<(u16, String)>>,
    sock: std::net::UdpSocket,
    origin: Ipv4Addr,
}

/// The message id hash of a description: changed with its version, as a changed
/// description needs (RFC 2974 §5).
fn sap_hash(sdp: &Sdp) -> u16 {
    (sdp.session_id as u16)
        ^ (sdp.session_id >> 16) as u16
        ^ (sdp.version as u16).wrapping_mul(0x9e37)
}

impl SapAnnouncer {
    pub fn start(sdp: &Sdp, interface: Ipv4Addr, ttl: u32) -> io::Result<SapAnnouncer> {
        let sock = sender_socket(interface, ttl)?;
        let current = Arc::new(Mutex::new((sap_hash(sdp), sdp.to_text())));
        let (origin, stop) = (sdp.origin, Arc::new(AtomicBool::new(false)));
        let s = stop.clone();
        let (gone, now) = (sock.try_clone()?, current.clone());
        let on_stop = idn_open_protocol::shutdown::register(move || {
            let (hash, text) = now.lock_ok().clone();
            let _ = gone.send_to(
                &sap_packet(origin, hash, &text, true),
                SocketAddrV4::new(SAP_GROUP, SAP_PORT),
            );
        });
        let (th_sock, th_now) = (sock.try_clone()?, current.clone());
        let thread = std::thread::Builder::new()
            .name("idn-aes67-sap".into())
            .spawn(move || {
                let to = SocketAddrV4::new(SAP_GROUP, SAP_PORT);
                let mut next = Instant::now();
                while !s.load(Ordering::Relaxed) {
                    if Instant::now() >= next {
                        let (hash, text) = th_now.lock_ok().clone();
                        let _ = th_sock.send_to(&sap_packet(origin, hash, &text, false), to);
                        next += SAP_INTERVAL;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let (hash, text) = th_now.lock_ok().clone();
                let _ = th_sock.send_to(&sap_packet(origin, hash, &text, true), to);
            })?;
        Ok(SapAnnouncer {
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
            _on_stop: on_stop,
            current,
            sock,
            origin,
        })
    }

    /// Announces `sdp` instead, at once: the old description withdrawn, the new one sent
    /// (with its own message id), then every interval as before.
    pub fn update(&self, sdp: &Sdp) {
        let to = SocketAddrV4::new(SAP_GROUP, SAP_PORT);
        let mut cur = self.current.lock_ok();
        let _ = self
            .sock
            .send_to(&sap_packet(self.origin, cur.0, &cur.1, true), to);
        *cur = (sap_hash(sdp), sdp.to_text());
        let _ = self
            .sock
            .send_to(&sap_packet(self.origin, cur.0, &cur.1, false), to);
    }
}

/// A stream heard announced.
#[derive(Debug, Clone)]
pub struct Announced {
    pub sdp: Sdp,
    /// The announcement as heard.
    pub text: String,
    /// Who announced it: the address the announcements come from.
    pub from: std::net::IpAddr,
    /// When it was last heard.
    pub heard: Instant,
}

/// The most streams a listener keeps.
const MAX_ANNOUNCED: usize = 1024;
/// The most streams it keeps from one address (equipment announces a few each): one
/// machine's flood can't fill the list and keep everyone else's streams out.
const MAX_PER_SENDER: usize = 64;

type Streams = HashMap<(std::net::IpAddr, u16), Announced>;

/// One SAP packet heard from `src` into the table. Origins and hashes are the packets'
/// own word, so the table is bounded (in all and per sender, the stale entries dropped
/// first), and a stream is withdrawn or replaced only from the address that announced it
/// while it is current.
fn take_announcement(
    m: &mut Streams,
    src: std::net::IpAddr,
    (delete, hash, origin, text): (bool, u16, std::net::IpAddr, String),
) {
    let key = (origin, hash);
    let other = m
        .get(&key)
        .is_some_and(|a| a.from != src && a.heard.elapsed() < SAP_TIMEOUT);
    if other {
        return;
    }
    if delete {
        m.remove(&key);
        return;
    }
    let Ok(sdp) = Sdp::parse(&text) else { return };
    if !m.contains_key(&key) {
        let room = |m: &Streams| {
            m.len() < MAX_ANNOUNCED && m.values().filter(|a| a.from == src).count() < MAX_PER_SENDER
        };
        if !room(m) {
            m.retain(|_, a| a.heard.elapsed() < SAP_TIMEOUT);
            if !room(m) {
                return;
            }
        }
    }
    m.insert(
        key,
        Announced {
            sdp,
            text,
            from: src,
            heard: Instant::now(),
        },
    );
}

/// Listens for SAP announcements: the streams on the network, until dropped.
pub struct SapListener {
    streams: Arc<Mutex<HashMap<(std::net::IpAddr, u16), Announced>>>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

impl SapListener {
    pub fn start(interface: Ipv4Addr) -> io::Result<SapListener> {
        let sock = receiver_socket(SAP_GROUP, SAP_PORT, interface)?;
        let streams: Arc<Mutex<HashMap<(std::net::IpAddr, u16), Announced>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, s) = (streams.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("idn-aes67-sap-listen".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65536];
                while !s.load(Ordering::Relaxed) {
                    let Ok((n, from)) = sock.recv_from(&mut buf) else {
                        continue;
                    };
                    if let Some(a) = parse_sap(&buf[..n]) {
                        take_announcement(&mut st.lock_ok(), from.ip(), a);
                    }
                }
            })?;
        Ok(SapListener {
            streams,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }

    /// The streams heard (not withdrawn, heard within 5 minutes), by name.
    pub fn streams(&self) -> Vec<Announced> {
        let mut m = self.streams.lock_ok();
        m.retain(|_, a| a.heard.elapsed() < SAP_TIMEOUT);
        let mut v: Vec<Announced> = m.values().cloned().collect();
        v.sort_by(|a, b| a.sdp.name.cmp(&b.sdp.name));
        v
    }

    /// The stream `name` as announced last (a changed description, a new grandmaster,
    /// comes with a new version while the old one may still be listed).
    pub fn newest(&self, name: &str) -> Option<Announced> {
        self.streams()
            .into_iter()
            .filter(|a| a.sdp.name == name)
            .max_by_key(|a| (a.sdp.version, a.heard))
    }
}

// ---- RTP ----

/// An RTP packet's fields this uses.
#[derive(Debug, PartialEq)]
pub struct Rtp<'a> {
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: &'a [u8],
}

pub fn rtp_packet(pt: u8, seq: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.extend_from_slice(&[0x80, pt & 0x7f]);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// An RTP sequence number extended past its 16 bits: the value nearest the highest so far
/// (`last`). The first one is numbered from 2^32, so that a packet from just before it (or
/// before a wrap) can't go below zero.
fn extend_seq(last: Option<u64>, seq: u16) -> u64 {
    let Some(last) = last else {
        return (1 << 32) | seq as u64;
    };
    let base = last & !0xffff;
    [base - 0x10000, base, base + 0x10000]
        .map(|b| b | seq as u64)
        .into_iter()
        .min_by_key(|c| c.abs_diff(last))
        .unwrap()
}

pub fn parse_rtp(b: &[u8]) -> Option<Rtp<'_>> {
    if b.len() < 12 || b[0] >> 6 != 2 {
        return None;
    }
    let cc = (b[0] & 0x0f) as usize;
    let mut i = 12 + cc * 4;
    if b[0] & 0x10 != 0 {
        // A header extension: 4 bytes, then its length in words.
        let len = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]) as usize;
        i += 4 + len * 4;
    }
    let mut end = b.len();
    if b[0] & 0x20 != 0 {
        end = end.checked_sub(*b.last()? as usize)?; // padding
    }
    Some(Rtp {
        payload_type: b[1] & 0x7f,
        seq: u16::from_be_bytes([b[2], b[3]]),
        timestamp: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        ssrc: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        payload: b.get(i..end)?,
    })
}

/// Interleaved samples (−1…1) as big-endian PCM (scaled by 2^23 or 2^15, as decoded, so
/// a decoded stream sent again is the same to the bit).
pub fn encode(samples: &[f32], enc: Encoding, out: &mut Vec<u8>) {
    out.clear();
    idn_common::pcm::write(enc.pcm(), true, samples, out);
}

/// Big-endian PCM as interleaved samples.
pub fn decode(b: &[u8], enc: Encoding, out: &mut Vec<f32>) {
    idn_common::pcm::read(enc.pcm(), true, b, out);
}

// ---- The media clock ----

/// Site time now (TAI ns since the PTP epoch): the site clock's when usable (and whether
/// it was), else this machine's clock (UTC + TAI's offset).
fn tai_now() -> (Ns, bool) {
    match idn_open_protocol::node::clock::site_now() {
        Some(t) => (t, true),
        None => {
            let utc = idn_common::epoch::now().as_nanos() as Ns;
            (utc + UTC_OFFSET_S as Ns * 1_000_000_000, false)
        }
    }
}

/// The monotonic time ([`idn_open_protocol::ptp::mono_ns`]) at site time `t`.
fn mono_at(t: Ns) -> i64 {
    let now_mono = idn_open_protocol::ptp::mono_ns();
    match idn_open_protocol::node::clock::status()
        .filter(|c| c.usable())
        .and_then(|c| c.mapping)
    {
        Some(m) => m.mono(t),
        None => now_mono + (t - tai_now().0) as i64,
    }
}

/// A site time as an IDN timestamp (100 ns since 1970, UTC).
fn idn_timestamp(t: Ns) -> i64 {
    ((t - UTC_OFFSET_S as Ns * 1_000_000_000) / 100) as i64
}

mod receive;
mod send;
pub use receive::*;
pub use send::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_extend_across_wraps_from_any_start() {
        for first in [0u16, 1, 32767, 32768, 40000, 65535] {
            let mut last = None;
            let mut prev = None;
            // Forward through a wrap, then one packet from just before the first.
            for k in 0..70000u32 {
                let seq = first.wrapping_add(k as u16);
                let ext = extend_seq(last, seq);
                if let Some(p) = prev {
                    assert_eq!(ext, p + 1, "start {first}, packet {k}");
                }
                prev = Some(ext);
                last = Some(last.map_or(ext, |l: u64| l.max(ext)));
            }
            let start = extend_seq(None, first);
            assert_eq!(
                extend_seq(Some(start), first.wrapping_sub(1)),
                start - 1,
                "start {first}"
            );
        }
    }

    /// A flood from one machine fills its share of the list, not the list: a real stream
    /// announced after it is listed. Another address can't withdraw or
    /// replace a current stream by naming its origin and hash.
    #[test]
    fn one_senders_flood_cant_fill_the_list_or_touch_others_streams() {
        let mut m: Streams = HashMap::new();
        let text = sdp().to_text();
        let flood: std::net::IpAddr = "192.168.1.66".parse().unwrap();
        for h in 0..2000u16 {
            let origin = std::net::IpAddr::from([10, 0, (h >> 8) as u8, h as u8]);
            take_announcement(&mut m, flood, (false, h, origin, text.clone()));
        }
        assert_eq!(m.len(), MAX_PER_SENDER);
        let real: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        take_announcement(&mut m, real, (false, 7, real, text.clone()));
        assert!(m.contains_key(&(real, 7)), "the real stream is listed");
        // The flooder names the real stream: neither a withdrawal nor a replacement takes.
        take_announcement(&mut m, flood, (true, 7, real, text.clone()));
        assert!(m.contains_key(&(real, 7)));
        take_announcement(&mut m, flood, (false, 7, real, text.clone()));
        assert_eq!(m[&(real, 7)].from, real);
        // Its own withdrawal does.
        take_announcement(&mut m, real, (true, 7, real, text));
        assert!(!m.contains_key(&(real, 7)));
    }

    fn sdp() -> Sdp {
        Sdp {
            name: "Stage box 1-8".into(),
            session_id: 1234567,
            version: 3,
            origin: Ipv4Addr::new(192, 168, 1, 20),
            group: Ipv4Addr::new(239, 69, 12, 34),
            ttl: 32,
            port: 5004,
            payload: 98,
            encoding: Encoding::L24,
            rate: 48_000,
            channels: 8,
            ptime_us: 1000,
            ptp: Some(("00-1d-c1-ff-fe-12-34-56".into(), 0)),
            offset: 0,
            source: None,
        }
    }

    #[test]
    fn sdp_round_trips() {
        let s = sdp();
        let text = s.to_text();
        assert!(text.contains("a=rtpmap:98 L24/48000/8\r\n"), "{text}");
        assert!(
            text.contains("a=ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0\r\n"),
            "{text}"
        );
        assert_eq!(Sdp::parse(&text).unwrap(), s);
        assert_eq!(s.payload_bytes(), 48 * 8 * 3);
    }

    #[test]
    fn another_makers_sdp_is_read() {
        // As AES67 equipment announces (a media-level c=, a source filter, 125 µs).
        let text = "v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.30\r\ns=Console Out 1-2\r\n\
                    c=IN IP4 239.69.1.30/32\r\nt=0 0\r\na=keywds:Dante\r\nm=audio 5004 RTP/AVP 97\r\n\
                    i=2 channels: Left, Right\r\nc=IN IP4 239.69.1.30/32\r\na=recvonly\r\n\
                    a=rtpmap:97 L24/48000/2\r\na=ptime:0.125\r\na=ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-AA-BB-CC:0\r\n\
                    a=mediaclk:direct=4038122592\r\na=source-filter: incl IN IP4 239.69.1.30 192.168.1.30\r\n";
        let s = Sdp::parse(text).unwrap();
        assert_eq!(
            (s.name.as_str(), s.payload, s.channels, s.ptime_us, s.offset),
            ("Console Out 1-2", 97, 2, 125, 4038122592)
        );
        assert_eq!(s.frames_per_packet(), 6);
        assert_eq!(s.source, Some(Ipv4Addr::new(192, 168, 1, 30)));
        assert_eq!(s.ptp, Some(("00-1d-c1-ff-fe-aa-bb-cc".into(), 0)));
        assert!(Sdp::parse("v=0\r\ns=x\r\nm=video 5004 RTP/AVP 96\r\n").is_err());
        // A rate or packet time no stream has (it made the receiver ask for gigabytes).
        let huge = text.replace("L24/48000/2", "L24/4000000000/2");
        assert!(Sdp::parse(&huge).is_err());
        let long = text.replace("ptime:0.125", "ptime:1000000");
        assert!(Sdp::parse(&long).is_err());
    }

    #[test]
    fn sap_round_trips() {
        let s = sdp().to_text();
        let p = sap_packet(Ipv4Addr::new(192, 168, 1, 20), 0xbeef, &s, false);
        assert_eq!(
            parse_sap(&p),
            Some((
                false,
                0xbeef,
                Ipv4Addr::new(192, 168, 1, 20).into(),
                s.clone()
            ))
        );
        let p = sap_packet(Ipv4Addr::new(192, 168, 1, 20), 0xbeef, &s, true);
        assert!(parse_sap(&p).unwrap().0);
        // Without the payload type, as some senders do.
        let mut q = p[..8].to_vec();
        q[0] = 0x20;
        q.extend_from_slice(s.as_bytes());
        assert_eq!(parse_sap(&q).unwrap().3, s);
    }

    #[test]
    fn samples_survive_l24_and_l16() {
        let x: Vec<f32> = (0..96).map(|i| ((i as f32) * 0.37).sin() * 0.9).collect();
        for (enc, step) in [
            (Encoding::L24, 0.5 / 8_388_608.0),
            (Encoding::L16, 0.5 / 32_768.0),
        ] {
            let (mut b, mut y) = (Vec::new(), Vec::new());
            encode(&x, enc, &mut b);
            assert_eq!(b.len(), 96 * enc.bytes());
            decode(&b, enc, &mut y);
            assert!(
                x.iter()
                    .zip(&y)
                    .all(|(a, b)| (a - b).abs() <= step as f32 * 1.01),
                "{enc:?}"
            );
            // Decoded and sent again: the same bytes.
            let mut b2 = Vec::new();
            encode(&y, enc, &mut b2);
            assert_eq!(b, b2, "{enc:?}");
        }
        let p = rtp_packet(98, 65535, 0xdead_beef, 7, &[1, 2, 3]);
        assert_eq!(
            parse_rtp(&p),
            Some(Rtp {
                payload_type: 98,
                seq: 65535,
                timestamp: 0xdead_beef,
                ssrc: 7,
                payload: &[1, 2, 3]
            })
        );
    }

    #[test]
    fn groups_stay_in_239_69() {
        for n in ["a", "Stage", "IDN program", ""] {
            let g = group_for(n);
            assert!(
                g.octets()[0] == 239
                    && g.octets()[1] == 69
                    && g.octets()[2] > 0
                    && (1..255).contains(&g.octets()[3]),
                "{g}"
            );
        }
    }
}
