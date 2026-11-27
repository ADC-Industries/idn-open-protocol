//! Lookups: an instance resolved, many at once, a multicast group's holder (§2.6).

use super::*;

/// What a lookup found: the SRV target's address and port, and the TXT strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub addr: SocketAddr,
    pub txt: Vec<String>,
}

/// The records of a response that answer `instance` (its SRV, TXT and the target's A).
pub fn resolved_from(m: &dns::Message, instance: &Name, from: Ipv4Addr) -> Option<Resolved> {
    let mut srv = None;
    let mut txt = None;
    for r in &m.records {
        if !dns::name_eq(&r.name, instance) || r.ttl == 0 {
            continue;
        }
        match &r.data {
            RData::Srv { port, target, .. } => srv = Some((*port, target.clone())),
            RData::Txt(t) => {
                txt = Some(
                    t.iter()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .collect(),
                )
            }
            _ => {}
        }
    }
    let (port, target) = srv?;
    let ip = m
        .records
        .iter()
        .find_map(|r| match r.data {
            RData::A(a) if dns::name_eq(&r.name, &target) => Some(a),
            _ => None,
        })
        .unwrap_or(from);
    Some(Resolved {
        addr: SocketAddr::from((ip, port)),
        txt: txt.unwrap_or_default(),
    })
}

/// Looks up `<instance>.<kind>._udp.local` on the interface `ip` (§6.5): asked for its
/// SRV and TXT with the QU bit, three times within `timeout`.
pub fn resolve(ip: Ipv4Addr, instance: &str, kind: &str, timeout: Duration) -> Option<Resolved> {
    let mut name = vec![instance.to_owned()];
    name.extend(type_name(kind));
    // The query out on the interface (bound to its address, multicast through it).
    let s = {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
        s.set_multicast_if_v4(&ip).ok()?;
        s.bind(&SocketAddr::from((ip, 0)).into()).ok()?;
        UdpSocket::from(s)
    };
    let q = dns::encode_query(
        0,
        &[(name.clone(), dns::TYPE_SRV), (name.clone(), dns::TYPE_TXT)],
        true,
    );
    let until = std::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 9000];
    let mut asked = 0;
    while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
        if asked < 3 {
            asked += 1;
            send(&s, &q, None);
        }
        let _ = s.set_read_timeout(Some(left.min(timeout / 3).max(Duration::from_millis(10))));
        if let Ok((n, from)) = s.recv_from(&mut buf)
            && let Some(m) = dns::parse(&buf[..n]).filter(dns::Message::is_response)
            && let std::net::IpAddr::V4(f) = from.ip()
            && let Some(r) = resolved_from(&m, &name, f)
        {
            return Some(r);
        }
    }
    None
}

/// Looks up many instances at once (`(instance, kind)`), within `timeout`: their questions
/// sent together (five instances a packet), asked again up to three times for those not
/// yet answered, every answer heard taken. A receiver with 64 subscriptions looked them
/// up one after another, 0.6 s each, so a new one waited most of a minute (lab 528(b)).
pub fn resolve_many(
    ip: Ipv4Addr,
    wanted: &[(String, String)],
    timeout: Duration,
) -> Vec<Option<Resolved>> {
    let mut out: Vec<Option<Resolved>> = vec![None; wanted.len()];
    if wanted.is_empty() {
        return out;
    }
    let names: Vec<Name> = wanted
        .iter()
        .map(|(i, k)| {
            let mut n = vec![i.clone()];
            n.extend(type_name(k));
            n
        })
        .collect();
    let s = match (|| -> io::Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_multicast_if_v4(&ip)?;
        s.bind(&SocketAddr::from((ip, 0)).into())?;
        Ok(UdpSocket::from(s))
    })() {
        Ok(s) => s,
        Err(_) => return out,
    };
    let until = std::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 9000];
    for round in 0..3u32 {
        let open: Vec<usize> = (0..names.len()).filter(|&i| out[i].is_none()).collect();
        if open.is_empty() {
            break;
        }
        for chunk in open.chunks(5) {
            let qs: Vec<(Name, u16)> = chunk
                .iter()
                .flat_map(|&i| {
                    [
                        (names[i].clone(), dns::TYPE_SRV),
                        (names[i].clone(), dns::TYPE_TXT),
                    ]
                })
                .collect();
            send(&s, &dns::encode_query(0, &qs, true), None);
        }
        let round_end = std::time::Instant::now() + timeout / 3;
        let end = if round == 2 {
            until
        } else {
            round_end.min(until)
        };
        while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
            let _ = s.set_read_timeout(Some(left.max(Duration::from_millis(1))));
            let Ok((n, from)) = s.recv_from(&mut buf) else {
                break;
            };
            let (Some(m), std::net::IpAddr::V4(f)) = (dns::parse(&buf[..n]), from.ip()) else {
                continue;
            };
            if !m.is_response() {
                continue;
            }
            for (i, name) in names.iter().enumerate() {
                if out[i].is_none() {
                    out[i] = resolved_from(&m, name, f);
                }
            }
            if out.iter().all(Option::is_some) {
                return out;
            }
        }
    }
    out
}

