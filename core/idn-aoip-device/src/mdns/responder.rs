//! The responder: answers on the device's interface, announces and says goodbye (RFC 6762, §2).

use super::*;
use idn_common::sync::LockOk as _;

/// The answer to a query (§2.1): datagrams of the answers (and their additional records)
/// for each question this device can answer, and whether any asked for a unicast reply
/// (the QU bit). `None` when nothing is ours. Each question once, at most
/// [`QUESTIONS_MOST`]; each record once.
pub fn answer(
    q: &dns::Message,
    services: &[Service],
    host: &str,
    ip: Ipv4Addr,
) -> Option<(Vec<Vec<u8>>, bool)> {
    if q.is_response() {
        return None;
    }
    let mut groups: Vec<(Vec<Record>, Vec<Record>)> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut asked = BTreeSet::new();
    let mut unicast = false;
    let mut host_asked = false;
    let any = |t: u16, want: u16| t == want || t == dns::TYPE_ANY;
    for (name, qtype, qclass) in q.questions.iter().take(QUESTIONS_MOST) {
        if !asked.insert(format!("{name:?}/{qtype}").to_ascii_lowercase()) {
            continue;
        }
        let before = groups.len();
        let mut once = |g: (Vec<Record>, Vec<Record>), key: String| {
            if seen.insert(key) {
                groups.push(g);
            }
        };
        for s in services {
            let inst = instance_name(s);
            if dns::name_eq(name, &type_name(&s.kind))
                && any(*qtype, dns::TYPE_PTR)
                && !known(q, &ptr(s, s.ttl))
            {
                once(
                    (
                        vec![ptr(s, s.ttl)],
                        vec![srv(s, host, s.ttl), txt(s, s.ttl)],
                    ),
                    format!("ptr {inst:?}"),
                );
            } else if dns::name_eq(name, &inst) {
                if any(*qtype, dns::TYPE_SRV) {
                    once((vec![srv(s, host, s.ttl)], vec![]), format!("srv {inst:?}"));
                }
                if any(*qtype, dns::TYPE_TXT) {
                    once((vec![txt(s, s.ttl)], vec![]), format!("txt {inst:?}"));
                }
            }
        }
        if dns::name_eq(name, &host_name(host)) && any(*qtype, dns::TYPE_A) {
            host_asked = true;
            once((vec![a(host, ip, 120)], vec![]), "a".into());
        }
        if groups.len() > before && qclass & dns::CLASS_TOP_BIT != 0 {
            unicast = true;
        }
    }
    if groups.is_empty() {
        return None;
    }
    let extra = if host_asked {
        vec![]
    } else {
        vec![a(host, ip, 120)]
    };
    Some((pack(&groups, &extra), unicast))
}

/// Whether the asker already has `r` (RFC 6762 §7.1: in its known answers with at least
/// half our TTL left): not answered again (bug run K3-6: every device answered its whole
/// channel set to every query).
pub(crate) fn known(q: &dns::Message, r: &Record) -> bool {
    q.records.iter().any(|k| {
        k.rtype == r.rtype
            && dns::name_eq(&k.name, &r.name)
            && k.data == r.data
            && k.ttl >= r.ttl / 2
    })
}

/// A query of `questions` with `known` in its answer section (RFC 6762 §7.1).
pub fn query_with_known(questions: &[(Name, u16)], known: &[Record]) -> Vec<u8> {
    let q: Vec<(Name, u16, u16)> = questions
        .iter()
        .map(|(n, t)| (n.clone(), *t, dns::CLASS_IN))
        .collect();
    let mut out = dns::encode_response_with_questions(0, &q, known, &[]);
    // A query: flags 0.
    out[2] = 0;
    out[3] = 0;
    out
}

/// The responder: answers on the interface `ip`, announces at start (twice, a second
/// apart) and says goodbye to what goes ([`Responder::set`]) and at the end (drop).
pub struct Responder {
    pub(crate) shared: Arc<Mutex<(Vec<Service>, String)>>,
    /// Who else in the program takes the responses heard on its socket ([`Responder::tap`]).
    pub(crate) taps: Taps,
    /// Multicast groups held for this device's flows (§2.6).
    pub(crate) reserved: Arc<Mutex<Vec<Ipv4Addr>>>,
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) ip: Ipv4Addr,
    pub(crate) stop: Arc<AtomicBool>,
    /// Instances to announce, sent by the responder's own thread (bug run K1-4, K3-3).
    pub(crate) pending: Arc<Mutex<Pending>>,
}

