//! idn-aoip (roadmap R.31): an IDN endpoint on a network of Dante-compatible
//! equipment. To that network it is one more device (its subscriptions and names set from
//! the controller the building uses); to IDN it is the source `HOST (NAME)` of what the
//! network sends it and the receiver `NAME` of what it sends the network.
//!
//! - The device: `idn_aoip_device` (written from the clean-room specs idn-aoip-control.md
//!   and idn-aoip-device.md): its ports answered, its mDNS, its flows' audio straight to
//!   and from the shared rings.
//! - The clock: the Node's site time, which follows the network's PTP leader.
//! - IDN: the rings read behind the clock into the IDN source ([`to_idn`]); the routable
//!   receiver's mixer written ahead of it into the rings ([`from_idn`]), but for the
//!   channels a desk on this machine claims and writes itself (R.33).
//!
//! Unofficial: not made, authorised or supported by Audinate. Dante is Audinate's
//! trademark, used only to say what this works with.
//!
//! Copyright ADC Industries. MIT OR Apache-2.0.

mod address;
mod args;
mod clock;
mod from_idn;
mod groups;
mod sound;
mod state;
mod subscriber;
mod to_idn;

use args::{NOTICE, USAGE};
use clock::{SiteClock, rate};
use idn_aoip_device::device::{Device, Identity, Subscription, status};
use idn_aoip_device::engine::{Engine, TxRun};
use idn_aoip_device::flowctl::FlowEvent;
use idn_aoip_device::mdns::Ports;
use idn_aoip_device::server::{Event, Server};
use idn_aoip_device::settings::Beat;
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::audio::Mixer;
use idn_open_protocol::node::Node;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    let host = idn_open_protocol::discovery::hostname();
    let args = match args::parse(&host) {
        Ok(a) => a,
        Err(e) if e.is_empty() => {
            println!("{USAGE}\n\n{NOTICE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("idn-aoip: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    println!("{NOTICE}");
    idn_open_protocol::shutdown::install();
    match run(args, host) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("idn-aoip: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The interface's IPv4 address: the one given, else the system's choice.
fn interface(given: Option<Ipv4Addr>) -> Result<Ipv4Addr, String> {
    if let Some(a) = given {
        return Ok(a);
    }
    idn_open_protocol::discovery::responder::HostInfo::local(None)
        .map(|h| h.addr)
        .map_err(|e| format!("no network interface ({e}); set --interface"))
}

fn run(args: args::Args, host: String) -> Result<(), String> {
    let node = Arc::new(
        Node::connect("IDN AoIP").map_err(|e| format!("needs this machine's IDN Node ({e})"))?,
    );
    let clock = Arc::new(SiteClock::default());
    clock.wait_usable()?;
    match idn_open_protocol::node::clock::ptp_grandmaster() {
        Some(gm) => println!("site clock: PTP, leader {gm}"),
        None => println!(
            "site clock: this machine's own (no PTP leader): the other network's equipment won't line up with it; set the Node's clock-protocol to v1 or v2 as that network's leader speaks"
        ),
    }
    let stop = Arc::new(AtomicBool::new(false));
    clock.follow(stop.clone()).map_err(|e| e.to_string())?;
    let ip = interface(args.interface)?;
    let aoip_dir = idn_common::paths::program_dir("aoip");
    let state_dir = aoip_dir.join(&args.device_name);
    // Renamed in the Node (or a new host name): the same device under its new name, its
    // state moved over, not a new device (bug run N3-2, N7-7: a new device ID, every
    // subscription, name and multicast flow gone).
    let renamed_from = state::follow_rename(&aoip_dir, &args.device_name, args.ports);
    let _ = std::fs::create_dir_all(&state_dir);
    let state_file = state_dir.join("device.json");

    // The device: its identity, channels, latencies; what it kept.
    // A port block set: a second device on this address, its own process ID (and so
    // device ID, §1.1).
    let process_id = args.ports.unwrap_or(0);
    // Its rate, from IDN (§N.40): everything below is made for it.
    clock::set_rate(args.rate);
    let mut device = Device::new(
        Identity::idn(&args.device_name, ip, process_id),
        args.rx,
        args.tx,
    );
    device.format.rate = rate();
    // The card's own hardware address and speed, as controllers show them (lab lt18:
    // Dante Controller showed 00:00:00:00:00:00).
    if let Some(l) = idn_common::net::links().into_iter().find(|l| l.addr == ip) {
        if let Some(m) = l.mac {
            device.identity.mac = m;
        }
        if let Some(s) = l.speed_mbps {
            device.identity.link_mbps = s.min(10_000) as u16;
        }
    }
    device.rx_latency_ns = args.latency_ms * 1_000_000;
    let receivers_ms = args.receivers_latency_ms.unwrap_or(args.latency_ms);
    device.tx_latency_ns = receivers_ms * 1_000_000;
    state::started_with_latency(device.rx_latency_ns);
    state::load(&state_file, &mut device);
    if let Some(old) = &renamed_from {
        // The name it is given now, over the one it kept.
        device.identity.name = args.device_name.clone();
        eprintln!(
            "idn-aoip: {old:?} renamed {:?}: its setup kept",
            args.device_name
        );
    }
    let cleaned = state::clean(&mut device);
    if cleaned > 0 {
        eprintln!(
            "idn-aoip: {cleaned} saved name(s) or subscription(s) outside the rules: set back"
        );
    }
    // In use from now on: flows, and the IDN source's time stamps (bug run I2-5).
    device.active_latency_ns = device.rx_latency_ns;
    // Kept from its first start (its device ID with it: bug run J5-5).
    state::save(&state_file, &device);
    if let Some(from) = &args.subscribe_from {
        for (i, c) in device.rx.iter_mut().enumerate() {
            c.subscription = Some(Subscription {
                channel: format!("TX {}", i + 1),
                device: from.clone(),
            });
            c.status = status::UNRESOLVED;
        }
        println!("subscribed {} channels from {from:?}", args.rx);
    }
    let latency = u64::from(device.rx_latency_ns) * u64::from(rate()) / 1_000_000_000;

    // The rings, in shared memory, and where a desk finds them.
    let rings: &'static idn_common::netrings::Rings = Box::leak(Box::new(
        idn_common::netrings::Rings::create(args.rx, args.tx, rate(), latency as u32)
            .map_err(|e| format!("the rings' shared memory: {e}"))?,
    ));
    let rings_file = state_dir.join("rings");
    if let Err(e) = idn_common::fsutil::write_private(&rings_file, rings.descriptor().as_bytes()) {
        eprintln!(
            "idn-aoip: {}: {e} (a desk on this machine can't use the device directly)",
            rings_file.display()
        );
    }
    sound::keep(rings, args.rx, clock.clone(), stop.clone()).map_err(|e| e.to_string())?;

    // The engine, then the device on the network.
    let ring_sound = Arc::new(sound::RingSound::new(rings, args.rx, args.tx));
    let engine = Arc::new(
        Engine::start(clock.clone(), ring_sound.clone(), rate(), ip).map_err(|e| e.to_string())?,
    );
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let beat_engine = engine.clone();
    let beat_clock = clock.clone();
    let beat_sound = ring_sound.clone();
    let beat = Arc::new(move || {
        let usable = beat_clock.site_ns().is_some();
        let slots: Vec<u32> = beat_engine
            .rx
            .lock_ok()
            .values()
            .map(|r| r.most_late.swap(0, Ordering::Relaxed) as u32)
            .collect();
        let (tx_levels, rx_levels) = beat_sound.take_levels();
        Beat {
            clock_ppb: usable.then_some(0),
            tx_levels,
            rx_levels,
            rx_latency: (!slots.is_empty()).then_some((rate(), slots)),
        }
    });
    let leader_clock = clock.clone();
    let clock_status = Arc::new(move || {
        let usable = leader_clock.site_ns().is_some();
        let leader = (*leader_clock.leader.read().ok()?)?;
        Some((usable, 0, leader))
    });
    // A name another device on the network already answers for (two machines from one
    // image, a device of that name): this one takes "-2" and says so (bug run K3-2).
    if idn_aoip_device::mdns::resolve(
        ip,
        &device.identity.name,
        "_netaudio-arc",
        Duration::from_millis(600),
    )
    .is_some_and(|r| r.addr != std::net::SocketAddr::from((ip, Ports::block(args.ports).arc)))
    {
        let base: String = device.identity.name.chars().take(29).collect();
        let next = format!("{base}-2");
        eprintln!(
            "idn-aoip: the name {:?} is another device's on this network: {next:?} instead",
            device.identity.name
        );
        state::clash_name(&device.identity.name, &next);
        device.identity.name = next;
    }
    // The name in use ("-2" or not; bug run N6-6: the start line gave the asked-for one).
    let name_in_use = device.identity.name.clone();
    let latency_in_use_ms = f64::from(device.rx_latency_ns) / 1e6;
    let version = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .collect::<Vec<_>>();
    let server = Arc::new(
        Server::start(
            device,
            Ports::block(args.ports),
            ev_tx,
            beat,
            clock_status,
            (
                *version.first().unwrap_or(&0) as u8,
                *version.get(1).unwrap_or(&0) as u8,
                *version.get(2).unwrap_or(&0),
            ),
        )
        .map_err(|e| format!("the device on the network: {e}"))?,
    );
    // Its goodbyes on a stop (SIGINT, SIGTERM).
    let on_stop = {
        let (st, sv) = (stop.clone(), server.clone());
        idn_open_protocol::shutdown::register(move || {
            st.store(true, Ordering::Relaxed);
            sv.shutdown();
        })
    };
    println!(
        "on the network as {:?}: {} channels in, {} out, {} ms latency, {} Hz",
        name_in_use,
        args.rx,
        args.tx,
        latency_in_use_ms,
        rate()
    );

    // Transmit flows the device has (multicast ones kept from before) into the engine.
    let max_fpp = Arc::new(AtomicUsize::new(0));
    let start_tx = |i: usize| {
        let d = server.device.lock_ok();
        let Some(f) = d.tx_flows.flows.get(i).and_then(|f| f.as_ref()) else {
            return;
        };
        max_fpp.fetch_max(usize::from(f.fpp), Ordering::Release);
        engine.tx.lock_ok().insert(
            (i + 1) as u16,
            TxRun::new(
                SocketAddr::from(f.to),
                f.channels.clone(),
                f.bytes,
                f.fpp,
                f.multicast,
            ),
        );
    };
    // A multicast flow's group checked and held before it sends (§2.6): another group
    // when someone holds it.
    let mut held: std::collections::BTreeMap<u16, Ipv4Addr> = Default::default();
    let claim = |n: u16, held: &mut std::collections::BTreeMap<u16, Ipv4Addr>| {
        groups::claim(&server, ip, n, held, &start_tx)
    };
    let multicast: Vec<usize> = server
        .device
        .lock_ok()
        .tx_flows
        .flows
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.as_ref().filter(|f| f.multicast).map(|_| i))
        .collect();
    for i in multicast {
        claim((i + 1) as u16, &mut held);
    }
    server.announce();
    // The network's devices read for the Site Manager (R.32 stage 1a): beside being one.
    idn_aoip_device::devices::start(
        ip,
        args.device_name.clone(),
        Ports::block(args.ports).arc,
        state_dir.clone(),
        server.mdns_heard(),
    );

    // IDN: the source and the receiver.
    let mut threads = Vec::new();
    let in_counts = Arc::new(to_idn::Counts::default());
    let out_counts = Arc::new(from_idn::Counts::default());
    let publisher = if args.rx > 0 {
        // With the machine's site identity, as the desk's and the plugins' sources (lab
        // Alpha85 542-A: published without it, the site's own machines didn't see the
        // source, and a pool input's IDN route found "no source").
        let cfg = PublisherConfig {
            secure: Some(node.secure_config()),
            link: Some(node.link_config()),
            discovery_server: node
                .status()
                .ok()
                .and_then(|s| s.attr("server").map(str::to_owned))
                .filter(|s| !s.is_empty()),
            ..Default::default()
        };
        Some(Publisher::new(cfg).map_err(|e| format!("publisher: {e}"))?)
    } else {
        None
    };
    // The IDN source, kept to say where each of its channels comes from (§A.16).
    let mut idn_out: Option<Arc<Output>> = None;
    if let Some(p) = &publisher {
        let out = Arc::new(
            Output::new(p, OutputConfig::new(&args.name)).map_err(|e| format!("output: {e}"))?,
        );
        out.set_audio_origin(&state::origin(&server.device.lock_ok().clone()));
        idn_out = Some(out.clone());
        threads.push(
            to_idn::spawn(
                rings,
                args.rx,
                out,
                latency,
                clock.clone(),
                stop.clone(),
                in_counts.clone(),
            )
            .map_err(|e| e.to_string())?,
        );
        println!("into IDN as the source \"{host} ({})\"", args.name);
    }
    // (Held while it runs: the routes go with it.)
    let mut idn_receiver = None;
    if args.tx > 0 {
        let receivers_latency = (receivers_ms * rate() / 1000) as usize;
        let lead = from_idn::lead(0);
        let mixer = Mixer::new(rate(), args.tx as u16, &host);
        mixer.set_output_delay(from_idn::output_delay(lead, receivers_latency));
        let floor = Arc::new(AtomicU32::new(from_idn::lane_floor_ms(
            lead,
            receivers_latency,
        )));
        idn_receiver = Some(from_idn::receiver(
            &node,
            &args.name,
            args.tx as u16,
            &mixer,
            floor.clone(),
        )?);
        threads.push(
            from_idn::spawn(
                rings,
                mixer,
                from_idn::Writer {
                    clock: clock.clone(),
                    stop: stop.clone(),
                    counts: out_counts.clone(),
                    max_fpp: max_fpp.clone(),
                    tx: args.tx,
                    receivers_latency,
                    floor,
                },
            )
            .map_err(|e| e.to_string())?,
        );
        println!(
            "from IDN: the receiver {:?} in the routing matrix",
            args.name
        );
    }

    // The receive side.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    {
        let sub = subscriber::Subscriber {
            server: server.clone(),
            engine: engine.clone(),
            ip,
            stop: stop.clone(),
        };
        threads.push(
            std::thread::Builder::new()
                .name("idn-aoip-subscribe".into())
                .spawn(move || sub.run(wake_rx))
                .map_err(|e| e.to_string())?,
        );
    }
    let _ = wake_tx.send(());

    // Until stopped or --seconds: the device's events, a line every 10 s.
    let started = Instant::now();
    // The address it chose itself gone from the machine (DHCP gave another): started again
    // on the new one (lab Alpha84 537-B: it stayed on the old address, not listed). An
    // address given with --interface is kept: it is the person's choice.
    let mut address_gone: u32 = 0;
    let mut again = false;
    // The card it runs on, by its MAC and name: the new address is looked for on it alone
    // (bug run M2-1: a pulled cable restarted it on Wi-Fi or a container bridge, for good).
    let card = address::Card::of(ip);
    let mut next = Instant::now() + Duration::from_secs(10);
    let mut save_due: Option<Instant> = None;
    // The routes the desk's channels leave out, as last said (R.33; `None`: no desk).
    let mut routes_said: Option<Vec<idn_open_protocol::node::routing::Route>> = None;
    while !stop.load(Ordering::Relaxed) {
        match ev_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Flow(FlowEvent::Started(i) | FlowEvent::Changed(i))) => start_tx(i),
            Ok(Event::Flow(FlowEvent::Stopped(i))) => {
                engine.tx.lock_ok().remove(&((i + 1) as u16));
            }
            Ok(Event::Changed(c)) => {
                use idn_aoip_device::arc::Change;
                match c {
                    Change::FlowCreated(n) => claim(n, &mut held),
                    Change::FlowDeleted(n) => {
                        engine.tx.lock_ok().remove(&n);
                        if let Some(g) = held.remove(&n) {
                            server.release(g);
                        }
                    }
                    Change::Subscription(_) => {
                        let _ = wake_tx.send(());
                    }
                    Change::Latency(ns) => println!(
                        "receive latency set to {:.1} ms: kept, used from the next start",
                        f64::from(ns) / 1e6
                    ),
                    _ => {}
                }
                save_due = Some(Instant::now() + Duration::from_millis(500));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        // Flows whose receiver went quiet (no keepalives for 4 s): their numbers free in
        // the device too, and off the flow pages (bug run I1-1).
        for (n, to) in engine.take_expired() {
            // Set up again by its receiver before this saw it expire (it is in the engine
            // again, to the same address): the new flow, kept (bug run K2-3: its entry
            // was freed while the engine sent it, and the number given to another).
            if engine.tx.lock_ok().get(&n).is_some_and(|f| f.to == to) {
                continue;
            }
            if server.device.lock_ok().tx_flows.expired(n, to) {
                println!("flow {n} to {to}: no keepalives for 4 s; stopped");
            }
        }
        // A route to a channel the desk has is left out while it does: said once each time
        // that list changes (not each block).
        if let Some((_, rx)) = &idn_receiver {
            match idn_common::netrings::Claim::of_word(out_counts.desk.load(Ordering::Relaxed)) {
                None => routes_said = None,
                Some(k) => {
                    let now = from_idn::routes_taken(&rx.routes(), Some(k));
                    let changed = match &routes_said {
                        None => !now.is_empty(),
                        Some(was) => *was != now,
                    };
                    if changed {
                        println!("{}", from_idn::routes_taken_line(&now));
                    }
                    routes_said = Some(now);
                }
            }
        }
        if save_due.is_some_and(|t| Instant::now() >= t) {
            save_due = None;
            {
                // A copy saved: the device isn't held while the disk writes (bug run N2-2).
                let copy = server.device.lock_ok().clone();
                state::save(&state_file, &copy);
                // Its subscriptions may have changed: the IDN source's origin with them.
                if let Some(o) = &idn_out {
                    o.set_audio_origin(&state::origin(&copy));
                }
            }
        }
        if args
            .seconds
            .is_some_and(|s| started.elapsed() >= Duration::from_secs(s))
        {
            break;
        }
        if let Some(name) = server.dead() {
            eprintln!("idn-aoip: the thread {name:?} ended; stopping");
            break;
        }
        if let Some(t) = threads.iter().find(|t| t.is_finished()) {
            eprintln!(
                "idn-aoip: the thread {:?} ended; stopping",
                t.thread().name().unwrap_or("?")
            );
            break;
        }
        if Instant::now() >= next {
            // (From now: after a stall the looks don't come back to back; M2-1.)
            next = Instant::now() + Duration::from_secs(10);
            if args.interface.is_none()
                && let Some(card) = &card
            {
                address_gone = if card.moved_from(ip) {
                    address_gone + 1
                } else {
                    0
                };
                // At two looks in a row (10–20 s): not a moment's flap.
                if address_gone >= 2 {
                    eprintln!(
                        "idn-aoip: this machine's address {ip} is gone; starting again on its new one"
                    );
                    again = true;
                    break;
                }
            }
            let take = |a: &AtomicU32| {
                let p = f32::from_bits(a.swap(0, Ordering::Relaxed));
                if p > 0.0 {
                    format!("{:.1} dBFS", 20.0 * p.log10())
                } else {
                    "silent".into()
                }
            };
            let c = &engine.counts;
            println!(
                "{:5.0} s: in {} blocks ({} skips, peak {}), out {} blocks ({} skips, {} late, peak {}), packets {} sent {} received {} dropped, {} flows in {} out, clock {}{}",
                started.elapsed().as_secs_f64(),
                in_counts.blocks.load(Ordering::Relaxed),
                in_counts.skipped.load(Ordering::Relaxed),
                take(&in_counts.peak),
                out_counts.blocks.load(Ordering::Relaxed),
                out_counts.skipped.load(Ordering::Relaxed),
                out_counts.late.load(Ordering::Relaxed),
                take(&out_counts.peak),
                c.sent.load(Ordering::Relaxed),
                c.received.load(Ordering::Relaxed),
                c.dropped.load(Ordering::Relaxed),
                engine.rx.lock_ok().len(),
                engine.tx.lock_ok().len(),
                if clock.site_ns().is_some() {
                    "usable"
                } else {
                    "not usable"
                },
                // The desk in the rings (R.33): the transmit channels it has.
                match idn_common::netrings::Claim::of_word(out_counts.desk.load(Ordering::Relaxed))
                {
                    Some(k) => format!(", the desk on TX {k} of {}", args.tx),
                    None => String::new(),
                },
            );
        }
    }
    stop.store(true, Ordering::Relaxed);
    {
        // A copy saved: the device isn't held while the disk writes (bug run N2-2).
        let copy = server.device.lock_ok().clone();
        state::save(&state_file, &copy);
    }
    server.shutdown();
    drop(on_stop);
    for t in threads {
        let _ = t.join();
    }
    let _ = std::fs::remove_file(&rings_file);
    // Its list of the network goes with it (bug run I4-5).
    let _ = std::fs::remove_file(state_dir.join("devices.xml"));
    drop(publisher);
    // Under the Node: started again at once; by hand: said, and ended with a failure.
    if again {
        if idn_common::switches::on("IDN_SUPERVISED") {
            std::process::exit(idn_common::supervisor::AGAIN_NOW);
        }
        return Err("this machine's address changed: start it again".into());
    }
    Ok(())
}
