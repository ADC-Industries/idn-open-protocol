//! Setup checker for IDN / NDI-Compatible networks: run it on any machine on the AV
//! network and it explains, in plain language, what it finds and what to fix.
//!
//! Options: `idn-doctor --help`.

use idn_open_protocol::discovery::{self, BrowseOptions, Source};
use idn_open_protocol::frame::{Limits, read_frame};
use idn_open_protocol::input::{ConnectionState, Input, InputConfig, InputEvent, Transport};
use std::net::TcpStream;
use std::time::{Duration, Instant};

struct Report {
    problems: usize,
    warnings: usize,
}

impl Report {
    fn ok(&self, m: &str) {
        println!("  ✔ {m}");
    }
    fn info(&self, m: &str) {
        println!("    {m}");
    }
    fn warn(&mut self, m: &str) {
        self.warnings += 1;
        println!("  ⚠ {m}");
    }
    fn fail(&mut self, m: &str) {
        self.problems += 1;
        println!("  ✖ {m}");
    }
}

fn read_sys(path: String) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Whether the routing table has a default route or a route for 224.0.0.0/4.
fn multicast_routed() -> bool {
    std::fs::read_to_string("/proc/net/route").is_ok_and(|t| {
        t.lines().skip(1).any(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() > 7 && (f[1] == "00000000" || f[1] == "000000E0")
        })
    })
}

/// Service types typical of phones, TVs and speakers: their presence suggests the AV
/// network is shared with guests or office devices.
const CONSUMER: &[(&str, &str)] = &[
    ("_airplay._tcp", "AirPlay (Apple TV, Macs, iPhones)"),
    ("_raop._tcp", "AirPlay audio"),
    ("_googlecast._tcp", "Chromecast / Google TV"),
    ("_spotify-connect._tcp", "Spotify Connect speakers"),
    ("_companion-link._tcp", "iPhones / iPads"),
    ("_apple-mobdev2._tcp", "iPhones / iPads"),
    ("_amzn-wplay._tcp", "Amazon Fire TV"),
    ("_androidtvremote2._tcp", "Android TV"),
    ("_sonos._tcp", "Sonos speakers"),
    ("_hap._tcp", "HomeKit devices"),
];