/// The name a multicast group is held by (§2.6): `<d>.<c>.<b>.<a>.in-addr.local`.
pub fn reservation_name(g: Ipv4Addr) -> Name {
    let o = g.octets();
    let mut n: Name = o.iter().rev().map(|b| b.to_string()).collect();
    n.extend(["in-addr".to_owned(), "local".to_owned()]);
    n
}

pub(crate) fn reservation(g: Ipv4Addr, ttl: u32) -> Record {
    Record {
        name: reservation_name(g),
        rtype: dns::TYPE_A,
        class: UNIQUE,
        ttl,
        data: RData::A(g),
    }
}

/// The name of the marker a device adds to its group answers, so its own check after
/// reserving can leave its own answer out (§2.6: other implementations have a marker of their own;
/// this is IDN's, with the same purpose).
pub(crate) fn origin_name() -> Name {
    vec!["_idn-aoip-origin".into(), "local".into()]
}

/// This process's marker: unique to it among the devices on one address.
pub fn origin() -> String {
    static ORIGIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ORIGIN
        .get_or_init(|| {
            let t = idn_common::epoch::now().as_nanos();
            format!("{:x},{}", t, std::process::id())
        })
        .clone()
}

/// The answer to a question for a group this device holds, with its marker.
pub(crate) fn answer_reservation(q: &dns::Message, held: &[Ipv4Addr]) -> Option<(Vec<u8>, bool)> {
    if q.is_response() || held.is_empty() {
        return None;
    }
    let mut answers = Vec::new();
    let mut unicast = false;
    for (name, qtype, qclass) in &q.questions {
        for g in held {
            if dns::name_eq(name, &reservation_name(*g))
                && (*qtype == dns::TYPE_A || *qtype == dns::TYPE_ANY)
            {
                answers.push(reservation(*g, 120));
                unicast |= qclass & dns::CLASS_TOP_BIT != 0;
            }
        }
    }
    let marker = Record {
        name: origin_name(),
        rtype: dns::TYPE_TXT,
        class: dns::CLASS_IN,
        ttl: 120,
        data: RData::Txt(vec![origin().into_bytes()]),
    };
    (!answers.is_empty()).then(|| (dns::encode_response(0, &answers, &[marker]), unicast))
}

/// Whether another device holds `group` (§2.6): its `in-addr.local` name asked for three
/// times, 400 ms apart, on the interface `ip`; this process's own answer (its marker) left
/// out, so the check after reserving (§2.6 step 3) sees only others.
pub fn group_taken(ip: Ipv4Addr, group: Ipv4Addr) -> bool {
    let own = origin().into_bytes();
    let name = reservation_name(group);
    let Ok(s) = (|| -> io::Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_multicast_if_v4(&ip)?;
        s.bind(&SocketAddr::from((ip, 0)).into())?;
        Ok(UdpSocket::from(s))
    })() else {
        return false;
    };
    let q = dns::encode_query(0, &[(name.clone(), dns::TYPE_A)], true);
    let mut buf = vec![0u8; 2048];
    for _ in 0..3 {
        send(&s, &q, None);
        let until = std::time::Instant::now() + Duration::from_millis(400);
        while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
            let _ = s.set_read_timeout(Some(left.max(Duration::from_millis(1))));
            let Ok((n, _)) = s.recv_from(&mut buf) else {
                break;
            };
            if dns::parse(&buf[..n]).is_some_and(|m| {
                let mine = m.records.iter().any(|r| {
                    dns::name_eq(&r.name, &origin_name())
                        && matches!(&r.data, RData::Txt(v) if v.first() == Some(&own))
                });
                m.is_response()
                    && !mine
                    && m.records.iter().any(|r| {
                        r.ttl > 0 && r.rtype == dns::TYPE_A && dns::name_eq(&r.name, &name)
                    })
            }) {
                return true;
            }
        }
    }
    false
}

/// Sends `d` to the mDNS group (or `to`).
pub fn send(s: &UdpSocket, d: &[u8], to: Option<SocketAddr>) {
    let to = to.unwrap_or(SocketAddr::V4(SocketAddrV4::new(GROUP, PORT)));
    let _ = s.send_to(d, to);
}
