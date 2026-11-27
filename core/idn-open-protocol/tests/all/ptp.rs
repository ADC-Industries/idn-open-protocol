//! The site clock over loopback (spec/extensions/idn-clock.md §C.6, test 2). --nocapture
//! prints the slave's offset each second and the agreement.

use idn_open_protocol::ptp::{Clock, ClockConfig, State, mono_ns};
use std::time::{Duration, Instant};

/// A test port: a thousand up in a release build, so the debug and release runs of a
/// parallel full run don't hear each other's clocks (bug run K8-8: a test locked to the
/// other run's leader).
/// A test's port: debug and release apart, and each test process's by its own offset (0
/// to 78, even): two runs at once on one network (another checkout's, the lab's) heard
/// each other's clocks, and a follower took the other run's time (2026-11-19).
fn port(p: u16) -> u16 {
    let base = if cfg!(debug_assertions) { p } else { p + 1000 };
    base + (std::process::id() % 40) as u16 * 2
}

#[test]
fn two_clocks_agree() {
    // Test ports, so as not to need privileges or meet a real grandmaster.
    let cfg = |id: u8, priority1: u8, start_offset_ns: i64| ClockConfig {
        event_port: port(26319),
        general_port: port(26320),
        start_offset_ns,
        ..ClockConfig::standard([0x1d, 0x4e, 0, 0, 0, 0, 0, id], priority1)
    };
    // The Site Manager's machine (priority1 200) and an ordinary one, which starts 5 ms off.
    let manager = Clock::start(cfg(1, 200, 0)).unwrap();
    let other = Clock::start(cfg(2, 248, 5_000_000)).unwrap();
    let start = Instant::now();
    let locked = loop {
        let (a, b) = (manager.status(), other.status());
        if a.state == State::Master && b.state == (State::Slave { locked: true }) {
            break start.elapsed();
        }
        assert!(
            start.elapsed() < Duration::from_secs(40),
            "manager {:?}, other {:?} (offset {:.0} ns)",
            a.state,
            b.state,
            b.offset_ns
        );
        if start.elapsed().as_millis() % 1000 < 200 {
            eprintln!(
                "{:>5.1} s  {:?}  offset {:.0} ns  delay {:.0} ns",
                start.elapsed().as_secs_f64(),
                b.state,
                b.offset_ns,
                b.delay_ns
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(other.status().grandmaster, [0x1d, 0x4e, 0, 0, 0, 0, 0, 1]);
    // One process, one monotonic clock: the two mappings should give the same site time.
    let mut worst = 0i128;
    for _ in 0..20 {
        let m = mono_ns();
        let d = manager.status().mapping.site(m) - other.status().mapping.site(m);
        worst = worst.max(d.abs());
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("locked after {locked:?}; worst difference {worst} ns over 5 s");
    assert!(worst < 50_000, "{worst} ns");
}

/// A PTP v1 leader as Dante-compatible equipment without AES67 runs one (IEEE 1588-2002):
/// Sync and Follow_Up every 250 ms, a Delay_Resp to each Delay_Req. Its time is this
/// process's monotonic clock plus `offset_ns`. Written from the standard's message layout,
/// as statime's v1 support (third-party/statime) reads it.
mod v1_leader {
    use idn_open_protocol::ptp::{PTP_GROUP, is_v1, mono_ns, recv_timed, timestamps_on};
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    pub const UUID: [u8; 6] = [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c];

    fn socket(port: u16) -> UdpSocket {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.set_reuse_address(true).unwrap();
        s.set_reuse_port(true).unwrap();
        s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
            .unwrap();
        s.join_multicast_v4(&PTP_GROUP, &Ipv4Addr::UNSPECIFIED)
            .unwrap();
        s.set_multicast_loop_v4(true).unwrap();
        s.into()
    }

    /// The 40-byte v1 header.
    fn header(b: &mut [u8], event: bool, seq: u16, control: u8) {
        b[0..2].copy_from_slice(&1u16.to_be_bytes()); // versionPTP
        b[2..4].copy_from_slice(&1u16.to_be_bytes()); // versionNetwork
        b[4..9].copy_from_slice(b"_DFLT"); // subdomain
        b[20] = if event { 1 } else { 2 }; // messageType
        b[21] = 1; // sourceCommunicationTechnology: Ethernet
        b[22..28].copy_from_slice(&UUID);
        b[28..30].copy_from_slice(&1u16.to_be_bytes()); // sourcePortId
        b[30..32].copy_from_slice(&seq.to_be_bytes());
        b[32] = control;
    }

    fn timestamp(b: &mut [u8], ns: i128) {
        b[0..4].copy_from_slice(&((ns / 1_000_000_000) as u32).to_be_bytes());
        b[4..8].copy_from_slice(&((ns % 1_000_000_000) as u32).to_be_bytes());
    }

    fn sync(seq: u16, now: i128) -> [u8; 124] {
        let mut b = [0u8; 124];
        header(&mut b, true, seq, 0);
        let c = &mut b[40..];
        timestamp(&mut c[0..8], now); // the rough origin time; Follow_Up has the precise one
        c[10..12].copy_from_slice(&37i16.to_be_bytes()); // currentUTCOffset
        // The grandmaster: this clock.
        c[13] = 1;
        c[14..20].copy_from_slice(&UUID);
        c[20..22].copy_from_slice(&1u16.to_be_bytes());
        c[27] = 3; // stratum
        c[28..32].copy_from_slice(b"DFLT");
        c[43] = (-2i8) as u8; // syncInterval: 2^-2 s
        c[55] = 3; // localClockStratum
        c[56..60].copy_from_slice(b"DFLT");
        c[61] = 1;
        c[62..68].copy_from_slice(&UUID);
        c[70..72].copy_from_slice(&1u16.to_be_bytes());
        c[83] = 1; // utcReasonable
        b
    }

    fn follow_up(seq: u16, of: u16, precise: i128) -> [u8; 52] {
        let mut b = [0u8; 52];
        header(&mut b, false, seq, 2);
        b[42..44].copy_from_slice(&of.to_be_bytes());
        timestamp(&mut b[44..52], precise);
        b
    }

    fn delay_resp(req: &[u8], received: i128) -> [u8; 60] {
        let mut b = [0u8; 60];
        let seq = u16::from_be_bytes([req[30], req[31]]);
        header(&mut b, false, seq, 3);
        timestamp(&mut b[40..48], received);
        b[49] = req[21];
        b[50..56].copy_from_slice(&req[22..28]);
        b[56..58].copy_from_slice(&req[28..30]);
        b[58..60].copy_from_slice(&req[30..32]);
        b
    }

    /// Runs until `stop`.
    pub fn run(event_port: u16, general_port: u16, offset_ns: i128, stop: Arc<AtomicBool>) {
        let event = socket(event_port);
        let general = socket(general_port);
        event
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        // Delay_Req stamped by the kernel as it arrived, as a real leader stamps it (a
        // thread's late wake-up skewed the measured path under load).
        timestamps_on(&event);
        let to_event = SocketAddrV4::new(PTP_GROUP, event_port);
        let to_general = SocketAddrV4::new(PTP_GROUP, general_port);
        let now = || mono_ns() as i128 + offset_ns;
        let (mut seq, mut next) = (0u16, 0i128);
        let mut buf = [0u8; 512];
        while !stop.load(Ordering::Relaxed) {
            if now() >= next {
                let t = now();
                event.send_to(&sync(seq, t), to_event).unwrap();
                general
                    .send_to(&follow_up(seq, seq, t), to_general)
                    .unwrap();
                seq = seq.wrapping_add(1);
                next = t + 250_000_000;
            }
            if let Ok((n, mono)) = recv_timed(&event, &mut buf) {
                let got = mono as i128 + offset_ns;
                // Delay_Req (control 1) from a follower; our own Sync comes back too.
                if n >= 40 && is_v1(&buf[..n]) && buf[32] == 1 && buf[22..28] != UUID {
                    general
                        .send_to(&delay_resp(&buf[..n], got), to_general)
                        .unwrap();
                }
            }
        }
    }
}

#[test]
fn a_v1_leader_is_followed() {
    use idn_open_protocol::ptp::Protocol;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    // (Not 26419/26420: idn-node's clock test uses those. Every test's ports here are
    // below Linux's ephemeral range, 32768-60999: in it another test's socket could take
    // one first, a full run's failures in 0.009 s, 2026-11-16.)
    let (event_port, general_port) = (port(26719), port(26720));
    // A leader whose time is about 2023, unrelated to this machine's.
    let offset_ns: i128 = 1_700_000_000 * 1_000_000_000 - mono_ns() as i128;
    let stop = Arc::new(AtomicBool::new(false));
    let leader = {
        let stop = stop.clone();
        std::thread::spawn(move || v1_leader::run(event_port, general_port, offset_ns, stop))
    };
    let follower = Clock::start(ClockConfig {
        protocol: Protocol::V1,
        event_port,
        general_port,
        ..ClockConfig::standard([0x1d, 0x4e, 0, 0, 0, 0, 0x11, 0x22], 248)
    })
    .unwrap();
    let start = Instant::now();
    let result = loop {
        let s = follower.status();
        if s.state == (State::Slave { locked: true }) {
            break Ok(s);
        }
        if start.elapsed() > Duration::from_secs(60) {
            break Err(format!("{:?}, offset {:.0} ns", s.state, s.offset_ns));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let s = match result {
        Ok(s) => s,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            let _ = leader.join();
            panic!("never locked to the v1 leader: {e}");
        }
    };
    // statime names a v1 clock by its UUID padded with zeros.
    let mut gm = [0u8; 8];
    gm[..6].copy_from_slice(&v1_leader::UUID);
    assert_eq!(s.grandmaster, gm);
    // Site time is the leader's: one process, one monotonic clock.
    let mut worst = 0i128;
    for _ in 0..12 {
        let m = mono_ns();
        let d = follower.status().mapping.site(m) - (m as i128 + offset_ns);
        worst = worst.max(d.abs());
        std::thread::sleep(Duration::from_millis(250));
    }
    stop.store(true, Ordering::Relaxed);
    leader.join().unwrap();
    eprintln!(
        "locked after {:?}; worst difference {worst} ns",
        start.elapsed()
    );
    // 250 µs, not the v2 test's 50 µs: this stand-in leader stamps a Delay_Req when its
    // thread reads it (no kernel time), which skews the measured path by tens of µs
    // (62 µs seen). Still far inside Dante's smallest latency (0.25 ms) and IDN's buffers.
    assert!(worst < 250_000, "{worst} ns from the leader's time");
}

/// Two v1 clocks and no other leader (a network of computers only, R.31): one leads in v1
/// (statime's v1 leader, IDN change 5: Sync with Follow_Up, Delay_Resp), the other follows
/// it and agrees with it. Each offers itself as the weakest v1 leader, so they choose by
/// identity rather than each waiting for the other.
#[test]
fn two_v1_clocks_choose_a_leader_and_agree() {
    use idn_open_protocol::ptp::Protocol;
    let cfg = |id: u8, start_offset_ns: i64| ClockConfig {
        protocol: Protocol::V1,
        event_port: port(26519),
        general_port: port(26520),
        start_offset_ns,
        ..ClockConfig::standard([0x1d, 0x4e, 0, 0, 0, id, 0, 0], 248)
    };
    let a = Clock::start(cfg(1, 0)).unwrap();
    let b = Clock::start(cfg(2, 5_000_000)).unwrap();
    let start = Instant::now();
    let (leader, follower) = loop {
        let (sa, sb) = (a.status(), b.status());
        match (sa.state, sb.state) {
            (State::Master, State::Slave { locked: true }) => break (&a, &b),
            (State::Slave { locked: true }, State::Master) => break (&b, &a),
            _ => {}
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "a {:?}, b {:?} (offsets {:.0} / {:.0} ns)",
            sa.state,
            sb.state,
            sa.offset_ns,
            sb.offset_ns
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let locked = start.elapsed();
    // The follower names the leader by its v1 UUID (the identity's first six bytes, its
    // multicast bit cleared: 1d → 1c).
    let gm = follower.status().grandmaster;
    assert_eq!(gm[6..], [0, 0]);
    assert!(gm == [0x1c, 0x4e, 0, 0, 0, 1, 0, 0] || gm == [0x1c, 0x4e, 0, 0, 0, 2, 0, 0]);
    let mut worst = 0i128;
    for _ in 0..20 {
        let m = mono_ns();
        let d = leader.status().mapping.site(m) - follower.status().mapping.site(m);
        worst = worst.max(d.abs());
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("locked after {locked:?}; worst difference {worst} ns over 5 s");
    assert!(worst < 50_000, "{worst} ns");
}

/// The bridge (R.31): a v1 leader (Dante-compatible equipment without AES67), a Node set
/// to both (its v1 port follows the leader, its v2 port leads the site) and an ordinary
/// v2 Node, which takes the v1 leader's time through the bridge without speaking v1.
#[test]
fn a_bridge_carries_a_v1_leaders_time_to_v2_nodes() {
    use idn_open_protocol::ptp::Protocol;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (event_port, general_port) = (port(26619), port(26620));
    let offset_ns: i128 = 1_700_000_000 * 1_000_000_000 - mono_ns() as i128;
    let stop = Arc::new(AtomicBool::new(false));
    let leader = {
        let stop = stop.clone();
        std::thread::spawn(move || v1_leader::run(event_port, general_port, offset_ns, stop))
    };
    let cfg = |protocol, id: u8, priority1| ClockConfig {
        protocol,
        event_port,
        general_port,
        ..ClockConfig::standard([0x1d, 0x4e, 0, 0, 0, id, 0, 0], priority1)
    };
    // The bridge, and an ordinary Node (the Site Manager's machine would be 200).
    let bridge = Clock::start(cfg(Protocol::Both, 1, 248)).unwrap();
    let node = Clock::start(cfg(Protocol::V2, 2, 200)).unwrap();
    let start = Instant::now();
    let result = loop {
        let (b, n) = (bridge.status(), node.status());
        if b.state == (State::Slave { locked: true }) && n.state == (State::Slave { locked: true })
        {
            break Ok(());
        }
        if start.elapsed() > Duration::from_secs(90) {
            break Err(format!(
                "bridge {:?} (offset {:.0} ns), node {:?} (offset {:.0} ns)",
                b.state, b.offset_ns, n.state, n.offset_ns
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if let Err(e) = result {
        stop.store(true, Ordering::Relaxed);
        let _ = leader.join();
        panic!("{e}");
    }
    let locked = start.elapsed();
    // Both name the v1 leader as the grandmaster.
    let mut gm = [0u8; 8];
    gm[..6].copy_from_slice(&v1_leader::UUID);
    assert_eq!(bridge.status().grandmaster, gm);
    assert_eq!(node.status().grandmaster, gm);
    // The v2 Node's site time is the v1 leader's (250 µs: the stand-in leader's own
    // stamping, as in a_v1_leader_is_followed, plus a second hop). Twelve looks a quarter
    // second apart: the typical one within that, none past 1 ms (bug run M7-T: the worst
    // of twelve was a scheduling hiccup's under a full parallel run); the samples and both
    // clocks' state said when it fails.
    let mut d = Vec::new();
    for _ in 0..12 {
        let m = mono_ns();
        d.push((node.status().mapping.site(m) - (m as i128 + offset_ns)).abs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let (b, n) = (bridge.status(), node.status());
    stop.store(true, Ordering::Relaxed);
    leader.join().unwrap();
    let mut sorted = d.clone();
    sorted.sort_unstable();
    let (typical, worst) = (sorted[sorted.len() / 2], sorted[sorted.len() - 1]);
    eprintln!(
        "both locked after {locked:?}; the v2 Node within {typical} ns (worst {worst}) of the v1 leader"
    );
    assert!(
        typical < 250_000 && worst < 1_000_000,
        "from the leader's time (ns): {d:?}; bridge offset {:.0} delay {:.0}, node offset {:.0} delay {:.0}",
        b.offset_ns,
        b.delay_ns,
        n.offset_ns,
        n.delay_ns
    );
}

/// Two bridges and a preferred v2 grandmaster (bug run H, H1-1): each bridge relays the
/// grandmaster into v1 as preferred; heard by the other bridge, that relay outranked the
/// grandmaster itself (v1's preferred reads as priority1 64), so the bridges followed each
/// other in a loop and the grandmaster went passive. Now a relay of a grandmaster heard
/// directly isn't a leader of its own: both bridges and the grandmaster stay as they
/// should, and stay.
#[test]
fn two_bridges_follow_the_grandmaster_not_each_other() {
    use idn_open_protocol::ptp::Protocol;
    let cfg = |protocol, identity: [u8; 8], priority1| ClockConfig {
        protocol,
        event_port: port(26919),
        general_port: port(26920),
        ..ClockConfig::standard(identity, priority1)
    };
    // An EUI-64 identity (ff:fe in the middle), as most grandmasters have: its v1 UUID
    // leaves those out, so the relay came back as another clock's (an identity like the
    // Nodes' own comes back as itself, and the loop didn't form).
    let want = [0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x12, 0x34, 0x56];
    let gm = Clock::start(cfg(Protocol::V2, want, 100)).unwrap();
    let a = Clock::start(cfg(Protocol::Both, [0x1d, 0x4e, 0, 0, 0, 1, 0, 0], 248)).unwrap();
    let b = Clock::start(cfg(Protocol::Both, [0x1d, 0x4e, 0, 0, 0, 2, 0, 0], 248)).unwrap();
    let settled = |c: &Clock| {
        let s = c.status();
        s.state == (State::Slave { locked: true }) && s.grandmaster == want
    };
    let start = Instant::now();
    loop {
        if gm.status().state == State::Master && settled(&a) && settled(&b) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "grandmaster {:?}; a {:?} following {:02x?}; b {:?} following {:02x?}",
            gm.status().state,
            a.status().state,
            a.status().grandmaster,
            b.status().state,
            b.status().grandmaster
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // And it holds (the loop formed a few BMCA rounds after the bridges' v1 ports led).
    for _ in 0..50 {
        assert_eq!(gm.status().state, State::Master, "the grandmaster gave way");
        assert!(
            settled(&a) && settled(&b),
            "a {:?}, b {:?}",
            a.status(),
            b.status()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// idn-aoip-device.md §8.4: under v1 a Node follows the network's leader unless
/// told to lead. Two v1 clocks, the second told to lead (a priority set by hand, 100):
/// the second leads whatever the identities say, and the first follows it.
#[test]
fn a_v1_clock_told_to_lead_leads() {
    use idn_open_protocol::ptp::Protocol;
    let cfg = |id: u8, leads: bool, priority1: u8| ClockConfig {
        protocol: Protocol::V1,
        event_port: port(26819),
        general_port: port(26820),
        leads,
        ..ClockConfig::standard([0x1d, 0x4e, 0, 0, 0, id, 0, 0], priority1)
    };
    // The lower identity (it would win a tie) is the one not told to lead.
    let follows = Clock::start(cfg(1, false, 248)).unwrap();
    let leads = Clock::start(cfg(9, true, 100)).unwrap();
    let start = Instant::now();
    loop {
        let (f, l) = (follows.status(), leads.status());
        if f.state == (State::Slave { locked: true }) && l.state == State::Master {
            assert_eq!(f.grandmaster, [0x1c, 0x4e, 0, 0, 0, 9, 0, 0]);
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "follows {:?}, leads {:?}",
            f.state,
            l.state
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