/// Announcements waiting: each instance named is announced as it is when sent (an
/// instance renamed again meanwhile isn't brought back), twice a second apart; at most a
/// burst every [`BURST_EVERY`] (bug run K1-4, K3-3: a thread a rename, each sending what
/// was current a second before; no limit).
#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) now: Vec<String>,
    pub(crate) later: Vec<String>,
    pub(crate) later_at: Option<std::time::Instant>,
    pub(crate) last_burst: Option<std::time::Instant>,
}

pub(crate) const BURST_EVERY: Duration = Duration::from_millis(250);

impl Pending {
    pub(crate) fn add(list: &mut Vec<String>, names: impl IntoIterator<Item = String>) {
        for n in names {
            if !list.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                list.push(n);
            }
        }
    }

    /// The instances due now (and `later` moved on), if a burst may go.
    pub(crate) fn due(&mut self, now: std::time::Instant) -> Option<Vec<String>> {
        if self
            .last_burst
            .is_some_and(|t| now.duration_since(t) < BURST_EVERY)
        {
            return None;
        }
        let first = std::mem::take(&mut self.now);
        let mut names = first.clone();
        if self.later_at.is_some_and(|t| now >= t) {
            Pending::add(&mut names, std::mem::take(&mut self.later));
            self.later_at = None;
        }
        if names.is_empty() {
            return None;
        }
        // The ones sent the first time once more, a second on (RFC 6762 §8.3).
        if !first.is_empty() {
            Pending::add(&mut self.later, first);
            self.later_at.get_or_insert(now + Duration::from_secs(1));
        }
        self.last_burst = Some(now);
        Some(names)
    }
}

/// The announcement of the `names` among `services` now.
pub(crate) fn announce_named(
    so: &UdpSocket,
    services: &[Service],
    host: &str,
    ip: Ipv4Addr,
    names: &[String],
) {
    let picked: Vec<Service> = services
        .iter()
        .filter(|s| {
            let k = format!("{:?}", instance_name(s));
            names.iter().any(|n| n.eq_ignore_ascii_case(&k))
        })
        .cloned()
        .collect();
    if picked.is_empty() {
        return;
    }
    for d in announcement(&picked, host, ip, false, true) {
        send(so, &d, None);
    }
}

pub(crate) type Taps = Arc<Mutex<Vec<std::sync::mpsc::SyncSender<(dns::Message, Ipv4Addr)>>>>;

/// The responses a [`Responder`] hears, for a browser in the same program (bug run
/// I3-12: a second socket on 5353 in one process took a share of the unicast datagrams,
/// so some never reached the responder), and its socket to ask from.
pub struct Heard {
    pub rx: std::sync::mpsc::Receiver<(dns::Message, Ipv4Addr)>,
    pub socket: Arc<UdpSocket>,
}

/// The reply to a one-shot ("legacy") query from another port than 5353 (RFC 6762
/// §6.7): its message ID, its questions echoed, the records without the cache-flush bit
/// and with TTLs of at most 10 s; the packets' records, each once, in as few datagrams as
/// fit [`LEGACY_MOST`] bytes (bug run K3-5: one datagram of 50–100 KB, past RFC 6762
/// §17's 9000, or not sent at all).
pub(crate) fn legacy_reply(q: &dns::Message, packets: &[Vec<u8>]) -> Vec<Vec<u8>> {
    // Its first question echoed (a one-shot resolver asks one; bug run L3-2: 64 long ones
    // repeated in every datagram, ~190 of 9 KB back for one query).
    let questions: Vec<(Name, u16, u16)> = q.questions.iter().take(1).cloned().collect();
    let mut q = q.clone();
    q.questions = questions;
    let q = &q;
    let mut records: Vec<Record> = Vec::new();
    for r in packets
        .iter()
        .filter_map(|p| dns::parse(p))
        .flat_map(|m| m.records)
    {
        let mut r = r;
        r.class &= !dns::CLASS_TOP_BIT;
        r.ttl = r.ttl.min(10);
        if !records.contains(&r) {
            records.push(r);
        }
    }
    let mut out = Vec::new();
    let mut part: Vec<Record> = Vec::new();
    for r in records {
        part.push(r);
        if part.len() > 1
            && dns::encode_response_with_questions(q.id, &q.questions, &part, &[]).len()
                > LEGACY_MOST
        {
            let last = part.pop().expect("two or more");
            out.push(dns::encode_response_with_questions(
                q.id,
                &q.questions,
                &part,
                &[],
            ));
            part = vec![last];
        }
    }
    if !part.is_empty() {
        out.push(dns::encode_response_with_questions(
            q.id,
            &q.questions,
            &part,
            &[],
        ));
    }
    // At most LEGACY_DATAGRAMS (a one-shot resolver reads the first).
    out.truncate(LEGACY_DATAGRAMS);
    out
}

