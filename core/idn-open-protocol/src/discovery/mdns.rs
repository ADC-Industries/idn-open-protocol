//! The mDNS side of finding sources: the record cache and the querier's sockets (§5.1).

use super::dns::{Name, RData, TYPE_A, TYPE_PTR, TYPE_SRV, TYPE_TXT, name, name_eq};
use super::*;
use idn_common::backoff::Backoff;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::BTreeMap;
use std::net::{SocketAddrV4, UdpSocket};
use std::time::Instant;

// ---------------------------------------------------------------------------------------
// mDNS browser (§5.1).

/// Options for [`browse`] and [`find`].
#[derive(Debug, Clone, Default)]
pub struct BrowseOptions {
    /// The IPv4 address of the network interface to search on. `None`: all interfaces.
    pub interface: Option<Ipv4Addr>,
}

#[derive(Debug)]
pub(super) struct Instance {
    label: String,
    /// The instance's name as labels (a label may hold a '.', so the joined key can't be
    /// split back: R1-95).
    name: Name,
    srv: Option<(Name, u16)>,
    txt: Option<Vec<Vec<u8>>>,
    /// Last time any record of this instance was received.
    seen: Instant,
}

impl Default for Instance {
    fn default() -> Self {
        Instance {
            label: String::new(),
            name: Name::new(),
            srv: None,
            txt: None,
            seen: Instant::now(),
        }
    }
}

/// The interface whose subnet a search prefers addresses on: the one searched on, or
/// the main one (that routes to the mDNS group).
pub(super) fn preferred_interface(iface: Option<Ipv4Addr>) -> Option<ifaces::Interface> {
    let addr = iface.or_else(|| responder::HostInfo::local(None).ok().map(|h| h.addr))?;
    ifaces::ipv4_multicast_interfaces()
        .into_iter()
        .find(|i| i.addr == addr)
}

/// mDNS state built from responses.
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct Cache {
    pub(super) instances: BTreeMap<String, Instance>,
    /// Each host's addresses, and when each was last heard. A host on several networks
    /// announces each with its own address (and on this machine every one of them
    /// arrives, looped back).
    pub(super) hosts: BTreeMap<String, Vec<(Ipv4Addr, Instant)>>,
    /// Addresses on this interface's subnet are preferred: the network searched on (or
    /// the main one).
    prefer: Option<ifaces::Interface>,
    /// Instances that said goodbye, and when. Records for them are ignored for a second
    /// (RFC 6762 §10.1): responses read from the browser's other socket may predate the
    /// goodbye and would otherwise bring the source back until it expires.
    gone: BTreeMap<String, Instant>,
}

/// How long records for an instance are ignored after its goodbye.
/// Most instances and host addresses the mDNS cache keeps.
pub(super) const MAX_INSTANCES: usize = 1024;
pub(super) const MAX_HOSTS: usize = 1024;
/// Addresses kept per host.
const MAX_HOST_ADDRS: usize = 8;

pub(super) const GOODBYE_GRACE: Duration = Duration::from_secs(1);

fn key(n: &[String]) -> String {
    n.join(".").to_ascii_lowercase()
}

impl Cache {
    /// A cache that prefers host addresses reachable through `iface` (an interface
    /// address), or through the main interface if `None`.
    pub fn preferring(iface: Option<Ipv4Addr>) -> Self {
        Cache {
            prefer: preferred_interface(iface),
            ..Default::default()
        }
    }

    /// The address to use for `host`: one on the preferred subnet, else the newest.
    fn addr_of(&self, host: &[String]) -> Option<Ipv4Addr> {
        let addrs = self.hosts.get(&key(host))?;
        let newest = |v: &mut dyn Iterator<Item = &(Ipv4Addr, Instant)>| {
            v.max_by_key(|(_, t)| *t).map(|(a, _)| *a)
        };
        self.prefer
            .as_ref()
            .and_then(|p| newest(&mut addrs.iter().filter(|(a, _)| p.contains(*a))))
            .or_else(|| newest(&mut addrs.iter()))
    }

