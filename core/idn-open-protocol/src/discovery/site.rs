//! The IDN Site Manager on mDNS (spec/extensions/idn-node.md §N.11.4): announced as
//! `_idn-site._tcp.local` so that a new IDN Node finds its site with no address typed in.

use super::dns::{
    self, CLASS_IN, CLASS_TOP_BIT, Name, RData, Record, TYPE_A, TYPE_ANY, TYPE_PTR, TYPE_SRV,
    TYPE_TXT,
};
use super::{MDNS_GROUP, MDNS_PORT, udp_socket};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The service type.
pub const SITE_SERVICE: &str = "_idn-site._tcp.local";

/// The service type announced and looked for: `IDN_SITE_SERVICE` when set (tests: a name
/// of their own, so that no real IDN Node on the LAN finds and joins a test's site; one did,
/// 2026-11-05), else [`SITE_SERVICE`].
pub fn site_service() -> String {
    std::env::var("IDN_SITE_SERVICE")
        .ok()
        .filter(|v| v.starts_with('_') && v.ends_with("._tcp.local") && v.len() < 64)
        .unwrap_or_else(|| SITE_SERVICE.to_owned())
}

/// A Site Manager found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteInfo {
    /// Its instance name (the host's name).
    pub name: String,
    /// Where its Discovery Server listens: address...
    pub addr: Ipv4Addr,
    /// ...and port.
    pub port: u16,
    /// The site CA's fingerprint, from the TXT record.
    pub ca: String,
}

impl SiteInfo {
    /// `address:port`, as a Discovery Server setting.
    pub fn server(&self) -> String {
        format!("{}:{}", self.addr, self.port)
    }
}

fn records(
    service: &str,
    host: &str,
    addr: Ipv4Addr,
    port: u16,
    ca: &str,
    ttl_scale: u32,
) -> Vec<Record> {
    let service = dns::name(service);
    let mut inst = vec![host.to_owned()];
    inst.extend(service.iter().cloned());
    let target = dns::name(&format!("{}.local", host.to_lowercase()));
    vec![
        Record {
            name: service,
            rtype: TYPE_PTR,
            class: CLASS_IN,
            ttl: 4500 * ttl_scale,
            data: RData::Ptr(inst.clone()),
        },
        Record {
            name: inst.clone(),
            rtype: TYPE_SRV,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 120 * ttl_scale,
            data: RData::Srv {
                priority: 0,
                weight: 0,
                port,
                target: target.clone(),
            },
        },
        Record {
            name: inst,
            rtype: TYPE_TXT,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 4500 * ttl_scale,
            data: RData::Txt(vec![b"v=1".to_vec(), format!("ca={ca}").into_bytes()]),
        },
        Record {
            name: target,
            rtype: TYPE_A,
            class: CLASS_IN,
            ttl: 120,
            data: RData::A(addr),
        },
    ]
}

/// Announces the Site Manager until dropped (a goodbye then withdraws it).
pub struct SiteAnnouncer {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

impl SiteAnnouncer {
    /// Announces the Discovery Server at `port` with the site CA `ca` (fingerprint), on
    /// every multicast-capable interface, and answers queries for it.
    pub fn start(port: u16, ca: &str) -> io::Result<Self> {
        Self::start_as(&site_service(), port, ca)
    }

