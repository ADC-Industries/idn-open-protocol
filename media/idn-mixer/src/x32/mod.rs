//! Controlling a Behringer X32's preamps over Ethernet (plan §D.8, beside R.27's audio
//! over the X-USB card, spec §M.23): read and set each preamp's gain and 48 V phantom
//! power and each input channel's polarity, know which X32 input each of the card's 32
//! USB channels carries, and hear changes made on the X32 itself.
//!
//! The protocol is the X32's OSC remote protocol on UDP port 10023, from public
//! material only (see [`proto`] for the source and the page of each message). Nothing
//! is taken from Behringer's applications.
//!
//! What goes to the X32, and nothing else:
//! - the documented queries (a message with no arguments): `/info` (finding the desk,
//!   and every few seconds to know it is still there), `/xinfo`, `/-stat/aes50/A|B`,
//!   the card routing blocks, the user routes and output sources they point at,
//!   `/-ha/NN/index`, and the gain, phantom and polarity of the preamps the USB
//!   channels carry;
//! - `/xremote`, renewed every 9 s (it lapses after 10 s), so the X32 sends its changes;
//! - the sets the API is asked for. Gain sets are coalesced: at most one per address
//!   every 40 ms, the latest value winning (a dragged control doesn't flood the desk).
//!   Phantom power is only ever set by [`X32::set_phantom`] /
//!   [`X32::set_headamp_phantom`] with an explicit value; nothing turns it on by
//!   default.
//!
//! When the X32 stops answering for 10 s the module says so ([`Change::Disconnected`]),
//! keeps asking `/info` every 2 s, and on the first answer subscribes again and reads
//! everything again.

#[cfg(test)]
pub(crate) mod fake;
pub mod proto;
#[cfg(test)]
mod tests;

pub use proto::{Hop, PORT, Source, UsbChannel, float_to_gain, gain_step, gain_to_float};

use idn_common::sync::LockOk as _;
use idn_open_protocol::osc::{Arg, Message, decode, encode};
use proto::Tables;
use std::collections::{BTreeMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The module's clocks. The defaults follow the protocol; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// `/xremote` again after this (the X32 forgets it after 10 s).
    pub renew: Duration,
    /// `/info` while connected, to know the X32 is still there.
    pub ping: Duration,
    /// Disconnected when nothing was heard for this long.
    pub dead_after: Duration,
    /// `/info` while disconnected.
    pub retry: Duration,
    /// At most one set per address this often (latest value wins).
    pub set_every: Duration,
    /// Everything is read again this often (UDP loses packets; a scene recall on the
    /// X32 may not report every value).
    pub resync: Duration,
    /// The worker's longest sleep.
    pub tick: Duration,
    /// Queries sent per tick at most (the reads at connect are paced).
    pub burst: usize,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            renew: Duration::from_secs(9),
            ping: Duration::from_secs(3),
            dead_after: Duration::from_secs(10),
            retry: Duration::from_secs(2),
            set_every: Duration::from_millis(40),
            resync: Duration::from_secs(60),
            tick: Duration::from_millis(20),
            burst: 32,
        }
    }
}

/// The X32's identity (`/info`, `/xinfo`, `/-stat/aes50`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Info {
    /// The console model: "X32", "X32RACK", "X32C", "X32P", "X32CORE", "M32", "M32C",
    /// "M32R".
    pub model: String,
    /// The console's firmware version, e.g. "4.06".
    pub firmware: String,
    /// The OSC server's version, e.g. "V2.05".
    pub server_version: String,
    /// The OSC server's name ("osc-server").
    pub server_name: String,
    /// The desk's network name (`/xinfo`), e.g. "X32-02-4A-53".
    pub name: Option<String>,
    /// Local mic preamps the model has (headamps 0…N-1).
    pub local_preamps: u8,
    /// The boxes on AES50 ports A and B, as the X32 detected them (e.g. "S16"), by
    /// `/-stat/aes50/A|B`.
    pub aes50: [Vec<String>; 2],
}