    /// Applies the records of one mDNS response. TTL 0 ("goodbye") removes entries.
    pub fn apply(&mut self, msg: &dns::Message) {
        let service = name(SERVICE);
        let is_instance = |n: &[String]| n.len() == service.len() + 1 && name_eq(&n[1..], &service);
        self.gone.retain(|_, t| t.elapsed() < GOODBYE_GRACE);
        // An instance's goodbye anywhere in the message applies to all its records in it.
        for r in &msg.records {
            if let RData::Ptr(target) = &r.data
                && r.ttl == 0
                && name_eq(&r.name, &service)
                && is_instance(target)
            {
                self.gone.insert(key(target), Instant::now());
            }
        }
        let gone = |n: &[String]| self.gone.contains_key(&key(n));
        // Bounded: any host on the network can send records. Entries already known keep
        // being updated; new ones beyond the cap are ignored until others expire.
        let full = |m: &BTreeMap<String, Instance>, k: &str| {
            m.len() >= MAX_INSTANCES && !m.contains_key(k)
        };
        for r in &msg.records {
            match &r.data {
                RData::Ptr(target) if name_eq(&r.name, &service) && is_instance(target) => {
                    if r.ttl == 0 || gone(target) {
                        self.instances.remove(&key(target));
                    } else if !full(&self.instances, &key(target)) {
                        let e = self.instances.entry(key(target)).or_default();
                        e.label = target[0].clone();
                        e.name = target.clone();
                        e.seen = Instant::now();
                    }
                }
                RData::Srv { .. } | RData::Txt(_) if is_instance(&r.name) && gone(&r.name) => {}
                RData::Srv { .. } | RData::Txt(_) if full(&self.instances, &key(&r.name)) => {}
                RData::Srv { port, target, .. } if is_instance(&r.name) => {
                    let e = self.instances.entry(key(&r.name)).or_default();
                    e.label = r.name[0].clone();
                    e.name = r.name.clone();
                    e.seen = Instant::now();
                    e.srv = (r.ttl != 0).then(|| (target.clone(), *port));
                }
                RData::Txt(strings) if is_instance(&r.name) => {
                    let e = self.instances.entry(key(&r.name)).or_default();
                    e.label = r.name[0].clone();
                    e.name = r.name.clone();
                    e.txt = (r.ttl != 0).then(|| strings.clone());
                }
                RData::A(ip) => {
                    let k = key(&r.name);
                    if r.ttl == 0 {
                        if let Some(v) = self.hosts.get_mut(&k) {
                            v.retain(|(a, _)| a != ip);
                            if v.is_empty() {
                                self.hosts.remove(&k);
                            }
                        }
                    } else if self.hosts.len() < MAX_HOSTS || self.hosts.contains_key(&k) {
                        let v = self.hosts.entry(k).or_default();
                        v.retain(|(a, _)| a != ip);
                        if v.len() >= MAX_HOST_ADDRS {
                            // The longest unheard goes.
                            let oldest = (0..v.len()).min_by_key(|&i| v[i].1).unwrap();
                            v.remove(oldest);
                        }
                        v.push((*ip, Instant::now()));
                    }
                }
                _ => {}
            }
        }
    }

    /// Forgets instances and host addresses not heard from within `max_age` (no goodbye
    /// was received).
    /// Whether anything was forgotten.
    pub fn expire(&mut self, max_age: Duration) -> bool {
        let count = |c: &Cache| {
            (
                c.instances.len(),
                c.hosts.values().map(Vec::len).sum::<usize>(),
            )
        };
        let before = count(self);
        self.instances.retain(|_, i| i.seen.elapsed() < max_age);
        for v in self.hosts.values_mut() {
            v.retain(|(_, t)| t.elapsed() < max_age);
        }
        self.hosts.retain(|_, v| !v.is_empty());
        before != count(self)
    }

    /// Questions still needed to resolve every known instance.
    pub(super) fn missing(&self) -> Vec<(Name, u16)> {
        let mut q = Vec::new();
        for (k, inst) in &self.instances {
            let n = if inst.name.is_empty() {
                name(k)
            } else {
                inst.name.clone()
            };
            if inst.srv.is_none() {
                q.push((n.clone(), TYPE_SRV));
            }
            if inst.txt.is_none() {
                q.push((n, TYPE_TXT));
            }
            if let Some((target, _)) = &inst.srv
                && !self.hosts.contains_key(&key(target))
            {
                q.push((target.clone(), TYPE_A));
            }
        }
        q
    }

