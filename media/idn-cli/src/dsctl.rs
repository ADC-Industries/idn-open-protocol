//! Discovery Server client tool (spec chapter 8).
//!
//! Options: `idn-dsctl --help`.

use idn_open_protocol::discovery::ds::{self, Role, Session, SourceChange, SourceWatcher, Subject};
use idn_open_protocol::xml::Element;
use std::time::{Duration, Instant};

fn hostname() -> String {
    idn_open_protocol::discovery::hostname()
}

/// The scope words after `set KEY VALUE` / `unset KEY` as request attributes.
fn scope_attrs(e: Element, words: &[&str]) -> Result<Element, String> {
    Ok(match words {
        [] => e,
        ["device", fp] => e.with_attr("fp", fp),
        ["output", name @ ..] if !name.is_empty() => e.with_attr("output", &name.join(" ")),
        ["input", name @ ..] if !name.is_empty() => e.with_attr("input", &name.join(" ")),
        _ => return Err("scope: nothing (site-wide), device FP, output NAME or input NAME".into()),
    })
}

fn scope_text(s: &idn_open_protocol::trust::OptionScope) -> String {
    use idn_open_protocol::trust::OptionScope::*;
    match s {
        Site => "site".into(),
        Device(fp) => format!("device {fp}"),
        Output(n) => format!("output {n:?}"),
        Input(n) => format!("input {n:?}"),
    }
}