/// The most datagrams of one legacy reply.
pub(crate) const LEGACY_DATAGRAMS: usize = 4;
/// The most bytes of unicast replies a second, all askers together (L3-2).
pub(crate) const UNICAST_BYTES_PER_SECOND: usize = 256 << 10;

/// The largest legacy reply datagram (RFC 6762 §17).
pub(crate) const LEGACY_MOST: usize = 8900;

/// Unicast replies (a QU question, a legacy query) to one source and question at most
/// once a second, and at most this many a second in all (bug run K3-4: a spoofed 45-byte
/// query brought 21 KB to anyone, as often as asked).
pub(crate) const UNICAST_MOST_PER_SECOND: usize = 64;

/// `services`' records (`goodbye`: TTL 0) in datagrams of at most [`PACKET_MOST`]
/// bytes, each service's records together; the host's address in the last one.
pub(crate) fn announcement(
    services: &[Service],
    host: &str,
    ip: Ipv4Addr,
    goodbye: bool,
    with_host: bool,
) -> Vec<Vec<u8>> {
    let t = |s: &Service| if goodbye { 0 } else { s.ttl };
    let mut groups: Vec<(Vec<Record>, Vec<Record>)> = services
        .iter()
        .map(|s| (vec![ptr(s, t(s)), srv(s, host, t(s)), txt(s, t(s))], vec![]))
        .collect();
    if with_host && !services.is_empty() {
        groups.push((vec![a(host, ip, if goodbye { 0 } else { 120 })], vec![]));
    }
    pack(&groups, &[])
}

/// A socket on the mDNS port (shared), the group joined on the interface `ip`.
pub fn listener(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    // Only the group joined here, on `ip`'s interface (bug run K3-1: on Linux every
    // interface's mDNS came in where any program had joined it, and devices' addresses
    // flipped between networks).
    #[cfg(target_os = "linux")]
    s.set_multicast_all_v4(false)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    s.join_multicast_v4(&GROUP, &ip)?;
    s.set_multicast_if_v4(&ip)?;
    s.set_multicast_loop_v4(true)?;
    s.set_multicast_ttl_v4(255)?;
    s.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(s.into())
}

