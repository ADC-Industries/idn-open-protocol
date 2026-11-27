//! The device on the network: a socket on each of its ports (§1.3) answering through
//! [`crate::arc`], [`crate::settings`] and [`crate::flowctl`], the heartbeat every second
//! (§4.8), notifications after changes (§4.7), and its mDNS ([`crate::mdns`]).
//!
//! What the controller changes comes out as [`Event`]s for the program (saving names
//! and subscriptions, starting and stopping flows' audio, subscribing).

use crate::arc::{self, Change};
use crate::device::{self, Device};
use crate::flowctl::FlowEvent;
use crate::mdns::{self, Ports, Responder};
use crate::settings::{self, Asked, Seq};
use idn_common::sync::LockOk as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What happened, for the program around the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A controller changed something (names, subscriptions, latency, multicast flows).
    Changed(Change),
    /// A receiver's flow-control request started, changed or stopped a unicast flow.
    Flow(FlowEvent),
}

/// What the heartbeat says that the device itself can't know (its clock, levels and
/// receive latency), filled by the program each second.
pub type BeatSource = Arc<dyn Fn() -> settings::Beat + Send + Sync>;

/// The clock status a settings request is answered with (§4.6): `None` while the clock
/// isn't running or its leader isn't known.
pub type ClockSource = Arc<dyn Fn() -> Option<(bool, i32, [u8; 8])> + Send + Sync>;

/// The running device. Dropping it stops its threads and says goodbye on mDNS.
pub struct Server {
    pub device: Arc<Mutex<Device>>,
    stop: Arc<AtomicBool>,
    mdns: Arc<Mutex<Option<Responder>>>,
    ports: Ports,
    notify: Arc<UdpSocket>,
    seq: Arc<Mutex<Seq>>,
    /// Its threads: a dead one is said ([`Server::dead`]), all waited for at shutdown so
    /// its ports are free when it returns (bug run I1-11).
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

fn bind(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    // Windows lets a socket on one address take a port another program holds on every
    // address (it would take that program's requests): so the port is tried on every
    // address first, and taken there by someone is "in use" as on Linux.
    #[cfg(windows)]
    if let Err(e) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        && e.kind() == io::ErrorKind::AddrInUse
    {
        return Err(in_use(port, e));
    }
    // Every address only when this one isn't the machine's (bug run K1-3: on Windows the
    // fallback after "in use" took the port beside the program holding it, and no request
    // came).
    let s = UdpSocket::bind((ip, port))
        .or_else(|e| {
            if e.kind() == io::ErrorKind::AddrNotAvailable {
                UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
            } else {
                Err(e)
            }
        })
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AddrInUse {
                in_use(port, e)
            } else {
                io::Error::new(e.kind(), format!("port {port}: {e}"))
            }
        })?;
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(s)
}

/// A device's port taken: most often another device program on the same computer.
fn in_use(port: u16, e: io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "port {port} is in use: another program on this computer is already a device on \
             that network (Dante Virtual Soundcard, Dante Via, or idn-aoip running already); \
             one per computer, or give this one other ports (--ports)"
        ),
    )
}

/// A socket for sending to the notification and heartbeat groups on `ip`'s interface.
fn sender(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let s = UdpSocket::bind((ip, 0))?;
    s.set_multicast_ttl_v4(1)?;
    // Out through `ip`'s card (Windows takes the default route's otherwise; I1-8).
    if !ip.is_unspecified() {
        let _ = socket2::SockRef::from(&s).set_multicast_if_v4(&ip);
    }
    Ok(s)
}

