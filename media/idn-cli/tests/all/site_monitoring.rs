//! Site-wide monitoring through a Discovery Server (§E.5.1, §8.3.6–§8.3.9): IDN outputs
//! and inputs advertise their live status; a Monitor sees every device; registrations and
//! advertisements survive a server restart.

use crate::common;
use common::free_port;
use common::idn;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::discovery::{DeviceEvent, DeviceKind, Monitor, ds, ds_server};
use idn_open_protocol::input::{Input, InputConfig};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn feed(out: Arc<Output>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut n = 0u8;
        while !stop.load(Ordering::Relaxed) {
            let px: Arc<[u8]> = vec![n; 160 * 90 * 4].into();
            let _ = out.send(VideoFrame::rgba(160, 90, px, AlphaMode::Opaque));
            n = n.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(40));
        }
    })
}

/// Waits until `pred` holds for the monitor's device list.
fn wait_devices(
    m: &Monitor,
    secs: u64,
    pred: impl Fn(&[idn_open_protocol::discovery::Device]) -> bool,
) -> bool {
    idn_common::testing::wait_until(Duration::from_secs(secs), || pred(&m.devices()))
}

#[test]
fn monitor_sees_outputs_and_inputs_with_live_values() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || ds_server::serve(l));

    let publisher = Publisher::new(PublisherConfig {
        discovery_server: Some(server.clone()),
        ..Default::default()
    })
    .unwrap();
    let out = Arc::new(Output::new(&publisher, OutputConfig::new("Monitored")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let f = feed(out.clone(), stop.clone());
    let monitor = Monitor::start(&server).unwrap();

    let input = Input::open(InputConfig {
        discovery_server: Some(server.clone()),
        receiver_name: "Watcher".into(),
        // The link's figures are what is checked: not shared memory on this machine.
        machine: false,
        ..InputConfig::new(out.name())
    })
    .unwrap();
    let src_name = out.name().to_owned();
    // Counters are reported every 5 s, so give it a little over that.
    let ok = wait_devices(&monitor, 12, |ds| {
        let src = ds
            .iter()
            .find(|d| d.kind == DeviceKind::Source && d.name == src_name);
        let rcv = ds
            .iter()
            .find(|d| d.kind == DeviceKind::Receiver && d.name.ends_with("(Watcher)"));
        matches!((src, rcv), (Some(s), Some(r))
            if s.get("connected-receivers") == Some("1") && s.fps().is_some_and(|f| f > 15.0)
                && s.get("video-resolution") == Some("160x90") && s.get("video-frames-sent").is_some()
                && r.connected() && r.receiving_from() == Some(src_name.as_str()) && r.fps().is_some_and(|f| f > 15.0)
                // Two IDN devices: the IDN link, encrypted but not authenticated (§E.11.7).
                && r.encrypted() == Some(true) && r.get("x-idn-authenticated") == Some("false")
                && r.get("x-idn-transport") == Some("link")
                && r.frames_dropped().is_some() && r.is_idn())
    });
    assert!(ok, "{:#?}", monitor.devices());
    assert!(monitor.server_reachable());

    // An input that goes away is removed from the table promptly (§8.3.8).
    while monitor.recv(Duration::from_millis(10)).is_some() {}
    drop(input);
    let end = Instant::now() + Duration::from_secs(3);
    let mut removed = false;
    while Instant::now() < end && !removed {
        if let Some(DeviceEvent::Removed(d)) = monitor.recv(Duration::from_millis(100)) {
            removed = d.kind == DeviceKind::Receiver;
        }
    }
    assert!(removed, "receiver removal reported");
    stop.store(true, Ordering::Relaxed);
    f.join().unwrap();
}

#[test]
fn registrations_and_monitoring_survive_a_server_restart() {
    use std::process::Stdio;
    let port = free_port();
    let server = format!("127.0.0.1:{port}");
    let start_server = || {
        idn("discovery-server")
            .args(["--port", &port.to_string()])
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut ds_proc = start_server();
    std::thread::sleep(Duration::from_millis(300));

    let publisher = Publisher::new(PublisherConfig {
        discovery_server: Some(server.clone()),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Survivor")).unwrap();
    let monitor = Monitor::start(&server).unwrap();
    let name = out.name().to_owned();
    assert!(wait_devices(&monitor, 5, |ds| ds
        .iter()
        .any(|d| d.name == name && d.get("source-name").is_some())));

    // The server goes away: the monitor keeps the last known state and says so.
    let _ = ds_proc.kill();
    let _ = ds_proc.wait();
    idn_common::testing::wait_until(Duration::from_secs(3), || !monitor.server_reachable());
    assert!(!monitor.server_reachable());
    assert!(
        monitor.devices().iter().any(|d| d.name == name),
        "cached during the outage"
    );

    // It comes back: the output re-registers and re-advertises by itself.
    std::thread::sleep(Duration::from_secs(1));
    let mut ds_proc = start_server();
    let addr = ds::server_addr(&server).unwrap();
    idn_common::testing::wait_for(
        "the source registration restored",
        Duration::from_secs(8),
        || {
            ds::query_sources(addr, Duration::from_secs(1))
                .is_ok_and(|v| v.iter().any(|s| s.name == name))
        },
    );
    assert!(
        wait_devices(&monitor, 8, |ds| ds.iter().any(|d| d.name == name))
            && monitor.server_reachable()
    );
    let _ = ds_proc.kill();
    let _ = ds_proc.wait();
}

/// An unlisted publisher's outputs appear nowhere (the switcher's private media feeds),
/// while a listed one's next to it do.
#[test]
fn unlisted_outputs_are_not_registered() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || ds_server::serve(l));
    let listed = Publisher::new(PublisherConfig {
        discovery_server: Some(server.clone()),
        ..Default::default()
    })
    .unwrap();
    let hidden = Publisher::new(PublisherConfig {
        discovery_server: Some(server.clone()),
        unlisted: true,
        ..Default::default()
    })
    .unwrap();
    let a = Output::new(&listed, OutputConfig::new("Listed")).unwrap();
    let b = Output::new(&hidden, OutputConfig::new("Hidden")).unwrap();
    let addr = ds::server_addr(&server).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let names = loop {
        let names: Vec<String> = ds::query_sources(addr, Duration::from_secs(2))
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();
        if names.iter().any(|n| n == a.name()) || Instant::now() > end {
            break names;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(names.iter().any(|n| n == a.name()), "{names:?}");
    assert!(!names.iter().any(|n| n == b.name()), "{names:?}");
}
