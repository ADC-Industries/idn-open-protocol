//! Lists IDN / NDI-Compatible sources found by mDNS (spec §5.1) and, optionally, by querying hosts'
//! listing ports (§5.2).
//!
//! Options: `idn-find --help`.

use idn_open_protocol::discovery::{
    BrowseOptions, LISTING_PORT, Source, browse, groups_match, query_listing,
};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

pub fn main() {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let mut args = idn_open_protocol::cli::Args::new(
        "usage: idn-find [--seconds N] [--iface IPV4] [--groups a,b] [--host IP[:PORT]]... [--server HOST[:PORT]] [--no-node]\n\
         \n\
         Without --server, the machine's IDN Node's Discovery Server when it has one (unless\n\
         --no-node), else mDNS.",
    );
    let (mut secs, mut opts, mut groups, mut hosts) =
        (3u64, BrowseOptions::default(), None, Vec::new());
    let mut server: Option<String> = None;
    let mut use_node = true;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => secs = args.value("--seconds"),
            "--iface" => opts.interface = Some(args.value("--iface")),
            "--groups" => {
                groups = Some(
                    args.value::<String>("--groups")
                        .split(',')
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                )
            }
            "--host" => {
                let h: String = args.value("--host");
                let h = idn_open_protocol::discovery::ds::with_default_port(&h, LISTING_PORT);
                match h.to_socket_addrs().ok().and_then(|mut i| i.next()) {
                    Some(a) => hosts.push(a),
                    None => args.fail(&format!("invalid --host: {h}")),
                }
            }
            "--server" => server = Some(args.value("--server")),
            "--no-node" => use_node = false,
            "-h" | "--help" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    // The machine's Discovery Server, as the IDN Node has it (sources of an
    // enrolled site are listed there, not on mDNS).
    if server.is_none() && use_node {
        server = idn_open_protocol::node::Node::connect("idn-find")
            .ok()
            .and_then(|n| n.enrollment().node_machine())
            .and_then(|m| m.discovery_server);
        if let Some(sv) = &server {
            eprintln!("idn-find: the IDN Node's Discovery Server, {sv}");
        }
    }
    let mut found: Vec<(&str, Source)> = Vec::new();
    // With a Discovery Server configured, it replaces mDNS (chapter 8).
    if let Some(sv) = &server {
        match idn_open_protocol::discovery::ds::server_addr(sv).and_then(|a| {
            idn_open_protocol::discovery::ds::query_sources(a, Duration::from_secs(secs))
        }) {
            Ok(v) => found.extend(v.into_iter().map(|s| ("server", s))),
            Err(e) => {
                eprintln!("idn-find: discovery server {sv}: {e}");
                std::process::exit(1);
            }
        }
    } else {
        match browse(Duration::from_secs(secs), &opts) {
            Ok(v) => found.extend(v.into_iter().map(|s| ("mdns", s))),
            Err(e) => {
                eprintln!("idn-find: mDNS browse failed: {e}");
                if hosts.is_empty() {
                    std::process::exit(1);
                }
            }
        }
    }
    for h in &hosts {
        match query_listing(*h, Duration::from_secs(secs)) {
            Ok(v) => found.extend(v.into_iter().map(|s| ("listing", s))),
            Err(e) => eprintln!("listing {h} failed: {e}"),
        }
    }
    let mut shown = 0;
    for (via, s) in &found {
        if let Some(g) = &groups
            && !groups_match(&s.groups, g)
        {
            continue;
        }
        shown += 1;
        let addr = s
            .socket_addr()
            .map_or_else(|| format!("?:{}", s.port), |a: SocketAddr| a.to_string());
        println!(
            "{:<40} {:<22} groups={}{}{} [{via}{}]",
            s.name,
            addr,
            s.groups.join(","),
            if s.routing { " routing" } else { "" },
            // Discovery hint (§E.2.2): an IDN device, and whether it can encrypt.
            match (s.is_idn(), s.offers("secure")) {
                (false, _) => String::new(),
                (true, false) => format!(" IDN({})", s.idn_ext.join(" ")),
                (true, true) => format!(" IDN+encryption({})", s.idn_ext.join(" ")),
            },
            if s.host.is_empty() {
                String::new()
            } else {
                format!(" host {}", s.host)
            },
        );
    }
    eprintln!("{shown} source(s)");
}
