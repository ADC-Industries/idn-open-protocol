//! mDNS responder that publishes NDI sources (spec §5.1.2) and a host listing server
//! (§5.2).

use super::dns::{
    self, CLASS_IN, CLASS_TOP_BIT, Name, RData, Record, TYPE_A, TYPE_ANY, TYPE_PTR, TYPE_SRV,
    TYPE_TXT, name, name_eq,
};
use super::{MDNS_GROUP, MDNS_PORT, SERVICE, udp_socket};
use idn_common::sync::LockOk as _;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A source to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// Full NDI name, `HOST (source name)`.
    pub name: String,
    pub port: u16,
    pub groups: Vec<String>,
    pub routing: bool,
    /// IDN extension list for the discovery hint (§E.2.2); `None` for plain NDI sources.
    pub idn_ext: Option<String>,
}

/// Host details for the A and SRV records.
#[derive(Debug, Clone)]
pub struct HostInfo {
    /// mDNS host name without `.local`.
    pub host: String,
    /// The address published (with [`all_interfaces`](HostInfo::all_interfaces): the
    /// main one, used where no interface applies).
    pub addr: Ipv4Addr,
    pub listing_port: u16,
    /// Publish on every multicast-capable interface, each with its own address (a server
    /// on a church LAN and a dedicated video network is found on both). Set by
    /// [`HostInfo::local`] when no interface is given; otherwise only `addr`'s.
    pub all_interfaces: bool,
}

impl HostInfo {
    /// This machine's host name and the address of the interface that routes to the
    /// mDNS group (or `interface`, if given).
    pub fn local(interface: Option<Ipv4Addr>) -> io::Result<Self> {
        // Kept in the case the system has it (mDNS host names are case-insensitive).
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| super::hostname().to_lowercase());
        // Prefer the interface that routes to the mDNS group; without such a route, the
        // first multicast-capable interface.
        let routed = || -> io::Result<Ipv4Addr> {
            idn_common::net::local_v4_towards(MDNS_GROUP)
                .ok_or_else(|| io::Error::other("no IPv4 route"))
        };
        let addr = match interface {
            Some(a) => a,
            None => match routed() {
                Ok(a) => a,
                Err(_) => super::ifaces::ipv4_multicast_interfaces()
                    .first()
                    .map(|i| i.addr)
                    .ok_or_else(|| {
                        io::Error::other("no multicast-capable IPv4 interface; use --iface")
                    })?,
            },
        };
        Ok(HostInfo {
            host,
            addr,
            listing_port: super::LISTING_PORT,
            all_interfaces: interface.is_none(),
        })
    }
}

/// The records of `sources`, with `addr` as the host's address.
fn records(host: &HostInfo, addr: Ipv4Addr, sources: &[Published], ttl_scale: u32) -> Vec<Record> {
    let service = name(SERVICE);
    let target = name(&format!("{}.local", host.host));
    let mut out = Vec::new();
    for s in sources {
        let mut inst = vec![s.name.clone()];
        inst.extend(service.iter().cloned());
        let mut txt = vec![
            format!("groups={}", s.groups.join(",")).into_bytes(),
            format!("discovery={}", host.listing_port).into_bytes(),
        ];
        if s.routing {
            txt.push(b"routing".to_vec());
        }
        if let Some(l) = &s.idn_ext {
            txt.push(crate::ext::hint(l).into_bytes());
        }
        out.push(Record {
            name: service.clone(),
            rtype: TYPE_PTR,
            class: CLASS_IN,
            ttl: 4500 * ttl_scale,
            data: RData::Ptr(inst.clone()),
        });
        out.push(Record {
            name: inst.clone(),
            rtype: TYPE_SRV,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 120 * ttl_scale,
            data: RData::Srv {
                priority: 0,
                weight: 0,
                port: s.port,
                target: target.clone(),
            },
        });
        out.push(Record {
            name: inst,
            rtype: TYPE_TXT,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 4500 * ttl_scale,
            data: RData::Txt(txt),
        });
    }
    // The host A record is usually also owned by the system's mDNS responder, so it is
    // published as a shared record (no cache-flush bit, which would evict the host's other
    // addresses) and never withdrawn by a goodbye.
    if !sources.is_empty() && ttl_scale != 0 {
        out.push(Record {
            name: target,
            rtype: TYPE_A,
            class: CLASS_IN,
            ttl: 120,
            data: RData::A(addr),
        });
    }
    out
}