/// A preamp (X32 headamp) and the polarity of the X32 channel that takes it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Preamp {
    /// The headamp, 0…127 (local 0…31, AES50 A 32…79, AES50 B 80…127).
    pub headamp: u8,
    /// Gain, dB, -12…+60 in 0.5 dB steps.
    pub gain_db: f32,
    /// 48 V phantom power.
    pub phantom: bool,
    /// The X32 channel whose polarity [`Preamp::invert`] is (see [`UsbChannel::channel`]).
    pub channel: Option<u8>,
    /// That channel's polarity invert; `None` with no channel, or not read yet.
    ///
    /// Note: the X32 applies it in its own channel strip. A card channel carrying the
    /// input itself (Local, AES50) is taken before the strip, so this does not change
    /// what arrives over USB; one carrying a direct out may (by its tap point).
    pub invert: Option<bool>,
}

/// A change on the X32 (made there, by another client, or by this one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// It answered (first time, or after it stopped answering).
    Connected,
    /// It stopped answering.
    Disconnected,
    /// Its identity was read.
    Info,
    /// What the USB channels carry changed ([`X32::routing`]).
    Routing,
    /// A headamp's gain or phantom power changed (0…127).
    Headamp(u8),
    /// The preamp behind USB channel 1…32 changed (gain, phantom or polarity).
    Preamp {
        /// The USB channel, 1…32.
        usb: u8,
    },
}

/// Why a set was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The X32 is not answering.
    NotConnected,
    /// Not a USB channel (1…32), or its routing was not read yet.
    NoUsbChannel(u8),
    /// The USB channel carries no preamp (a bus, the card, an output…).
    NoPreamp(u8),
    /// No X32 channel takes it, so it has no polarity to set.
    NoChannel(u8),
    /// Not a headamp this X32 has (0…127, local ones by its model).
    NoHeadamp(u8),
    /// Not a channel 1…32.
    BadChannel(u8),
    /// Not a number.
    BadValue,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotConnected => write!(f, "the X32 is not answering"),
            Error::NoUsbChannel(n) => write!(f, "USB channel {n}: none, or not read yet"),
            Error::NoPreamp(n) => write!(f, "USB channel {n} carries no preamp"),
            Error::NoChannel(n) => write!(f, "USB channel {n}: no X32 channel takes it"),
            Error::NoHeadamp(n) => write!(f, "headamp {n}: not on this X32"),
            Error::BadChannel(n) => write!(f, "channel {n}: not 1…32"),
            Error::BadValue => write!(f, "not a number"),
        }
    }
}

impl std::error::Error for Error {}

/// An X32 found on the network.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Found {
    /// Where it answered from.
    pub addr: SocketAddr,
    /// Its model.
    pub model: String,
    /// Its firmware.
    pub firmware: String,
}

/// The whole picture, for a state document.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    /// The X32's address.
    pub address: SocketAddr,
    /// Answering.
    pub connected: bool,
    /// Its identity, once read.
    pub info: Option<Info>,
    /// The USB channels read so far, with their preamps where read.
    pub usb: Vec<(UsbChannel, Option<Preamp>)>,
}

/// An X32 address from text: "192.168.1.62", "192.168.1.62:10023", a host name.
pub fn parse_address(s: &str) -> Result<SocketAddr, String> {
    let s = s.trim();
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a);
    }
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, PORT));
    }
    let with_port = if s.contains(':') {
        s.to_owned()
    } else {
        format!("{s}:{PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("X32 address {s:?}: {e}"))?
        .next()
        .ok_or_else(|| format!("X32 address {s:?}: not found"))
}

fn bind_for(to: &SocketAddr) -> std::io::Result<UdpSocket> {
    if to.is_ipv4() {
        UdpSocket::bind(("0.0.0.0", 0))
    } else {
        UdpSocket::bind(("::", 0))
    }
}

fn info_of(m: &Message) -> Option<(String, String, String, String)> {
    let s = |i: usize| match m.args.get(i) {
        Some(Arg::Str(s)) => Some(s.clone()),
        _ => None,
    };
    // p. 10: server version, server name, console model, console version.
    Some((s(0)?, s(1)?, s(2)?, s(3)?))
}

/// Finds X32s: `/info` broadcast on the LAN (port 10023), answers collected for
/// `wait`.
pub fn discover(wait: Duration) -> std::io::Result<Vec<Found>> {
    discover_on(&[SocketAddr::from(([255, 255, 255, 255], PORT))], wait)
}

/// Finds X32s at these addresses (broadcast or not): each is sent `/info`.
pub fn discover_on(targets: &[SocketAddr], wait: Duration) -> std::io::Result<Vec<Found>> {
    let Some(first) = targets.first() else {
        return Ok(Vec::new());
    };
    let socket = bind_for(first)?;
    socket.set_broadcast(true)?;
    let ask = encode(&Message::new(proto::INFO, vec![]));
    for t in targets {
        socket.send_to(&ask, t)?;
    }
    let end = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    let mut buf = vec![0u8; 2048];
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        socket.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        for m in decode(&buf[..n]).unwrap_or_default() {
            if m.addr != proto::INFO {
                continue;
            }
            if let Some((_, _, model, firmware)) = info_of(&m)
                && !found.iter().any(|f| f.addr == from)
            {
                found.push(Found {
                    addr: from,
                    model,
                    firmware,
                });
            }
        }
    }
    Ok(found)
}

