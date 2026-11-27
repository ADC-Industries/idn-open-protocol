//! Browser and responder against each other over real multicast on this host, plus the
//! listing port (spec chapter 5).

use idn_open_protocol::discovery::responder::{HostInfo, Published, Responder};
use idn_open_protocol::discovery::{BrowseOptions, browse, find, groups_match, query_listing};
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

#[test]
fn browse_find_and_listing() {
    let free = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut host = HostInfo::local(None).unwrap();
    host.listing_port = free;
    let tag = std::process::id();
    let a = format!("TEST{tag} (ONE)");
    let b = format!("TEST{tag} (TWO)");
    let pubs = vec![
        Published {
            name: a.clone(),
            port: 15961,
            groups: vec!["public".into()],
            routing: false,
            idn_ext: None,
        },
        Published {
            name: b.clone(),
            port: 15962,
            groups: vec!["Studio A".into(), "Cam".into()],
            routing: true,
            idn_ext: Some("resume/1 stats/1 secure/1".into()),
        },
    ];
    let addr = host.addr;
    let r = Responder::start(host, pubs, true).unwrap();
    let opts = BrowseOptions {
        interface: Some(addr),
    };

    let s = find(&b, Duration::from_secs(5), &opts)
        .unwrap()
        .expect("source TWO found");
    assert_eq!(s.socket_addr(), Some(SocketAddr::new(addr.into(), 15962)));
    assert_eq!(s.groups, ["Studio A", "Cam"]);
    assert!(s.routing);
    assert_eq!(s.listing_port, Some(free));
    // Discovery hint (§E.2.2).
    assert_eq!(s.idn_ext, ["resume/1", "stats/1", "secure/1"]);
    assert!(s.is_idn() && s.offers("secure") && !s.offers("color"));
    assert!(!groups_match(&s.groups, &["public".into()]));

    let all = browse(Duration::from_secs(2), &opts).unwrap();
    assert!(
        all.iter()
            .any(|s| s.name == a && s.port == 15961 && !s.is_idn()),
        "{all:?}"
    );

    let listed = query_listing(SocketAddr::new(addr.into(), free), Duration::from_secs(2)).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[1].name, b);

    // Goodbye on drop: the sources disappear from a fresh browse.
    drop(r);
    let after = browse(Duration::from_secs(2), &opts).unwrap();
    assert!(
        !after.iter().any(|s| s.name == a || s.name == b),
        "{after:?}"
    );
}

/// The long-running browser reports a source appearing and, on its goodbye, leaving.
#[test]
fn browser_reports_add_and_remove() {
    use idn_open_protocol::discovery::{Browser, BrowserConfig, SourceEvent};
    let mut host = HostInfo::local(None).unwrap();
    host.listing_port = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = host.addr;
    let b = Browser::start(BrowserConfig {
        interface: Some(addr),
        ..Default::default()
    })
    .unwrap();
    let name = format!("BROWSE{} (CAM)", std::process::id());
    let r = Responder::start(host, vec![], false).unwrap();
    r.add(Published {
        name: name.clone(),
        port: 15970,
        groups: vec!["public".into()],
        routing: false,
        idn_ext: None,
    });
    let wait_for = |pred: &dyn Fn(&SourceEvent) -> bool| {
        let end = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < end {
            if let Some(e) = b.recv(Duration::from_millis(200))
                && pred(&e)
            {
                return true;
            }
        }
        false
    };
    assert!(wait_for(
        &|e| matches!(e, SourceEvent::Added(s) if s.name == name && s.port == 15970)
    ));
    assert!(b.sources().iter().any(|s| s.name == name));
    r.remove(&name, 15970);
    assert!(wait_for(
        &|e| matches!(e, SourceEvent::Removed(s) if s.name == name)
    ));
    assert!(!b.sources().iter().any(|s| s.name == name));
}

