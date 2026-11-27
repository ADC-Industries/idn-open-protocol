//! A device's services (§2.2–§2.5) and their records, packed into datagrams.

use super::*;

/// The device's ports (§1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub arc: u16,
    pub cmc: u16,
    pub flow: u16,
    pub settings: u16,
}

impl Ports {
    /// The defaults (§1.3), or a block from `base`: ARC, CMC, flow control, settings.
    pub fn block(base: Option<u16>) -> Ports {
        match base {
            Some(n) => Ports {
                arc: n,
                cmc: n + 1,
                flow: n + 2,
                settings: n + 3,
            },
            None => Ports {
                arc: 4440,
                cmc: 8800,
                flow: 4455,
                settings: 8700,
            },
        }
    }
}

/// One DNS-SD service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Service {
    /// `_netaudio-arc` and so on (then `._udp.local`).
    pub kind: String,
    pub instance: String,
    pub port: u16,
    pub ttl: u32,
    pub txt: Vec<String>,
}

/// The device's host label: its name with what a host label can't hold made `-`.
pub fn host_label(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() { "idn-aoip".into() } else { s }
}

/// The ARC, CMC and transmit-channel services of a device (§2.2–§2.4). `bundles`: per
/// transmit channel (1-based) the multicast flow and position it is in, if any.
pub fn services(dev: &Device, ports: Ports, bundles: &[(usize, u16, usize)]) -> Vec<Service> {
    let id = &dev.identity;
    let name = &id.name;
    let mut out = vec![
        Service {
            kind: "_netaudio-arc".into(),
            instance: name.clone(),
            port: ports.arc,
            ttl: 4500,
            txt: vec![
                "arcp_vers=2.7.41".into(),
                "arcp_min=0.2.4".into(),
                "router_vers=4.0.2".into(),
                format!("router_info={}", id.board_name),
                format!("mf={}", id.manufacturer),
                format!("model={}", id.model_number),
            ],
        },
        Service {
            kind: "_netaudio-cmc".into(),
            instance: name.clone(),
            port: ports.cmc,
            ttl: 120,
            txt: vec![
                format!("id={}", id.device_id_hex()),
                format!("process={}", id.process_id),
                "cmcp_vers=1.2.0".into(),
                "cmcp_min=1.0.0".into(),
                "server_vers=4.0.2".into(),
                "channels=0x6000004d".into(),
                format!("mf={}", id.manufacturer),
                format!("model={}", id.model_number),
                String::new(),
                String::new(),
            ],
        },
    ];
    let f = dev.format;
    let bytes = f.bits / 8;
    let nchan = dev.tx.len().min(8);
    for (i, ch) in dev.tx.iter().enumerate() {
        let n = i + 1;
        let mut txt = vec![
            "txtvers=2".into(),
            "dbcp1=0x1102".into(),
            "dbcp=0x1004".into(),
            format!("id={n}"),
            format!("rate={}", f.rate),
            format!("pcm={bytes} {:x}", f.pcm_type),
            format!("enc={}", f.bits),
            format!("en={}", f.bits),
            // The transmit latency is the right value here (§2.4).
            format!("latency_ns={}", dev.tx_latency_ns),
            "fpp=32,2".into(),
            format!("nchan={nchan}"),
        ];
        for (c, flow, pos) in bundles {
            if *c == n {
                txt.push(format!("b.{flow}={pos}"));
            }
        }
        let factory = Device::factory_channel_name(n);
        let mut with_default = txt.clone();
        with_default.push("default".into());
        out.push(Service {
            kind: "_netaudio-chan".into(),
            instance: format!("{factory}@{name}"),
            port: ports.flow,
            ttl: 120,
            txt: with_default,
        });
        if ch.name != factory {
            out.push(Service {
                kind: "_netaudio-chan".into(),
                instance: format!("{}@{name}", ch.name),
                port: ports.flow,
                ttl: 120,
                txt,
            });
        }
    }
    // Each multicast transmit flow (§2.5).
    for (i, fl) in dev.tx_flows.flows.iter().enumerate() {
        // Only once its group is checked and held (bug run K1-2).
        let Some(fl) = fl.as_ref().filter(|f| f.multicast && f.held) else {
            continue;
        };
        let n = i + 1;
        out.push(Service {
            kind: "_netaudio-bund".into(),
            instance: format!("{n}@{name}"),
            port: ports.flow,
            ttl: 120,
            txt: vec![
                "txtvers=1".into(),
                format!("id={n}"),
                format!("nchan={}", fl.channels.len()),
                format!("latency_ns={}", dev.tx_latency_ns),
                // One number here: the flow's (§2.5; "largest,smallest" is the channel
                // services' form, and a receiver reading one integer never joined: lab
                // 530(a)).
                format!("fpp={}", fl.fpp),
                format!("rate={}", f.rate),
                format!("enc={}", f.bits),
                format!("a.0={}", fl.to.0),
                format!("p.0={}", fl.to.1),
            ],
        });
    }
    out
}