struct State {
    connected: bool,
    heard: Instant,
    info: Option<Info>,
    tables: Tables,
    routing: Vec<Option<UsbChannel>>,
    gain: [Option<f32>; proto::HEADAMPS],
    phantom: [Option<bool>; proto::HEADAMPS],
    invert: [Option<bool>; proto::CHANNELS],
    /// Queries sent since the last (re)connect or resync.
    asked: HashSet<String>,
    /// Queries waiting to go (paced).
    queue: std::collections::VecDeque<String>,
}

impl State {
    fn new() -> State {
        State {
            connected: false,
            heard: Instant::now(),
            info: None,
            tables: Tables::default(),
            routing: vec![None; proto::USB_CHANNELS],
            gain: [None; proto::HEADAMPS],
            phantom: [None; proto::HEADAMPS],
            invert: [None; proto::CHANNELS],
            asked: HashSet::new(),
            queue: Default::default(),
        }
    }

    fn local_preamps(&self) -> u8 {
        self.info.as_ref().map_or(32, |i| i.local_preamps)
    }

    fn has_headamp(&self, ha: u8) -> bool {
        usize::from(ha) < proto::HEADAMPS && (ha >= 32 || ha < self.local_preamps())
    }

    fn ask(&mut self, addr: String) {
        if self.asked.insert(addr.clone()) {
            self.queue.push_back(addr);
        }
    }

    /// Everything is to be read again.
    fn read_all(&mut self) {
        self.asked.clear();
        self.queue.clear();
        for a in [proto::XINFO, proto::AES50_STAT[0], proto::AES50_STAT[1]] {
            self.ask(a.into());
        }
        for a in proto::CARD_BLOCKS {
            self.ask(a.into());
        }
        for ch in 1..=proto::CHANNELS as u8 {
            self.ask(proto::channel_headamp(ch));
        }
        self.ask_needed();
    }

    /// Asks what the routing read so far points at.
    fn ask_needed(&mut self) {
        for a in self.tables.needs() {
            self.ask(a);
        }
        let mut want = Vec::new();
        for u in self.routing.iter().flatten() {
            if let Some(ha) = u.headamp.filter(|&h| self.has_headamp(h)) {
                want.push(proto::headamp_gain(ha));
                want.push(proto::headamp_phantom(ha));
            }
            if let Some(ch) = u.channel {
                want.push(proto::channel_invert(ch));
            }
        }
        for a in want {
            self.ask(a);
        }
    }

    fn preamp(&self, u: &UsbChannel) -> Option<Preamp> {
        let ha = u.headamp.filter(|&h| self.has_headamp(h))?;
        Some(Preamp {
            headamp: ha,
            gain_db: self.gain[usize::from(ha)]?,
            phantom: self.phantom[usize::from(ha)]?,
            channel: u.channel,
            invert: u.channel.and_then(|c| self.invert[usize::from(c) - 1]),
        })
    }

    fn usb_with_headamp(&self, ha: u8) -> Vec<u8> {
        self.routing
            .iter()
            .flatten()
            .filter(|u| u.headamp == Some(ha))
            .map(|u| u.usb)
            .collect()
    }

    fn usb_with_channel(&self, ch: u8) -> Vec<u8> {
        self.routing
            .iter()
            .flatten()
            .filter(|u| u.channel == Some(ch))
            .map(|u| u.usb)
            .collect()
    }

    /// Follows the tables again; true when what the USB channels carry changed.
    fn reroute(&mut self) -> bool {
        let new: Vec<Option<UsbChannel>> = (1..=proto::USB_CHANNELS as u8)
            .map(|u| self.tables.usb(u))
            .collect();
        let changed = new != self.routing;
        self.routing = new;
        changed
    }