impl Server {
    /// Starts `device` on its interface address with `ports`, sending events to
    /// `events`; `beat` and `clock` say what the heartbeat and the clock status carry.
    pub fn start(
        device: Device,
        ports: Ports,
        events: Sender<Event>,
        beat: BeatSource,
        clock: ClockSource,
        version: (u8, u8, u16),
    ) -> io::Result<Server> {
        let ip = device.identity.ip;
        let arc_s = bind(ip, ports.arc)?;
        let cmc_s = bind(ip, ports.cmc)?;
        let flow_s = bind(ip, ports.flow)?;
        let set_s = bind(ip, ports.settings)?;
        let notify = Arc::new(sender(ip)?);
        let device = Arc::new(Mutex::new(device));
        let stop = Arc::new(AtomicBool::new(false));
        let seq = Arc::new(Mutex::new(Seq::default()));
        let host = mdns::host_label(&device.lock_ok().identity.name);
        let svc = mdns::services(&device.lock_ok(), ports, &[]);
        let responder = Responder::start(ip, host, svc)
            .map_err(|e| io::Error::new(e.kind(), format!("mDNS: {e}")))?;
        let mdns_r = Arc::new(Mutex::new(Some(responder)));
        let s = Server {
            device: device.clone(),
            stop: stop.clone(),
            mdns: mdns_r.clone(),
            ports,
            notify: notify.clone(),
            seq: seq.clone(),
            threads: Mutex::new(Vec::new()),
        };

        // ARC (§3).
        {
            let (dev, st, ev, n, sq, md) = (
                device.clone(),
                stop.clone(),
                events.clone(),
                notify.clone(),
                seq.clone(),
                mdns_r.clone(),
            );
            s.keep(spawn("idn-aoip-arc", move || {
                let mut buf = vec![0u8; 2048];
                let mut last_check: Option<Instant> = None;
                while !st.load(Ordering::Relaxed) {
                    let Ok((len, from)) = arc_s.recv_from(&mut buf) else {
                        continue;
                    };
                    // A name another device on the network has: refused, not taken (bug
                    // run K3-2: both answered for it, and the other device renamed itself;
                    // every subscription to it broke). Its own answer is from this address.
                    // (A valid name only, and one check a second at most: bug run M1-2, each
                    // cost this thread 600 ms and a stream of renames held every
                    // controller's requests. Past that, refused. A reset to the start name
                    // is checked too: M1-8.)
                    let asked = arc::rename_device_to(&buf[..len]).or_else(|| {
                        arc::is_rename_reset(&buf[..len])
                            .then(|| dev.lock_ok().identity.default_name.clone())
                    });
                    if let Some(name) = asked
                        && device::device_name_ok(&name)
                        && !dev.lock_ok().identity.name.eq_ignore_ascii_case(&name)
                        && (last_check
                            .is_some_and(|t: Instant| t.elapsed() < Duration::from_secs(1))
                            || {
                                last_check = Some(Instant::now());
                                mdns::resolve(
                                    ip,
                                    &name,
                                    "_netaudio-arc",
                                    Duration::from_millis(600),
                                )
                                .is_some_and(|r| r.addr != SocketAddr::from((ip, ports.arc)))
                            })
                    {
                        if let Some(r) = arc::refusal(&buf[..len]) {
                            let _ = arc_s.send_to(&r, from);
                        }
                        continue;
                    }
                    let a = arc::answer(&mut dev.lock_ok(), &buf[..len]);
                    if let Some(r) = a.reply {
                        let _ = arc_s.send_to(&r, from);
                    }
                    if a.changes.is_empty() {
                        continue;
                    }
                    let d = dev.lock_ok().clone();
                    let mut renamed_rx = Vec::new();
                    let mut notify = false;
                    let mut announce = false;
                    for c in a.changes {
                        match &c {
                            Change::RxName(ch) | Change::Subscription(ch) => {
                                renamed_rx.push(*ch);
                                notify = true;
                            }
                            // §4.7: after a TX rename too (controllers read the lists again
                            // on it; bug run I1-10). The mask names RX channels: none here.
                            Change::TxName(_) => {
                                notify = true;
                                announce = true;
                            }
                            Change::DeviceName(_) | Change::FlowDeleted(_) => announce = true,
                            // Announced by the program once its group is checked and held
                            // (§2.6; bug run I1-5: it was advertised before).
                            Change::FlowCreated(_) | Change::Latency(_) => {}
                        }
                        let _ = ev.send(Event::Changed(c));
                    }
                    if notify {
                        let content = settings::channels_changed(&renamed_rx);
                        let m = settings::message(
                            &d,
                            settings::SETTINGS,
                            sq.lock_ok().bump(),
                            settings::kind::CHANNELS_CHANGED,
                            &content,
                        );
                        let _ = n.send_to(
                            &m,
                            SocketAddrV4::new(settings::NOTIFY.0, settings::NOTIFY.1),
                        );
                    }
                    if announce {
                        // The device as it is once the responder is held (mdns, then the
                        // device): an older copy set after a newer one advertised the old
                        // name (bug run N2-3).
                        let held = md.lock_ok();
                        if let Some(r) = held.as_ref() {
                            let d = dev.lock_ok().clone();
                            r.set(
                                mdns::services(&d, ports, &bundles(&d)),
                                mdns::host_label(&d.identity.name),
                            );
                        }
                    }
                }
            })?);
        }

        // Flow control (§6).
        {
            let (dev, st, ev) = (device.clone(), stop.clone(), events.clone());
            s.keep(spawn("idn-aoip-flowctl", move || {
                let mut buf = vec![0u8; 2048];
                while !st.load(Ordering::Relaxed) {
                    let Ok((len, from)) = flow_s.recv_from(&mut buf) else {
                        continue;
                    };
                    let (reply, event) = {
                        let mut d = dev.lock_ok();
                        let (count, rate) = (d.tx.len(), d.format.rate);
                        let ip = match from.ip() {
                            std::net::IpAddr::V4(a) => Some(a),
                            std::net::IpAddr::V6(_) => None,
                        };
                        d.tx_flows.answer_from(&buf[..len], ip, count, rate)
                    };
                    if let Some(r) = reply {
                        let _ = flow_s.send_to(&r, from);
                    }
                    if let Some(e) = event {
                        let _ = ev.send(Event::Flow(e));
                    }
                }
            })?);
        }

        // Settings requests (§4.2): answered to the group and to the asker.
        {
            let (dev, st, n, sq, clk) = (
                device.clone(),
                stop.clone(),
                notify.clone(),
                seq.clone(),
                clock.clone(),
            );
            s.keep(spawn("idn-aoip-settings", move || {
                let mut buf = vec![0u8; 2048];
                let mut said: std::collections::HashMap<[u8; 8], Instant> = Default::default();
                while !st.load(Ordering::Relaxed) {
                    let Ok((len, from)) = set_s.recv_from(&mut buf) else {
                        continue;
                    };
                    let Some(asked) = settings::asked(&buf[..len]) else {
                        continue;
                    };
                    let d = dev.lock_ok().clone();
                    let (kind, content) = match asked {
                        Asked::Board => (settings::kind::BOARD, settings::board(&d, version)),
                        Asked::Product => (settings::kind::PRODUCT, settings::product(&d, version)),
                        Asked::Network => (settings::kind::NETWORK, settings::network(&d)),
                        Asked::ClearConfig => {
                            (settings::kind::CLEAR_CONFIG, settings::clear_config())
                        }
                        Asked::SampleRate => {
                            (settings::kind::SAMPLE_RATE, settings::sample_rate(&d))
                        }
                        Asked::Encoding => (settings::kind::ENCODING, settings::encoding(&d)),
                        Asked::Clock => match clk() {
                            Some((locked, ppb, leader)) => (
                                settings::kind::CLOCK,
                                settings::clock(&d, locked, ppb, leader),
                            ),
                            // No clock or no known leader: no reply (§4.6).
                            None => continue,
                        },
                    };
                    let m = settings::message(
                        &d,
                        settings::SETTINGS,
                        sq.lock_ok().bump(),
                        kind,
                        &content,
                    );
                    // To the group at most once a kind every 250 ms (bug run K1-4: each
                    // request of a stranger's flood was multicast); the asker always.
                    let now = Instant::now();
                    let to_group = said.get(&kind).is_none_or(|t: &Instant| {
                        now.duration_since(*t) >= Duration::from_millis(250)
                    });
                    if to_group {
                        said.insert(kind, now);
                        let _ = n.send_to(
                            &m,
                            SocketAddrV4::new(settings::NOTIFY.0, settings::NOTIFY.1),
                        );
                    }
                    // To the asker too for these, and for the others when the group's was
                    // held back (bug run L3-9: a second controller got nothing).
                    if !to_group
                        || matches!(
                            asked,
                            Asked::Board | Asked::Product | Asked::SampleRate | Asked::Encoding
                        )
                    {
                        let _ = set_s.send_to(&m, from);
                    }
                }
            })?);
        }

        // The sample-rate and encoding status, said once at the start (as netaudio's own
        // device does, §4.10): a controller has the current values before it asks.
        {
            let d = device.lock_ok().clone();
            for (kind, content) in [
                (settings::kind::SAMPLE_RATE, settings::sample_rate(&d)),
                (settings::kind::ENCODING, settings::encoding(&d)),
            ] {
                let m =
                    settings::message(&d, settings::SETTINGS, seq.lock_ok().bump(), kind, &content);
                let _ = notify.send_to(
                    &m,
                    SocketAddrV4::new(settings::NOTIFY.0, settings::NOTIFY.1),
                );
            }
        }

        // CMC registration (§4.9).
        {
            let (dev, st) = (device.clone(), stop.clone());
            let settings_port = ports.settings;
            s.keep(spawn("idn-aoip-cmc", move || {
                let mut buf = vec![0u8; 2048];
                while !st.load(Ordering::Relaxed) {
                    let Ok((len, from)) = cmc_s.recv_from(&mut buf) else {
                        continue;
                    };
                    let req = &buf[..len];
                    if !settings::is_cmc_registration(req) {
                        continue;
                    }
                    let Some(h) = arc::request_header(req) else {
                        continue;
                    };
                    let body = settings::cmc_registration(&dev.lock_ok(), settings_port);
                    let _ = cmc_s.send_to(&arc::reply(h, arc::OK, &body), from);
                }
            })?);
        }

        // At start: board and product info, once (§4.2); then the heartbeat each second.
        {
            let d = device.lock_ok().clone();
            for (kind, content) in [
                (settings::kind::BOARD, settings::board(&d, version)),
                (settings::kind::PRODUCT, settings::product(&d, version)),
            ] {
                let m =
                    settings::message(&d, settings::SETTINGS, seq.lock_ok().bump(), kind, &content);
                let _ = notify.send_to(
                    &m,
                    SocketAddrV4::new(settings::NOTIFY.0, settings::NOTIFY.1),
                );
            }
        }
        {
            let (dev, st, n, sq) = (device.clone(), stop.clone(), notify.clone(), seq.clone());
            s.keep(spawn("idn-aoip-heartbeat", move || {
                let mut next = Instant::now();
                while !st.load(Ordering::Relaxed) {
                    next += Duration::from_secs(1);
                    if let Some(w) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(w);
                    } else if Instant::now().duration_since(next) > Duration::from_secs(1) {
                        // A stall (a paused machine, sleep): on from now, not the missed
                        // ones back to back (bug run K1-10).
                        next = Instant::now();
                    }
                    let d = dev.lock_ok().clone();
                    let s = sq.lock_ok().bump();
                    let content = settings::heartbeat(s, &beat());
                    let m = settings::message(
                        &d,
                        settings::BEAT,
                        s,
                        settings::kind::HEARTBEAT,
                        &content,
                    );
                    let _ = n.send_to(
                        &m,
                        SocketAddrV4::new(settings::HEARTBEAT.0, settings::HEARTBEAT.1),
                    );
                }
            })?);
        }
        Ok(s)
    }

    /// Sends the channel-change notification for receive channels `changed` (a status
    /// moved, §6.7: the program's receiver calls this).
    pub fn channels_changed(&self, changed: &[usize]) {
        let d = self.device.lock_ok().clone();
        let content = settings::channels_changed(changed);
        let m = settings::message(
            &d,
            settings::SETTINGS,
            self.seq.lock_ok().bump(),
            settings::kind::CHANNELS_CHANGED,
            &content,
        );
        let _ = self.notify.send_to(
            &m,
            SocketAddrV4::new(settings::NOTIFY.0, settings::NOTIFY.1),
        );
    }

    /// The device's services again (after the program changed names or flows itself).
    pub fn announce(&self) {
        // The responder held first, then the device read (N2-3).
        let held = self.mdns.lock_ok();
        if let Some(r) = held.as_ref() {
            let d = self.device.lock_ok().clone();
            r.set(
                mdns::services(&d, self.ports, &bundles(&d)),
                mdns::host_label(&d.identity.name),
            );
        }
    }

    /// The responses its mDNS responder hears, for the network's device list in the same
    /// program (one socket on 5353 per process, bug run I3-12).
    pub fn mdns_heard(&self) -> Option<mdns::Heard> {
        self.mdns.lock_ok().as_ref().map(|r| r.tap())
    }

    /// Holds a multicast group for this device's flow (§2.6), or lets it go.
    pub fn reserve(&self, group: Ipv4Addr) {
        if let Some(r) = self.mdns.lock_ok().as_ref() {
            r.reserve(group);
        }
    }

    pub fn release(&self, group: Ipv4Addr) {
        if let Some(r) = self.mdns.lock_ok().as_ref() {
            r.release(group);
        }
    }

    /// Where a datagram from this device's flow sockets goes for a multicast flow.
    pub fn multicast_to(group: Ipv4Addr, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(group, port))
    }
}