/// Listing-port connections stay open as NDI hosts keep them, but closed ones are let
/// go (no sockets left in CLOSE_WAIT) and at most 64 are held.
#[test]
fn the_listing_port_lets_closed_connections_go() {
    use std::io::Read;
    use std::net::TcpStream;
    let mut host = HostInfo::local(None).unwrap();
    let port = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    host.listing_port = port;
    let _r = Responder::start(host, vec![], true).unwrap();
    let connect = || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut doc = Vec::new();
        let mut b = [0u8; 4096];
        while !doc.ends_with(&[0]) {
            let n = c.read(&mut b).unwrap();
            assert!(n > 0);
            doc.extend_from_slice(&b[..n]);
        }
        c
    };
    // Sockets of this port in CLOSE_WAIT (state 08 in /proc/net/tcp).
    let close_wait = || {
        let t = std::fs::read_to_string("/proc/net/tcp").unwrap_or_default();
        t.lines()
            .filter(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() > 3 && f[1].ends_with(&format!(":{port:04X}")) && f[3] == "08"
            })
            .count()
    };
    let gone: Vec<_> = (0..10).map(|_| connect()).collect();
    drop(gone);
    std::thread::sleep(Duration::from_millis(1600));
    assert_eq!(close_wait(), 0);

    let mut held: Vec<_> = (0..70).map(|_| connect()).collect();
    std::thread::sleep(Duration::from_millis(300));
    let mut b = [0u8; 16];
    let r = held[0].read(&mut b);
    assert!(
        matches!(&r, Ok(0))
            || matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "the oldest was closed: {r:?}"
    );
    let last = held.last_mut().unwrap();
    last.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let e = last.read(&mut b).unwrap_err();
    assert!(
        matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{e}"
    );
}

/// Many sources: answers are split into packets that fit the MTU, and every source is
/// found (one 12 kB packet used to be cut off at 9000 bytes and lost entirely).
#[test]
fn forty_sources_are_all_found() {
    let mut host = HostInfo::local(None).unwrap();
    host.listing_port = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let tag = std::process::id();
    let pubs: Vec<Published> = (0..40)
        .map(|i| Published {
            name: format!("MANY{tag} (Sanctuary camera number {i:02} with a long name)"),
            port: 16000 + i,
            groups: vec!["public".into(), "Worship Team".into()],
            routing: false,
            idn_ext: Some("resume/1 stats/1 audio-f32/1 color/1 channels/1".into()),
        })
        .collect();
    let addr = host.addr;
    let _r = Responder::start(host, pubs, false).unwrap();
    let opts = BrowseOptions {
        interface: Some(addr),
    };
    let end = std::time::Instant::now() + Duration::from_secs(6);
    let mut n = 0;
    while n < 40 && std::time::Instant::now() < end {
        n = browse(Duration::from_secs(2), &opts)
            .unwrap()
            .iter()
            .filter(|s| s.name.starts_with(&format!("MANY{tag} ")) && s.addr.is_some())
            .count();
    }
    assert_eq!(n, 40);
}

/// On a machine with several networks, a source is found on each, with that network's
/// address (it used to be announced on one interface, with that address everywhere).
#[test]
fn sources_are_found_on_every_interface_with_its_address() {
    use idn_open_protocol::discovery::ifaces::ipv4_multicast_interfaces;
    let mut host = HostInfo::local(None).unwrap();
    let Some(other) = ipv4_multicast_interfaces()
        .into_iter()
        .map(|i| i.addr)
        .find(|a| *a != host.addr && !a.is_loopback())
    else {
        return; // one network only
    };
    host.listing_port = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let name = format!("MULTI{} (CAM)", std::process::id());
    let main = host.addr;
    let _r = Responder::start(
        host,
        vec![Published {
            name: name.clone(),
            port: 15990,
            groups: vec!["public".into()],
            routing: false,
            idn_ext: None,
        }],
        false,
    )
    .unwrap();
    for iface in [main, other] {
        let s = find(
            &name,
            Duration::from_secs(5),
            &BrowseOptions {
                interface: Some(iface),
            },
        )
        .unwrap()
        .unwrap_or_else(|| panic!("not found from {iface}"));
        assert_eq!(s.addr, Some(iface.into()), "from {iface}");
    }
}