    /// One message from the X32 (an answer or a pushed change): what it changed.
    fn handle(&mut self, m: &Message, out: &mut Vec<Change>) {
        let int = || match m.args.first() {
            Some(Arg::Int(v)) => Some(*v),
            Some(Arg::Float(v)) => Some(v.round() as i32),
            Some(Arg::Bool(b)) => Some(i32::from(*b)),
            // An enum may come as its name (p. 12).
            Some(Arg::Str(s)) if s.eq_ignore_ascii_case("ON") => Some(1),
            Some(Arg::Str(s)) if s.eq_ignore_ascii_case("OFF") => Some(0),
            _ => None,
        };
        let a = m.addr.as_str();
        let parts: Vec<&str> = a.split('/').skip(1).collect();
        let num = |s: &str| s.parse::<u8>().ok();
        let mut routing_changed = false;
        match parts.as_slice() {
            ["info"] => {
                if let Some((server_version, server_name, model, firmware)) = info_of(m) {
                    let i = self.info.get_or_insert_with(Info::default);
                    let fresh = i.model != model || i.firmware != firmware;
                    i.local_preamps = proto::local_preamps(&model);
                    i.server_version = server_version;
                    i.server_name = server_name;
                    i.model = model;
                    i.firmware = firmware;
                    if fresh {
                        out.push(Change::Info);
                    }
                }
            }
            ["xinfo"] => {
                if let (Some(i), Some(Arg::Str(name))) = (self.info.as_mut(), m.args.get(1))
                    && i.name.as_deref() != Some(name)
                {
                    i.name = Some(name.clone());
                    out.push(Change::Info);
                }
            }
            ["-stat", "aes50", port @ ("A" | "B")] => {
                if let (Some(i), Some(Arg::Str(s))) = (self.info.as_mut(), m.args.first()) {
                    // p. 65: up to four letters, a box each.
                    let boxes: Vec<String> = s
                        .chars()
                        .take(4)
                        .filter_map(proto::aes50_box)
                        .map(String::from)
                        .collect();
                    let k = usize::from(*port == "B");
                    if i.aes50[k] != boxes {
                        i.aes50[k] = boxes;
                        out.push(Change::Info);
                    }
                }
            }
            ["headamp", n, what @ ("gain" | "phantom")] => {
                let Some(ha) = num(n).filter(|&h| usize::from(h) < proto::HEADAMPS) else {
                    return;
                };
                let i = usize::from(ha);
                let changed = if *what == "gain" {
                    let Some(Arg::Float(f)) = m.args.first() else {
                        return;
                    };
                    let db = Some(float_to_gain(*f));
                    std::mem::replace(&mut self.gain[i], db) != db
                } else {
                    let Some(v) = int() else { return };
                    let on = Some(v != 0);
                    std::mem::replace(&mut self.phantom[i], on) != on
                };
                if changed {
                    out.push(Change::Headamp(ha));
                    out.extend(
                        self.usb_with_headamp(ha)
                            .into_iter()
                            .map(|usb| Change::Preamp { usb }),
                    );
                }
            }
            ["ch", n, "preamp", "invert"] => {
                let (Some(ch @ 1..=32), Some(v)) = (num(n), int()) else {
                    return;
                };
                let on = Some(v != 0);
                if std::mem::replace(&mut self.invert[usize::from(ch) - 1], on) != on {
                    out.extend(
                        self.usb_with_channel(ch)
                            .into_iter()
                            .map(|usb| Change::Preamp { usb }),
                    );
                }
            }
            // A channel's source changed: which headamp it takes is read again.
            ["ch", n, "config", "source"] => {
                if let Some(ch @ 1..=32) = num(n) {
                    let a = proto::channel_headamp(ch);
                    self.asked.remove(&a);
                    self.ask(a);
                }
            }
            // The input routing changed: every channel's headamp is read again.
            ["config", "routing", "IN" | "PLAY" | "routswitch", ..] => {
                for ch in 1..=proto::CHANNELS as u8 {
                    let a = proto::channel_headamp(ch);
                    self.asked.remove(&a);
                    self.ask(a);
                }
            }
            ["config", "routing", "CARD", _] => {
                if let (Some(k), Some(v)) = (proto::CARD_BLOCKS.iter().position(|b| *b == a), int())
                {
                    self.tables.card[k] = Some(v);
                    routing_changed = true;
                }
            }
            ["config", "userrout", dir @ ("in" | "out"), n] => {
                let (Some(n @ 1..), Some(v)) = (num(n), int()) else {
                    return;
                };
                let t: &mut [Option<i32>] = if *dir == "in" {
                    &mut self.tables.user_in
                } else {
                    &mut self.tables.user_out
                };
                if let Some(slot) = t.get_mut(usize::from(n) - 1) {
                    *slot = Some(v);
                    routing_changed = true;
                }
            }
            ["outputs", set @ ("main" | "p16"), n, "src"] => {
                let (Some(n @ 1..=16), Some(v)) = (num(n), int()) else {
                    return;
                };
                let t = if *set == "main" {
                    &mut self.tables.outputs
                } else {
                    &mut self.tables.p16
                };
                t[usize::from(n) - 1] = Some(v);
                routing_changed = true;
            }
            ["-ha", n, "index"] => {
                let (Some(i @ 0..=31), Some(v)) = (num(n), int()) else {
                    return;
                };
                self.tables.channel_headamp[usize::from(i)] = Some(v);
                routing_changed = true;
            }
            _ => {}
        }
        if routing_changed {
            let before: Vec<Option<Preamp>> = self
                .routing
                .iter()
                .map(|u| u.as_ref().and_then(|u| self.preamp(u)))
                .collect();
            if self.reroute() {
                out.push(Change::Routing);
                for (i, b) in before.into_iter().enumerate() {
                    let now = self.routing[i].as_ref().and_then(|u| self.preamp(u));
                    if now != b {
                        out.push(Change::Preamp { usb: i as u8 + 1 });
                    }
                }
            }
            self.ask_needed();
        }
    }
}

