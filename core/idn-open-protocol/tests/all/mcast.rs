//! Encrypted multicast (`mcast/1`, spec/extensions/idn-extensions.md §E.17): IDN displays
//! on a link share one sealed, signed multicast stream; nothing on the network is clear.

use crate::common;
use common::free_port;
use idn_common::sync::LockOk;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent, Transport};
use idn_open_protocol::link::LinkConfig;
use idn_open_protocol::mcast::{McConfig, Mode};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An output sending multicast in `mc`, fed at 25 fps.
struct Rig {
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    _p: Publisher,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
    }
}

fn rig(mc: McConfig) -> Rig {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (25, 1),
                multicast: Some(mc),
                ..OutputConfig::new("Stage")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let start = Instant::now();
        let mut n = 0u32;
        while !s.load(Ordering::Relaxed) {
            let px = vec![(n % 200) as u8; 160 * 90 * 4];
            let _ = o.send(VideoFrame::rgba(160, 90, px.into(), AlphaMode::Opaque));
            n += 1;
            let due = start + Duration::from_millis(40) * n;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    });
    Rig {
        out,
        stop,
        feeder: Some(feeder),
        _p: p,
    }
}

/// An IDN display (link, takes multicast) or a plain receiver (multicast, else QUIC).
fn input(port: u16, idn: bool) -> Arc<Input> {
    Arc::new(
        Input::open(InputConfig {
            // Over the connection: the transport is what is tested (§E.26's ring otherwise).
            machine: false,
            transports: if idn {
                vec![Transport::Link, Transport::Multicast]
            } else {
                vec![Transport::Multicast, Transport::Quic]
            },
            link: idn.then(LinkConfig::ephemeral),
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap(),
    )
}

/// What an input saw: pictures (with arrival and transport) and the multicast
/// announcement.
#[derive(Default)]
struct Seen {
    pictures: Vec<(Instant, &'static str)>,
    announcement: Option<String>,
}

fn watch(
    input: Arc<Input>,
    stop: Arc<AtomicBool>,
) -> (Arc<Mutex<Seen>>, std::thread::JoinHandle<()>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s = seen.clone();
    let t = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match input.recv(Duration::from_millis(20)) {
                Some(InputEvent::Video(v)) => s.lock_ok().pictures.push((Instant::now(), v.via)),
                Some(InputEvent::Metadata { xml, .. }) if xml.starts_with("<ndi_multicast_v2") => {
                    s.lock_ok().announcement = Some(xml)
                }
                _ => {}
            }
        }
    });
    (seen, t)
}

/// The longest gap between pictures after the first second.
fn longest_gap(p: &[(Instant, &str)]) -> Duration {
    let start = p.first().map(|f| f.0 + Duration::from_secs(1));
    p.windows(2)
        .filter(|w| start.is_some_and(|s| w[0].0 >= s))
        .map(|w| w[1].0 - w[0].0)
        .max()
        .unwrap_or_default()
}

