//! Finding sources: a live source list for your UI.
//!
//! [`Browser`] keeps a current list and reports changes as [`SourceEvent`]s. It finds
//! NDI and IDN sources alike, on the local network (mDNS) or through a Discovery Server.
//! For a one-off look, use [`browse`] or [`find`].
//!
//! You don't need discovery to receive: [`Input`](crate::input::Input) resolves names
//! by itself and also accepts `ip:port`.
//!
//! ```no_run
//! use idn_open_protocol::discovery::{Browser, BrowserConfig, SourceEvent};
//! use std::time::Duration;
//!
//! # fn main() -> std::io::Result<()> {
//! let browser = Browser::start(BrowserConfig::default())?;
//! for s in browser.sources() {
//!     println!("already there: {}", s.name);
//! }
//! while let Some(ev) = browser.recv(Duration::from_secs(1)) {
//!     match ev {
//!         SourceEvent::Added(s) => {
//!             let lock = if s.offers("secure") { " (can encrypt)" } else { "" };
//!             println!("new source: {}{lock}", s.name);
//!         }
//!         SourceEvent::Changed(s) => println!("changed: {}", s.name),
//!         SourceEvent::Removed(s) => println!("gone: {}", s.name),
//!     }
//! }
//! # Ok(())
//! # }
//! ```

#[doc(hidden)]
pub mod dns;
#[doc(hidden)]
pub mod ds;
#[doc(hidden)]
pub mod ds_server;
#[doc(hidden)]
pub mod ifaces;
mod monitor;
pub use monitor::{Device, DeviceEvent, DeviceKind, Monitor};

/// An IDN Discovery Server running in this process: sources register with it, finders
/// and monitors query it, and with an [`Authority`](crate::trust::Authority) it is the
/// site's trust authority (approve, deny, revoke). Compatible with NDI clients. Stops
/// when dropped.
///
/// ```no_run
/// let server = idn_open_protocol::discovery::DiscoveryServer::start(5959, None)?;
/// println!("discovery server on port {}", server.port());
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct DiscoveryServer {
    port: u16,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

impl DiscoveryServer {
    /// Starts serving on TCP `port` (5959 is the standard one; 0 picks a free port).
    pub fn start(port: u16, authority: Option<crate::trust::Authority>) -> io::Result<Self> {
        let l = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
        let port = l.local_addr()?.port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let trust = authority.map_or(ds_server::Trust::None, ds_server::Trust::Authority);
            let _ = ds_server::serve_with(l, trust, &s);
        });
        Ok(DiscoveryServer {
            port,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }

    /// A Discovery Server with no authority of its own whose trust connections (joining,
    /// renewal, the CRL, options) are handed to `relay` with their hello: a remote site's
    /// server, its trust carried by the site link (idn-remote.md X.12.2).
    pub fn start_with_relay(
        port: u16,
        relay: Arc<dyn Fn(TcpStream, Vec<u8>) + Send + Sync>,
    ) -> io::Result<Self> {
        let l = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
        let port = l.local_addr()?.port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let _ = ds_server::serve_with(l, ds_server::Trust::Relay(relay), &s);
        });
        Ok(DiscoveryServer {
            port,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }

    /// The TCP port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[doc(hidden)]
pub mod responder;
pub mod service;
pub mod site;

mod browser;
mod mdns;
#[cfg(test)]
mod tests;

pub use browser::{Browser, BrowserConfig, SourceEvent, browse, find, service_types, shared};
pub(crate) use browser::{Change, ChangeEvent, queue_change};
pub(crate) use mdns::udp_socket;
pub use mdns::{BrowseOptions, Cache, browse_until, hostname};

use crate::control::{parse_element, unescape};
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

#[doc(hidden)]
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
#[doc(hidden)]
pub const MDNS_PORT: u16 = 5353;
#[doc(hidden)]
pub const SERVICE: &str = "_ndi._tcp.local";
/// Host listing port (§5.2).
#[doc(hidden)]
pub const LISTING_PORT: u16 = 5960;

/// A source on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Full name, `HOST (source name)`, e.g. `CAM-1 (Main)`. Pass it to
    /// [`InputConfig::new`](crate::input::InputConfig::new) to receive it.
    pub name: String,
    /// The source's IPv4 address. Always set for sources from [`Browser`]; may be
    /// missing from [`browse`] results whose host hasn't answered yet.
    pub addr: Option<IpAddr>,
    /// The TCP port receivers connect to.
    pub port: u16,
    /// The machine's mDNS host name (e.g. `cam-1.local`); empty with a Discovery Server.
    pub host: String,
    /// Groups the source belongs to; `public` unless configured otherwise. See
    /// [`groups_match`].
    pub groups: Vec<String>,
    /// A routing source: it forwards receivers to another source (e.g. a router output).
    pub routing: bool,
    /// The port of the host's source-list service, if it offers one.
    pub listing_port: Option<u16>,
    /// IDN features the source announced, e.g. `["resume/1", "stats/1", "secure/1"]`;
    /// empty for NDI sources. A hint for the UI: what a session actually uses is decided
    /// when it connects. See [`is_idn`](Source::is_idn) and [`offers`](Source::offers).
    pub idn_ext: Vec<String>,
}

impl Source {
    /// Address and port to connect to, once the address is known.
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.addr.map(|a| SocketAddr::new(a, self.port))
    }

    /// The source announced itself as an IDN device.
    pub fn is_idn(&self) -> bool {
        !self.idn_ext.is_empty()
    }

    /// The source announced IDN feature `name` (any version). `offers("secure")` means
    /// it has a site passphrase and can encrypt; whether it is *your* passphrase shows
    /// only on connect ([`InputStats::encrypted`](crate::input::InputStats::encrypted)).
    pub fn offers(&self, name: &str) -> bool {
        self.idn_ext
            .iter()
            .any(|t| t.split_once('/').is_some_and(|(n, _)| n == name))
    }

    /// What follows `name/` in the announcement: a screen that takes control gives its
    /// machine's key (`idnfp`) and its site (`idnca`) this way (§E.25).
    pub fn announced(&self, name: &str) -> Option<&str> {
        self.idn_ext.iter().find_map(|t| {
            t.split_once('/')
                .filter(|(n, _)| *n == name)
                .map(|(_, v)| v)
        })
    }

    /// A screen whose keyboard and mouse its site's devices or linked computers may use
    /// (§E.25), as it announced.
    pub fn takes_control(&self) -> bool {
        self.offers("control")
    }
}