struct Pending {
    sets: BTreeMap<String, Arg>,
    sent: Instant,
}

struct Shared {
    addr: SocketAddr,
    socket: UdpSocket,
    timing: Timing,
    state: Mutex<State>,
    pending: Mutex<Pending>,
    listeners: Mutex<Vec<Sender<Change>>>,
    stop: AtomicBool,
}

impl Shared {
    fn send(&self, m: &Message) {
        let _ = self.socket.send_to(&encode(m), self.addr);
    }

    fn notify(&self, changes: &[Change]) {
        if changes.is_empty() {
            return;
        }
        self.listeners
            .lock_ok()
            .retain(|l| changes.iter().all(|c| l.send(*c).is_ok()));
    }

    /// Sends the waiting sets if the last went long enough ago (or `now`).
    fn flush(&self, force: bool) {
        let sets = {
            let mut p = self.pending.lock_ok();
            if p.sets.is_empty() || (!force && p.sent.elapsed() < self.timing.set_every) {
                return;
            }
            p.sent = Instant::now();
            std::mem::take(&mut p.sets)
        };
        for (addr, arg) in sets {
            self.send(&Message::new(addr, vec![arg]));
        }
    }

    /// A set: kept (latest wins) and sent at the next slot.
    fn set(&self, addr: String, arg: Arg) {
        self.pending.lock_ok().sets.insert(addr, arg);
        self.flush(false);
    }

    fn run(&self) {
        let t = &self.timing;
        let mut buf = vec![0u8; 65536];
        let long_ago = Instant::now()
            .checked_sub(t.retry.max(t.ping).max(t.renew))
            .unwrap_or_else(Instant::now);
        let mut pinged = long_ago;
        let mut renewed = long_ago;
        let mut synced = Instant::now();
        let mut burst_at = long_ago;
        let _ = self.socket.set_read_timeout(Some(t.tick));
        while !self.stop.load(Ordering::Relaxed) {
            let mut changes = Vec::new();
            if let Ok((n, from)) = self.socket.recv_from(&mut buf)
                && from == self.addr
                && let Ok(msgs) = decode(&buf[..n])
            {
                let mut s = self.state.lock_ok();
                s.heard = Instant::now();
                let mut connected_now = false;
                for m in &msgs {
                    if !s.connected && m.addr == proto::INFO && info_of(m).is_some() {
                        s.connected = true;
                        connected_now = true;
                    }
                    s.handle(m, &mut changes);
                }
                if connected_now {
                    // Info first, then Connected: a listener sees the model with it.
                    changes.push(Change::Connected);
                    s.read_all();
                    renewed = long_ago;
                    synced = Instant::now();
                }
            }
            let now = Instant::now();
            let mut queries = Vec::new();
            {
                let mut s = self.state.lock_ok();
                if s.connected && now.duration_since(s.heard) >= t.dead_after {
                    s.connected = false;
                    s.queue.clear();
                    changes.push(Change::Disconnected);
                    self.pending.lock_ok().sets.clear();
                }
                if s.connected {
                    if now.duration_since(renewed) >= t.renew {
                        renewed = now;
                        queries.push(proto::XREMOTE.to_owned());
                    }
                    if now.duration_since(synced) >= t.resync {
                        synced = now;
                        s.read_all();
                    }
                    // At most `burst` queries a tick, however fast the answers come.
                    if !s.queue.is_empty() && now.duration_since(burst_at) >= t.tick {
                        burst_at = now;
                        let k = t.burst.min(s.queue.len());
                        queries.extend(s.queue.drain(..k));
                    }
                }
                let every = if s.connected { t.ping } else { t.retry };
                if now.duration_since(pinged) >= every {
                    pinged = now;
                    queries.push(proto::INFO.to_owned());
                }
            }
            // The subscription goes first: changes from now on are heard.
            for q in queries {
                self.send(&Message::new(q, vec![]));
            }
            self.flush(false);
            self.notify(&changes);
        }
    }
}

