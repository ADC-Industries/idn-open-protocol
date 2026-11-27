//! The receive side (idn-aoip-device.md §6.5–§7): the receive channels' subscriptions
//! turned into flows. Each pass (every 2 s, or at once when a subscription changes) looks
//! the transmitters' channels up on mDNS, plans with [`idn_aoip_device::receiver::plan`],
//! sets up, joins and stops flows, drops lost ones, and sets each channel's status (a
//! notification when one moves).

use idn_aoip_device::device::Device;
use idn_aoip_device::engine::{self, Engine, RxRun};
use idn_aoip_device::flowctl::{self, Setup};
use idn_aoip_device::flows::RxFlow;
use idn_aoip_device::mdns;
use idn_aoip_device::receiver::{
    self, Action, BundleInfo, ChanInfo, Flow, Kind, RESOLVE_EVERY, Want,
};
use idn_aoip_device::server::Server;
use idn_common::sync::LockOk as _;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// The receive ports advertised (idn-aoip-device.md §5.7, 0x3300: 14336–14591): unicast
/// flows bind inside them.
const RX_PORTS: std::ops::RangeInclusive<u16> = 14336..=14591;

/// A line about subscribing, stamped with the wall-clock time in ms (lab 530(b): to time
/// each step against a capture).
fn log(line: &str) {
    let ms = idn_common::epoch::millis();
    println!("subscribe [{}.{:03}]: {line}", ms / 1000, ms % 1000);
}

/// When a lookup that found nothing is tried again: after 1, 2, 4 s, then every 9 s.
fn retry_after(misses: u32) -> Duration {
    if misses >= 4 {
        RESOLVE_EVERY
    } else {
        Duration::from_secs(1u64 << misses.saturating_sub(1))
    }
}

/// A socket for a unicast receive flow: a port in the advertised ranges, else any.
/// Its keepalives go to the sender from it, so it is kept from reading a gone sender's
/// "port unreachable" as errors on Windows ([`engine::no_udp_resets`]).
fn rx_socket(ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let s = RX_PORTS
        .into_iter()
        .find_map(|p| UdpSocket::bind((ip, p)).ok())
        .map_or_else(|| UdpSocket::bind((ip, 0)), Ok)?;
    engine::no_udp_resets(&s);
    Ok(s)
}

/// A socket joined to a multicast group on the interface.
fn group_socket(ip: Ipv4Addr, group: Ipv4Addr, port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    // Only this socket's own group: every multicast flow is on the media port, and Linux
    // otherwise hands a socket every group any socket on the machine joined (bug run
    // I2-1: two joined flows took each other's packets). Windows filters already.
    #[cfg(target_os = "linux")]
    s.set_multicast_all_v4(false)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    s.join_multicast_v4(&group, &ip)?;
    Ok(s.into())
}

/// One flow-control request from a fresh socket, its reply within 3 s (§6.1).
fn ask(ip: Ipv4Addr, to: SocketAddr, req: &[u8], opcode: u16) -> Option<(u16, Vec<u8>)> {
    let s = UdpSocket::bind((ip, 0)).ok()?;
    s.send_to(req, to).ok()?;
    let seq = u16::from_be_bytes([req[4], req[5]]);
    let until = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 2048];
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        s.set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .ok()?;
        let (n, _) = s.recv_from(&mut buf).ok()?;
        if let Some((r, body)) = flowctl::parse_reply(&buf[..n], seq, opcode) {
            return Some((r, body.to_vec()));
        }
    }
    None
}

/// The receive side's state.
pub struct Subscriber {
    pub server: Arc<Server>,
    pub engine: Arc<Engine>,
    pub ip: Ipv4Addr,
    pub stop: Arc<AtomicBool>,
}