    /// Sources with a known SRV record.
    pub fn sources(&self) -> Vec<Source> {
        self.instances
            .values()
            .filter_map(|i| {
                let (target, port) = i.srv.as_ref()?;
                let (groups, routing, listing_port) = parse_txt(i.txt.as_deref().unwrap_or(&[]));
                let idn_ext = parse_txt_idn(i.txt.as_deref().unwrap_or(&[]));
                Some(Source {
                    name: i.label.clone(),
                    addr: self.addr_of(target).map(IpAddr::V4),
                    port: *port,
                    host: target.join("."),
                    groups,
                    routing,
                    listing_port,
                    idn_ext,
                })
            })
            .collect()
    }
}

/// This machine's name, as IDN names use it (upper case, "HOST (name)"): the kernel's
/// host name on Linux, else the `HOSTNAME` or `COMPUTERNAME` environment variable
/// (Windows), else "IDN".
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()))
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| "IDN".into())
}

pub(crate) fn udp_socket(
    bind: SocketAddrV4,
    reuse: bool,
    iface: Ipv4Addr,
) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if reuse {
        s.set_reuse_address(true)?;
        #[cfg(unix)]
        s.set_reuse_port(true)?;
    }
    s.bind(&SocketAddr::V4(bind).into())?;
    // Room for a burst of queries (a flood) so that a real querier's
    // packet behind it isn't dropped by the kernel before the responder gets to it.
    let _ = s.set_recv_buffer_size(1 << 20);
    s.set_multicast_if_v4(&iface)?;
    s.set_multicast_ttl_v4(255)?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// Browses for NDI sources for up to `timeout`, stopping early when `done` returns true
/// for the sources found so far.
/// The mDNS sockets of a browser: a shared port-5353 socket joined on every interface,
/// plus one-shot sockets per interface (RFC 6762 §5.1). Queries go out on every IPv4
/// interface explicitly, so hosts without a multicast route still query (milestone 4).
pub(super) struct MdnsQuerier {
    /// The interfaces were chosen automatically (not given): [`refresh`] follows changes.
    ///
    /// [`refresh`]: MdnsQuerier::refresh
    auto: bool,
    pub(super) ifaces: Vec<Ipv4Addr>,
    pub(super) shared: Option<UdpSocket>,
    /// The interfaces `shared` has joined the mDNS group on.
    joined: Vec<Ipv4Addr>,
    pub(super) oneshot: Vec<(Ipv4Addr, UdpSocket)>,
}

/// The interfaces to use: the given one, else every multicast-capable IPv4 interface
/// (the unspecified address while there is none).
pub(super) fn mdns_interfaces(opts: &BrowseOptions) -> Vec<Ipv4Addr> {
    match opts.interface {
        Some(a) => vec![a],
        None => {
            let v: Vec<Ipv4Addr> = ifaces::ipv4_multicast_interfaces()
                .iter()
                .map(|i| i.addr)
                .collect();
            if v.is_empty() {
                vec![Ipv4Addr::UNSPECIFIED]
            } else {
                v
            }
        }
    }
}

impl MdnsQuerier {
    pub(super) fn open(opts: &BrowseOptions) -> io::Result<Self> {
        let mut q = MdnsQuerier {
            auto: opts.interface.is_none(),
            ifaces: Vec::new(),
            shared: None,
            joined: Vec::new(),
            oneshot: Vec::new(),
        };
        q.use_interfaces(mdns_interfaces(opts));
        if q.oneshot.is_empty() && q.shared.is_none() {
            return Err(io::Error::other("could not open any mDNS socket"));
        }
        Ok(q)
    }

