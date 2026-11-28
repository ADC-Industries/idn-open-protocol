//! Any DNS-SD service on mDNS, for protocols beside IDN/NDI's own (`_ndi._tcp`, which
//! [`super::responder`] announces): instances "NAME" of `SERVICE` at a TCP port of this
//! machine, announced until dropped, and found by asking. Open Media Transport's sources
//! (`_omt._tcp`, `idn-omt`) use it.

use super::dns::{
    self, CLASS_IN, CLASS_TOP_BIT, Name, RData, Record, TYPE_A, TYPE_ANY, TYPE_PTR, TYPE_SRV,
    TYPE_TXT,
};
use super::{MDNS_GROUP, MDNS_PORT, udp_socket};
use idn_common::sync::LockOk as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An instance found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Its instance name ("STUDIO (Program)").
    pub instance: String,
    /// Where it listens.
    pub addr: Ipv4Addr,
    /// Its port.
    pub port: u16,
}

/// The service's labels ("_omt._tcp.local" → `_omt`, `_tcp`, `local`).
fn labels(service: &str) -> Name {
    dns::name(service)
}

fn records(
    service: &str,
    host: &str,
    addr: Ipv4Addr,
    list: &[(String, u16)],
    ttl: u32,
) -> Vec<Record> {
    let svc = labels(service);
    let target = dns::name(&format!("{}.local", host.to_lowercase()));
    let mut out = Vec::new();
    for (instance, port) in list {
        // The instance is one label, whatever it holds (dots, brackets).
        let mut inst = vec![instance.clone()];
        inst.extend(svc.iter().cloned());
        out.push(Record {
            name: svc.clone(),
            rtype: TYPE_PTR,
            class: CLASS_IN,
            ttl: 4500 * ttl,
            data: RData::Ptr(inst.clone()),
        });
        out.push(Record {
            name: inst.clone(),
            rtype: TYPE_SRV,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 120 * ttl,
            data: RData::Srv {
                priority: 0,
                weight: 0,
                port: *port,
                target: target.clone(),
            },
        });
        out.push(Record {
            name: inst,
            rtype: TYPE_TXT,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 4500 * ttl,
            data: RData::Txt(vec![Vec::new()]),
        });
    }
    if !list.is_empty() {
        out.push(Record {
            name: target,
            rtype: TYPE_A,
            class: CLASS_IN | CLASS_TOP_BIT,
            ttl: 120 * ttl,
            data: RData::A(addr),
        });
    }
    out
}

struct Shared {
    service: String,
    host: String,
    list: Mutex<Vec<(String, u16)>>,
    /// Announce again now (the list changed).
    changed: AtomicBool,
    stop: AtomicBool,
}

