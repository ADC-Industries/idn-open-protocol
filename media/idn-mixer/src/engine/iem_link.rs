//! The IEM flow on the network (§M.26, idn-iem.md §I.3–§I.6): the marked channels, as the
//! clock thread taps them, go out as an IDN audio output (wired listeners; multicast
//! through `mcast/1`) and, while phones listen, as one Opus stream a channel in the
//! flow's UDP packets (multicast on a group of the desk's own, and unicast to each phone
//! that asked). Phones' sessions and talkback arrive on the same UDP port.

use super::*;
use crate::iem::{FlowChannel, IemConfig, TALKBACK_INPUTS};
use idn_iem::wire::{self, Packet, Status};
use idn_open_protocol::trust::token::{Claims, Scope};
use idn_open_protocol::web::{TokenCheck, Who};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::mpsc::{Receiver, SyncSender};

/// One block of the flow's channels, as the clock thread tapped them: which channels
/// (from 0) and their samples at the desk's rate.
pub(super) struct Block {
    pub ids: Arc<Vec<usize>>,
    pub audio: Vec<Vec<f32>>,
}

/// The flow thread's ends of the clock thread's channels.
pub(super) struct FlowEnds {
    blocks: Receiver<Block>,
    spare: SyncSender<Vec<Vec<f32>>>,
}

/// Sessions at most (phones listening by multicast or talking count too).
const MAX_SESSIONS: usize = 256;
/// Requests a second at most, from one address and from all (each checks a signature).
const SUBSCRIBES_PER_IP: u32 = 5;
const SUBSCRIBES_ALL: u32 = 50;
/// A talker's lane goes this long after its last frame (its buffer played out): the next
/// press starts afresh, not on a lane widened by the silence between.
const TALK_IDLE: Duration = Duration::from_millis(300);
/// A token is checked again (its user removed, its life over) this often.
const RECHECK: Duration = Duration::from_secs(30);

/// A phone pressing push-to-talk: its frames decoded into its mix's talkback input.
struct Talker {
    dec: idn_iem::codec::Decoder,
    lane: idn_open_protocol::audio::Lane,
    seq: u32,
    last: Instant,
    /// Its talkback input, 1 to [`TALKBACK_INPUTS`].
    slot: usize,
}

/// A phone's session.
struct Session {
    addr: SocketAddr,
    flags: u8,
    /// Its holder, for the state and the audit.
    who: Who,
    claims: Option<Claims>,
    /// The mix whose talkback it may talk into.
    talk_mix: Option<String>,
    last: Instant,
    checked: Instant,
    talker: Option<Talker>,
}

#[derive(Default)]
struct Sessions {
    map: HashMap<u64, Session>,
    /// Requests this second, by address and in all.
    per_ip: HashMap<IpAddr, (Instant, u32)>,
    all: Option<(Instant, u32)>,
}

impl Sessions {
    fn unicast(&self) -> usize {
        self.map
            .values()
            .filter(|s| s.flags & wire::WANT_UNICAST != 0)
            .count()
    }
    fn multicast(&self) -> usize {
        self.map
            .values()
            .filter(|s| s.flags & wire::WANT_MULTICAST != 0)
            .count()
    }
}

/// The IEM side's shared state.
pub(super) struct Hub {
    pub cfg: IemConfig,
    /// The flow's id (random at the start, never 0).
    pub flow: u32,
    /// The UDP socket phones reach (sessions, talkback; unicast audio leaves from it).
    pub(super) socket: Option<Arc<UdpSocket>>,
    /// The multicast group the Opus copy goes to (port [`idn_common::ports::MCAST_IEM`]).
    pub group: Option<SocketAddrV4>,
    /// The interface (its address) the multicast copy leaves by, when one was found
    /// (idn-iem.md §I.5); `None`: the system's choice.
    pub interface: Option<Ipv4Addr>,
    /// To the flow thread, and its buffers back (the clock thread's ends).
    pub to_flow: SyncSender<Block>,
    pub spare: Mutex<Receiver<Vec<Vec<f32>>>>,
    /// Blocks the flow thread was too far behind to take.
    pub dropped: AtomicU64,
    /// The flow's channels as last described, and their version (`layout`).
    layout: Mutex<(Vec<FlowChannel>, u16)>,
    sessions: Mutex<Sessions>,
    /// The talkback inputs (one channel each), filled on the clock thread.
    pub talk: Arc<Mixer>,
    /// Talkers with a lane now (the clock thread fills the talkback inputs while any).
    pub talking: std::sync::atomic::AtomicUsize,
    /// The first talkback input's desk input number (after the routed ones).
    pub first_talkback: usize,
    tokens: Option<Arc<TokenCheck>>,
    publisher: Publisher,
    /// The IDN output's full name, while there is one.
    source: Mutex<Option<String>>,
    /// Frames and packets sent, packets that couldn't be.
    frames: AtomicU64,
    packets: AtomicU64,
    send_errors: AtomicU64,
}

