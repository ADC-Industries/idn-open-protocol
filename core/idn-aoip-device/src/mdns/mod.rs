//! The device's mDNS (§2): its services' records, answers to queries (unicast when asked
//! with the QU bit), an announcement at start and goodbyes (TTL 0) at exit and when a
//! service goes (§2.1's advice: as a normal responder does).

use crate::device::Device;
use idn_open_protocol::discovery::dns::{self, Name, RData, Record};
use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod lookup;
mod records;
mod responder;
pub use lookup::*;
pub use records::*;
pub use responder::*;

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Identity;

    fn dev() -> Device {
        let mut d = Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            2,
            2,
        );
        d.tx[0].name = "Main L".into();
        d
    }

    #[test]
    fn the_services() {
        let s = services(&dev(), Ports::block(None), &[(2, 1, 2)]);
        let kinds: Vec<(&str, &str)> = s
            .iter()
            .map(|x| (x.kind.as_str(), x.instance.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("_netaudio-arc", "Booth-IDN"),
                ("_netaudio-cmc", "Booth-IDN"),
                ("_netaudio-chan", "01@Booth-IDN"),
                ("_netaudio-chan", "Main L@Booth-IDN"),
                ("_netaudio-chan", "02@Booth-IDN"),
                ("_netaudio-chan", "TX 2@Booth-IDN"),
            ]
        );
        assert_eq!(s[0].port, 4440);
        assert_eq!(s[0].ttl, 4500);
        assert!(s[1].txt.contains(&"id=00000a0000070000".to_owned()));
        let chan = &s[2];
        assert_eq!(chan.port, 4455);
        assert!(chan.txt.contains(&"pcm=3 e".to_owned()));
        assert!(chan.txt.contains(&"default".to_owned()));
        assert!(!s[3].txt.contains(&"default".to_owned()));
        assert!(s[4].txt.contains(&"b.1=2".to_owned()));
        assert_eq!(Ports::block(Some(29240)).settings, 29243);
    }

    #[test]
    fn a_lookup_reads_the_answer() {
        let svc = services(&dev(), Ports::block(None), &[]);
        let ip = Ipv4Addr::new(10, 0, 0, 7);
        let inst = instance_name(&svc[3]);
        let q = dns::parse(&dns::encode_query(
            2,
            &[(inst.clone(), dns::TYPE_SRV), (inst.clone(), dns::TYPE_TXT)],
            true,
        ))
        .unwrap();
        let (resp, _) = answer(&q, &svc, "Booth-IDN", ip).unwrap();
        let r = resolved_from(&dns::parse(&resp[0]).unwrap(), &inst, Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(r.addr, SocketAddr::from((ip, 4455)));
        assert!(r.txt.contains(&"id=1".to_owned()));
        let info = crate::receiver::ChanInfo::from_txt(r.addr, &r.txt).unwrap();
        assert_eq!((info.id, info.nchan, info.bits), (1, 2, 24));
    }

    #[test]
    fn queries_answered() {
        let svc = services(&dev(), Ports::block(None), &[]);
        let ip = Ipv4Addr::new(10, 0, 0, 7);
        let q = dns::parse(&dns::encode_query(
            1,
            &[(type_name("_netaudio-arc"), dns::TYPE_PTR)],
            false,
        ))
        .unwrap();
        let (resp, unicast) = answer(&q, &svc, "Booth-IDN", ip).unwrap();
        assert!(!unicast);
        let m = dns::parse(&resp[0]).unwrap();
        assert!(m.is_response());
        assert!(
            m.records
                .iter()
                .any(|r| matches!(&r.data, RData::Ptr(n) if n[0] == "Booth-IDN"))
        );
        assert!(
            m.records
                .iter()
                .any(|r| matches!(r.data, RData::A(a) if a == ip))
        );
        assert!(
            m.records
                .iter()
                .any(|r| matches!(&r.data, RData::Srv { port: 4440, .. }))
        );
        // A channel's SRV, asked with the QU bit: unicast, port 4455.
        let inst = instance_name(&svc[3]);
        let q = dns::parse(&dns::encode_query(2, &[(inst, dns::TYPE_SRV)], true)).unwrap();
        let (resp, unicast) = answer(&q, &svc, "Booth-IDN", ip).unwrap();
        assert!(unicast);
        assert!(
            dns::parse(&resp[0])
                .unwrap()
                .records
                .iter()
                .any(|r| matches!(&r.data, RData::Srv { port: 4455, .. }))
        );
        // Not ours.
        let q = dns::parse(&dns::encode_query(
            3,
            &[(type_name("_ndi"), dns::TYPE_PTR)],
            false,
        ))
        .unwrap();
        assert!(answer(&q, &svc, "Booth-IDN", ip).is_none());
        // Goodbyes have TTL 0.
        assert!(
            all_records(&svc, "Booth-IDN", ip, true)
                .iter()
                .all(|r| r.ttl == 0)
        );
        assert_eq!(host_label("Booth IDN!"), "Booth-IDN-");
    }

    /// Bug run I3-1 / I3-3: 128 renamed channels each way and a query repeating one
    /// question: every answer there once, each datagram within PACKET_MOST.
    #[test]
    fn big_answers_split_and_questions_once() {
        let mut d = Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            128,
            128,
        );
        for (i, c) in d.tx.iter_mut().enumerate() {
            c.name = format!("A-long-channel-name-{i:03}-xxxxx");
        }
        let svc = services(&d, Ports::block(None), &[]);
        let ip = Ipv4Addr::new(10, 0, 0, 7);
        let chan = type_name("_netaudio-chan");
        let qs: Vec<_> = (0..500).map(|_| (chan.clone(), dns::TYPE_PTR)).collect();
        let q = dns::parse(&dns::encode_query(1, &qs, false)).unwrap();
        let (resp, _) = answer(&q, &svc, "Booth-IDN", ip).unwrap();
        assert!(resp.len() > 1);
        let mut ptrs = 0;
        for p in &resp {
            assert!(p.len() <= PACKET_MOST, "{}", p.len());
            let m = dns::parse(p).unwrap();
            ptrs += m
                .records
                .iter()
                .filter(|r| r.rtype == dns::TYPE_PTR)
                .count();
            // Each PTR with its SRV and TXT in the same datagram.
            for r in m.records.iter().filter(|r| r.rtype == dns::TYPE_PTR) {
                let RData::Ptr(inst) = &r.data else { panic!() };
                assert!(
                    m.records
                        .iter()
                        .any(|s| s.rtype == dns::TYPE_SRV && dns::name_eq(&s.name, inst))
                );
            }
        }
        let want = svc.iter().filter(|s| s.kind == "_netaudio-chan").count();
        assert_eq!(ptrs, want);
        // Announcements and goodbyes split the same way.
        for p in announcement(&svc, "Booth-IDN", ip, true, true) {
            assert!(p.len() <= PACKET_MOST);
        }
        // A legacy reply: every record once, in datagrams within LEGACY_MOST (K3-5).
        let legacy = legacy_reply(&q, &resp);
        assert!(legacy.len() > 1);
        let mut once = std::collections::BTreeSet::new();
        for p in &legacy {
            assert!(p.len() <= LEGACY_MOST + 600, "{}", p.len());
            let m = dns::parse(p).unwrap();
            assert_eq!(m.id, q.id);
            for r in m.records {
                assert!(once.insert(format!("{r:?}")), "twice: {r:?}");
            }
        }
    }

    /// A PTR the asker already has (half our TTL left or more) isn't sent again (K3-6).
    #[test]
    fn known_answers_held_back() {
        let svc = services(&dev(), Ports::block(None), &[]);
        let ip = Ipv4Addr::new(10, 0, 0, 7);
        let arc = type_name("_netaudio-arc");
        let mine = ptr(&svc[0], svc[0].ttl);
        let ask = |ttl: u32| {
            let mut k = mine.clone();
            k.ttl = ttl;
            dns::parse(&query_with_known(&[(arc.clone(), dns::TYPE_PTR)], &[k])).unwrap()
        };
        let q = ask(svc[0].ttl);
        assert!(!q.is_response());
        assert!(answer(&q, &svc, "Booth-IDN", ip).is_none());
        // Less than half left: answered.
        assert!(answer(&ask(svc[0].ttl / 4), &svc, "Booth-IDN", ip).is_some());
    }

    /// Announcements wait their turn: a burst at most every BURST_EVERY, each name again a
    /// second on, once (K1-4, K3-3).
    #[test]
    fn announcements_merge_and_wait() {
        let t0 = std::time::Instant::now();
        let mut p = Pending::default();
        Pending::add(&mut p.now, ["a".to_owned(), "b".to_owned()]);
        Pending::add(&mut p.now, ["A".to_owned()]);
        assert_eq!(p.due(t0), Some(vec!["a".to_owned(), "b".to_owned()]));
        Pending::add(&mut p.now, ["c".to_owned()]);
        assert_eq!(p.due(t0 + Duration::from_millis(100)), None);
        assert_eq!(
            p.due(t0 + Duration::from_millis(300)),
            Some(vec!["c".to_owned()])
        );
        assert_eq!(p.due(t0 + Duration::from_millis(600)), None);
        let again = p.due(t0 + Duration::from_millis(1100)).unwrap();
        assert_eq!(again.len(), 3);
        // Twice in all, not again.
        assert_eq!(p.due(t0 + Duration::from_millis(5000)), None);
    }
}