/// `<kind>._udp.local`.
pub fn type_name(kind: &str) -> Name {
    vec![kind.to_owned(), "_udp".into(), "local".into()]
}

pub(crate) fn instance_name(s: &Service) -> Name {
    let mut n = vec![s.instance.clone()];
    n.extend(type_name(&s.kind));
    n
}

pub(crate) fn host_name(host: &str) -> Name {
    vec![host.to_owned(), "local".into()]
}

/// The class of a unique record: the cache-flush bit set (§2.1).
pub(crate) const UNIQUE: u16 = dns::CLASS_IN | dns::CLASS_TOP_BIT;

pub(crate) fn ptr(s: &Service, ttl: u32) -> Record {
    Record {
        name: type_name(&s.kind),
        rtype: dns::TYPE_PTR,
        class: dns::CLASS_IN,
        ttl,
        data: RData::Ptr(instance_name(s)),
    }
}

pub(crate) fn srv(s: &Service, host: &str, ttl: u32) -> Record {
    Record {
        name: instance_name(s),
        rtype: dns::TYPE_SRV,
        class: UNIQUE,
        ttl,
        data: RData::Srv {
            priority: 0,
            weight: 0,
            port: s.port,
            target: host_name(host),
        },
    }
}

pub(crate) fn txt(s: &Service, ttl: u32) -> Record {
    Record {
        name: instance_name(s),
        rtype: dns::TYPE_TXT,
        class: UNIQUE,
        ttl,
        data: RData::Txt(s.txt.iter().map(|t| t.as_bytes().to_vec()).collect()),
    }
}

pub(crate) fn a(host: &str, ip: Ipv4Addr, ttl: u32) -> Record {
    Record {
        name: host_name(host),
        rtype: dns::TYPE_A,
        class: UNIQUE,
        ttl,
        data: RData::A(ip),
    }
}

/// Every record of `services` (`ttl` 0: their goodbyes), the host's address last.
pub fn all_records(services: &[Service], host: &str, ip: Ipv4Addr, goodbye: bool) -> Vec<Record> {
    service_records(services, host, ip, goodbye, true)
}

/// As [`all_records`]; `with_host` false leaves the host's address record out (services
/// withdrawn while the device stays: its address isn't said goodbye to, lab 530(a)).
pub(crate) fn service_records(
    services: &[Service],
    host: &str,
    ip: Ipv4Addr,
    goodbye: bool,
    with_host: bool,
) -> Vec<Record> {
    let t = |s: &Service| if goodbye { 0 } else { s.ttl };
    let mut v = Vec::new();
    for s in services {
        v.push(ptr(s, t(s)));
        v.push(srv(s, host, t(s)));
        v.push(txt(s, t(s)));
    }
    if with_host && !services.is_empty() {
        v.push(a(host, ip, if goodbye { 0 } else { 120 }));
    }
    v
}

/// The most a reply or announcement datagram carries (RFC 6762 §17: an mDNS packet fits
/// the link's MTU unless it can't be split; bug run I3-1: one datagram of every record was
/// 21 KB at 64 channels and over 64 KB, never sent, at 128).
pub const PACKET_MOST: usize = 1400;
/// Questions of one query looked at (bug run I3-3: a 9 KB query repeating one question
/// 1300 times made megabyte replies).
pub(crate) const QUESTIONS_MOST: usize = 64;

/// Records in datagrams of at most [`PACKET_MOST`] bytes: each group (an answer and its
/// additional records) kept in one datagram, `extra` (the host's address) added to each.
pub fn pack(groups: &[(Vec<Record>, Vec<Record>)], extra: &[Record]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let (mut ans, mut add): (Vec<Record>, Vec<Record>) = (Vec::new(), extra.to_vec());
    for (g_ans, g_add) in groups {
        let (mut a2, mut d2) = (ans.clone(), add.clone());
        a2.extend(g_ans.iter().cloned());
        d2.extend(g_add.iter().cloned());
        if !ans.is_empty() && dns::encode_response(0, &a2, &d2).len() > PACKET_MOST {
            out.push(dns::encode_response(0, &ans, &add));
            ans = g_ans.clone();
            add = extra.to_vec();
            add.extend(g_add.iter().cloned());
        } else {
            (ans, add) = (a2, d2);
        }
    }
    if !ans.is_empty() {
        out.push(dns::encode_response(0, &ans, &add));
    }
    out
}