impl Responder {
    pub fn start(ip: Ipv4Addr, host: String, services: Vec<Service>) -> io::Result<Responder> {
        let socket = Arc::new(listener(ip)?);
        let shared = Arc::new(Mutex::new((services, host)));
        let reserved: Arc<Mutex<Vec<Ipv4Addr>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let taps: Taps = Arc::default();
        let r = Responder {
            shared: shared.clone(),
            taps: taps.clone(),
            reserved: reserved.clone(),
            socket: socket.clone(),
            ip,
            stop: stop.clone(),
            pending: Arc::default(),
        };
        r.announce(false);
        let (sh, so, st, pend) = (
            shared.clone(),
            socket.clone(),
            stop.clone(),
            r.pending.clone(),
        );
        std::thread::Builder::new()
            .name("idn-aoip-mdns".into())
            .spawn(move || {
                let mut buf = vec![0u8; 9000];
                let mut recent: std::collections::HashMap<String, std::time::Instant> =
                    Default::default();
                let mut announced_again = false;
                let started = std::time::Instant::now();
                // Unicast replies sent in the last second: to whom, for what (K3-4).
                let mut unicast_sent: Vec<(std::time::Instant, SocketAddr, String, usize)> =
                    Vec::new();
                while !st.load(Ordering::Relaxed) {
                    if !announced_again && started.elapsed() >= Duration::from_secs(1) {
                        announced_again = true;
                        let (svc, host) = &*sh.lock_ok();
                        for d in announcement(svc, host, ip, false, true) {
                            send(&so, &d, None);
                        }
                    }
                    // Changes' announcements, as the services are now.
                    let due = pend.lock_ok().due(std::time::Instant::now());
                    if let Some(names) = due {
                        let (svc, host) = &*sh.lock_ok();
                        announce_named(&so, svc, host, ip, &names);
                    }
                    let Ok((n, from)) = so.recv_from(&mut buf) else {
                        continue;
                    };
                    let Some(q) = dns::parse(&buf[..n]) else {
                        continue;
                    };
                    // A response: for whoever in the program asked to hear them.
                    if q.is_response() {
                        if let std::net::IpAddr::V4(f) = from.ip() {
                            let mut t = taps.lock_ok();
                            t.retain(|tx| {
                                !matches!(
                                    tx.try_send((q.clone(), f)),
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_))
                                )
                            });
                        }
                        continue;
                    }
                    // A one-shot ("legacy") query from another port than 5353 (RFC 6762
                    // §6.7): answered to it, with its message ID.
                    let legacy = from.port() != PORT;
                    // A reply to this asker alone: at most once a second for the same
                    // questions, and a limited number a second in all (K3-4).
                    let now = std::time::Instant::now();
                    unicast_sent
                        .retain(|(t, _, _, _)| now.duration_since(*t) < Duration::from_secs(1));
                    let asked = format!("{:?}", q.questions).to_ascii_lowercase();
                    // The same asker's repeats (a lookup asks again when an answer is lost:
                    // bug run L3-7) up to three a second; a byte budget for all (L3-2).
                    let unicast_ok = unicast_sent.len() < UNICAST_MOST_PER_SECOND
                        && unicast_sent.iter().map(|x| x.3).sum::<usize>()
                            < UNICAST_BYTES_PER_SECOND
                        && unicast_sent
                            .iter()
                            .filter(|(_, f, a, _)| *f == from && *a == asked)
                            .count()
                            < 3;
                    let mut unicast_used = false;
                    let mut unicast_bytes = 0usize;
                    let held = reserved.lock_ok().clone();
                    if let Some((resp, unicast)) = answer_reservation(&q, &held) {
                        if legacy {
                            if unicast_ok {
                                unicast_used = true;
                                for d in legacy_reply(&q, &[resp]) {
                                    {
                                        unicast_bytes += d.len();
                                        send(&so, &d, Some(from));
                                    }
                                }
                            }
                        } else if !unicast {
                            send(&so, &resp, None);
                        } else if unicast_ok {
                            unicast_used = true;
                            {
                                unicast_bytes += resp.len();
                                send(&so, &resp, Some(from));
                            }
                        }
                    }
                    let (svc, host) = &*sh.lock_ok();
                    // A one-shot query: answered to it alone, every time (RFC 6762 §6.7;
                    // bug run J3-3: the once-a-second filter held some back).
                    if legacy {
                        if unicast_ok && let Some((resp, _)) = answer(&q, svc, host, ip) {
                            unicast_used = true;
                            for d in legacy_reply(&q, &resp) {
                                {
                                    unicast_bytes += d.len();
                                    send(&so, &d, Some(from));
                                }
                            }
                        }
                        if unicast_used {
                            unicast_sent.push((now, from, asked, unicast_bytes));
                        }
                        continue;
                    }
                    // A multicast answer to the same question at most once a second (RFC
                    // 6762 §6; bug run I3-3); unicast ones go to the asker only.
                    recent.retain(|_, t: &mut std::time::Instant| {
                        now.duration_since(*t) < Duration::from_secs(1)
                    });
                    let mut q = q;
                    let qu = q
                        .questions
                        .iter()
                        .any(|(_, _, c)| c & dns::CLASS_TOP_BIT != 0);
                    if !qu {
                        q.questions.retain(|(n, t, _)| {
                            let k = format!("{n:?}/{t}").to_ascii_lowercase();
                            !recent.contains_key(&k)
                        });
                    }
                    if let Some((resp, unicast)) = answer(&q, svc, host, ip) {
                        if !unicast {
                            for (n, t, _) in q.questions.iter().take(QUESTIONS_MOST) {
                                recent.insert(format!("{n:?}/{t}").to_ascii_lowercase(), now);
                            }
                            for d in resp {
                                send(&so, &d, None);
                            }
                        } else if unicast_ok {
                            unicast_used = true;
                            for d in resp {
                                {
                                    unicast_bytes += d.len();
                                    send(&so, &d, Some(from));
                                }
                            }
                        }
                    }
                    if unicast_used {
                        unicast_sent.push((now, from, asked, unicast_bytes));
                    }
                }
            })?;
        Ok(r)
    }

    /// The responses heard on its socket, from now on, and the socket (bug run I3-12).
    pub fn tap(&self) -> Heard {
        // Room for every device's channel answers at once while the browser resolves
        // (bug run J3-8: goodbyes were dropped from a 256-deep tap).
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        self.taps.lock_ok().push(tx);
        Heard {
            rx,
            socket: self.socket.clone(),
        }
    }

    pub(crate) fn announce(&self, goodbye: bool) {
        let (svc, host) = &*self.shared.lock_ok();
        for d in announcement(svc, host, self.ip, goodbye, true) {
            send(&self.socket, &d, None);
        }
    }

    /// The services now (a rename, a channel renamed, a flow): goodbyes for the instances
    /// that went, the new and changed ones announced (their SRV and TXT flush the old),
    /// twice a second apart (RFC 6762 §8.3). An instance whose TXT changed (a channel
    /// joining a multicast flow) isn't said goodbye to (bug run I3-4: controllers saw the
    /// channel go and come back).
    pub fn set(&self, services: Vec<Service>, host: String) {
        let same_host;
        // The new services stored in the same hold as the old ones are read, before the
        // goodbyes: the responder's thread can't answer from the old ones after them (bug
        // run N2-3: a renamed instance announced again just after its goodbye).
        let (gone, new) = {
            let mut held = self.shared.lock_ok();
            let (old, old_host) = std::mem::replace(&mut *held, (services.clone(), host.clone()));
            same_host = old_host == host;
            let key = |s: &Service| instance_name(s);
            let gone: Vec<Service> = old
                .iter()
                .filter(|s| !same_host || !services.iter().any(|n| dns::name_eq(&key(n), &key(s))))
                .cloned()
                .collect();
            let new: Vec<Service> = services
                .iter()
                .filter(|s| !same_host || !old.contains(s))
                .cloned()
                .collect();
            ((gone, old_host), new)
        };
        // The old host name's address goodbye only when the host name changed.
        for d in announcement(&gone.0, &gone.1, self.ip, true, !same_host) {
            send(&self.socket, &d, None);
        }
        // Announced by the responder's thread as they are when it sends (K1-4, K3-3).
        let names: Vec<String> = new
            .iter()
            .map(|s| format!("{:?}", instance_name(s)))
            .collect();
        if !names.is_empty() {
            Pending::add(&mut self.pending.lock_ok().now, names);
        }
    }
}