pub fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let mut args = idn_open_protocol::cli::Args::new(
        "usage: idn-dsctl --server HOST[:PORT] (sources | watch-sources | receivers | connect RECEIVER_ID SOURCE\n\
         \x20                                    | monitor RECEIVER_ID | status | properties) [--seconds N]\n\
         \x20      idn-dsctl --server HOST[:PORT] --trust-dir DIR [--passphrase P | env IDN_PASSPHRASE] [--admin-code C]\n\
         \x20                                    (options | set KEY VALUE [device FP | output NAME | input NAME]\n\
         \x20                                    | unset KEY [same scope])      (remote administration, admin role)\n\
         status: a live table of every advertising source and receiver, refreshed each second\n\
         properties: every property each one reports (the x-idn-… counters), after N s",
    );
    let (mut server, mut secs, mut pos) = (None::<String>, 10u64, Vec::new());
    let (mut trust_dir, mut passphrase, mut admin_code) =
        (None::<String>, None::<String>, None::<String>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server = Some(args.value("--server")),
            "--seconds" => secs = args.value("--seconds"),
            "--trust-dir" => trust_dir = Some(args.value("--trust-dir")),
            "--passphrase" => passphrase = Some(args.value("--passphrase")),
            "--admin-code" => admin_code = Some(args.value("--admin-code")),
            "-h" | "--help" => args.help(),
            s if !s.starts_with('-') => pos.push(s.to_owned()),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    let Some(server) = server else {
        args.fail("--server is required")
    };
    let addr = ds::server_addr(&server)?;
    let t = Duration::from_secs(5);
    let show = |s: &idn_open_protocol::discovery::Source| {
        format!(
            "{:<40} {:<22} groups={}",
            s.name,
            s.socket_addr().map_or("?".into(), |a| a.to_string()),
            s.groups.join(",")
        )
    };
    let words: Vec<&str> = pos.iter().map(String::as_str).collect();
    // Remote administration (§E.10.8, §E.20.5): as an enrolled device with the admin role.
    if matches!(words.first(), Some(&"options" | &"set" | &"unset")) {
        let Some(dir) = trust_dir else {
            args.fail("options/set/unset need --trust-dir (an enrolled admin device)")
        };
        let req = match words.as_slice() {
            ["options"] => Element::new("idn_admin").with_attr("op", "options"),
            ["set", key, value, rest @ ..] => {
                scope_attrs(Element::new("idn_admin").with_attr("op", "set"), rest)
                    .unwrap_or_else(|e| args.fail(&e))
                    .with_attr("key", key)
                    .with_attr("value", value)
            }
            ["unset", key, rest @ ..] => {
                scope_attrs(Element::new("idn_admin").with_attr("op", "unset"), rest)
                    .unwrap_or_else(|e| args.fail(&e))
                    .with_attr("key", key)
            }
            _ => args.fail("set KEY VALUE [scope] | unset KEY [scope] | options"),
        };
        let e = idn_open_protocol::trust::Enrollment::start(
            idn_open_protocol::trust::EnrollmentConfig {
                discovery_server: server.clone(),
                state_dir: dir.into(),
                name: format!("{} (dsctl)", hostname()),
                role: idn_open_protocol::trust::DeviceRole::App,
                passphrase: passphrase
                    .or_else(|| idn_common::switches::value("IDN_PASSPHRASE"))
                    .filter(|p| !p.is_empty()),
                admin_code,
            },
        )?;
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            match e.state() {
                idn_open_protocol::trust::TrustState::Enrolled { connected: true } => break,
                idn_open_protocol::trust::TrustState::Pending { code } => {
                    eprintln!(
                        "waiting for approval on the authority (code {})",
                        idn_open_protocol::trust::show_code(&code)
                    );
                    std::thread::sleep(Duration::from_secs(2));
                }
                // Not taking joins: an admin code is given only once joined (§E.10.8).
                idn_open_protocol::trust::TrustState::WaitingForPairing
                    if Instant::now() >= end =>
                {
                    eprintln!(
                        "idn-dsctl: the site isn't taking joins now: open the pairing window in the Site Manager's panel (then approve this device there), or give the site passphrase (--passphrase); an admin code is given once joined"
                    );
                    std::process::exit(1);
                }
                st if Instant::now() >= end => {
                    eprintln!("idn-dsctl: not enrolled and connected after 20 s: {st:?}");
                    std::process::exit(1);
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let r = e.admin(&req, Duration::from_secs(5))?;
        if r.attr("ok") != Some("true") {
            eprintln!(
                "refused: {}",
                r.attr("error")
                    .unwrap_or("not an admin, or an unknown operation")
            );
            std::process::exit(1);
        }
        match words[0] {
            "options" => {
                let doc = r.children.iter().find(|c| c.name == "idn_options");
                match doc.and_then(idn_open_protocol::trust::SiteOptions::from_element) {
                    Some(o) => {
                        println!("version {}", o.version);
                        for (scope, k, v) in o.entries() {
                            println!("  {}: {k} = {v}", scope_text(&scope));
                        }
                    }
                    None => println!("no options set"),
                }
            }
            _ => println!("ok, version {}", r.attr("version").unwrap_or("?")),
        }
        return Ok(());
    }
    match words.as_slice() {
        ["sources"] => {
            for s in ds::query_sources(addr, t)? {
                println!("{}", show(&s));
            }
        }
        ["watch-sources"] => {
            let mut w = SourceWatcher::connect(addr)?;
            w.set_timeout(Some(Duration::from_millis(500)))?;
            let end = idn_open_protocol::cli::deadline(secs);
            while Instant::now() < end {
                match w.next_change() {
                    Ok(Some(SourceChange::All(v))) => {
                        v.iter().for_each(|s| println!("  {}", show(s)))
                    }
                    Ok(Some(SourceChange::Added(s))) => println!("+ {}", show(&s)),
                    Ok(Some(SourceChange::Removed(s))) => println!("- {}", show(&s)),
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        }
        ["status"] => {
            // Site-wide monitoring (§E.5.1): every device advertising on the server.
            let m = idn_open_protocol::discovery::Monitor::start(&server)?;
            let started = Instant::now();
            let end = idn_open_protocol::cli::deadline(secs);
            while Instant::now() < end {
                std::thread::sleep(Duration::from_secs(1));
                println!(
                    "\n{} ({})",
                    started.elapsed().as_secs(),
                    if m.server_reachable() {
                        "server reachable"
                    } else {
                        "SERVER UNREACHABLE, last known state"
                    }
                );
                for d in m.devices() {
                    let or = |v: Option<String>| v.unwrap_or_else(|| "-".into());
                    let what = match d.kind {
                        idn_open_protocol::discovery::DeviceKind::Source => format!(
                            "{} receiver(s)",
                            d.get("connected-receivers").unwrap_or("?")
                        ),
                        idn_open_protocol::discovery::DeviceKind::Receiver => {
                            format!("<- {}", d.receiving_from().unwrap_or("(nothing)"))
                        }
                    };
                    println!(
                        "  {:<8} {:<34} {:<28} {:>6} fps  dropped {:>5}  rtt {:>6}  enc {:<5}  {}",
                        format!("{:?}", d.kind),
                        d.name,
                        what,
                        or(d.fps().map(|f| format!("{f:.1}"))),
                        or(d.frames_dropped().map(|n| n.to_string())),
                        or(d.rtt_ms().map(|r| format!("{r:.1}"))),
                        or(d.encrypted().map(|e| e.to_string())),
                        if d.is_idn() { "IDN" } else { "NDI" }
                    );
                }
            }
        }
        ["properties"] => {
            // Every reported property of every device, once they have been heard for the
            // time asked (a source reports its counters every few seconds).
            let m = idn_open_protocol::discovery::Monitor::start(&server)?;
            std::thread::sleep(Duration::from_secs(secs.max(1)));
            for d in m.devices() {
                println!("{:?} {}", d.kind, d.name);
                for (k, v) in &d.properties {
                    println!("  {k} = {v}");
                }
            }
        }
        ["receivers"] => {
            let mut l = Session::connect(addr, Subject::Receiver, Role::Listener, t)?;
            l.send(&Element::new("query_receivers"))?;
            l.set_timeout(Some(t))?;
            for m in l.recv()?.unwrap_or_default() {
                for r in m.children_named("receiver") {
                    println!(
                        "{}  {:<36} type={} address={} input={}",
                        r.child_text("receiver_id"),
                        r.child_text("name"),
                        r.child_text("type"),
                        r.child_text("address"),
                        r.child_text("input_name")
                    );
                }
            }
        }
        ["connect", rid, src] => {
            let mut l = Session::connect(addr, Subject::Receiver, Role::Listener, t)?;
            l.send(&ds::connect_command(rid, src))?;
            std::thread::sleep(Duration::from_millis(300));
            println!("sent connect \"{src}\" to {rid}");
        }
        ["monitor", rid] => {
            let mut l = Session::connect(addr, Subject::Receiver, Role::Listener, t)?;
            let mut caps = Element::new("monitor_capabilities");
            for (n, _) in ds::RECEIVER_PROPERTIES {
                caps.children
                    .push(Element::new("property").with_attr("name", n));
            }
            l.send(
                &Element::new("monitor_request")
                    .with_attr("unsubscribe", "false")
                    .with_child(Element::new("receiver_id").with_text(rid))
                    .with_child(Element::new("listener_id"))
                    .with_child(caps),
            )?;
            l.set_timeout(Some(Duration::from_millis(500)))?;
            let end = idn_open_protocol::cli::deadline(secs);
            while Instant::now() < end {
                let msgs = match l.recv() {
                    Ok(Some(msgs)) => msgs,
                    // The server went away: say so, instead of spinning (R1-129).
                    Ok(None) => {
                        eprintln!("the Discovery Server closed the connection");
                        break;
                    }
                    Err(e) if idn_open_protocol::transport::is_poll_timeout(&e) => continue,
                    Err(e) => {
                        eprintln!("{e}");
                        break;
                    }
                };
                {
                    for m in msgs {
                        for r in m
                            .child("listener")
                            .into_iter()
                            .flat_map(|x| x.children_named("receiver"))
                        {
                            for p in r.children_named("property") {
                                println!(
                                    "{} = {}",
                                    p.attr("name").unwrap_or("?"),
                                    p.attr("value").unwrap_or("")
                                );
                            }
                        }
                    }
                }
            }
        }
        _ => args.fail("unknown command"),
    }
    Ok(())
}