/// Listing-port connections kept open at once; the oldest is closed beyond that.
const MAX_LISTING_OPEN: usize = 64;

/// Records answering one question: (answers, additional).
fn answer(all: &[Record], q: &Name, qtype: u16) -> (Vec<Record>, Vec<Record>) {
    let matches = |r: &Record| name_eq(&r.name, q) && (qtype == TYPE_ANY || qtype == r.rtype);
    let ans: Vec<Record> = all.iter().filter(|r| matches(r)).cloned().collect();
    // For a PTR answer, add the instances' SRV/TXT and the host's A (RFC 6763 §12).
    let add = if ans.iter().any(|r| r.rtype == TYPE_PTR) {
        all.iter()
            .filter(|r| r.rtype != TYPE_PTR)
            .cloned()
            .collect()
    } else if ans.iter().any(|r| r.rtype == TYPE_SRV) {
        all.iter().filter(|r| r.rtype == TYPE_A).cloned().collect()
    } else {
        Vec::new()
    };
    (ans, add)
}

/// Largest mDNS packet sent: within a 1500-byte Ethernet MTU, so never fragmented (and
/// far below the 9000 bytes of RFC 6762 §17).
const MAX_PACKET: usize = 1400;

/// Builds one DNS message from (answers, additional).
type Encoder<'a> = &'a dyn Fn(&[Record], &[Record]) -> Vec<u8>;

/// Splits a response into packets of at most [`MAX_PACKET`] bytes. A source's records
/// stay together (its PTR, SRV and TXT); the host's A record goes in every packet.
fn packets(ans: &[Record], add: &[Record], encode: Encoder) -> Vec<Vec<u8>> {
    // The source a record belongs to: its instance name (a PTR's target). None: A.
    let key = |r: &Record| match &r.data {
        RData::Ptr(t) => Some(t.join(".").to_lowercase()),
        RData::A(_) => None,
        _ => Some(r.name.join(".").to_lowercase()),
    };
    let (a_ans, a_add): (Vec<Record>, Vec<Record>) = (
        ans.iter().filter(|r| key(r).is_none()).cloned().collect(),
        add.iter().filter(|r| key(r).is_none()).cloned().collect(),
    );
    let mut groups: Vec<(String, Vec<Record>, Vec<Record>)> = Vec::new();
    for (r, in_ans) in ans
        .iter()
        .map(|r| (r, true))
        .chain(add.iter().map(|r| (r, false)))
    {
        let Some(k) = key(r) else { continue };
        let i = match groups.iter().position(|g| g.0 == k) {
            Some(i) => i,
            None => {
                groups.push((k, Vec::new(), Vec::new()));
                groups.len() - 1
            }
        };
        if in_ans {
            groups[i].1.push(r.clone());
        } else {
            groups[i].2.push(r.clone());
        }
    }
    let build = |ga: &[Record], gd: &[Record]| {
        let ans: Vec<Record> = ga.iter().chain(&a_ans).cloned().collect();
        let add: Vec<Record> = gd.iter().chain(&a_add).cloned().collect();
        encode(&ans, &add)
    };
    let mut out = Vec::new();
    let (mut cur_a, mut cur_d): (Vec<Record>, Vec<Record>) = (Vec::new(), Vec::new());
    for (_, ga, gd) in groups {
        let (mut try_a, mut try_d) = (cur_a.clone(), cur_d.clone());
        try_a.extend(ga.iter().cloned());
        try_d.extend(gd.iter().cloned());
        if (!cur_a.is_empty() || !cur_d.is_empty()) && build(&try_a, &try_d).len() > MAX_PACKET {
            out.push(build(&cur_a, &cur_d));
            (cur_a, cur_d) = (ga, gd);
        } else {
            (cur_a, cur_d) = (try_a, try_d);
        }
    }
    if !cur_a.is_empty() || !cur_d.is_empty() || !a_ans.is_empty() {
        out.push(build(&cur_a, &cur_d));
    }
    out
}