/// A connection to an X32's remote control. Stopped when dropped (the X32 forgets the
/// subscription 10 s later).
pub struct X32 {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl X32 {
    /// Starts talking to the X32 at `addr` (port 10023, see [`parse_address`]). Returns
    /// at once; [`X32::connected`] turns true when it answers.
    pub fn connect(addr: SocketAddr) -> std::io::Result<X32> {
        X32::connect_with(addr, Timing::default())
    }

    /// [`X32::connect`] with other clocks.
    pub fn connect_with(addr: SocketAddr, timing: Timing) -> std::io::Result<X32> {
        let socket = bind_for(&addr)?;
        let shared = Arc::new(Shared {
            addr,
            socket,
            timing,
            state: Mutex::new(State::new()),
            pending: Mutex::new(Pending {
                sets: BTreeMap::new(),
                sent: Instant::now(),
            }),
            listeners: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("idn-mixer-x32".into())
            .spawn(move || sh.run())?;
        Ok(X32 {
            shared,
            thread: Some(thread),
        })
    }

    /// Finds an X32 on the LAN ([`discover`], up to `wait`) and connects to the first.
    pub fn find(wait: Duration) -> std::io::Result<Option<X32>> {
        match discover(wait)?.first() {
            Some(f) => X32::connect(f.addr).map(Some),
            None => Ok(None),
        }
    }

    /// Its address.
    pub fn address(&self) -> SocketAddr {
        self.shared.addr
    }

    /// Answering.
    pub fn connected(&self) -> bool {
        self.shared.state.lock_ok().connected
    }

    /// Waits up to `wait` for it to answer; whether it does.
    pub fn wait_connected(&self, wait: Duration) -> bool {
        let end = Instant::now() + wait;
        while !self.connected() {
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Its model and firmware, once read.
    pub fn info(&self) -> Option<Info> {
        self.shared.state.lock_ok().info.clone()
    }

    /// USB channel `usb` (1…32): what it carries, once its routing was read.
    pub fn usb(&self, usb: u8) -> Option<UsbChannel> {
        let s = self.shared.state.lock_ok();
        s.routing.get(usize::from(usb).checked_sub(1)?)?.clone()
    }

    /// All 32 USB channels, once the card routing was read.
    pub fn routing(&self) -> Option<[UsbChannel; 32]> {
        let s = self.shared.state.lock_ok();
        let v: Option<Vec<UsbChannel>> = s.routing.iter().cloned().collect();
        v?.try_into().ok()
    }

    /// The preamp behind USB channel `usb` (1…32), once read; `None` when it carries
    /// none.
    pub fn preamp(&self, usb: u8) -> Option<Preamp> {
        let s = self.shared.state.lock_ok();
        let u = s.routing.get(usize::from(usb).checked_sub(1)?)?.as_ref()?;
        s.preamp(u)
    }

    /// Headamp `index` (0…127), if this module has read it (the ones the USB channels
    /// carry are read; others only when the X32 reports a change).
    pub fn headamp(&self, index: u8) -> Option<(f32, bool)> {
        let s = self.shared.state.lock_ok();
        let i = usize::from(index);
        Some((*s.gain.get(i)?.as_ref()?, (*s.phantom.get(i)?)?))
    }

    /// Everything, for a state document.
    pub fn status(&self) -> Status {
        let s = self.shared.state.lock_ok();
        Status {
            address: self.shared.addr,
            connected: s.connected,
            info: s.info.clone(),
            usb: s
                .routing
                .iter()
                .flatten()
                .map(|u| (u.clone(), s.preamp(u)))
                .collect(),
        }
    }

    /// Changes from now on. Each call gives its own receiver; dropped ones are
    /// forgotten.
    pub fn changes(&self) -> Receiver<Change> {
        let (tx, rx) = channel();
        self.shared.listeners.lock_ok().push(tx);
        rx
    }

    fn usb_channel(&self, usb: u8) -> Result<UsbChannel, Error> {
        let s = self.shared.state.lock_ok();
        if !s.connected {
            return Err(Error::NotConnected);
        }
        s.routing
            .get(usize::from(usb).wrapping_sub(1))
            .cloned()
            .flatten()
            .ok_or(Error::NoUsbChannel(usb))
    }

    fn usb_headamp(&self, usb: u8) -> Result<u8, Error> {
        self.usb_channel(usb)?.headamp.ok_or(Error::NoPreamp(usb))
    }

    /// Sets the gain of the preamp behind USB channel `usb`; the gain set (clamped to
    /// -12…+60 dB, 0.5 dB steps).
    pub fn set_gain(&self, usb: u8, db: f32) -> Result<f32, Error> {
        let ha = self.usb_headamp(usb)?;
        self.set_headamp_gain(ha, db)
    }

    /// Turns 48 V phantom power on or off for the preamp behind USB channel `usb`.
    pub fn set_phantom(&self, usb: u8, on: bool) -> Result<(), Error> {
        let ha = self.usb_headamp(usb)?;
        self.set_headamp_phantom(ha, on)
    }

    /// Sets the polarity of the X32 channel that takes USB channel `usb`'s input (see
    /// [`Preamp::invert`] for what it changes).
    pub fn set_invert(&self, usb: u8, on: bool) -> Result<(), Error> {
        let ch = self
            .usb_channel(usb)?
            .channel
            .ok_or(Error::NoChannel(usb))?;
        self.set_channel_invert(ch, on)
    }

    fn check_headamp(&self, index: u8) -> Result<(), Error> {
        let s = self.shared.state.lock_ok();
        if !s.connected {
            return Err(Error::NotConnected);
        }
        if !s.has_headamp(index) {
            return Err(Error::NoHeadamp(index));
        }
        Ok(())
    }

    /// Applies a set to the cache now and tells the listeners (the X32 echoes a set
    /// only to its other clients, p. 10).
    fn apply_local(&self, m: Message) {
        let mut changes = Vec::new();
        self.shared.state.lock_ok().handle(&m, &mut changes);
        self.shared.notify(&changes);
        let Message { addr, mut args } = m;
        self.shared.set(addr, args.remove(0));
    }

    /// Sets headamp `index`'s (0…127) gain; the gain set.
    pub fn set_headamp_gain(&self, index: u8, db: f32) -> Result<f32, Error> {
        let f = gain_to_float(db).ok_or(Error::BadValue)?;
        self.check_headamp(index)?;
        self.apply_local(Message::new(
            proto::headamp_gain(index),
            vec![Arg::Float(f)],
        ));
        Ok(float_to_gain(f))
    }

    /// Turns headamp `index`'s (0…127) 48 V phantom power on or off. Sent at once
    /// (with any waiting gain sets).
    pub fn set_headamp_phantom(&self, index: u8, on: bool) -> Result<(), Error> {
        self.check_headamp(index)?;
        self.apply_local(Message::new(
            proto::headamp_phantom(index),
            vec![Arg::Int(i32::from(on))],
        ));
        self.shared.flush(true);
        Ok(())
    }

    /// Sets X32 channel `channel`'s (1…32) polarity invert. Sent at once.
    pub fn set_channel_invert(&self, channel: u8, on: bool) -> Result<(), Error> {
        if !(1..=32).contains(&channel) {
            return Err(Error::BadChannel(channel));
        }
        if !self.connected() {
            return Err(Error::NotConnected);
        }
        self.apply_local(Message::new(
            proto::channel_invert(channel),
            vec![Arg::Int(i32::from(on))],
        ));
        self.shared.flush(true);
        Ok(())
    }
}

impl Drop for X32 {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