/// The announced video group: (address, id).
fn video_group(xml: &str) -> (SocketAddrV4, u32) {
    let i = xml.find(r#"type="vmchq""#).unwrap();
    let attr = |k: &str| {
        let a = &xml[i..];
        let v = &a[a.find(&format!("{k}=\"")).unwrap() + k.len() + 2..];
        v[..v.find('"').unwrap()].to_owned()
    };
    (
        attr("address").parse().unwrap(),
        attr("id").parse().unwrap(),
    )
}

#[test]
fn idn_displays_share_sealed_multicast_and_others_get_unicast() {
    let r = rig(McConfig {
        mode: Mode::Encrypted,
        key_interval: Duration::from_millis(1500),
        ..McConfig::default()
    });
    let port = r.out.port();
    let stop = Arc::new(AtomicBool::new(false));
    let display = input(port, true);
    let plain = input(port, false);
    let (seen, t1) = watch(display.clone(), stop.clone());
    let (seen_plain, t2) = watch(plain.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(1500));

    // Listen on the group: every datagram is sealed (§E.17.3), none a clear §6.3.3 packet.
    let (group, id) = video_group(seen.lock_ok().announcement.as_deref().expect("announced"));
    let sniff = {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        s.set_reuse_port(true).unwrap();
        // Only this group: every stream of a kind shares the port (idn_common::ports), and
        // the other tests' plain streams run at the same time.
        #[cfg(target_os = "linux")]
        s.set_multicast_all_v4(false).unwrap();
        s.bind(&std::net::SocketAddr::from((Ipv4Addr::UNSPECIFIED, group.port())).into())
            .unwrap();
        UdpSocket::from(s)
    };
    sniff
        .join_multicast_v4(group.ip(), &Ipv4Addr::LOCALHOST)
        .ok();
    sniff
        .join_multicast_v4(group.ip(), &Ipv4Addr::UNSPECIFIED)
        .ok();
    sniff
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut buf = [0u8; 2048];
    let mut sniffed = 0;
    for _ in 0..50 {
        if let Ok(n) = sniff.recv(&mut buf) {
            assert_eq!(buf[0], 0xE1, "a clear datagram on the group");
            assert!((28..=1472).contains(&n), "{n} bytes");
            assert_ne!(&buf[3..7], &id.to_le_bytes(), "a clear §6.3.3 header");
            sniffed += 1;
        }
    }
    assert!(sniffed > 10, "{sniffed} datagrams on the group");
    // Forged datagrams, sealed-looking and not: dropped and counted, video goes on.
    for i in 0..20u8 {
        let mut fake = vec![0xE1, 0, 0, 0];
        fake.extend_from_slice(&[i; 60]);
        sniff.send_to(&fake, group).unwrap();
        sniff.send_to(&[i; 40], group).unwrap();
    }

    // A second display joins and leaves: the key changes, the first keeps every frame.
    let changes = r.out.stats().mc_key_changes;
    let second = input(port, true);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(r.out.stats().mc_receivers, 2);
    drop(second);
    std::thread::sleep(Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    t1.join().unwrap();
    t2.join().unwrap();

    let st = r.out.stats();
    assert!(
        st.mc_key_changes >= changes + 2,
        "{} -> {}",
        changes,
        st.mc_key_changes
    );
    assert_eq!(st.mc_receivers, 1);
    let s = seen.lock_ok();
    // About 150 at 25 a second on a quiet machine; the gaps below are what matters
    // (fewer arrived while the whole suite ran at once).
    assert!(s.pictures.len() > 60, "{} pictures", s.pictures.len());
    assert!(s.pictures.iter().skip(5).all(|p| p.1 == "multicast"));
    let gap = longest_gap(&s.pictures);
    assert!(
        gap < Duration::from_millis(200),
        "a {gap:?} gap at a key change"
    );
    let ds = display.stats();
    assert!(
        ds.extensions.iter().any(|e| e == "mcast/1"),
        "{:?}",
        ds.extensions
    );
    assert!(ds.mc_rejected >= 20, "{} rejected", ds.mc_rejected);
    // The plain receiver was never told the groups: unicast.
    let p = seen_plain.lock_ok();
    assert!(p.announcement.is_none());
    assert!(p.pictures.len() > 100 && p.pictures.iter().all(|f| f.1 != "multicast"));
}

#[test]
fn per_receiver_and_plain_modes() {
    for (mode, idn_via, plain_via) in [
        (Mode::PerReceiver, "link", "multicast"),
        (Mode::Plain, "multicast", "multicast"),
    ] {
        let r = rig(McConfig {
            mode,
            ..McConfig::default()
        });
        let stop = Arc::new(AtomicBool::new(false));
        let display = input(r.out.port(), true);
        let plain = input(r.out.port(), false);
        let (a, t1) = watch(display.clone(), stop.clone());
        let (b, t2) = watch(plain.clone(), stop.clone());
        std::thread::sleep(Duration::from_secs(5));
        stop.store(true, Ordering::Relaxed);
        t1.join().unwrap();
        t2.join().unwrap();
        // Settled on its way: 18 of the last 20 (on a loaded machine a picture or two
        // came the other way as multicast lost some, which failed it now and then).
        for (seen, via) in [(&a, idn_via), (&b, plain_via)] {
            let s = seen.lock_ok();
            let last: Vec<_> = s.pictures.iter().rev().take(20).map(|p| p.1).collect();
            assert!(
                last.len() == 20 && last.iter().filter(|v| **v == via).count() >= 18,
                "{mode:?}: {last:?}"
            );
        }
        assert!(!display.stats().extensions.iter().any(|e| e == "mcast/1"));
        assert_eq!(r.out.stats().mc_key_changes, 0);
    }
}

#[test]
fn lost_messages_are_resent_over_the_link_and_shown_in_order() {
    // Every 7th message never reaches the network (§E.17.6): about 14% loss.
    let r = rig(McConfig {
        mode: Mode::Encrypted,
        test_drop_every: 7,
        ..McConfig::default()
    });
    let display = input(r.out.port(), true);
    let mut stamps = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Some(InputEvent::Video(v)) = display.recv(Duration::from_millis(20))
            && v.via == "multicast"
        {
            stamps.push(v.header.timestamp);
        }
    }
    let st = display.stats();
    let resent = r.out.stats().mc_resent;
    // About 4 s on multicast at 25 fps, every picture there and in order.
    assert!(stamps.len() > 90, "{} pictures", stamps.len());
    assert!(stamps.windows(2).all(|w| w[0] < w[1]), "out of order");
    let span = (stamps.last().unwrap() - stamps[0]) as f64 / 1e7;
    let expected = span * 25.0;
    assert!(
        stamps.len() as f64 >= expected * 0.97,
        "{} of {expected:.0}",
        stamps.len()
    );
    assert!(
        st.mc_repaired > 10 && resent >= st.mc_repaired as u64,
        "{} repaired, {resent} resent",
        st.mc_repaired
    );
    assert_eq!(st.mc_rejected, 0);
}