/// Where a responder publishes: its socket and interfaces.
struct Net {
    sock: UdpSocket,
    host: HostInfo,
    /// The interfaces published on: each answers with its own address.
    ifaces: Mutex<Vec<super::ifaces::Interface>>,
    /// Held from choosing an interface to sending on it.
    send: Mutex<()>,
}

impl Net {
    fn group() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT))
    }

    /// The interfaces to publish on now.
    fn wanted(host: &HostInfo) -> Vec<super::ifaces::Interface> {
        let fixed = || {
            vec![super::ifaces::Interface {
                name: String::new(),
                addr: host.addr,
                netmask: Ipv4Addr::BROADCAST,
                loopback: host.addr.is_loopback(),
            }]
        };
        if !host.all_interfaces {
            return fixed();
        }
        let v = super::ifaces::ipv4_multicast_interfaces();
        if v.is_empty() { fixed() } else { v }
    }

    /// Joins interfaces that came up and leaves those gone; returns the new ones.
    fn rescan(&self) -> Vec<Ipv4Addr> {
        let now = Self::wanted(&self.host);
        let mut cur = self.ifaces.lock_ok();
        let added: Vec<Ipv4Addr> = now
            .iter()
            .map(|i| i.addr)
            .filter(|a| !cur.iter().any(|c| c.addr == *a))
            .filter(|a| self.sock.join_multicast_v4(&MDNS_GROUP, a).is_ok())
            .collect();
        for c in cur.iter().filter(|c| !now.iter().any(|n| n.addr == c.addr)) {
            let _ = self.sock.leave_multicast_v4(&MDNS_GROUP, &c.addr);
        }
        *cur = now
            .into_iter()
            .filter(|n| added.contains(&n.addr) || cur.iter().any(|c| c.addr == n.addr))
            .collect();
        added
    }

    /// The interface a querier at `ip` is on (the main one if none is).
    fn iface_for(&self, ip: Ipv4Addr) -> Ipv4Addr {
        let cur = self.ifaces.lock_ok();
        cur.iter()
            .find(|i| i.contains(ip))
            .or(cur.iter().find(|i| i.addr == self.host.addr))
            .or(cur.first())
            .map_or(self.host.addr, |i| i.addr)
    }

    /// Sends `pkts` to `to`; multicast goes out on `iface`.
    fn send_on(&self, iface: Ipv4Addr, pkts: &[Vec<u8>], to: SocketAddr) {
        let _g = self.send.lock_ok();
        if to == Self::group() {
            let _ = socket2::SockRef::from(&self.sock).set_multicast_if_v4(&iface);
        }
        for p in pkts {
            let _ = self.sock.send_to(p, to);
        }
    }

    /// Multicasts `sources`' records on every interface (or only `only`), each with that
    /// interface's address.
    fn announce(&self, sources: &[Published], ttl_scale: u32, only: Option<&[Ipv4Addr]>) {
        let ifaces: Vec<Ipv4Addr> = self.ifaces.lock_ok().iter().map(|i| i.addr).collect();
        for i in ifaces
            .into_iter()
            .filter(|i| only.is_none_or(|o| o.contains(i)))
        {
            let recs = records(&self.host, i, sources, ttl_scale);
            let pkts = packets(&recs, &[], &|a, d| dns::encode_response(0, a, d));
            self.send_on(i, &pkts, Self::group());
        }
    }
}

/// How often a responder on every interface looks for new ones.
const IFACE_SCAN: Duration = Duration::from_secs(5);

/// Publishes sources on mDNS and serves the listing port until dropped.
pub struct Responder {
    stop: Arc<AtomicBool>,
    sources: Arc<Mutex<Vec<Published>>>,
    threads: Vec<JoinHandle<()>>,
    net: Arc<Net>,
    _listing: Option<Listing>,
    /// Goodbyes on SIGINT/SIGTERM (§5.1.2), when `shutdown::install` is in effect.
    _on_signal: crate::shutdown::Registration,
}