    /// As [`SiteAnnouncer::start`], under the service type `service` (tests: one of their
    /// own, that only their Node looks for, through `IDN_SITE_SERVICE`).
    pub fn start_as(service: &str, port: u16, ca: &str) -> io::Result<Self> {
        let service = service.to_owned();
        let host = super::hostname();
        let sock = udp_socket(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT),
            true,
            Ipv4Addr::UNSPECIFIED,
        )?;
        let ifaces: Vec<Ipv4Addr> = super::ifaces::ipv4_multicast_interfaces()
            .into_iter()
            .map(|i| i.addr)
            .collect();
        for i in &ifaces {
            let _ = sock.join_multicast_v4(&MDNS_GROUP, i);
        }
        // The loop waits in recv_from: blocking with a 100 ms timeout (udp_socket's are
        // non-blocking, for the browser's poll; here a would-block came back at once and
        // the thread spun a core).
        sock.set_nonblocking(false)?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, ca) = (stop.clone(), ca.to_owned());
        let thread = std::thread::Builder::new()
            .name("idn-site-mdns".into())
            .spawn(move || announce_loop(&service, sock, ifaces, &host, port, &ca, &s2))?;
        Ok(SiteAnnouncer {
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }
}

/// Sends the records on each interface with that interface's address.
#[allow(clippy::too_many_arguments)]
fn send_all(
    service: &str,
    sock: &UdpSocket,
    ifaces: &[Ipv4Addr],
    host: &str,
    port: u16,
    ca: &str,
    ttl: u32,
    to: Option<SocketAddr>,
) {
    if let Some(to) = to {
        // A unicast reply: once, with the address of the interface the asker is reached
        // through (the first interface's was another network's on a machine with two).
        let routed = match to.ip() {
            std::net::IpAddr::V4(ip) => idn_common::net::local_v4_towards(ip),
            _ => None,
        };
        if let Some(i) = routed.or_else(|| ifaces.first().copied()) {
            let msg = dns::encode_response(0, &records(service, host, i, port, ca, ttl), &[]);
            let _ = sock.send_to(&msg, to);
        }
        return;
    }
    for &i in ifaces {
        let r = records(service, host, i, port, ca, ttl);
        let msg = dns::encode_response(0, &r, &[]);
        let _ = socket2::SockRef::from(sock).set_multicast_if_v4(&i);
        let _ = sock.send_to(&msg, SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
    }
}

fn announce_loop(
    service_name: &str,
    sock: UdpSocket,
    ifaces: Vec<Ipv4Addr>,
    host: &str,
    port: u16,
    ca: &str,
    stop: &AtomicBool,
) {
    let service = dns::name(service_name);
    let mut next = Instant::now();
    let mut announced = 0u32;
    let mut buf = [0u8; 9000];
    while !stop.load(Ordering::Relaxed) {
        if Instant::now() >= next {
            send_all(service_name, &sock, &ifaces, host, port, ca, 1, None);
            announced += 1;
            // Three times a second apart at start (RFC 6762 §8.3), then every minute.
            next = Instant::now()
                + if announced < 3 {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(60)
                };
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let Some(m) = dns::parse(&buf[..n]) else {
                    continue;
                };
                if m.is_response() {
                    continue;
                }
                let asked = m.questions.iter().any(|(q, t, _)| {
                    dns::name_eq(q, &service) && (*t == TYPE_PTR || *t == TYPE_ANY)
                });
                if asked {
                    // A one-shot querier (not from port 5353) is answered directly.
                    let to = (from.port() != MDNS_PORT).then_some(from);
                    send_all(service_name, &sock, &ifaces, host, port, ca, 1, to);
                }
            }
            // (A read timeout, as Windows tells it too: nothing came, no extra wait; bug
            // run B.)
            Err(e) if crate::transport::is_poll_timeout(&e) => {}
            Err(e) if idn_common::backoff::retry_at_once(&e) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    // Goodbye (TTL 0).
    send_all(service_name, &sock, &ifaces, host, port, ca, 0, None);
}

/// The Site Managers that answer within `timeout`.
pub fn find_sites(timeout: Duration) -> Vec<SiteInfo> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let q = dns::encode_query(0, &[(dns::name(&site_service()), TYPE_PTR)], false);
    for i in super::ifaces::ipv4_multicast_interfaces() {
        let _ = socket2::SockRef::from(&sock).set_multicast_if_v4(&i.addr);
        let _ = sock.send_to(&q, SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
    }
    let service = dns::name(&site_service());
    let end = Instant::now() + timeout;
    let mut found: Vec<SiteInfo> = Vec::new();
    let mut buf = [0u8; 9000];
    while Instant::now() < end {
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Some(m) = dns::parse(&buf[..n]) else {
            continue;
        };
        if !m.is_response() {
            continue;
        }
        let instances: Vec<Name> = m
            .records
            .iter()
            .filter(|r| dns::name_eq(&r.name, &service))
            .filter_map(|r| match &r.data {
                RData::Ptr(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        for inst in instances {
            let srv = m.records.iter().find_map(|r| match &r.data {
                RData::Srv { port, target, .. } if dns::name_eq(&r.name, &inst) => {
                    Some((*port, target.clone()))
                }
                _ => None,
            });
            let ca = m
                .records
                .iter()
                .filter(|r| dns::name_eq(&r.name, &inst))
                .find_map(|r| match &r.data {
                    RData::Txt(t) => t.iter().find_map(|x| {
                        std::str::from_utf8(x)
                            .ok()?
                            .strip_prefix("ca=")
                            .map(str::to_owned)
                    }),
                    _ => None,
                })
                .unwrap_or_default();
            let Some((port, target)) = srv else { continue };
            let addr = m.records.iter().find_map(|r| match r.data {
                RData::A(a) if dns::name_eq(&r.name, &target) => Some(a),
                _ => None,
            });
            let Some(addr) = addr else { continue };
            let name = inst.first().cloned().unwrap_or_default();
            // (At most 64: forged answers grew the list, and its look-ups, without end
            // while the browse ran; bug run 2026-11-25.)
            if found.len() < 64 && !found.iter().any(|f| f.name == name && f.ca == ca) {
                found.push(SiteInfo {
                    name,
                    addr,
                    port,
                    ca,
                });
            }
        }
    }
    found
}
