//! Publishes IDN source names (NDI-Compatible) on mDNS (spec §5.1.2) and serves the host listing port
//! (§5.2) for a while. Discovery only: nothing listens on the advertised control ports.
//!
//! Options: `idn-publish --help`.
//! The host part of the full NDI name is this machine's upper-case host name.

use idn_open_protocol::discovery::responder::{HostInfo, Published, Responder};
use std::time::Duration;

pub fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    // Ctrl+C / service stop: send mDNS goodbyes and departure notices first (§5.1.2).
    idn_open_protocol::shutdown::install();
    let mut args = idn_open_protocol::cli::Args::new(
        "usage: idn-publish [--seconds N] [--iface IPV4] [--no-listing] [--listing-port P] [--server HOST[:PORT]]\n\
         \x20                  \"NAME:PORT[:groups][:routing]\"...",
    );
    let (mut secs, mut iface, mut listing, mut listing_port, mut specs) =
        (30u64, None, true, None, Vec::new());
    let mut server: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => secs = args.value("--seconds"),
            "--iface" => iface = Some(args.value("--iface")),
            "--no-listing" => listing = false,
            "--listing-port" => listing_port = Some(args.value("--listing-port")),
            "--server" => server = Some(args.value("--server")),
            "-h" | "--help" => args.help(),
            s if !s.starts_with('-') => specs.push(s.to_owned()),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    if specs.is_empty() {
        args.fail("no source given");
    }
    let mut host = HostInfo::local(iface)?;
    if let Some(p) = listing_port {
        host.listing_port = p;
    }
    let upper = host.host.to_uppercase();
    let mut sources = Vec::new();
    for spec in &specs {
        let mut parts = spec.split(':');
        let name = parts.next().unwrap_or_default();
        let Some(port) = parts.next().and_then(|p| p.parse().ok()) else {
            args.fail(&format!(
                "bad source (want NAME:PORT[:groups][:routing]): {spec}"
            ))
        };
        let mut groups = vec!["public".to_owned()];
        let mut routing = false;
        for p in parts {
            if p == "routing" {
                routing = true;
            } else {
                groups = p.split(',').map(str::to_owned).collect();
            }
        }
        sources.push(Published {
            name: format!("{upper} ({name})"),
            port,
            groups,
            routing,
            idn_ext: None,
        });
    }
    for s in &sources {
        println!(
            "publishing \"{}\" port {} groups={}{} on {} ({}.local), listing port {}",
            s.name,
            s.port,
            s.groups.join(","),
            if s.routing { " routing" } else { "" },
            host.addr,
            host.host,
            if listing {
                host.listing_port.to_string()
            } else {
                "off".into()
            }
        );
    }
    if let Some(sv) = &server {
        // Discovery Server registration (§8.2.1) instead of mDNS, kept alive across server
        // restarts.
        let addr = idn_open_protocol::discovery::ds::server_addr(sv)?;
        let regs: Vec<_> = sources
            .iter()
            .map(|s| idn_open_protocol::discovery::ds::SourceRegistration::start(addr, s))
            .collect();
        println!(
            "registered {} source(s) with discovery server {addr}",
            regs.len()
        );
        std::thread::sleep(Duration::from_secs(secs));
        drop(regs);
        println!("stopped (registrations closed)");
        return Ok(());
    }
    let r = Responder::start(host, sources, listing)?;
    std::thread::sleep(Duration::from_secs(secs));
    drop(r);
    println!("stopped (goodbye sent)");
    Ok(())
}
