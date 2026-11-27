//! A router (a routing source, §1.8): an input connected to it gets the source it is
//! routed to, and the next one when it is switched, without being opened again.

use idn_open_protocol::api::{
    Output, OutputConfig, Publisher, PublisherConfig, Router, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn solid(w: usize, h: usize, px: [u8; 4]) -> Arc<[u8]> {
    px.iter()
        .copied()
        .cycle()
        .take(w * h * 4)
        .collect::<Vec<u8>>()
        .into()
}

/// Waits for a picture whose mean luma is near `y`, counting "routed" events on the way.
fn wait_for(input: &Input, y: i32, routed: &mut usize) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Video(v)) => {
                let p = &v.picture.planes[0].data;
                let mean = p.iter().map(|&v| v as i64).sum::<i64>() / p.len().max(1) as i64;
                if (mean as i32 - y).abs() < 12 {
                    return true;
                }
            }
            Some(InputEvent::State(ConnectionState::Routed { .. })) => *routed += 1,
            _ => {}
        }
    }
    false
}

#[test]
fn an_input_follows_the_router_to_each_source() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let a = Output::new(&publisher, OutputConfig::new("Cam A")).unwrap();
    let b = Output::new(&publisher, OutputConfig::new("Cam B")).unwrap();
    let (pa, pb) = (a.port(), b.port());
    let router = Router::new(&publisher, "Feed").unwrap();
    assert!(router.name().ends_with("(Feed)"));
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (w, h) = (160, 90);
            let black = solid(w, h, [0, 0, 0, 255]);
            let white = solid(w, h, [255, 255, 255, 255]);
            while !stop.load(Ordering::Relaxed) {
                let _ = a.send(VideoFrame::rgba(w, h, black.clone(), AlphaMode::Opaque));
                let _ = b.send(VideoFrame::rgba(w, h, white.clone(), AlphaMode::Opaque));
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    // By address: this test has no discovery.
    router.route(Some(&format!("127.0.0.1:{pa}")));
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", router.port()))).unwrap();
    let mut routed = 0;
    assert!(
        wait_for(&input, 16, &mut routed),
        "the router's first source: A, black"
    );
    assert_eq!(
        router.target().as_deref(),
        Some(format!("127.0.0.1:{pa}").as_str())
    );
    // Switched: the same input moves to B, and back to A.
    router.route(Some(&format!("127.0.0.1:{pb}")));
    assert!(wait_for(&input, 235, &mut routed), "switched to B, white");
    router.route(Some(&format!("127.0.0.1:{pa}")));
    assert!(wait_for(&input, 16, &mut routed), "back to A");
    assert!(routed >= 3, "routed {routed} times");
    // Cleared: receivers stay where they are.
    router.route(None);
    assert!(wait_for(&input, 16, &mut routed));
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

#[test]
fn an_encrypting_site_routes_too() {
    use idn_open_protocol::secure::SecureConfig;
    let mut sc = SecureConfig::new("St Mark's AV").unwrap();
    sc.require = true;
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        secure: Some(sc.clone()),
        ..Default::default()
    })
    .unwrap();
    let a = Output::new(&publisher, OutputConfig::new("Cam A")).unwrap();
    let pa = a.port();
    let router = Router::new(&publisher, "Feed").unwrap();
    router.route(Some(&format!("127.0.0.1:{pa}")));
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let white = solid(160, 90, [255, 255, 255, 255]);
            while !stop.load(Ordering::Relaxed) {
                let _ = a.send(VideoFrame::rgba(160, 90, white.clone(), AlphaMode::Opaque));
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let input = Input::open(InputConfig {
        secure: Some(sc),
        ..InputConfig::new(&format!("127.0.0.1:{}", router.port()))
    })
    .unwrap();
    let mut routed = 0;
    assert!(
        wait_for(&input, 235, &mut routed),
        "routed, encrypted both ways"
    );
    assert_eq!(routed, 1);
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

/// This machine's address on its network, if it has one (no packet is sent).
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr()
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback())
}