/// Announces instances of a service until dropped (a goodbye then withdraws them).
pub struct ServiceAnnouncer {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServiceAnnouncer {
    /// Starts announcing nothing yet of `service` ("_omt._tcp.local") on every
    /// multicast-capable interface, answering queries for it.
    pub fn start(service: &str) -> io::Result<Self> {
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
        let shared = Arc::new(Shared {
            service: service.to_owned(),
            host: super::hostname(),
            list: Mutex::new(Vec::new()),
            changed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("idn-service-mdns".into())
            .spawn(move || announce_loop(sock, ifaces, &sh))?;
        Ok(ServiceAnnouncer {
            shared,
            thread: Some(thread),
        })
    }

    /// The instances announced from now on: (name, port). Ones taken away are withdrawn.
    pub fn set(&self, list: Vec<(String, u16)>) {
        *self.shared.list.lock_ok() = list;
        self.shared.changed.store(true, Ordering::Relaxed);
    }
}

impl Drop for ServiceAnnouncer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sends `list` on each interface with that interface's address, or to one asker.
fn send(
    sock: &UdpSocket,
    ifaces: &[Ipv4Addr],
    sh: &Shared,
    list: &[(String, u16)],
    ttl: u32,
    to: Option<SocketAddr>,
) {
    if let Some(to) = to {
        let routed = match to.ip() {
            std::net::IpAddr::V4(ip) => idn_common::net::local_v4_towards(ip),
            _ => None,
        };
        if let Some(i) = routed.or_else(|| ifaces.first().copied()) {
            let msg = dns::encode_response(0, &records(&sh.service, &sh.host, i, list, ttl), &[]);
            let _ = sock.send_to(&msg, to);
        }
        return;
    }
    for &i in ifaces {
        let msg = dns::encode_response(0, &records(&sh.service, &sh.host, i, list, ttl), &[]);
        let _ = socket2::SockRef::from(sock).set_multicast_if_v4(&i);
        let _ = sock.send_to(&msg, SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
    }
}

fn announce_loop(sock: UdpSocket, ifaces: Vec<Ipv4Addr>, sh: &Shared) {
    let svc = labels(&sh.service);
    let (mut next, mut announced) = (Instant::now(), 0u32);
    let mut said: Vec<(String, u16)> = Vec::new();
    let mut buf = [0u8; 9000];
    while !sh.stop.load(Ordering::Relaxed) {
        if sh.changed.swap(false, Ordering::Relaxed) {
            let now = sh.list.lock_ok().clone();
            let gone: Vec<(String, u16)> =
                said.iter().filter(|x| !now.contains(x)).cloned().collect();
            if !gone.is_empty() {
                send(&sock, &ifaces, sh, &gone, 0, None);
            }
            said = now;
            (next, announced) = (Instant::now(), 0);
        }
        if Instant::now() >= next && !said.is_empty() {
            send(&sock, &ifaces, sh, &said, 1, None);
            announced += 1;
            // Three times a second apart at first (RFC 6762 §8.3), then every minute.
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
                if m.is_response() || said.is_empty() {
                    continue;
                }
                let asked = m
                    .questions
                    .iter()
                    .any(|(q, t, _)| dns::name_eq(q, &svc) && (*t == TYPE_PTR || *t == TYPE_ANY));
                if asked {
                    let to = (from.port() != MDNS_PORT).then_some(from);
                    send(&sock, &ifaces, sh, &said, 1, to);
                }
            }
            // (A read timeout, as Windows tells it too (`TimedOut`): no wait, nothing came;
            // it was taken as an error and waited 200 ms more; bug run B.)
            Err(e) if crate::transport::is_poll_timeout(&e) => {}
            Err(e) if idn_common::backoff::retry_at_once(&e) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    if !said.is_empty() {
        send(&sock, &ifaces, sh, &said, 0, None);
    }
}

/// The instances of `service` that answer within `timeout`.
pub fn find(service: &str, timeout: Duration) -> Vec<Found> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let svc = labels(service);
    let q = dns::encode_query(0, &[(svc.clone(), TYPE_PTR)], false);
    for i in super::ifaces::ipv4_multicast_interfaces() {
        let _ = socket2::SockRef::from(&sock).set_multicast_if_v4(&i.addr);
        let _ = sock.send_to(&q, SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
    }
    let end = Instant::now() + timeout;
    let mut found: Vec<Found> = Vec::new();
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
            .filter(|r| dns::name_eq(&r.name, &svc) && r.ttl > 0)
            .filter_map(|r| match &r.data {
                RData::Ptr(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        for inst in instances {
            let Some((port, target)) = m.records.iter().find_map(|r| match &r.data {
                RData::Srv { port, target, .. } if dns::name_eq(&r.name, &inst) => {
                    Some((*port, target.clone()))
                }
                _ => None,
            }) else {
                continue;
            };
            let Some(addr) = m.records.iter().find_map(|r| match r.data {
                RData::A(a) if dns::name_eq(&r.name, &target) => Some(a),
                _ => None,
            }) else {
                continue;
            };
            let instance = inst.first().cloned().unwrap_or_default();
            if !found.iter().any(|f| f.instance == instance) {
                found.push(Found {
                    instance,
                    addr,
                    port,
                });
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CPU ticks (user + system) of this process's threads named `name`, from /proc.
    #[cfg(target_os = "linux")]
    fn thread_ticks(name: &str) -> Option<u64> {
        let mut total = None;
        for t in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
            let comm = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
            if comm.trim_end() != name {
                continue;
            }
            let stat = std::fs::read_to_string(t.path().join("stat")).ok()?;
            // After the ")" that ends the name: fields 3 on; utime and stime are 14 and 15.
            let rest = stat.rsplit_once(") ")?.1;
            let f: Vec<&str> = rest.split_whitespace().collect();
            let (u, s): (u64, u64) = (f.get(11)?.parse().ok()?, f.get(12)?.parse().ok()?);
            *total.get_or_insert(0) += u + s;
        }
        total
    }

    /// An announcer with nothing to answer waits in its socket rather than spinning a core
    /// (Beta18 lab: an idle Node used 100 % of one).
    #[cfg(target_os = "linux")]
    #[test]
    fn an_idle_announcer_sleeps() {
        let Ok(a) = ServiceAnnouncer::start("_idn-idle-test._tcp.local") else {
            return; // (no mDNS socket here)
        };
        a.set(vec![("Idle test".into(), 9)]);
        std::thread::sleep(Duration::from_millis(300));
        let Some(before) = thread_ticks("idn-service-mdns") else {
            return;
        };
        std::thread::sleep(Duration::from_secs(2));
        let after = thread_ticks("idn-service-mdns").unwrap_or(before);
        // Spinning was about 200 ticks (two seconds of a core at 100 Hz); waiting, ~0.
        let used = after.saturating_sub(before);
        assert!(used < 40, "{used} ticks in 2 s");
    }

    #[test]
    fn records_hold_each_instance_as_one_label() {
        let r = records(
            "_omt._tcp.local",
            "Studio",
            Ipv4Addr::new(10, 0, 0, 5),
            &[("STUDIO (Program 1.5)".into(), 6400)],
            1,
        );
        let msg = dns::encode_response(0, &r, &[]);
        let m = dns::parse(&msg).unwrap();
        let ptr = m
            .records
            .iter()
            .find_map(|r| match &r.data {
                RData::Ptr(p) => Some(p.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(ptr[0], "STUDIO (Program 1.5)");
        assert!(
            m.records
                .iter()
                .any(|r| matches!(r.data, RData::Srv { port: 6400, .. }))
        );
        assert!(
            m.records
                .iter()
                .any(|r| r.data == RData::A(Ipv4Addr::new(10, 0, 0, 5)))
        );
    }
}