impl Server {
    /// Stops answering and says goodbye on mDNS (the responder's drop): at a stop, while
    /// other parts of the program may still hold the server.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.mdns.lock_ok().take();
        // Each wakes within its read timeout (the heartbeat's second at most).
        let me = std::thread::current().id();
        for t in std::mem::take(&mut *self.threads.lock_ok()) {
            if t.thread().id() != me {
                let _ = t.join();
            }
        }
    }

    /// A thread of its that ended while it runs (a panic): its name.
    pub fn dead(&self) -> Option<String> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        self.threads
            .lock_ok()
            .iter()
            .find(|t| t.is_finished())
            .map(|t| t.thread().name().unwrap_or("?").to_owned())
    }

    fn keep(&self, t: std::thread::JoinHandle<()>) {
        self.threads.lock_ok().push(t);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Each transmit channel's multicast flow and position (§2.4's `b.<flow>=<position>`).
fn bundles(d: &Device) -> Vec<(usize, u16, usize)> {
    let mut v = Vec::new();
    for (i, f) in d.tx_flows.flows.iter().enumerate() {
        if let Some(f) = f.as_ref().filter(|f| f.multicast && f.held) {
            for (pos, &ch) in f.channels.iter().enumerate() {
                if ch > 0 {
                    v.push((usize::from(ch), (i + 1) as u16, pos + 1));
                }
            }
        }
    }
    v
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn(f)
}