/// A random number (a flow id, a group address): not a secret.
fn random_u64() -> u64 {
    u64::from_le_bytes(idn_open_protocol::random::secure_random())
}

impl Hub {
    /// Binds the UDP port (`cfg.port`; taken, another free one), picks the flow's id and
    /// multicast group, and sends the multicast copy by `interface` (the LAN's; see
    /// [`crate::iem::multicast_interface`]). `routable`: the desk inputs before the
    /// talkback inputs.
    pub fn new(
        cfg: IemConfig,
        interface: Option<Ipv4Addr>,
        rate: u32,
        routable: usize,
        tokens: Option<Arc<TokenCheck>>,
        publisher: Publisher,
    ) -> (Hub, FlowEnds) {
        let socket = cfg.port.and_then(|p| {
            let s = UdpSocket::bind(("0.0.0.0", p)).or_else(|e| {
                if p == 0 {
                    return Err(e);
                }
                idn_open_protocol::log::log(&format!(
                    "mixer: IEM: UDP port {p}: {e}; another one taken (GET /api/v1/iem says which)"
                ));
                UdpSocket::bind(("0.0.0.0", 0))
            });
            match s {
                Ok(s) => {
                    let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                    let _ = s.set_multicast_ttl_v4(1);
                    // Out by the LAN's interface: on a machine with an overlay too (wt0)
                    // the system's own choice may be the overlay (follow-up 3).
                    if let Some(i) = interface
                        && let Err(e) = socket2::SockRef::from(&s).set_multicast_if_v4(&i)
                    {
                        idn_open_protocol::log::log(&format!(
                            "mixer: IEM: multicast by {i}: {e} (the system's choice used)"
                        ));
                    }
                    Some(Arc::new(s))
                }
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "mixer: IEM: no UDP port ({e}): phones can't take the flow"
                    ));
                    None
                }
            }
        });
        let r = random_u64();
        let flow = (r as u32).max(1);
        let group = (socket.is_some() && cfg.multicast).then(|| {
            let (a, b) = ((r >> 32) as u8, (r >> 40) as u8);
            SocketAddrV4::new(
                Ipv4Addr::new(239, 255, a.max(1), b.max(1)),
                idn_common::ports::MCAST_IEM,
            )
        });
        let (to_flow, blocks) = std::sync::mpsc::sync_channel(8);
        let (spare_tx, spare) = std::sync::mpsc::sync_channel(8);
        for _ in 0..4 {
            let _ = spare_tx.try_send(Vec::new());
        }
        let hub = Hub {
            cfg,
            flow,
            socket,
            group,
            interface,
            to_flow,
            spare: Mutex::new(spare),
            dropped: AtomicU64::new(0),
            layout: Mutex::new((Vec::new(), 0)),
            sessions: Mutex::default(),
            talk: Mixer::new(rate, TALKBACK_INPUTS as u16, "talkback"),
            talking: Default::default(),
            first_talkback: routable + 1,
            tokens,
            publisher,
            source: Mutex::new(None),
            frames: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
        };
        (
            hub,
            FlowEnds {
                blocks,
                spare: spare_tx,
            },
        )
    }

    /// The UDP port phones reach, if any.
    pub fn port(&self) -> Option<u16> {
        self.socket
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
    }

    /// The version of channel list `list`: the last one's, or the next when it differs
    /// (the flow's packets and `GET /api/v1/iem` carry it).
    pub fn layout_of(&self, list: &[FlowChannel]) -> u16 {
        let mut l = self.layout.lock_ok();
        if l.0 != list {
            l.0 = list.to_vec();
            l.1 = l.1.wrapping_add(1);
        }
        l.1
    }

    /// The phones' sessions (unicast, multicast) and the talkback inputs in use now.
    pub fn status(&self) -> Value {
        let s = self.sessions.lock_ok();
        let phones: Vec<Value> = s
            .map
            .iter()
            .map(|(client, x)| {
                json!({
                    "client": format!("{client:016x}"),
                    "user": x.who.user,
                    "name": x.who.name,
                    "scope": x.who.scope.as_string(),
                    "address": x.addr.to_string(),
                    "unicast": x.flags & wire::WANT_UNICAST != 0,
                    "multicast": x.flags & wire::WANT_MULTICAST != 0,
                    "talk_mix": x.talk_mix,
                    "talking": x.talker.as_ref().is_some_and(|t| t.last.elapsed() < Duration::from_millis(200)),
                })
            })
            .collect();
        json!({
            "port": self.port(),
            "multicast": self.group.map(|g| json!({"group": g.ip().to_string(), "port": g.port(),
                "interface": self.interface.map(|i| i.to_string())})),
            "source": *self.source.lock_ok(),
            "phones": phones,
            "unicast": s.unicast(),
            "max_phones": self.cfg.max_phones,
            "frames": self.frames.load(Ordering::Relaxed),
            "packets": self.packets.load(Ordering::Relaxed),
            "send_errors": self.send_errors.load(Ordering::Relaxed),
            "dropped_blocks": self.dropped.load(Ordering::Relaxed),
        })
    }

    /// The phones on mix `id` now (§M.28, the mix's `who`): those talking into it or
    /// holding its `iem:<mix>` token; each holder's name and whether it talks.
    pub fn phones_on(&self, id: &str) -> Vec<Value> {
        self.sessions
            .lock_ok()
            .map
            .values()
            // Its talkback's mix, else its token's own (`iem:<mix>`).
            .filter(|x| {
                x.talk_mix.as_deref() == Some(id)
                    || matches!(&x.who.scope, Scope::Iem(m) if m == id)
            })
            .map(|x| {
                json!({
                    "name": x.who.name,
                    "user": x.who.user,
                    "talking": x.talker.as_ref().is_some_and(|t| t.last.elapsed() < Duration::from_millis(200)),
                })
            })
            .collect()
    }

    /// The talkback inputs a phone talks into now (by slot, from 0).
    pub fn talking_slots(&self) -> [bool; TALKBACK_INPUTS] {
        let mut out = [false; TALKBACK_INPUTS];
        for s in self.sessions.lock_ok().map.values() {
            if let Some(t) = &s.talker
                && t.last.elapsed() < Duration::from_millis(200)
            {
                out[t.slot - 1] = true;
            }
        }
        out
    }

    /// The IDN output's full name (`HOST (Mixer IEM)`), while it exists.
    pub fn source(&self) -> Option<String> {
        self.source.lock_ok().clone()
    }

    fn send(&self, sock: &UdpSocket, b: &[u8], to: SocketAddr) {
        match sock.send_to(b, to) {
            Ok(_) => {
                self.packets.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.send_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn reply(&self, sock: &UdpSocket, to: SocketAddr, client: u64, status: Status, msg: &str) {
        let layout = self.layout.lock_ok().1;
        let p = Packet::Reply(wire::Reply {
            flow: self.flow,
            client,
            status,
            layout,
            keepalive_ms: idn_iem::KEEPALIVE_MS,
            timeout_ms: self.cfg.timeout.as_millis().min(u128::from(u16::MAX)) as u16,
            message: msg.to_owned(),
        });
        self.send(sock, &p.to_bytes(), to);
    }
}

/// Starts the flow thread and, with a UDP port, the sessions' thread.
pub(super) fn start(
    sh: &Arc<Shared>,
    ends: FlowEnds,
) -> io::Result<Vec<std::thread::JoinHandle<()>>> {
    let mut threads = Vec::new();
    {
        let sh = sh.clone();
        threads.push(
            std::thread::Builder::new()
                .name("idn-mixer-iem".into())
                .spawn(move || flow(&sh, ends))?,
        );
    }
    if let Some(sock) = sh.iem.socket.clone() {
        let sh = sh.clone();
        threads.push(
            std::thread::Builder::new()
                .name("idn-mixer-iem-net".into())
                .spawn(move || serve(&sh, &sock))?,
        );
    }
    Ok(threads)
}

/// What the flow thread keeps.
struct Flow {
    ids: Arc<Vec<usize>>,
    chans: Vec<FlowChannel>,
    layout: u16,
    out: Option<Output>,
    /// Each channel's encoder and its samples waiting for a whole frame (48 kHz).
    enc: Vec<idn_iem::codec::Encoder>,
    acc: Vec<Vec<f32>>,
    conv: Option<idn_open_protocol::audio::resample::Rate>,
    seq: u32,
    looked: Instant,
    last_block: Instant,
}

impl Flow {
    /// The channel list, the IDN output and the encoders for the channels of `ids` (names
    /// from the settings now).
    fn relayout(&mut self, sh: &Shared, ids: &Arc<Vec<usize>>, rate: u32, block: usize) {
        let s = sh.desk.lock_ok().settings_arc();
        let chans = crate::iem::describe(&s, ids);
        let layout = sh.iem.layout_of(&chans);
        self.looked = Instant::now();
        let same_ids = *self.ids == **ids;
        self.ids = ids.clone();
        if layout == self.layout && self.out.is_some() == !chans.is_empty() && same_ids {
            return;
        }
        let relabel = self.chans != chans;
        self.layout = layout;
        self.chans = chans;
        if !same_ids {
            // Encoders and frames afresh for the new list (a frame's channels together).
            // (All or none: one missing, no frame was ever encoded and the waiting
            // samples grew a block every period for as long as anyone listened; bug review
            // 3.)
            let enc: Vec<_> = (0..ids.len())
                .filter_map(|_| {
                    idn_iem::codec::Encoder::new(idn_iem::FRAME_SAMPLES, idn_iem::BITRATE)
                        .map_err(|e| idn_open_protocol::log::log(&format!("mixer: IEM: {e}")))
                        .ok()
                })
                .collect();
            if enc.len() == ids.len() {
                self.enc = enc;
                self.acc = vec![Vec::with_capacity(idn_iem::FRAME_SAMPLES * 4); ids.len()];
            } else {
                self.enc = Vec::new();
                self.acc = Vec::new();
            }
            self.conv = (rate != idn_iem::RATE && !ids.is_empty())
                .then(|| {
                    idn_open_protocol::audio::resample::Rate::new(
                        rate,
                        idn_iem::RATE,
                        ids.len(),
                        block,
                        0.0,
                    )
                    .ok()
                })
                .flatten();
        }
        // The IDN output: made again with the new labels (its receivers reconnect).
        if relabel || self.out.is_none() != self.chans.is_empty() {
            self.out = None;
            *sh.iem.source.lock_ok() = None;
            if !self.chans.is_empty() {
                let name = format!("{} IEM", sh.cfg.name);
                let cfg = OutputConfig {
                    audio_labels: self.chans.iter().map(FlowChannel::label).collect(),
                    roles: vec![IEM_ROLE.into()],
                    // One stream for every wired listener (`mcast/1`), for those that ask.
                    multicast: Some(Default::default()),
                    ..OutputConfig::new(&name)
                };
                match Output::new(&sh.iem.publisher, cfg) {
                    Ok(o) => {
                        // Its sound: the desk's IEM channels (idn-routing.md §A.16).
                        o.set_audio_origin(&idn_open_protocol::node::routing::AudioOrigin {
                            kind: "desk".into(),
                            device: sh.cfg.name.clone(),
                            channels: self.chans.iter().map(FlowChannel::label).collect(),
                        });
                        *sh.iem.source.lock_ok() = Some(sh.iem.publisher.full_name(&name));
                        self.out = Some(o);
                    }
                    Err(e) => idn_open_protocol::log::log(&format!(
                        "mixer: IEM: the IDN output {name:?}: {e}"
                    )),
                }
            }
        }
    }
}

/// What programs looking for a desk's IEM flow find it by (§E.2.2's roles).
pub const IEM_ROLE: &str = "iem/1";

/// The flow thread: each block to the IDN output, and coded for the phones while any
/// listens.
fn flow(sh: &Arc<Shared>, ends: FlowEnds) {
    let rate = rate_hz();
    let block = sh.desk.lock_ok().block();
    let mut f = Flow {
        ids: Arc::default(),
        chans: Vec::new(),
        layout: 0,
        out: None,
        enc: Vec::new(),
        acc: Vec::new(),
        conv: None,
        seq: 0,
        looked: Instant::now(),
        last_block: Instant::now(),
    };
    let mut frame = vec![0f32; idn_iem::FRAME_SAMPLES];
    while !sh.stop.load(Ordering::Relaxed) {
        let b = match ends.blocks.recv_timeout(Duration::from_millis(100)) {
            Ok(b) => b,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // No channel marked now (the clock thread sends nothing): the output goes.
                if f.out.is_some() && f.last_block.elapsed() > Duration::from_millis(500) {
                    f.relayout(sh, &Arc::default(), rate, block);
                }
                continue;
            }
            Err(_) => break,
        };
        f.last_block = Instant::now();
        if *b.ids != *f.ids || f.looked.elapsed() > Duration::from_millis(500) {
            f.relayout(sh, &b.ids, rate, block);
        }
        if let Some(o) = &f.out {
            let _ = o.send_audio(
                AudioBlock {
                    sample_rate: rate,
                    channels: b.audio.clone(),
                },
                None,
            );
        }
        let (uni, multi) = {
            let s = sh.iem.sessions.lock_ok();
            (s.unicast(), s.multicast())
        };
        let Some(sock) = sh.iem.socket.clone().filter(|_| uni + multi > 0) else {
            f.acc.iter_mut().for_each(Vec::clear);
            let _ = ends.spare.try_send(b.audio);
            continue;
        };
        // To 48 kHz, onto each channel's waiting samples.
        match &mut f.conv {
            Some(c) => {
                c.push(&b.audio);
                let acc = &mut f.acc;
                let _ = c.pull(|o, n| {
                    for (a, x) in acc.iter_mut().zip(o) {
                        a.extend_from_slice(&x[..n]);
                    }
                });
            }
            None => {
                for (a, x) in f.acc.iter_mut().zip(&b.audio) {
                    a.extend_from_slice(x);
                }
            }
        }
        let _ = ends.spare.try_send(b.audio);
        while f
            .acc
            .first()
            .is_some_and(|a| a.len() >= idn_iem::FRAME_SAMPLES)
            && f.enc.len() == f.acc.len()
        {
            let (time, flags) = match idn_open_protocol::node::clock::site_now() {
                Some(t) => (t.max(0) as u64, wire::TIME_SITE),
                None => (idn_open_protocol::ptp::mono_ns().max(0) as u64, 0),
            };
            // (The frame's first sample was taken the waiting samples' time ago.)
            let behind = f.acc[0].len() as u64 * 1_000_000_000 / u64::from(idn_iem::RATE);
            let mut entries = Vec::with_capacity(f.acc.len());
            for ((a, e), ch) in f.acc.iter_mut().zip(&mut f.enc).zip(&f.chans) {
                frame.copy_from_slice(&a[..idn_iem::FRAME_SAMPLES]);
                a.drain(..idn_iem::FRAME_SAMPLES);
                for v in &mut frame {
                    *v = if v.is_finite() {
                        v.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                }
                if let Ok(data) = e.encode(&frame) {
                    entries.push(wire::Entry {
                        id: ch.id,
                        data: data.to_vec(),
                    });
                }
            }
            let packets = wire::pack_frame(
                sh.iem.flow,
                f.layout,
                f.seq,
                time.saturating_sub(behind),
                flags,
                idn_iem::FRAME_SAMPLES as u16,
                entries,
            );
            f.seq = f.seq.wrapping_add(1);
            sh.iem.frames.fetch_add(1, Ordering::Relaxed);
            let to: Vec<SocketAddr> = {
                let s = sh.iem.sessions.lock_ok();
                s.map
                    .values()
                    .filter(|x| x.flags & wire::WANT_UNICAST != 0)
                    .map(|x| x.addr)
                    .collect()
            };
            for p in &packets {
                if let Some(g) = sh.iem.group.filter(|_| multi > 0) {
                    sh.iem.send(&sock, p, SocketAddr::V4(g));
                }
                for a in &to {
                    sh.iem.send(&sock, p, *a);
                }
            }
        }
    }
}

/// The sessions' thread: requests, keepalives, talkback; sessions that went quiet, and
/// tokens no longer valid, end.
fn serve(sh: &Arc<Shared>, sock: &UdpSocket) {
    let mut buf = vec![0u8; wire::MAX_PACKET + 1];
    let mut swept = Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(p) = wire::parse(&buf[..n]) {
                    handle(sh, sock, p, from);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            // (Windows says a phone's port closed as an error on the next receive.)
            Err(_) => {}
        }
        if swept.elapsed() >= Duration::from_millis(100) {
            swept = Instant::now();
            sweep(sh);
        }
    }
}

/// Sessions not renewed within the timeout, or whose token no longer holds (a `control`
/// one: also once the desk's `iem_control` is off), end;
/// talkers quiet for a moment let their lane go.
fn sweep(sh: &Shared) {
    let hub = &sh.iem;
    // (Read before the sessions are held: the desk's lock is never taken inside theirs.)
    let control_ok = sh.desk.lock_ok().settings().iem_control;
    let mut s = hub.sessions.lock_ok();
    // The subscribe rate table: addresses not heard from for a second go.
    let now = Instant::now();
    s.per_ip
        .retain(|_, (t, _)| now.duration_since(*t) < Duration::from_secs(1));
    let timeout = hub.cfg.timeout;
    let mut ended = Vec::new();
    // Sessions that went quiet or whose token no longer holds: (why, its mix, where).
    let mut dropped: Vec<(&'static str, Option<String>, SocketAddr, Who, u64)> = Vec::new();
    s.map.retain(|client, x| {
        let quiet = x.last.elapsed() >= timeout;
        let mut keep = !quiet;
        if keep && x.checked.elapsed() >= RECHECK {
            x.checked = Instant::now();
            if let (Some(c), Some(t)) = (&x.claims, &hub.tokens) {
                keep = t.recheck(c);
            }
        }
        // A control token taken while the desk allowed it: ends once it no longer does
        // (bug run 2026-11-24: it went on talking into every mix).
        if keep && !control_ok && x.claims.is_some() && matches!(x.who.scope, Scope::Control) {
            keep = false;
        }
        if !keep {
            ended.push((x.who.clone(), *client));
            let mix = match &x.who.scope {
                Scope::Iem(m) => Some(m.clone()),
                _ => x.talk_mix.clone(),
            };
            let why = if quiet { "timeout" } else { "token" };
            dropped.push((why, mix, x.addr, x.who.clone(), *client));
        }
        keep
    });
    for x in s.map.values_mut() {
        if x.talker
            .as_ref()
            .is_some_and(|t| t.last.elapsed() > TALK_IDLE)
        {
            x.talker = None;
        }
    }
    let talking = s.map.values().filter(|x| x.talker.is_some()).count();
    hub.talking.store(talking, Ordering::Relaxed);
    drop(s);
    for (who, client) in ended {
        sh.audit_iem(&who, "iem_session_end", client, None);
    }
    // Each a typed event (§M.27): a phone that stopped renewing its session (out of the
    // Wi-Fi's reach, its app stopped), or whose token was withdrawn.
    for (why, mix, peer, who, client) in dropped {
        let whose = if who.name.is_empty() {
            peer.to_string()
        } else {
            format!("{} ({peer})", who.name)
        };
        let text = match (why, &mix) {
            ("timeout", Some(m)) => format!("IEM phone {whose} of mix {m} dropped: no keepalive"),
            ("timeout", None) => format!("IEM phone {whose} dropped: no keepalive"),
            (_, Some(m)) => {
                format!("IEM phone {whose} of mix {m} ended: its token no longer holds")
            }
            (_, None) => format!("IEM phone {whose} ended: its token no longer holds"),
        };
        sh.events.emit(
            "iem_phone_dropped",
            "warning",
            &text,
            json!({"mix": mix, "peer": peer.to_string(),
                   "client": format!("{client:016x}"), "user": who.user, "reason": why}),
        );
    }
}

impl Shared {
    fn audit_iem(&self, who: &Who, action: &str, client: u64, error: Option<&str>) {
        self.audit.record(
            who,
            action,
            &json!({"client": format!("{client:016x}")}),
            error,
        );
    }
}

/// What a request's token allows: listening (read, control, an IEM mix) and talking into
/// one mix (control, or the token's own mix).
fn handle(sh: &Arc<Shared>, sock: &UdpSocket, p: Packet, from: SocketAddr) {
    let hub = &sh.iem;
    match p {
        Packet::Subscribe(req) => subscribe(sh, sock, req, from),
        Packet::Keepalive { client, .. } => {
            let mut s = hub.sessions.lock_ok();
            match s.map.get_mut(&client).filter(|x| x.addr == from) {
                Some(x) => {
                    x.last = Instant::now();
                    drop(s);
                    hub.reply(sock, from, client, Status::Ok, "");
                }
                None => {
                    drop(s);
                    hub.reply(sock, from, client, Status::Unknown, "subscribe again");
                }
            }
        }
        Packet::Bye { client, .. } => {
            let gone = {
                let mut s = hub.sessions.lock_ok();
                let mine = s.map.get(&client).is_some_and(|x| x.addr == from);
                mine.then(|| s.map.remove(&client)).flatten()
            };
            if let Some(x) = gone {
                sh.audit_iem(&x.who, "iem_session_end", client, None);
            }
        }
        Packet::Talk(t) => talk(sh, sock, t, from),
        Packet::Audio(_) | Packet::Reply(_) => {}
    }
}

fn subscribe(sh: &Arc<Shared>, sock: &UdpSocket, req: wire::Subscribe, from: SocketAddr) {
    let hub = &sh.iem;
    let client = req.client;
    let refuse = |status: Status, msg: &str| {
        hub.reply(sock, from, client, status, msg);
    };
    // Bounded: a few requests a second from one address, and in all.
    {
        let mut s = hub.sessions.lock_ok();
        let now = Instant::now();
        let all = s.all.get_or_insert((now, 0));
        if now.duration_since(all.0) >= Duration::from_secs(1) {
            *all = (now, 0);
        }
        all.1 += 1;
        // Over the limit in all, or a new address with the table full: refused before
        // the table is touched (it is pruned by the sweep, not at each packet: a flood
        // from many addresses walked it under this lock at each one; bug run 2026-11-24).
        if all.1 > SUBSCRIBES_ALL || (s.per_ip.len() >= 1024 && !s.per_ip.contains_key(&from.ip()))
        {
            drop(s);
            return refuse(Status::Busy, "too many requests: try again in a second");
        }
        let mine = s.per_ip.entry(from.ip()).or_insert((now, 0));
        if now.duration_since(mine.0) >= Duration::from_secs(1) {
            *mine = (now, 0);
        }
        mine.1 += 1;
        if mine.1 > SUBSCRIBES_PER_IP {
            drop(s);
            return refuse(Status::Busy, "too many requests: try again in a second");
        }
    }
    if client == 0 || req.flags & (wire::WANT_UNICAST | wire::WANT_MULTICAST | wire::WANT_TALK) == 0
    {
        return refuse(Status::Bad, "a client id and what it wants");
    }
    // Who: a token from the network (an IEM mix's, read or control); this machine needs
    // none.
    let (who, claims) = if req.token.is_empty() {
        if !(from.ip().is_loopback() && hub.cfg.trust_loopback) {
            return refuse(
                Status::TokenNeeded,
                "a site token (iem:<mix>, read or control) is needed",
            );
        }
        let mut w = Who::host("local");
        w.peer = from.to_string();
        (w, None)
    } else {
        let Some(check) = &hub.tokens else {
            return refuse(
                Status::TokenRefused,
                "this desk takes no site tokens (its machine is in no site)",
            );
        };
        match check.check(&req.token) {
            Ok(c) => {
                let mut w = Who::host(&c.sub);
                w.name = if c.name.is_empty() {
                    c.sub.clone()
                } else {
                    c.name.clone()
                };
                w.scope = c.scope.clone();
                w.peer = from.to_string();
                w.via = idn_open_protocol::web::auth::Via::Token;
                w.token = Some(c.clone());
                (w, Some(c))
            }
            Err(e) => {
                sh.audit_iem(
                    &{
                        let mut w = Who::host("?");
                        w.peer = from.to_string();
                        w
                    },
                    "iem_subscribe",
                    client,
                    Some(&e.to_string()),
                );
                return refuse(Status::TokenRefused, &e.to_string());
            }
        }
    };
    // A `control` token over the flow only when the desk allows it (an
    // option, off by default; it crosses the network in plain UDP). This machine's own
    // requests carry none.
    if claims.is_some()
        && matches!(who.scope, Scope::Control)
        && !sh.desk.lock_ok().settings().iem_control
    {
        sh.audit_iem(
            &who,
            "iem_subscribe",
            client,
            Some("control token, not allowed here"),
        );
        return refuse(
            Status::NotAllowed,
            "a control token isn't taken on the IEM flow (the desk's iem_control): use an iem:<mix> or read token",
        );
    }
    let listen = matches!(who.scope, Scope::Read | Scope::Control | Scope::Iem(_));
    if !listen {
        sh.audit_iem(&who, "iem_subscribe", client, Some("a bus token"));
        return refuse(Status::NotAllowed, "a bus token can't take the IEM flow");
    }
    // Talking: into the token's own mix, or (control) the one asked for; the mix must be
    // on the desk.
    let talk_mix = if req.flags & wire::WANT_TALK != 0 {
        let mix = match &who.scope {
            Scope::Iem(m) if req.mix.is_empty() || req.mix == *m => m.clone(),
            Scope::Control if !req.mix.is_empty() => req.mix.clone(),
            Scope::Control => return refuse(Status::Bad, "talk: which mix"),
            _ => {
                sh.audit_iem(&who, "iem_subscribe", client, Some("talk outside its mix"));
                return refuse(Status::NotAllowed, "this token can't talk into that mix");
            }
        };
        let exists = sh
            .desk
            .lock_ok()
            .settings()
            .iem_mixes
            .iter()
            .any(|m| m.id == mix);
        if !exists {
            return refuse(Status::NotAllowed, "no such mix on the desk");
        }
        Some(mix)
    } else {
        None
    };
    let mut s = hub.sessions.lock_ok();
    let fresh = !s.map.contains_key(&client);
    let was_unicast = s
        .map
        .get(&client)
        .is_some_and(|x| x.flags & wire::WANT_UNICAST != 0);
    if req.flags & wire::WANT_UNICAST != 0 && !was_unicast && s.unicast() >= hub.cfg.max_phones {
        drop(s);
        sh.audit_iem(&who, "iem_subscribe", client, Some("full"));
        return refuse(
            Status::Full,
            &format!("the desk sends to {} phones already", hub.cfg.max_phones),
        );
    }
    if fresh && s.map.len() >= MAX_SESSIONS {
        drop(s);
        return refuse(Status::Full, "too many sessions");
    }
    let talker = s.map.remove(&client).and_then(|x| x.talker);
    s.map.insert(
        client,
        Session {
            addr: from,
            flags: req.flags,
            who: who.clone(),
            claims,
            talk_mix,
            last: Instant::now(),
            checked: Instant::now(),
            talker,
        },
    );
    drop(s);
    if fresh {
        sh.audit_iem(&who, "iem_subscribe", client, None);
    }
    hub.reply(sock, from, client, Status::Ok, "");
    // (A new listener changes the state's list of phones.)
    *sh.version.lock_ok() += 1;
    sh.changed.notify_all();
}

fn talk(sh: &Arc<Shared>, sock: &UdpSocket, t: wire::Talk, from: SocketAddr) {
    let hub = &sh.iem;
    let fs = t.frame_samples as usize;
    if !(120..=idn_iem::MAX_TALK_SAMPLES).contains(&fs) || !fs.is_multiple_of(120) {
        return;
    }
    // Its mix (the sessions held a moment), then that mix's talkback input from the desk
    // with the sessions let go: the desk's lock is never taken inside theirs (bug run
    // 2026-11-25: here it was, and the flow thread waited behind the clock's block).
    let mix = {
        let s = hub.sessions.lock_ok();
        s.map
            .get(&t.client)
            .filter(|x| x.addr == from)
            .map(|x| x.talk_mix.clone())
    };
    let Some(mix) = mix else {
        return hub.reply(sock, from, t.client, Status::Unknown, "subscribe again");
    };
    let Some(mix) = mix else {
        return hub.reply(
            sock,
            from,
            t.client,
            Status::NotAllowed,
            "not a talking session",
        );
    };
    // Its mix's talkback input now (a mix deleted: nothing more).
    let slot = sh
        .desk
        .lock_ok()
        .settings()
        .iem_mixes
        .iter()
        .find(|m| m.id == mix)
        .map(|m| m.talkback)
        .filter(|&k| (1..=TALKBACK_INPUTS).contains(&k));
    let mut s = hub.sessions.lock_ok();
    // (Gone or changed meanwhile: as if it had come a moment later.)
    let Some(x) = s
        .map
        .get_mut(&t.client)
        .filter(|x| x.addr == from && x.talk_mix.as_deref() == Some(mix.as_str()))
    else {
        return;
    };
    x.last = Instant::now();
    let Some(slot) = slot else {
        x.talker = None;
        return;
    };
    if x.talker.as_ref().is_some_and(|tk| tk.slot != slot) {
        x.talker = None;
    }
    if x.talker.is_none() {
        let Ok(dec) = idn_iem::codec::Decoder::new() else {
            return;
        };
        let lane = hub.talk.lane(&LaneConfig {
            latency_ms: 40,
            max_correction_ppm: 1000.0,
            adaptive: true,
            min_ms: 20,
            keep_start_level: false,
        });
        x.talker = Some(Talker {
            dec,
            lane,
            seq: t.seq.wrapping_sub(1),
            last: Instant::now(),
            slot,
        });
        hub.talking.fetch_add(1, Ordering::Relaxed);
    }
    let tk = x.talker.as_mut().expect("made above");
    let gap = t.seq.wrapping_sub(tk.seq);
    if gap == 0 || gap > u32::MAX / 2 {
        // A frame again, or late: dropped.
        return;
    }
    let map = [(0usize, (slot - 1) as u16)];
    // A few frames lost: concealed (more: the lane covers the gap).
    for _ in 1..gap.min(4) {
        if let Ok(pcm) = tk.dec.decode(None, fs) {
            tk.lane.push(idn_iem::RATE, &[pcm.to_vec()], &map);
        }
    }
    tk.seq = t.seq;
    tk.last = Instant::now();
    if let Ok(pcm) = tk.dec.decode(Some(&t.data), fs) {
        let pcm: Vec<f32> = pcm
            .iter()
            .map(|v| {
                if v.is_finite() {
                    v.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect();
        tk.lane.push(idn_iem::RATE, &[pcm], &map);
    }
    if t.flags & wire::TALK_LAST != 0 {
        // Released: the lane plays what it holds and goes at the next sweep after.
        tk.last = Instant::now() - TALK_IDLE + Duration::from_millis(100);
    }
}