impl Subscriber {
    /// Runs until `stop`; `wake` hurries the next pass (a subscription changed).
    pub fn run(self, wake: Receiver<()>) {
        let mut flows: BTreeMap<u16, Flow> = BTreeMap::new();
        // Each looked-up channel: when, what was found, and how many misses in a row (a miss
        // is tried again after 1, 2, 4 s, then every 9 s: lab 529(c), a first miss held a new
        // subscription 9 s).
        let mut resolved: BTreeMap<(String, String), (Instant, Option<ChanInfo>, u32)> =
            BTreeMap::new();
        let mut pass_every = Duration::from_secs(2);
        let mut failed: BTreeMap<usize, u16> = BTreeMap::new();
        // What each channel was subscribed to when its failure was kept: a new
        // subscription starts clean (bug run J3-2).
        let mut failed_for: BTreeMap<usize, (String, String)> = BTreeMap::new();
        let mut seq = 0u16;
        let own_name = |s: &Server| s.device.lock_ok().identity.name.clone();
        while !self.stop.load(Ordering::Relaxed) {
            let _ = wake.recv_timeout(pass_every);
            let now = Instant::now();
            let dev: Device = self.server.device.lock_ok().clone();
            let me = own_name(&self.server);
            // Packets seen: the flows' last packet from the engine.
            {
                let rx = self.engine.rx.lock_ok();
                for f in flows.values_mut() {
                    if let Some(run) = rx.get(&f.number) {
                        f.last_packet = *run.last_packet.lock_ok();
                    }
                }
            }
            // Lost flows dropped; their channels resolved again (§6.7).
            let lost: Vec<u16> = flows
                .values()
                .filter(|f| f.lost(now))
                .map(|f| f.number)
                .collect();
            for n in lost {
                if let Some(f) = flows.remove(&n) {
                    self.engine.rx.lock_ok().remove(&n);
                    resolved.retain(|(d, _), _| *d != f.device);
                }
            }
            // The wants: each subscribed channel; those not yet found (or not tried for
            // 9 s) looked up together.
            let mut keys: Vec<(usize, String, String)> = Vec::new();
            for (i, c) in dev.rx.iter().enumerate() {
                let Some(sub) = &c.subscription else {
                    failed.remove(&(i + 1));
                    continue;
                };
                let device = if sub.device == "." {
                    me.clone()
                } else {
                    sub.device.clone()
                };
                let now_for = (device.clone(), sub.channel.clone());
                if failed_for.get(&(i + 1)).is_some_and(|f| *f != now_for) {
                    failed.remove(&(i + 1));
                }
                failed_for.insert(i + 1, now_for);
                keys.push((i + 1, device, sub.channel.clone()));
            }
            // Lookups of what nothing is subscribed to now forgotten (bug run M2-2: an old
            // one was used again on a resubscription, the wrong channel after renames).
            resolved.retain(|(d, c), _| keys.iter().any(|(_, kd, kc)| kd == d && kc == c));
            let mut lookups: Vec<(String, String)> = Vec::new();
            for (_, device, channel) in &keys {
                let key = (device.clone(), channel.clone());
                // Found and working: kept. Found but its set-up failed (misses set): looked
                // up again after the wait, as one not found (bug run I2-10: a transmitter
                // that moved stayed failed until a restart).
                let fresh = resolved.get(&key).is_some_and(|(at, info, misses)| {
                    (info.is_some() && *misses == 0)
                        || now.duration_since(*at) < retry_after(*misses)
                });
                let instance = format!("{channel}@{device}");
                if !fresh && !lookups.iter().any(|(i, _)| *i == instance) {
                    lookups.push((instance, "_netaudio-chan".into()));
                }
            }
            if !lookups.is_empty() {
                let began = Instant::now();
                let found = mdns::resolve_many(self.ip, &lookups, Duration::from_millis(600));
                let n_found = found.iter().filter(|r| r.is_some()).count();
                // Said when something was found or looked up for the first time (not each
                // 9 s retry of a channel that isn't there).
                let first = lookups.iter().any(|(i, _)| {
                    i.rsplit_once('@')
                        .is_some_and(|(c, d)| !resolved.contains_key(&(d.to_owned(), c.to_owned())))
                });
                if n_found > 0 || first {
                    log(&format!(
                        "looked up {} channel(s) in {} ms: {n_found} found",
                        lookups.len(),
                        began.elapsed().as_millis()
                    ));
                }
                for ((instance, _), r) in lookups.iter().zip(found) {
                    let Some((channel, device)) = instance.rsplit_once('@') else {
                        continue;
                    };
                    let info = r.and_then(|r| ChanInfo::from_txt(r.addr, &r.txt));
                    let key = (device.to_owned(), channel.to_owned());
                    let misses = match (&info, resolved.get(&key)) {
                        (Some(_), _) => 0,
                        (None, Some((_, _, m))) => m + 1,
                        (None, None) => 1,
                    };
                    resolved.insert(key, (now, info, misses));
                }
            }
            // Each receive channel's (device, channel), for a failed set-up's own retries.
            let key_of: BTreeMap<usize, (String, String)> = keys
                .iter()
                .map(|(rx, d, c)| (*rx, (d.clone(), c.clone())))
                .collect();
            let wants: Vec<Want> = keys
                .into_iter()
                .map(|(rx, device, channel)| {
                    // A set-up refused or not answered (misses set, info kept): asked again
                    // only after its wait, not every pass (bug run K2-2: every 2 s, 3 s
                    // waits holding the other channels).
                    let mut info = resolved
                        .get(&(device.clone(), channel.clone()))
                        .filter(|(at, _, misses)| {
                            *misses == 0 || now.duration_since(*at) >= retry_after(*misses)
                        })
                        .and_then(|(_, i, _)| i.clone());
                    // A source at another rate: no flow asked for, its status "formats do not
                    // match" (idn-aoip-device.md §1.5); set up again once the rates agree.
                    let own = dev.format.rate;
                    match info
                        .as_ref()
                        .and_then(|i| i.rate.map(|r| (r, i.bundle.is_some())))
                    {
                        Some((r, multicast)) if r != own => {
                            failed.insert(
                                rx,
                                if multicast {
                                    receiver::FORMAT_MULTICAST
                                } else {
                                    receiver::FORMAT_UNICAST
                                },
                            );
                            // Looked up again every 9 s (its rate may change: bug
                            // run J3-1, it stayed "formats don't match" for good).
                            if let Some(e) = resolved.get_mut(&(device.clone(), channel.clone())) {
                                e.0 = now;
                                e.2 = e.2.max(4);
                            }
                            info = None;
                        }
                        // Cleared only once a lookup says the rates agree: while one waits
                        // (no info) it stays "formats don't match" (lab Alpha84 535: it
                        // went to Unresolved 8 s of every 10, since bug run K2-2).
                        _ if info.is_some()
                            && failed.get(&rx).is_some_and(|&c| {
                                c == receiver::FORMAT_UNICAST || c == receiver::FORMAT_MULTICAST
                            }) =>
                        {
                            failed.remove(&rx);
                        }
                        _ => {}
                    }
                    Want { rx, device, info }
                })
                .collect();
            let current: Vec<Flow> = flows.values().cloned().collect();
            let p = receiver::plan(&wants, &current, dev.rx.len(), dev.active_latency_ns);
            for a in &p.actions {
                match a {
                    Action::Stop { number } => {
                        log(&format!("flow {number}: stopped (nothing takes it)"));
                        if let Some(f) = flows.remove(number) {
                            self.engine.rx.lock_ok().remove(number);
                            if let Kind::Unicast {
                                flowctl,
                                handle: Some(h),
                            } = f.kind
                            {
                                seq = seq.wrapping_add(1);
                                let _ = ask(
                                    self.ip,
                                    flowctl,
                                    &flowctl::stop_request(seq, h),
                                    flowctl::op::STOP,
                                );
                            }
                        }
                    }
                    Action::Setup {
                        number,
                        device,
                        flowctl,
                        slots,
                        bits,
                        fpp,
                        latency_ns,
                    } => {
                        let Ok(sock) = rx_socket(self.ip) else {
                            continue;
                        };
                        let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
                        seq = seq.wrapping_add(1);
                        let req = flowctl::setup_request(
                            seq,
                            &Setup {
                                device: me.clone(),
                                flow_name: flowctl::flow_name(
                                    usize::from(*number),
                                    dev.identity.process_id,
                                ),
                                rate: dev.format.rate,
                                bits: u32::from(*bits),
                                channels: slots.clone(),
                                fpp: *fpp,
                                port,
                                ip: self.ip,
                            },
                        );
                        let rxs: Vec<usize> = p
                            .slots
                            .iter()
                            .filter(|(_, (n, _))| n == number)
                            .map(|(rx, _)| *rx)
                            .collect();
                        let began = Instant::now();
                        let answer = ask(self.ip, *flowctl, &req, flowctl::op::SETUP);
                        log(&format!(
                            "flow {number} from {device} ({} channel(s), port {port}): {} in {} ms",
                            slots.len(),
                            match &answer {
                                Some((1, _)) => "set up".to_owned(),
                                Some((code, _)) => format!("refused ({code:#06x})"),
                                None => "no answer".to_owned(),
                            },
                            began.elapsed().as_millis()
                        ));
                        match answer {
                            Some((1, body)) if body.len() >= 6 => {
                                let handle: flowctl::Handle =
                                    body[..6].try_into().unwrap_or_default();
                                let latency = u64::from(*latency_ns) * u64::from(dev.format.rate)
                                    / 1_000_000_000;
                                let mut map = vec![Vec::new(); slots.len()];
                                for (rx, (n, s)) in &p.slots {
                                    if n == number && *s < map.len() {
                                        map[*s].push(*rx);
                                    }
                                }
                                // Only the transmitter's packets (bug run N1-1).
                                let mut run =
                                    RxRun::new(sock, map, usize::from(*bits / 8), latency, false);
                                run.source = Some(flowctl.ip());
                                self.engine.rx.lock_ok().insert(*number, run);
                                flows.insert(
                                    *number,
                                    Flow {
                                        number: *number,
                                        device: device.clone(),
                                        kind: Kind::Unicast {
                                            flowctl: *flowctl,
                                            handle: Some(handle),
                                        },
                                        slots: slots.clone(),
                                        bits: *bits,
                                        fpp: *fpp,
                                        latency_ns: *latency_ns,
                                        created: now,
                                        last_packet: None,
                                    },
                                );
                                for rx in rxs {
                                    failed.remove(&rx);
                                }
                            }
                            other => {
                                let code = other.map_or(0, |(r, _)| r);
                                for rx in &rxs {
                                    failed.insert(*rx, code);
                                }
                                // The failed set-up's own channels wait to be tried again,
                                // not the device's others (bug run L3-1: those in a working
                                // flow lost their info, and the flow was stopped).
                                for rx in &rxs {
                                    if let Some(e) =
                                        key_of.get(rx).and_then(|k| resolved.get_mut(k))
                                    {
                                        e.0 = now;
                                        e.2 = e.2.max(4);
                                    }
                                }
                            }
                        }
                    }
                    Action::Join {
                        number,
                        device,
                        bundle,
                        slots,
                    } => {
                        // A join that can't be made: its channels wait before the next try
                        // (bug run M2-4: 600 ms and a log line every pass).
                        let mut wait = |p: &receiver::Plan| {
                            for (rx, (n, _)) in &p.slots {
                                if n == number
                                    && let Some(e) =
                                        key_of.get(rx).and_then(|k| resolved.get_mut(k))
                                {
                                    e.0 = now;
                                    e.2 = e.2.max(4);
                                }
                            }
                        };
                        let Some(b) = mdns::resolve(
                            self.ip,
                            &format!("{bundle}@{device}"),
                            "_netaudio-bund",
                            Duration::from_millis(600),
                        )
                        .and_then(|r| BundleInfo::from_txt(&r.txt)) else {
                            log(&format!(
                                "flow {number}: multicast flow {bundle}@{device} not found"
                            ));
                            wait(&p);
                            continue;
                        };
                        // The flow at another rate (its own TXT says, where the channel's
                        // didn't): not joined, "formats don't match" (bug run J3-5).
                        if b.rate.is_some_and(|r| r != dev.format.rate) {
                            for (rx, (n, _)) in &p.slots {
                                if n == number {
                                    failed.insert(*rx, receiver::FORMAT_MULTICAST);
                                }
                            }
                            wait(&p);
                            continue;
                        }
                        log(&format!(
                            "flow {number}: joining multicast flow {bundle}@{device} at {}:{}",
                            b.group, b.port
                        ));
                        let Ok(sock) = group_socket(self.ip, b.group, b.port) else {
                            wait(&p);
                            continue;
                        };
                        let latency = u64::from(flowctl::flow_latency_ns(
                            b.latency_ns,
                            dev.active_latency_ns,
                        )) * u64::from(dev.format.rate)
                            / 1_000_000_000;
                        // Sized as the flow's own TXT says (bug run K2-1: the channel's
                        // `nchan` is min(8, its device's channels), a 2-channel flow of a
                        // 64-channel device was read as 8 channels, garbled); a position
                        // past it fails.
                        let _ = slots;
                        let mut map = vec![Vec::new(); b.nchan];
                        for (rx, (n, s)) in &p.slots {
                            if n == number {
                                match map.get_mut(*s) {
                                    Some(m) => {
                                        m.push(*rx);
                                        if failed.get(rx) == Some(&receiver::BUNDLE_POSITION) {
                                            failed.remove(rx);
                                        }
                                    }
                                    None => {
                                        failed.insert(*rx, receiver::BUNDLE_POSITION);
                                        // Looked up again after the wait (its TXT may be
                                        // stale; bug run L3-8: failed for good).
                                        if let Some(e) =
                                            key_of.get(rx).and_then(|k| resolved.get_mut(k))
                                        {
                                            e.0 = now;
                                            e.2 = e.2.max(4);
                                        }
                                    }
                                }
                            }
                        }
                        self.engine.rx.lock_ok().insert(
                            *number,
                            RxRun::new(sock, map, usize::from(b.bits / 8), latency, true),
                        );
                        flows.insert(
                            *number,
                            Flow {
                                number: *number,
                                device: device.clone(),
                                kind: Kind::Multicast {
                                    group: b.group,
                                    port: b.port,
                                    bundle: *bundle,
                                },
                                slots: vec![0; b.nchan],
                                bits: b.bits,
                                fpp: b.fpp,
                                // What it plays at, as a set-up flow's (bug run N5-12: the
                                // transmitter's shown).
                                latency_ns: flowctl::flow_latency_ns(
                                    b.latency_ns,
                                    dev.active_latency_ns,
                                ),
                                created: now,
                                last_packet: None,
                            },
                        );
                    }
                }
            }
            // Statuses (§7.2), a notification for those that moved; the receive flow page.
            let mut moved = Vec::new();
            let mut rx_flows = Vec::new();
            for f in flows.values() {
                let mut slots: Vec<Vec<usize>> = vec![Vec::new(); f.slots.len().max(1)];
                for (rx, (n, s)) in &p.slots {
                    if *n == f.number && *s < slots.len() {
                        slots[*s].push(*rx);
                    }
                }
                // Channels that came to share a slot since the flow began (§7.3).
                if let Some(run) = self.engine.rx.lock_ok().get_mut(&f.number) {
                    run.slots = slots.clone();
                }
                let (port, group) = match &f.kind {
                    Kind::Multicast { group, port, .. } => (*port, Some(*group)),
                    Kind::Unicast { .. } => (
                        self.engine
                            .rx
                            .lock_ok()
                            .get(&f.number)
                            .and_then(|r| r.socket.local_addr().ok())
                            .map_or(0, |a| a.port()),
                        None,
                    ),
                };
                rx_flows.push(RxFlow {
                    number: f.number,
                    port,
                    group,
                    slots,
                    status: if f.receiving(now) { 9 } else { 8 },
                    latency_ns: f.latency_ns,
                });
            }
            {
                let mut d = self.server.device.lock_ok();
                for i in 0..d.rx.len() {
                    let subscribed = d.rx[i].subscription.is_some();
                    let flow = p.slots.get(&(i + 1)).and_then(|(n, _)| flows.get(n));
                    let s =
                        receiver::status_of(subscribed, flow, failed.get(&(i + 1)).copied(), now);
                    if d.rx[i].status != s {
                        d.rx[i].status = s;
                        moved.push(i + 1);
                    }
                }
                d.rx_flows = rx_flows;
            }
            if !moved.is_empty() {
                self.server.channels_changed(&moved);
                let d = self.server.device.lock_ok();
                use idn_aoip_device::device::status;
                let count = |f: &dyn Fn(u32) -> bool| {
                    d.rx.iter()
                        .filter(|c| c.subscription.is_some() && f(c.status))
                        .count()
                };
                // (Counted by status: a failure's code is above "in progress" too, and
                // was counted as receiving.)
                let receiving = count(&|s| s == status::UNICAST || s == status::MULTICAST);
                let waiting = count(&|s| s == status::UNRESOLVED || s == status::IN_PROGRESS);
                let format = count(&|s| s == status::CHANNEL_FORMAT || s == status::BUNDLE_FORMAT);
                let failed = count(&|s| s == status::FAILED || s == status::NO_FLOWS);
                // Not held while it logs (bug run N2-2: a stalled log pipe stalled the device).
                drop(d);
                log(&format!(
                    "{} channel(s) changed status: {receiving} receiving, {waiting} waiting{}{}",
                    moved.len(),
                    if format > 0 {
                        format!(", {format} at another sample rate (formats don't match)")
                    } else {
                        String::new()
                    },
                    if failed > 0 {
                        format!(", {failed} failed")
                    } else {
                        String::new()
                    },
                ));
            }
            // Passes each second while a recent miss is to be tried again soon, and each
            // 250 ms while a new flow waits for its first packet (its channels then show
            // receiving at once, not at the next 2 s pass: lab 530(b)).
            pass_every = if flows.values().any(|f| {
                f.last_packet.is_none()
                    && Instant::now().duration_since(f.created) < Duration::from_secs(5)
            }) {
                Duration::from_millis(250)
            } else if resolved.values().any(|(_, i, m)| i.is_none() && *m < 4) {
                Duration::from_secs(1)
            } else {
                Duration::from_secs(2)
            };
        }
    }
}
