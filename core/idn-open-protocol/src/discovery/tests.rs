//! Tests of the source list: TXT and listing parsing, the mDNS cache, change merging.

use super::dns::{
    CLASS_IN, CLASS_TOP_BIT, Name, RData, Record, TYPE_A, TYPE_PTR, TYPE_SRV, TYPE_TXT,
    encode_response, name,
};
use super::mdns::{GOODBYE_GRACE, MAX_HOSTS, MAX_INSTANCES, MdnsQuerier, mdns_interfaces};
use super::*;

#[test]
fn a_screen_that_takes_control_says_so_with_its_machine_and_site() {
    let hint = parse_txt_idn(&[
        b"x_idn_ext=resume/1 link/1 control/1 idnfp/0123456789abcdef idnca/fedcba9876543210"
            .to_vec(),
    ]);
    let s = Source {
        name: "DESK (Screen 1)".into(),
        addr: None,
        port: 1,
        host: String::new(),
        groups: vec![],
        routing: false,
        listing_port: None,
        idn_ext: hint,
    };
    assert!(s.takes_control());
    assert_eq!(s.announced("idnfp"), Some("0123456789abcdef"));
    assert_eq!(s.announced("idnca"), Some("fedcba9876543210"));
    let plain = Source {
        idn_ext: vec!["resume/1".into()],
        ..s
    };
    assert!(!plain.takes_control() && plain.announced("idnfp").is_none());
}

#[test]
fn txt_keys() {
    let (g, r, l) = parse_txt(&[
        b"groups=Studio A,Cam".to_vec(),
        b"discovery=5960".to_vec(),
        b"routing".to_vec(),
        b"x=y".to_vec(),
    ]);
    assert_eq!(g, ["Studio A", "Cam"]);
    assert!(r);
    assert_eq!(l, Some(5960));
    assert_eq!(parse_txt(&[]).0, ["public"]);
    assert!(parse_txt(&[b"groups=".to_vec()]).0.is_empty());
}

#[test]
fn group_filter() {
    let s = vec!["Studio A".to_owned(), "Cam".to_owned()];
    assert!(groups_match(&s, &["cam".to_owned()]));
    assert!(!groups_match(&s, &["public".to_owned()]));
    assert!(groups_match(
        &[" studio a".to_owned()],
        &["Studio A".to_owned()]
    ));
    assert!(!groups_match(&[], &["public".to_owned()]));
}

#[test]
fn listing_document() {
    let xml = r#"<ndi><services><service name="SND (TWO)" port="5962"><groups><group>public</group></groups></service><service name="SND (A&amp;B)" port="5961"><groups><group>x</group><group>y</group></groups></service></services></ndi>"#;
    let v = parse_listing(xml, None);
    assert_eq!(v.len(), 2);
    assert_eq!((v[0].name.as_str(), v[0].port), ("SND (TWO)", 5962));
    assert_eq!(v[1].name, "SND (A&B)");
    assert_eq!(v[1].groups, ["x", "y"]);
}

#[test]
fn the_cache_is_bounded() {
    let mut c = Cache::default();
    let ptr = |i: usize| Record {
        name: name(SERVICE),
        rtype: TYPE_PTR,
        class: CLASS_IN,
        ttl: 4500,
        data: RData::Ptr(name(&format!("FLOOD ({i})._ndi._tcp.local"))),
    };
    let a = |i: usize| Record {
        name: name(&format!("h{i}.local")),
        rtype: TYPE_A,
        class: CLASS_IN,
        ttl: 120,
        data: RData::A(Ipv4Addr::new(10, 0, (i >> 8) as u8, i as u8)),
    };
    for i in 0..MAX_INSTANCES + 100 {
        c.apply(&dns::parse(&encode_response(0, &[ptr(i), a(i)], &[])).unwrap());
    }
    assert_eq!(
        (c.instances.len(), c.hosts.len()),
        (MAX_INSTANCES, MAX_HOSTS)
    );
    // Known entries are still updated.
    c.apply(&dns::parse(&encode_response(0, &[a(0)], &[])).unwrap());
    assert!(c.hosts.contains_key("h0.local"));
}

#[test]
fn silent_sources_expire() {
    let inst = name("GONE (X)._ndi._tcp.local");
    let mut c = Cache::default();
    let rec = Record {
        name: name(SERVICE),
        rtype: TYPE_PTR,
        class: CLASS_IN,
        ttl: 4500,
        data: RData::Ptr(inst),
    };
    c.apply(&dns::parse(&encode_response(0, &[rec], &[])).unwrap());
    c.expire(Duration::from_secs(60));
    assert_eq!(c.instances.len(), 1);
    std::thread::sleep(Duration::from_millis(20));
    c.expire(Duration::from_millis(10));
    assert!(c.instances.is_empty());
}