impl Responder {
    /// Starts publishing. `serve_listing` also listens on `host.listing_port` (§5.2).
    pub fn start(host: HostInfo, sources: Vec<Published>, serve_listing: bool) -> io::Result<Self> {
        let sock = udp_socket(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT),
            true,
            host.addr,
        )?;
        let net = Arc::new(Net {
            sock,
            host: host.clone(),
            ifaces: Mutex::new(Vec::new()),
            send: Mutex::new(()),
        });
        if net.rescan().is_empty() {
            // Not even the main interface could be joined: report why.
            net.sock.join_multicast_v4(&MDNS_GROUP, &host.addr)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let sources = Arc::new(Mutex::new(sources));
        let mut threads = Vec::new();
        let dest = Net::group();

        // Announce twice, one second apart (RFC 6762 §8.3), then answer queries.
        {
            let (net, stop, sources) = (net.clone(), stop.clone(), sources.clone());
            threads.push(std::thread::spawn(move || {
                let service = name(SERVICE);
                let host_name = name(&format!("{}.local", net.host.host));
                // When each record was last multicast on each interface (RFC 6762 §6).
                let mut last_sent: std::collections::HashMap<
                    (String, u16, Ipv4Addr),
                    std::time::Instant,
                > = Default::default();
                let mut on_link_memo: std::collections::HashMap<
                    Ipv4Addr,
                    (std::time::Instant, bool),
                > = Default::default();
                // Unicast answers per querier in the current second (see below).
                let mut unicast_sent: std::collections::HashMap<
                    std::net::IpAddr,
                    (std::time::Instant, u32),
                > = Default::default();
                let mut buf = [0u8; 9000];
                let mut announces = 0;
                let mut next_announce = std::time::Instant::now();
                let mut next_scan = next_announce + IFACE_SCAN;
                // Interfaces that came up later, and when they get their second
                // announcement.
                let mut late: Vec<(Ipv4Addr, std::time::Instant)> = Vec::new();
                // The responder must outlive any one packet: an internal error on one is
                // logged and the loop goes on (a dead responder makes the source
                // unfindable).
                loop {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        while !stop.load(Ordering::Relaxed) {
                            let now = std::time::Instant::now();
                            // Every packet is built and sent while holding `sources`, so a goodbye
                            // from `remove` can never be overtaken by a stale announcement.
                            if announces < 2 && now >= next_announce {
                                net.announce(&sources.lock_ok(), 1, None);
                                announces += 1;
                                next_announce += Duration::from_secs(1);
                            }
                            if now >= next_scan {
                                next_scan = now + IFACE_SCAN;
                                let added = net.rescan();
                                if !added.is_empty() {
                                    net.announce(&sources.lock_ok(), 1, Some(&added));
                                    late.extend(
                                        added
                                            .into_iter()
                                            .map(|a| (a, now + Duration::from_secs(1))),
                                    );
                                }
                            }
                            let due: Vec<Ipv4Addr> = late
                                .iter()
                                .filter(|(_, t)| now >= *t)
                                .map(|(a, _)| *a)
                                .collect();
                            if !due.is_empty() {
                                late.retain(|(_, t)| now < *t);
                                net.announce(&sources.lock_ok(), 1, Some(&due));
                            }
                            let Ok((n, from)) = net.sock.recv_from(&mut buf) else {
                                std::thread::sleep(Duration::from_millis(10));
                                continue;
                            };
                            let Some(m) = dns::parse(&buf[..n]).filter(|m| !m.is_response()) else {
                                continue;
                            };
                            let oneshot = from.port() != MDNS_PORT;
                            let from_v4 = match from.ip() {
                                std::net::IpAddr::V4(a) => Some(a),
                                std::net::IpAddr::V6(_) => None,
                            };
                            // Unicast answers only to the local link (RFC 6762 §11): an off-link
                            // (spoofable) source must not turn us into a reflector.
                            // On-link: one of our interfaces' networks holds the address;
                            // anything else is looked up once a second per address, not per
                            // packet (a query flood used to cost an interface walk each).
                            let on_link = from_v4.is_some_and(|a| {
                                a.is_loopback()
                                    || net.ifaces.lock_ok().iter().any(|i| i.contains(a))
                                    || {
                                        let now = std::time::Instant::now();
                                        if on_link_memo.len() > 256 {
                                            on_link_memo.clear();
                                        }
                                        let e = on_link_memo
                                            .entry(a)
                                            .or_insert_with(|| (now, super::ifaces::is_on_link(a)));
                                        if now.duration_since(e.0) >= Duration::from_secs(1) {
                                            *e = (now, super::ifaces::is_on_link(a));
                                        }
                                        e.1
                                    }
                            });
                            if oneshot && !on_link {
                                continue;
                            }
                            // Most queries on a network are for other services: skip them before
                            // building any records (R1-100). Ours are the NDI service, its
                            // instances, and this host's name.
                            let ours = |q: &[String]| {
                                let n = q.len();
                                (n >= service.len()
                                    && q[n - service.len()..]
                                        .iter()
                                        .zip(&service)
                                        .all(|(a, b)| a.eq_ignore_ascii_case(b)))
                                    || (n == host_name.len()
                                        && q.iter()
                                            .zip(&host_name)
                                            .all(|(a, b)| a.eq_ignore_ascii_case(b)))
                            };
                            if !m.questions.iter().any(|(q, _, _)| ours(q)) {
                                continue;
                            }
                            // Answered with the address of the interface the querier is on.
                            let iface = from_v4.map_or(net.host.addr, |a| net.iface_for(a));
                            // Multicast answers go out at most once a second per record
                            // (below); a query whose every question was answered that
                            // recently costs nothing further (a flood of them must leave
                            // the responder time for other queriers).
                            let wants_unicast = oneshot
                                || m.questions.iter().any(|(_, _, c)| c & CLASS_TOP_BIT != 0);
                            if !(wants_unicast && on_link) {
                                let now = std::time::Instant::now();
                                let recent = |(q, t, _): &(Name, u16, u16)| {
                                    last_sent
                                        .get(&(q.join(".").to_ascii_lowercase(), *t, iface))
                                        .is_some_and(|s| {
                                            now.duration_since(*s) < Duration::from_secs(1)
                                        })
                                };
                                if m.questions.iter().all(recent) {
                                    continue;
                                }
                            }
                            // Answer from the sources as they are now, under the lock (see above).
                            let src = sources.lock_ok();
                            let all = records(&net.host, iface, &src, 1);
                            let (mut ans, mut add) = (Vec::new(), Vec::new());
                            let mut qs = Vec::new();
                            let mut unicast = oneshot;
                            for (q, t, c) in &m.questions {
                                let (a, d) = answer(&all, q, *t);
                                if !a.is_empty() {
                                    qs.push((q.clone(), *t, *c));
                                    unicast |= c & CLASS_TOP_BIT != 0;
                                }
                                ans.extend(a);
                                add.extend(d);
                            }
                            // Known answers (RFC 6762 §7.1): what the querier already holds with
                            // at least half its TTL left is not sent again.
                            ans.retain(|r| {
                                !m.records.iter().any(|k| {
                                    k.rtype == r.rtype
                                        && k.data == r.data
                                        && name_eq(&k.name, &r.name)
                                        && 2 * k.ttl >= r.ttl
                                })
                            });
                            // A record is multicast at most once a second per interface (RFC 6762
                            // §6): a query flood must not become a multicast flood (R1-93).
                            let multicast = !(unicast && on_link);
                            let rkey = |r: &Record| {
                                (r.name.join(".").to_ascii_lowercase(), r.rtype, iface)
                            };
                            if multicast {
                                let now = std::time::Instant::now();
                                last_sent
                                    .retain(|_, t| now.duration_since(*t) < Duration::from_secs(1));
                                ans.retain(|r| !last_sent.contains_key(&rkey(r)));
                                for r in &ans {
                                    last_sent.insert(rkey(r), now);
                                }
                            }
                            if !multicast {
                                // A querier asking for unicast answers gets a few a second, not
                                // one per query: a flood must not make this host a flood.
                                let now = std::time::Instant::now();
                                if unicast_sent.len() > 1024 {
                                    unicast_sent.retain(|_, (t, _)| {
                                        now.duration_since(*t) < Duration::from_secs(1)
                                    });
                                }
                                let e = unicast_sent.entry(from.ip()).or_insert((now, 0));
                                if now.duration_since(e.0) >= Duration::from_secs(1) {
                                    *e = (now, 0);
                                }
                                if e.1 >= 4 {
                                    continue;
                                }
                                e.1 += 1;
                            }
                            if ans.is_empty() {
                                continue;
                            }
                            add.retain(|r| !ans.contains(r));
                            add.dedup();
                            let pkts = if oneshot {
                                // One-shot replies echo the ID and questions, with short TTLs and
                                // no cache-flush bit (RFC 6762 §6.7).
                                for r in ans.iter_mut().chain(add.iter_mut()) {
                                    r.ttl = r.ttl.min(10);
                                    r.class &= !CLASS_TOP_BIT;
                                }
                                packets(&ans, &add, &|a, d| {
                                    dns::encode_response_with_questions(m.id, &qs, a, d)
                                })
                            } else {
                                packets(&ans, &add, &|a, d| dns::encode_response(0, a, d))
                            };
                            // A unicast-response query from off the link gets the multicast answer.
                            net.send_on(iface, &pkts, if unicast && on_link { from } else { dest });
                            drop(src);
                        }
                    }));
                    if r.is_ok() || stop.load(Ordering::Relaxed) {
                        break;
                    }
                    crate::log::log("mDNS responder: internal error on a packet; going on");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }));
        }

        let listing = serve_listing
            .then(|| Listing::start(host.listing_port, sources.clone()))
            .flatten();
        let on_signal = {
            let (net, sources) = (net.clone(), sources.clone());
            crate::shutdown::register(move || net.announce(&sources.lock_ok(), 0, None))
        };
        Ok(Responder {
            stop,
            sources,
            threads,
            net,
            _listing: listing,
            _on_signal: on_signal,
        })
    }

    /// Publishes another source: announced now and again after one second (RFC 6762
    /// §8.3). A source with the same name is replaced.
    pub fn add(&self, p: Published) {
        {
            let mut src = self.sources.lock_ok();
            src.retain(|x| x.name != p.name);
            src.push(p.clone());
            self.net.announce(std::slice::from_ref(&p), 1, None);
        }
        let (net, sources, stop) = (self.net.clone(), self.sources.clone(), self.stop.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            // Only if still published unchanged: a source removed (or replaced) within the
            // second must not be brought back by its own late announcement.
            let src = sources.lock_ok();
            if !stop.load(Ordering::Relaxed) && src.contains(&p) {
                net.announce(std::slice::from_ref(&p), 1, None);
            }
        });
    }

    /// Withdraws a source by full name, with goodbye records (TTL 0) for it only.
    /// Withdraws the source `name` published on `port` (a newer source of the same name
    /// on another port, as in make-before-break reconfiguration, stays).
    pub fn remove(&self, name: &str, port: u16) {
        let mut s = self.sources.lock_ok();
        let mine = |x: &Published| x.name == name && x.port == port;
        let gone: Vec<Published> = s.iter().filter(|x| mine(x)).cloned().collect();
        s.retain(|x| !mine(x));
        if !gone.is_empty() {
            self.net.announce(&gone, 0, None);
        }
    }

    pub fn host(&self) -> &HostInfo {
        &self.net.host
    }
}