    /// Opens and joins what `now` needs, drops what it no longer has.
    pub(super) fn use_interfaces(&mut self, now: Vec<Ipv4Addr>) {
        if self.shared.is_none() {
            self.shared = udp_socket(
                SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT),
                true,
                now[0],
            )
            .ok();
            self.joined.clear();
        }
        if let Some(s) = &self.shared {
            for &i in &now {
                if !self.joined.contains(&i) && s.join_multicast_v4(&MDNS_GROUP, &i).is_ok() {
                    self.joined.push(i);
                }
            }
            self.joined.retain(|i| {
                now.contains(i) || {
                    let _ = s.leave_multicast_v4(&MDNS_GROUP, i);
                    false
                }
            });
            if self.joined.is_empty() {
                self.shared = None;
            }
        }
        self.oneshot.retain(|(i, _)| now.contains(i));
        for &i in &now {
            if !self.oneshot.iter().any(|(o, _)| *o == i)
                && let Ok(s) = udp_socket(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), false, i)
            {
                self.oneshot.push((i, s));
            }
        }
        self.ifaces = now;
    }

    /// With automatic interfaces: picks up interfaces that came up or went away (DHCP
    /// finishing after start, a cable plugged in, a VPN). Returns whether one was added.
    pub(super) fn refresh(&mut self) -> bool {
        if !self.auto {
            return false;
        }
        let now = mdns_interfaces(&BrowseOptions { interface: None });
        if now == self.ifaces {
            return false;
        }
        let added = now.iter().any(|i| !self.ifaces.contains(i));
        self.use_interfaces(now);
        added
    }

    /// Sends the PTR query plus follow-ups for what the cache still lacks. Fails only if
    /// nothing could be sent on any interface.
    pub(super) fn query(&self, cache: &Cache) -> io::Result<()> {
        let dest = SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
        let mut qs = vec![(name(SERVICE), TYPE_PTR)];
        qs.extend(cache.missing());
        let (mut sent, mut last_err) = (0, None);
        let mut record = |r: io::Result<usize>| match r {
            Ok(_) => sent += 1,
            Err(e) => last_err = Some(e),
        };
        for chunk in qs.chunks(16) {
            let q = dns::encode_query(0, chunk, false);
            for (_, s) in &self.oneshot {
                record(s.send_to(&q, dest));
            }
            if let Some(s) = &self.shared {
                for i in &self.ifaces {
                    let r = socket2::SockRef::from(s).set_multicast_if_v4(i);
                    record(r.and_then(|_| s.send_to(&q, dest)));
                }
            }
        }
        if sent == 0 {
            let e = last_err.map_or_else(|| "no interface".to_owned(), |e| e.to_string());
            return Err(io::Error::other(format!(
                "mDNS query could not be sent on any interface ({e}); try --iface"
            )));
        }
        Ok(())
    }

    /// Applies every queued response to `cache`; returns whether anything arrived.
    pub(super) fn drain(&self, cache: &mut Cache) -> bool {
        let mut buf = [0u8; 9000];
        let mut got = false;
        for s in self
            .shared
            .iter()
            .chain(self.oneshot.iter().map(|(_, s)| s))
        {
            // At most 256 per socket per call: a flood (a switching loop replicating
            // multicast) must not starve queries, expiry and the stop check (R1-97).
            for _ in 0..256 {
                let Ok((n, _)) = s.recv_from(&mut buf) else {
                    break;
                };
                got = true;
                if let Some(m) = dns::parse(&buf[..n]).filter(|m| m.is_response()) {
                    cache.apply(&m);
                }
            }
        }
        got
    }
}

/// Browses for NDI sources for up to `timeout`, stopping early when `done` returns true
/// for the sources found so far.
#[doc(hidden)]
pub fn browse_until(
    timeout: Duration,
    opts: &BrowseOptions,
    mut done: impl FnMut(&[Source]) -> bool,
) -> io::Result<Vec<Source>> {
    let q = MdnsQuerier::open(opts)?;
    let mut cache = Cache::preferring(opts.interface);
    let start = Instant::now();
    let mut next_query = start;
    let mut pace = Backoff::new(Duration::from_millis(250), Duration::from_secs(1));
    let mut first = true;
    loop {
        let now = Instant::now();
        if now >= next_query {
            let r = q.query(&cache);
            if first {
                r?;
                first = false;
            }
            next_query = now + pace.next();
        }
        let got = q.drain(&mut cache);
        let sources = cache.sources();
        if done(&sources) || start.elapsed() >= timeout {
            return Ok(sources);
        }
        if !got {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