#[test]
fn a_receiver_elsewhere_is_given_an_address_it_can_reach() {
    let Some(ip) = lan_ip() else {
        eprintln!("no network address here: skipped");
        return;
    };
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let a = Output::new(&publisher, OutputConfig::new("Cam A")).unwrap();
    let router = Router::new(&publisher, "Feed").unwrap();
    // The target as discovery finds sources on this machine: at 127.0.0.1.
    router.route(Some(&format!("127.0.0.1:{}", a.port())));
    let input = Input::open(InputConfig::new(&format!("{ip}:{}", router.port()))).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let mut to = None;
    while Instant::now() < end && to.is_none() {
        if let Some(InputEvent::State(ConnectionState::Routed { addr, .. })) =
            input.recv(Duration::from_millis(100))
        {
            to = Some(addr);
        }
    }
    assert_eq!(to, Some(std::net::SocketAddr::new(ip, a.port())));
}

#[test]
fn a_router_that_comes_back_is_followed_again() {
    let publisher = Publisher::new(PublisherConfig::default()).unwrap();
    let a = Output::new(&publisher, OutputConfig::new("Cam A")).unwrap();
    let b = Output::new(&publisher, OutputConfig::new("Cam B")).unwrap();
    let (pa, pb) = (a.port(), b.port());
    let name = format!("Feed {}", std::process::id());
    let router = Router::new(&publisher, &name).unwrap();
    router.route(Some(&format!("127.0.0.1:{pa}")));
    let full = router.name().to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (black, white) = (
                solid(160, 90, [0, 0, 0, 255]),
                solid(160, 90, [255, 255, 255, 255]),
            );
            while !stop.load(Ordering::Relaxed) {
                let _ = a.send(VideoFrame::rgba(160, 90, black.clone(), AlphaMode::Opaque));
                let _ = b.send(VideoFrame::rgba(160, 90, white.clone(), AlphaMode::Opaque));
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    // By name: a router that starts again is on another port.
    let input = Input::open(InputConfig::new(&full)).unwrap();
    let mut routed = 0;
    assert!(wait_for(&input, 16, &mut routed), "routed to A");
    // The router ends (its Node restarted): the input stays on A …
    drop(router);
    assert!(wait_for(&input, 16, &mut routed), "still on A");
    // … and follows the router that starts again under the same name, now routing to B.
    let again = Router::new(&publisher, &name).unwrap();
    again.route(Some(&format!("127.0.0.1:{pb}")));
    assert!(
        wait_for(&input, 235, &mut routed),
        "followed the router again, to B"
    );
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

#[test]
fn a_router_started_again_on_its_port_is_followed_too() {
    // Within the first look (2 s), at the same address: it was taken for one that only
    // closed the connection, and never followed again.
    let publisher = Publisher::new(PublisherConfig::default()).unwrap();
    let a = Output::new(&publisher, OutputConfig::new("Cam A")).unwrap();
    let b = Output::new(&publisher, OutputConfig::new("Cam B")).unwrap();
    let (pa, pb) = (a.port(), b.port());
    let name = format!("Fixed {}", std::process::id());
    let router = Router::new(&publisher, &name).unwrap();
    let port = router.port();
    router.route(Some(&format!("127.0.0.1:{pa}")));
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (black, white) = (
                solid(160, 90, [0, 0, 0, 255]),
                solid(160, 90, [255, 255, 255, 255]),
            );
            while !stop.load(Ordering::Relaxed) {
                let _ = a.send(VideoFrame::rgba(160, 90, black.clone(), AlphaMode::Opaque));
                let _ = b.send(VideoFrame::rgba(160, 90, white.clone(), AlphaMode::Opaque));
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{port}"))).unwrap();
    let mut routed = 0;
    assert!(wait_for(&input, 16, &mut routed), "routed to A");
    drop(router);
    // (Its port free again a moment after: under the whole suite's load, not at once.)
    let mut tries = 0;
    let again = loop {
        match Router::on_port(&publisher, &name, port) {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 40 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            r => break r.unwrap(),
        }
    };
    again.route(Some(&format!("127.0.0.1:{pb}")));
    assert!(wait_for(&input, 235, &mut routed), "followed again, to B");
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
}

/// Lab 497-C: switched between two sources again and again on a busy machine, an input
/// never shows a picture of the source it left after the first of the one it went to.
/// (A link's reader that handed over one more picture just after the switch had it
/// stamped with the new session's number: 1 switch in 30, 10 in 30 under load.)
fn old_source_strays(machine: bool) -> (usize, usize) {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let a = Output::new(&publisher, OutputConfig::new("Cam A")).unwrap();
    let b = Output::new(&publisher, OutputConfig::new("Cam B")).unwrap();
    let (pa, pb) = (a.port(), b.port());
    let router = Router::new(&publisher, "Feed").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (w, h) = (160, 90);
            let black = solid(w, h, [0, 0, 0, 255]);
            let white = solid(w, h, [255, 255, 255, 255]);
            while !stop.load(Ordering::Relaxed) {
                let _ = a.send(VideoFrame::rgba(w, h, black.clone(), AlphaMode::Opaque));
                let _ = b.send(VideoFrame::rgba(w, h, white.clone(), AlphaMode::Opaque));
                std::thread::sleep(Duration::from_millis(8));
            }
        })
    };
    // A busy machine: the race needs the old reader to be a moment behind. Four busy
    // threads, not one per core: the timing tests beside it suffered (bug run I5-7); it
    // runs in the real-time group, one at a time with the lanes' tests.
    let busy: Vec<_> = (0..std::thread::available_parallelism().map_or(4, |n| n.get().min(4)))
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    router.route(Some(&format!("127.0.0.1:{pa}")));
    let input = Input::open(InputConfig {
        // Over shared memory (this machine's way), or the connection alone (as from
        // another machine).
        machine,
        ..InputConfig::new(&format!("127.0.0.1:{}", router.port()))
    })
    .unwrap();
    let mut routed = 0;
    assert!(
        wait_for(&input, 16, &mut routed),
        "the first source: A, black"
    );
    // Black is A (luma about 16), white is B (about 235).
    let is_b = |p: &[u8]| p.iter().map(|&v| v as i64).sum::<i64>() / p.len().max(1) as i64 > 125;
    let (mut strays, mut switches) = (0, 0);
    let mut to_b = true;
    for _ in 0..40 {
        let target: std::net::SocketAddr = format!("127.0.0.1:{}", if to_b { pb } else { pa })
            .parse()
            .unwrap();
        router.route(Some(&target.to_string()));
        // From where the input says it is routed there: once the new source shows, the
        // old one never again. (Pictures before that are the old source's by right: on
        // a busy machine the switch itself can take a while.)
        let give_up = Instant::now() + Duration::from_secs(5);
        let (mut there, mut arrived) = (false, None::<Instant>);
        while Instant::now() < give_up
            && arrived.is_none_or(|t| t.elapsed() < Duration::from_millis(250))
        {
            match input.recv(Duration::from_millis(20)) {
                Some(InputEvent::State(ConnectionState::Routed { addr, .. })) if addr == target => {
                    there = true
                }
                Some(InputEvent::Video(v)) if there => {
                    if is_b(&v.picture.planes[0].data) == to_b {
                        arrived.get_or_insert_with(Instant::now);
                    } else if arrived.is_some() {
                        strays += 1;
                    }
                }
                _ => {}
            }
        }
        if arrived.is_some() {
            switches += 1;
        }
        to_b = !to_b;
    }
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    for t in busy {
        t.join().unwrap();
    }
    eprintln!(
        "machine {machine}: {strays} strays, {switches} switches, via {:?}",
        input.stats().via
    );
    (strays, switches)
}

#[test]
fn no_picture_of_the_old_source_after_the_new_ones_first() {
    for machine in [true, false] {
        let (strays, switches) = old_source_strays(machine);
        assert!(
            switches >= 36,
            "machine {machine}: only {switches} of 40 switches arrived"
        );
        assert_eq!(
            strays, 0,
            "machine {machine}: pictures of the old source after the new one's first"
        );
    }
}