impl Drop for Responder {
    /// Sends goodbye records (TTL 0) for the NDI records only, and stops.
    fn drop(&mut self) {
        self.net.announce(&self.sources.lock_ok(), 0, None);
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Serves the host listing port (§5.2) until dropped: every connection gets the
/// document for `sources` (shared with whoever publishes them). Independent of mDNS,
/// because NDI tools configured with this host's address use this port instead of it.
pub struct Listing {
    sources: Arc<Mutex<Vec<Published>>>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

impl Listing {
    /// `None` when another NDI process on this host holds the port already (§5.2 says
    /// every process listens; only one can): then that process lists its sources and
    /// this one goes without, which is logged, not an error.
    pub fn start(port: u16, sources: Arc<Mutex<Vec<Published>>>) -> Option<Self> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .and_then(|l| l.set_nonblocking(true).map(|_| l));
        let listener = match listener {
            Ok(l) => l,
            Err(e) => {
                crate::log::log(&format!(
                    "listing port {port} held by another program on this host ({e}): this \
                     program's sources are found by mDNS and Discovery Servers, not by that port"
                ));
                return None;
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, sources) = (stop.clone(), sources.clone());
            std::thread::spawn(move || {
                // Connections are kept open after sending the list, as NDI hosts do, until
                // the peer closes them (checked every second), at most MAX_LISTING_OPEN.
                let mut open: std::collections::VecDeque<std::net::TcpStream> = Default::default();
                let mut swept = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut c, _)) => {
                            let doc = listing_document(&sources.lock_ok());
                            // A peer that doesn't read must not hold up the others.
                            let _ = c.set_write_timeout(Some(Duration::from_secs(1)));
                            let sent = c.write_all(doc.as_bytes()).and_then(|_| c.write_all(&[0]));
                            if sent.is_ok() && c.set_nonblocking(true).is_ok() {
                                if open.len() >= MAX_LISTING_OPEN {
                                    open.pop_front();
                                }
                                open.push_back(c);
                            }
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                    if swept.elapsed() >= Duration::from_secs(1) {
                        swept = std::time::Instant::now();
                        let mut b = [0u8; 512];
                        // Closed (or failed): forget it. Anything a peer sends is ignored.
                        open.retain_mut(|c| match std::io::Read::read(c, &mut b) {
                            Ok(0) => false,
                            Ok(_) => true,
                            Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
                        });
                    }
                }
            })
        };
        Some(Listing {
            sources,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }

    /// Lists `p` (replacing a source of the same name).
    pub fn add(&self, p: Published) {
        let mut src = self.sources.lock_ok();
        src.retain(|x| x.name != p.name);
        src.push(p);
    }

    pub fn remove(&self, name: &str, port: u16) {
        self.sources
            .lock_ok()
            .retain(|x| !(x.name == name && x.port == port));
    }
}

/// The §5.2 listing document (without its NUL).
pub fn listing_document(sources: &[Published]) -> String {
    use crate::control::escape_attr;
    let mut s = String::from("<ndi><services>");
    for p in sources {
        s += &format!(
            "<service name=\"{}\" port=\"{}\"><groups>",
            escape_attr(&p.name),
            p.port
        );
        for g in &p.groups {
            s += &format!("<group>{}</group>", escape_attr(g));
        }
        s += "</groups></service>";
    }
    s + "</services></ndi>"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_sources_are_split_into_packets_that_fit_the_mtu() {
        let host = HostInfo {
            host: "church-server".into(),
            addr: Ipv4Addr::new(10, 0, 0, 5),
            listing_port: 5960,
            all_interfaces: false,
        };
        let sources: Vec<Published> = (0..40)
            .map(|i| Published {
                name: format!("CHURCH-SERVER (Stage display number {i} for the band)"),
                port: 5961 + i,
                groups: vec!["public".into()],
                routing: false,
                idn_ext: Some("resume/1 stats/1 audio-f32/1 color/1 channels/1".into()),
            })
            .collect();
        let recs = records(&host, host.addr, &sources, 1);
        let pkts = packets(&recs, &[], &|a, d| dns::encode_response(0, a, d));
        assert!(pkts.len() > 5);
        let mut seen = 0;
        for p in &pkts {
            assert!(p.len() <= MAX_PACKET, "{}", p.len());
            let m = dns::parse(p).unwrap();
            assert!(
                m.records.iter().any(|r| r.data == RData::A(host.addr)),
                "A in every packet"
            );
            // Each source's PTR, SRV and TXT travel together.
            for r in m.records.iter().filter(|r| r.rtype == TYPE_PTR) {
                let RData::Ptr(inst) = &r.data else {
                    unreachable!()
                };
                assert!(
                    m.records
                        .iter()
                        .any(|x| x.rtype == TYPE_SRV && name_eq(&x.name, inst))
                );
                assert!(
                    m.records
                        .iter()
                        .any(|x| x.rtype == TYPE_TXT && name_eq(&x.name, inst))
                );
                seen += 1;
            }
        }
        assert_eq!(seen, 40);
    }
}