#[test]
fn cache_resolves_and_goodbye() {
    let inst = name("SND (LAB)._ndi._tcp.local");
    let rec = |n: Name, t, ttl, d| Record {
        name: n,
        rtype: t,
        class: CLASS_IN | CLASS_TOP_BIT,
        ttl,
        data: d,
    };
    let mut c = Cache::default();
    c.apply(
        &dns::parse(&encode_response(
            0,
            &[rec(name(SERVICE), TYPE_PTR, 4500, RData::Ptr(inst.clone()))],
            &[],
        ))
        .unwrap(),
    );
    assert!(c.sources().is_empty());
    assert_eq!(c.missing().len(), 2);
    c.apply(
        &dns::parse(&encode_response(
            0,
            &[
                rec(
                    inst.clone(),
                    TYPE_SRV,
                    120,
                    RData::Srv {
                        priority: 0,
                        weight: 0,
                        port: 5962,
                        target: name("snd.local"),
                    },
                ),
                rec(
                    inst.clone(),
                    TYPE_TXT,
                    4500,
                    RData::Txt(vec![b"groups=public".to_vec(), b"discovery=5960".to_vec()]),
                ),
            ],
            &[rec(
                name("SND.local"),
                TYPE_A,
                120,
                RData::A(Ipv4Addr::new(10, 1, 2, 3)),
            )],
        ))
        .unwrap(),
    );
    let s = c.sources();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].name, "SND (LAB)");
    assert_eq!(s[0].socket_addr(), Some("10.1.2.3:5962".parse().unwrap()));
    assert!(c.missing().is_empty());
    c.apply(
        &dns::parse(&encode_response(
            0,
            &[rec(name(SERVICE), TYPE_PTR, 0, RData::Ptr(inst.clone()))],
            &[],
        ))
        .unwrap(),
    );
    assert!(c.sources().is_empty());
    // A response read after the goodbye but sent before it (other socket) must not
    // bring the source back (RFC 6762 §10.1 grace).
    let full = encode_response(
        0,
        &[
            rec(name(SERVICE), TYPE_PTR, 4500, RData::Ptr(inst.clone())),
            rec(
                inst.clone(),
                TYPE_SRV,
                120,
                RData::Srv {
                    priority: 0,
                    weight: 0,
                    port: 5962,
                    target: name("snd.local"),
                },
            ),
        ],
        &[],
    );
    c.apply(&dns::parse(&full).unwrap());
    assert!(c.sources().is_empty(), "stale answer revived the source");
    // A real comeback after the grace period is accepted.
    std::thread::sleep(GOODBYE_GRACE + Duration::from_millis(20));
    c.apply(&dns::parse(&full).unwrap());
    assert_eq!(c.sources().len(), 1);
}

#[test]
fn unread_changes_merge_per_source() {
    use std::collections::VecDeque;
    let src = |name: &str, port| Source {
        name: name.into(),
        addr: None,
        port,
        host: String::new(),
        groups: Vec::new(),
        routing: false,
        listing_port: None,
        idn_ext: Vec::new(),
    };
    let mut q = VecDeque::new();
    // A source flapping for hours while the app only polls sources(): one event.
    queue_change(&mut q, SourceEvent::Added(src("A (x)", 1)));
    for p in 2..10_000 {
        queue_change(&mut q, SourceEvent::Changed(src("a (X)", p)));
    }
    assert_eq!(q.len(), 1);
    assert_eq!(
        q[0],
        SourceEvent::Added(src("a (X)", 9_999)),
        "added, with the latest values"
    );
    // Added then removed: nothing to tell.
    queue_change(&mut q, SourceEvent::Removed(src("A (x)", 9_999)));
    assert!(q.is_empty());
    // Removed then added again: changed.
    queue_change(&mut q, SourceEvent::Removed(src("B (x)", 1)));
    queue_change(&mut q, SourceEvent::Added(src("B (x)", 2)));
    queue_change(&mut q, SourceEvent::Added(src("C (x)", 1)));
    assert_eq!(
        Vec::from(q),
        [
            SourceEvent::Changed(src("B (x)", 2)),
            SourceEvent::Added(src("C (x)", 1))
        ]
    );
}

#[test]
fn a_browser_picks_up_interfaces_that_come_up_later() {
    let real = mdns_interfaces(&BrowseOptions { interface: None });
    if real == [Ipv4Addr::UNSPECIFIED] {
        return; // no multicast interface on this machine
    }
    let mut q = MdnsQuerier::open(&BrowseOptions { interface: None }).unwrap();
    // As at boot, before DHCP: nothing but the placeholder.
    q.use_interfaces(vec![Ipv4Addr::UNSPECIFIED]);
    assert!(q.refresh(), "new interfaces are noticed");
    assert_eq!(q.ifaces, real);
    assert_eq!(q.oneshot.len(), real.len());
    assert!(q.oneshot.iter().all(|(i, _)| real.contains(i)));
    assert!(!q.refresh(), "nothing changed since");
    // A fixed interface is never changed.
    let mut fixed = MdnsQuerier::open(&BrowseOptions {
        interface: Some(real[0]),
    })
    .unwrap();
    assert!(!fixed.refresh());
}