impl Responder {
    /// Holds `group` for this device (§2.6): its `in-addr.local` A record answered and
    /// announced.
    pub fn reserve(&self, group: Ipv4Addr) {
        let mut r = self.reserved.lock_ok();
        if !r.contains(&group) {
            r.push(group);
        }
        send(
            &self.socket,
            &dns::encode_response(0, &[reservation(group, 120)], &[]),
            None,
        );
    }

    /// Lets `group` go (a goodbye).
    pub fn release(&self, group: Ipv4Addr) {
        self.reserved.lock_ok().retain(|g| *g != group);
        send(
            &self.socket,
            &dns::encode_response(0, &[reservation(group, 0)], &[]),
            None,
        );
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Nothing answered after the goodbyes (bug run K3-9: the receive thread answered
        // with live records once more, and caches kept the device).
        let (svc, host) = std::mem::take(&mut *self.shared.lock_ok());
        for d in announcement(&svc, &host, self.ip, true, true) {
            send(&self.socket, &d, None);
        }
        // The groups it held go too (bug run I3-11).
        let held = std::mem::take(&mut *self.reserved.lock_ok());
        if !held.is_empty() {
            let recs: Vec<Record> = held.iter().map(|g| reservation(*g, 0)).collect();
            send(&self.socket, &dns::encode_response(0, &recs, &[]), None);
        }
    }
}