/// The discovery hint among TXT strings (§E.2.2).
#[doc(hidden)]
pub fn parse_txt_idn(strings: &[Vec<u8>]) -> Vec<String> {
    strings
        .iter()
        .map(|s| crate::ext::parse_hint(&String::from_utf8_lossy(s)))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// TXT strings → (groups, routing, listing port). Unknown keys are ignored (§5.1.3).
#[doc(hidden)]
pub fn parse_txt(strings: &[Vec<u8>]) -> (Vec<String>, bool, Option<u16>) {
    let (mut groups, mut routing, mut listing) = (vec!["public".to_owned()], false, None);
    for s in strings {
        let s = String::from_utf8_lossy(s);
        match s.split_once('=') {
            // An empty list belongs to no group, not to `public` (§5.1.3).
            Some(("groups", v)) => {
                groups = v
                    .split(',')
                    .filter(|g| !g.trim().is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            Some(("discovery", v)) => listing = v.trim().parse().ok(),
            None if s == "routing" => routing = true,
            _ => {}
        }
    }
    (groups, routing, listing)
}

/// Whether a source in `source_groups` should be listed for a receiver in
/// `receiver_groups`: true if they share a group. Comparison ignores case and
/// surrounding spaces, as NDI does. [`Browser`] lists all groups; filter with this.
pub fn groups_match(source_groups: &[String], receiver_groups: &[String]) -> bool {
    source_groups.iter().any(|s| {
        receiver_groups
            .iter()
            .any(|r| s.trim().eq_ignore_ascii_case(r.trim()))
    })
}

// ---------------------------------------------------------------------------------------
// Host listing port (§5.2).

/// Parses the listing document (without its NUL).
#[doc(hidden)]
pub fn parse_listing(xml: &str, addr: Option<IpAddr>) -> Vec<Source> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<service") {
        rest = &rest[i..];
        // Skip `<services>` and any other element whose name merely starts with "service".
        if !rest["<service".len()..]
            .starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
        {
            rest = &rest[1..];
            continue;
        }
        let end = rest
            .find("</service>")
            .map_or(rest.len(), |e| e + "</service>".len());
        let block = &rest[..end];
        if let Some(e) = parse_element(block).filter(|e| e.name == "service") {
            let mut groups = Vec::new();
            let mut g = block;
            while let Some(gi) = g.find("<group>") {
                g = &g[gi + "<group>".len()..];
                let Some(ge) = g.find("</group>") else { break };
                if let Some(t) = unescape(&g[..ge]) {
                    groups.push(t);
                }
                g = &g[ge..];
            }
            if let (Some(n), Some(Ok(port))) = (e.attr("name"), e.attr("port").map(str::parse)) {
                out.push(Source {
                    name: n.to_owned(),
                    addr,
                    port,
                    host: String::new(),
                    groups,
                    routing: false,
                    listing_port: Some(LISTING_PORT),
                    idn_ext: Vec::new(),
                });
            }
        }
        rest = &rest[end..];
    }
    out
}

/// Connects to a host's listing port and reads its source list (§5.2).
#[doc(hidden)]
pub fn query_listing(addr: SocketAddr, timeout: Duration) -> io::Result<Vec<Source>> {
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(nul) = buf.iter().position(|&b| b == 0) {
            buf.truncate(nul);
            break;
        }
        if buf.len() > 1 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "listing too large",
            ));
        }
    }
    Ok(parse_listing(
        &String::from_utf8_lossy(&buf),
        Some(addr.ip()),
    ))
}

// Shared between threads by applications.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Browser>();
    assert_send_sync::<Monitor>();
};