pub fn main() {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let mut args = idn_open_protocol::cli::Args::new(
        "usage: idn-doctor [--seconds N] [--iface IPV4] [--server HOST[:PORT]] [--no-node] [--test \"HOST (source)\"]... [--test-all]\n\
         \x20                 [--passphrase TEXT | env IDN_PASSPHRASE] (test encrypted media too)",
    );
    let mut secure_args = idn_open_protocol::cli::SecureArgs::default();
    let (mut secs, mut opts, mut server, mut tests, mut test_all) = (
        4u64,
        BrowseOptions::default(),
        None::<String>,
        Vec::new(),
        false,
    );
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => secs = args.value("--seconds"),
            "--iface" => opts.interface = Some(args.value("--iface")),
            "--server" => server = Some(args.value("--server")),
            "--test" => tests.push(args.value::<String>("--test")),
            "--test-all" => test_all = true,
            f if secure_args.take(f, &mut args) => {}
            "-h" | "--help" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    // The machine's Discovery Server, as the IDN Node has it, as `idn find` does: an
    // enrolled site lists its sources there, not on mDNS.
    if server.is_none() && !secure_args.no_node {
        server = idn_open_protocol::node::Node::connect("idn-doctor")
            .ok()
            .and_then(|n| n.enrollment().node_machine())
            .and_then(|m| m.discovery_server);
        if let Some(sv) = &server {
            println!("Using the IDN Node's Discovery Server, {sv} (--no-node: mDNS)");
        }
    }
    // With --trust-dir, the doctor enrols as a site-trust device and tests as one
    // (secure/2); it used to accept the flag and silently test unencrypted (R1-138).
    let secure = secure_args.config_with(
        &args,
        server.as_deref(),
        "idn-doctor",
        idn_open_protocol::trust::DeviceRole::Input,
    );
    let mut r = Report {
        problems: 0,
        warnings: 0,
    };

    println!("Network");
    let ifaces = discovery::ifaces::ipv4_multicast_interfaces();
    let mut fastest_mbps = 0u64;
    if ifaces.is_empty() {
        r.fail("no network interface with an IPv4 address is up");
    }
    // Virtual interfaces (container bridges, VPNs) have no device behind them: mention
    // them once instead of listing each.
    let physical = |n: &str| std::path::Path::new(&format!("/sys/class/net/{n}/device")).exists();
    let virtual_count = ifaces
        .iter()
        .filter(|i| !i.loopback && !physical(&i.name))
        .count();
    if virtual_count > 0 {
        r.info(&format!(
            "{virtual_count} virtual interface(s) (containers, VPN) ignored"
        ));
    }
    let shown: Vec<_> = ifaces
        .iter()
        .filter(|i| i.loopback || physical(&i.name))
        .collect();
    if shown.is_empty() && !ifaces.is_empty() {
        r.warn("only virtual interfaces are up (container or VM?): results may not reflect the real network");
    }
    for i in shown {
        let wifi = std::path::Path::new(&format!("/sys/class/net/{}/wireless", i.name)).exists()
            || i.name.starts_with("wl");
        let speed: Option<u64> = read_sys(format!("/sys/class/net/{}/speed", i.name))
            .and_then(|s| s.parse().ok())
            .filter(|&s: &u64| s > 0 && s < 1_000_000);
        let desc = format!(
            "{} {} {}",
            i.name,
            i.addr,
            speed.map_or(String::new(), |s| format!("({s} Mbit/s)"))
        );
        if i.loopback {
            r.warn(&format!(
                "{desc}: only the loopback interface is up — this machine is not on a network"
            ));
        } else if wifi {
            r.warn(&format!("{desc}: Wi-Fi. Video over Wi-Fi drops frames and adds delay; use a wired connection for cameras and the presentation server."));
        } else {
            match speed {
                Some(s) if s < 1000 => r.warn(&format!(
                    "{desc}: slower than gigabit. One 1080p stream needs roughly 20–150 Mbit/s."
                )),
                _ => r.ok(&format!("{desc}: wired")),
            }
        }
        fastest_mbps = fastest_mbps.max(speed.unwrap_or(0));
    }
    if multicast_routed() {
        r.ok("routing table has a default or multicast route");
    } else {
        r.info("no default or multicast route (fine: IDN tools query every interface directly)");
    }
    if std::path::Path::new("/run/avahi-daemon/pid").exists() {
        r.info("the system mDNS responder (Avahi) is running; IDN shares the mDNS port with it");
    }

    println!("\nShared network check");
    // No mDNS answers at all: the machine is probably deaf to mDNS (firewall on UDP 5353).
    let mut mdns_deaf = false;
    match discovery::service_types(Duration::from_secs(2), &opts) {
        Ok(types) => {
            mdns_deaf = types.is_empty();
            let found: Vec<&str> = CONSUMER
                .iter()
                .filter(|(t, _)| types.iter().any(|x| x.starts_with(t)))
                .map(|(_, d)| *d)
                .collect();
            if types.is_empty() {
                // A working mDNS network always shows at least one service type (this
                // machine's own responder, the video sources…): no answers means the check
                // could not run, e.g. a firewall that drops mDNS replies.
                r.warn("could not check for phones and TVs: no mDNS answers arrived (a firewall may block UDP 5353)");
            } else if found.is_empty() {
                r.ok(&format!(
                    "no phones, TVs or speakers announce themselves here ({} service types seen)",
                    types.len()
                ));
            } else {
                let mut uniq = found.clone();
                uniq.dedup();
                r.warn(&format!(
                    "consumer devices are on this network: {}.",
                    uniq.join(", ")
                ));
                r.info("If guest or office Wi-Fi shares this network, anyone can see and watch your video feeds,");
                r.info("and phones can slow it down. Put AV equipment on its own network or VLAN.");
            }
        }
        Err(e) => r.fail(&format!(
            "could not send mDNS queries: {e} (try --iface with this machine's address)"
        )),
    }

    println!("\nSources");
    let sources: Vec<Source> = match &server {
        Some(sv) => match discovery::ds::server_addr(sv)
            .and_then(|a| discovery::ds::query_sources(a, Duration::from_secs(secs)))
        {
            Ok(v) => {
                r.ok(&format!(
                    "discovery server {sv} answered with {} source(s)",
                    v.len()
                ));
                v
            }
            Err(e) => {
                r.fail(&format!("discovery server {sv}: {e}"));
                Vec::new()
            }
        },
        None => discovery::browse(Duration::from_secs(secs), &opts).unwrap_or_default(),
    };
    if sources.is_empty() {
        if mdns_deaf && server.is_none() {
            // Nothing found and nothing answered: one cause explains both.
            r.fail("no sources found, and this machine receives no mDNS answers at all. A firewall on this machine probably blocks UDP port 5353: allow it (and UDP in general for video).");
        } else {
            r.warn("no sources found. Check that cameras and senders are powered, on this network, and in the same groups (default \"public\").");
        }
    }
    for s in &sources {
        let Some(addr) = s.socket_addr() else {
            r.warn(&format!(
                "{}: announced, but its address did not resolve",
                s.name
            ));
            continue;
        };
        let t = Instant::now();
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(mut c) => {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
                let speaks = read_frame(&mut c, &Limits::default())
                    .ok()
                    .flatten()
                    .and_then(|f| f.text_payload().ok().map(|x| x.starts_with("<ndi_version")))
                    .unwrap_or(false);
                if speaks {
                    r.ok(&format!(
                        "{} at {addr}: reachable ({ms:.1} ms), groups {}",
                        s.name,
                        s.groups.join(",")
                    ));
                } else {
                    r.warn(&format!(
                        "{} at {addr}: port open but it did not greet like a video source",
                        s.name
                    ));
                }
            }
            Err(e) => r.fail(&format!(
                "{} at {addr}: cannot connect ({e}). A firewall may be blocking TCP {}.",
                s.name,
                addr.port()
            )),
        }
    }

    let to_test: Vec<String> = if test_all {
        sources.iter().map(|s| s.name.clone()).collect()
    } else {
        tests
    };
    if !to_test.is_empty() {
        println!("\nMedia test");
    }
    let mut total_mbps = 0.0;
    for name in &to_test {
        let run = |transports: Vec<Transport>| -> (bool, idn_open_protocol::input::InputStats, f64, String) {
            let cfg = InputConfig { transports, reconnect: false, discovery_server: server.clone(), interface: opts.interface, queue: 256, secure: secure.clone(), ..InputConfig::new(name) };
            let Ok(input) = Input::open(cfg) else { return (false, Default::default(), 0.0, "could not start a receiver".into()) };
            let (mut connected, mut first, mut err) = (false, None::<Instant>, String::new());
            // Phase 1: connect (name resolution can take a few seconds).
            let connect_by = Instant::now() + Duration::from_secs(10);
            while !connected && err.is_empty() && Instant::now() < connect_by {
                match input.recv(Duration::from_millis(200)) {
                    Some(InputEvent::State(ConnectionState::Connected(_))) => connected = true,
                    Some(InputEvent::State(ConnectionState::Lost(e))) => err = e,
                    _ => {}
                }
            }
            if !connected {
                if err.is_empty() {
                    err = "no answer within 10 s".into();
                }
                return (false, input.stats(), 0.0, err);
            }
            // Phase 2: measure, from one second after the first frame so start-up effects
            // (a transport being chosen, a queued frame or two) don't distort the rate.
            let end = idn_open_protocol::cli::deadline(secs.max(3).saturating_add(1));
            let mut base: Option<(Instant, idn_open_protocol::input::InputStats)> = None;
            while Instant::now() < end {
                if let Some(InputEvent::Video(_) | InputEvent::Compressed(_)) = input.recv(Duration::from_millis(200)) {
                    first.get_or_insert_with(Instant::now);
                }
                if base.is_none() && first.is_some_and(|t| t.elapsed() >= Duration::from_secs(1)) {
                    base = Some((Instant::now(), input.stats()));
                }
            }
            let mut st = input.stats();
            let dur = match &base {
                Some((t, b)) => {
                    st.video -= b.video;
                    st.video_bytes -= b.video_bytes;
                    st.audio_bytes -= b.audio_bytes;
                    t.elapsed().as_secs_f64()
                }
                None => first.map_or(0.0, |t| t.elapsed().as_secs_f64()),
            };
            (connected, st, dur, err)
        };
        let (connected, st, dur, err) = run(vec![Transport::Quic]);
        if !connected {
            r.fail(&format!("{name}: could not connect ({err})"));
            continue;
        }
        if st.video == 0 {
            // Control works but no media over QUIC: UDP is probably filtered. Retry with
            // media on the control connection to tell the two apart.
            let (_, st2, _, _) = run(Vec::new());
            if st2.video > 0 {
                r.fail(&format!("{name}: connects, but no video over QUIC/UDP. A firewall on this machine or the network blocks UDP; allow UDP in, or use TCP transports."));
            } else {
                r.fail(&format!(
                    "{name}: connects, but sends no video at all (is it producing pictures?)"
                ));
            }
            continue;
        }
        let fps = if dur > 0.0 {
            st.video as f64 / dur
        } else {
            0.0
        };
        let vmbps = st.video_bytes as f64 * 8.0 / dur.max(0.001) / 1e6;
        let ambps = st.audio_bytes as f64 * 8.0 / dur.max(0.001) / 1e6;
        let mbps = vmbps + ambps;
        total_mbps += mbps;
        let via = st
            .via
            .keys()
            .filter(|k| k.starts_with("video via"))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let mut msg = format!(
            "{name}: {fps:.1} frames/s, {mbps:.1} Mbit/s ({vmbps:.1} video + {ambps:.1} audio), {} decode errors ({via})",
            st.video_errors
        );
        // IDN sources also report their side (stats/1).
        if let Some(rtt) = st.rtt_ms {
            msg += &format!(", round trip {rtt:.1} ms");
        }
        if let Some(s) = &st.sender_report {
            msg += &format!(
                ", sender: quality {} / encode {:.1} ms / {} frames skipped",
                s.q, s.enc_ms, s.v_skipped
            );
            if s.v_skipped > 0 {
                r.warn(&format!("{name}: the sender skipped {} frames for this receiver (network or receiver too slow)", s.v_skipped));
            }
        }
        // secure/1 (§E.8.4): encrypted, and on which path.
        if secure.is_some() {
            if !st.encrypted {
                r.warn(&format!("{name}: NOT encrypted — the source has no passphrase, a different one, or is an NDI device"));
            } else if st.secure_fallback {
                r.warn(&format!(
                    "{name}: encrypted, but the media runs on the control connection because secure QUIC is blocked. \
                     A firewall on this machine or the network blocks UDP; allow UDP in for full performance."
                ));
            } else {
                msg += ", encrypted (secure QUIC)";
            }
        }
        if st.video_errors > 0 || st.hx_discarded > 0 {
            // Say what actually went wrong (R1-139).
            let mut why = Vec::new();
            if st.video_errors > 0 {
                why.push(format!(
                    "{} pictures did not decode (an incompatible or damaged stream)",
                    st.video_errors
                ));
            }
            if st.hx_discarded > 0 {
                why.push(format!(
                    "{} HX frames lost — check cabling and switch ports",
                    st.hx_discarded
                ));
            }
            r.warn(&format!("{msg}; {}", why.join("; ")));
        } else {
            r.ok(&msg);
        }
    }
    if total_mbps > 0.0 && fastest_mbps > 0 {
        let share = total_mbps / fastest_mbps as f64;
        if share > 0.7 {
            r.warn(&format!("the tested streams use {total_mbps:.0} Mbit/s, {:.0}% of this link. Lower the quality or bitrate, or use a faster link.", share * 100.0));
        } else {
            r.ok(&format!(
                "bandwidth: {total_mbps:.0} Mbit/s of {fastest_mbps} Mbit/s ({:.0}%)",
                share * 100.0
            ));
        }
    }

    println!();
    match (r.problems, r.warnings) {
        (0, 0) => println!("All good."),
        (0, w) => println!("No problems, {w} thing(s) worth checking."),
        (p, w) => println!("{p} problem(s), {w} warning(s)."),
    }
    std::process::exit(if r.problems > 0 { 1 } else { 0 });
}
