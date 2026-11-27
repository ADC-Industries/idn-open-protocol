//! Audio routing through the IDN Node (spec/extensions/idn-routing.md): routable
//! receivers, their routes, and the [`Router`] that receives what is routed to one.
//!
//! A program offers a receiver with [`AudioReceiver::register`]: a name and a number of
//! channels. Routes (receiver channel ← transmitter channel) are kept by the machine's
//! Node and set from the IDN Site Manager's matrix or `idn-node route`; the receiver
//! hears every change. [`Router`] does the receiving: one input per transmitter, only
//! the channels it needs (`channels/1`), each block handed on with where its channels go.

mod receiver;
mod router;
mod sync;
#[cfg(test)]
mod tests;

pub use receiver::AudioReceiver;
pub use router::{RoutedBlock, Router, RouterConfig, RouterEvent};
pub use sync::{SyncMember, group_floors, sync_members};

use crate::xml::Element;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// The most channels a receiver offers (and the highest channel number a route names,
/// plus one).
pub const MAX_CHANNELS: u16 = 256;

/// A receiver's latency as the site may set it (§A.12), in ms: from the ultra-low mode
/// (a wired network, small blocks) to well past the default (Wi-Fi).
pub const LATENCY_MS: std::ops::RangeInclusive<u32> = 1..=500;

/// A receiver's latency from a `latency_ms` attribute (§A.12): `None` when absent, empty
/// or outside [`LATENCY_MS`] (the program's own then).
pub fn parse_latency(v: Option<&str>) -> Option<u32> {
    v?.trim()
        .parse::<u32>()
        .ok()
        .filter(|ms| LATENCY_MS.contains(ms))
}

/// One route: receiver channel `ch` plays transmitter `tx`'s channel `tx_ch`. Channels
/// are numbered from 0.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Route {
    /// The receiver's channel.
    pub ch: u16,
    /// The transmitter, by its full source name, `HOST (name)`.
    pub tx: String,
    /// The transmitter's channel.
    pub tx_ch: u16,
}

/// How a routed channel is doing, as the receiving program reports it (§A.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteState {
    /// Waiting for the transmitter's sound.
    Connecting,
    /// Sound arrives.
    Ok,
    /// The transmitter isn't found, or the session with it fails.
    NoSource,
    /// The transmitter has fewer channels than the route names, or the receiver fewer.
    NoChannel,
}

impl RouteState {
    /// `connecting`, `ok`, `no_source` or `no_channel`.
    pub fn name(self) -> &'static str {
        match self {
            RouteState::Connecting => "connecting",
            RouteState::Ok => "ok",
            RouteState::NoSource => "no_source",
            RouteState::NoChannel => "no_channel",
        }
    }
}

/// What a routable receiver is (§A.16), as the program offering it says
/// ([`AudioReceiver::register_as`]); a program that doesn't say is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReceiverKind {
    /// A program's own sound (OBS, a presentation program, an audio editor's plugin).
    Program,
    /// A sound card's outputs (`idn-audio recv --routable`).
    SoundCard,
    /// The IDN ASIO driver: a program on this machine records it.
    AsioDriver,
    /// A network audio device's inputs, through this machine's pool (idn-aoip).
    NetworkDevice,
    /// A mixing desk's inputs (the IDN Mixer).
    Desk,
    /// A switcher's sound (its feeds and routed sound).
    Switcher,
    /// Not said (an older program), or something else.
    #[default]
    Other,
}

impl ReceiverKind {
    /// `program`, `sound_card`, `asio_driver`, `network_device`, `desk`, `switcher` or
    /// `other`.
    pub fn name(self) -> &'static str {
        match self {
            ReceiverKind::Program => "program",
            ReceiverKind::SoundCard => "sound_card",
            ReceiverKind::AsioDriver => "asio_driver",
            ReceiverKind::NetworkDevice => "network_device",
            ReceiverKind::Desk => "desk",
            ReceiverKind::Switcher => "switcher",
            ReceiverKind::Other => "other",
        }
    }

    /// From its name (unknown or empty: `Other`).
    pub fn parse(s: &str) -> ReceiverKind {
        match s {
            "program" => ReceiverKind::Program,
            "sound_card" => ReceiverKind::SoundCard,
            "asio_driver" => ReceiverKind::AsioDriver,
            "network_device" => ReceiverKind::NetworkDevice,
            "desk" => ReceiverKind::Desk,
            "switcher" => ReceiverKind::Switcher,
            _ => ReceiverKind::Other,
        }
    }
}

/// How a routed channel's sound arrives (§A.16), from the transport its blocks came by
/// ([`AudioIn::via`](crate::input::AudioIn::via)): `machine` (this machine's shared-memory
/// ring, `machine/1`, or in the same process) or `network`, and on the network
/// `multicast` or `unicast`.
pub fn via_of(transport: &str) -> (&'static str, Option<&'static str>) {
    match transport {
        "machine" | "local" => ("machine", None),
        "multicast" => ("network", Some("multicast")),
        "" => ("", None),
        _ => ("network", Some("unicast")),
    }
}

/// Where a transmitter's sound comes from (§A.16), told by the program sending it
/// ([`Output::set_audio_origin`](crate::api::Output::set_audio_origin)): a sound card's
/// inputs, a network audio device's channels, a desk's buses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioOrigin {
    /// `sound_card`, `network_device`, `desk`, `program`, … (free; these are the ones IDN's
    /// programs use).
    pub kind: String,
    /// The device: a sound card's name, a network device's, a desk's (empty: per channel).
    pub device: String,
    /// What each of the transmitter's channels is there, in order (`In 3`, `Main L`,
    /// `DEVICE/Mic 1`); empty: not said.
    pub channels: Vec<String>,
}

impl AudioOrigin {
    /// The registration's attributes (`origin_kind`, `origin_device`, `origin_channels`).
    pub fn attrs(&self) -> [(&'static str, String); 3] {
        [
            ("origin_kind", self.kind.clone()),
            ("origin_device", self.device.clone()),
            ("origin_channels", labels_attr(&self.channels)),
        ]
    }

    /// From an `audio_tx` (or registration) element's attributes; `None` without a kind.
    pub fn from_element(e: &Element) -> Option<AudioOrigin> {
        let kind = e.attr("origin_kind").filter(|k| !k.is_empty())?;
        Some(AudioOrigin {
            kind: kind.to_owned(),
            device: e.attr("origin_device").unwrap_or("").to_owned(),
            channels: parse_labels(e.attr("origin_channels")),
        })
    }

    /// As JSON: `{"kind", "device", "channels"}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"kind": self.kind, "device": self.device, "channels": self.channels})
    }
}

/// The routes of a `<routes>` element (§A.3), ordered by receiver channel.
pub fn parse_routes(e: &Element) -> Vec<Route> {
    let mut v: Vec<Route> = e
        .children_named("route")
        .filter_map(|r| {
            let ch = r
                .attr("ch")?
                .parse::<u16>()
                .ok()
                .filter(|&c| c < MAX_CHANNELS)?;
            let tx = r.attr("tx").filter(|t| !t.is_empty())?.to_owned();
            let tx_ch = r
                .attr("tx_ch")?
                .parse::<u16>()
                .ok()
                .filter(|&c| c < MAX_CHANNELS)?;
            Some(Route { ch, tx, tx_ch })
        })
        .collect();
    v.sort();
    v.dedup_by_key(|r| r.ch);
    v
}

/// `<routes receiver="…" latency_ms?>` with a `route` per entry; `latency_ms` is the
/// receiver's latency the site set (§A.12), if it did.
pub fn routes_element(receiver: &str, routes: &[Route], latency_ms: Option<u32>) -> Element {
    let mut e = Element::new("routes").with_attr("receiver", receiver);
    if let Some(ms) = latency_ms {
        e = e.with_attr("latency_ms", &ms.to_string());
    }
    for r in routes {
        e = e.with_child(
            Element::new("route")
                .with_attr("ch", &r.ch.to_string())
                .with_attr("tx", &r.tx)
                .with_attr("tx_ch", &r.tx_ch.to_string()),
        );
    }
    e
}

/// Channel labels as sent (`a,b,c`; commas in a label become spaces).
pub fn labels_attr(labels: &[String]) -> String {
    labels
        .iter()
        .map(|l| l.replace(',', " ").trim().to_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// Labels from `a,b,c` (empty: none).
pub fn parse_labels(s: Option<&str>) -> Vec<String> {
    match s.map(str::trim) {
        None | Some("") => Vec::new(),
        Some(s) => s.split(',').map(|l| l.trim().to_owned()).collect(),
    }
}

/// The routing matrix's data (the panels' `/ui/matrix.js`, idn-routing.md §A.6) from a
/// Node's status: its transmitters (`audio_tx`) and receivers (`audio_rx`, with their
/// routes and latency), all on `machine`. Shared by the Node's panel and the IDN Mixer's
/// Routing page.
pub fn matrix_json(status: &Element, machine: &str) -> serde_json::Value {
    use serde_json::json;
    json!({
        "transmitters": status.children_named("audio_tx").map(|t| json!({
            "name": t.attr("name"),
            "channels": t.attr("channels").and_then(|c| c.parse::<u16>().ok()).unwrap_or(2),
            "labels": parse_labels(t.attr("labels")),
            "machine": machine,
            "known": true,
            "origin": AudioOrigin::from_element(t).map(|o| o.to_json()),
        })).collect::<Vec<_>>(),
        "receivers": status.children_named("audio_rx").map(|x| json!({
            "name": x.attr("name"),
            "channels": x.attr("channels").and_then(|c| c.parse::<u16>().ok()).unwrap_or(0),
            "labels": parse_labels(x.attr("labels")),
            "app": x.attr("app"),
            "rx_kind": ReceiverKind::parse(x.attr("kind").unwrap_or("")).name(),
            "machine": machine,
            "latency_ms": parse_latency(x.attr("latency_ms")),
            "routes": x.children_named("route").map(route_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// A route of an `audio_rx` element as JSON (§A.4, §A.16): `ch`, `tx`, `tx_ch`, `state`,
/// `signal`, and how its sound arrives (`via` `machine` or `network`, `cast` `multicast`
/// or `unicast` on the network, `transport` as the library names it), once sound came.
pub fn route_json(r: &Element) -> serde_json::Value {
    let t = r.attr("via").unwrap_or("");
    let (via, cast) = via_of(t);
    serde_json::json!({
        "ch": r.attr("ch").and_then(|c| c.parse::<u16>().ok()),
        "tx": r.attr("tx").unwrap_or(""),
        "tx_ch": r.attr("tx_ch").and_then(|c| c.parse::<u16>().ok()),
        "state": r.attr("state").unwrap_or(""),
        "signal": r.attr("sig") == Some("1"),
        "via": (!via.is_empty()).then_some(via),
        "cast": cast,
        "transport": (!t.is_empty()).then_some(t),
    })
}

/// One route change: receiver `receiver`'s channel `ch` to `to` (`None` clears it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteChange {
    /// The receiver, by name on its machine.
    pub receiver: String,
    /// The receiver's channel.
    pub ch: u16,
    /// (transmitter, its channel); `None` clears the route.
    pub to: Option<(String, u16)>,
}

impl RouteChange {
    /// A `<route receiver ch tx tx_ch/>` element (idn-routing.md §A.3, §A.4).
    pub fn parse(e: &Element) -> io::Result<RouteChange> {
        use idn_common::err::invalid_input as bad;
        let receiver = e
            .attr("receiver")
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| bad("route: no receiver"))?
            .to_owned();
        let ch = e
            .attr("ch")
            .and_then(|c| c.parse::<u16>().ok())
            .filter(|&c| c < MAX_CHANNELS)
            .ok_or_else(|| bad("route: ch must be 0 to 255"))?;
        let to = match e.attr("tx").map(str::trim).filter(|t| !t.is_empty()) {
            None => None,
            Some(tx) => {
                let tx_ch = e
                    .attr("tx_ch")
                    .and_then(|c| c.parse::<u16>().ok())
                    .filter(|&c| c < MAX_CHANNELS)
                    .ok_or_else(|| bad("route: tx_ch must be 0 to 255"))?;
                Some((tx.to_owned(), tx_ch))
            }
        };
        Ok(RouteChange { receiver, ch, to })
    }

    /// The element form.
    pub fn to_element(&self) -> Element {
        let e = Element::new("route")
            .with_attr("receiver", &self.receiver)
            .with_attr("ch", &self.ch.to_string());
        match &self.to {
            Some((tx, tx_ch)) => e.with_attr("tx", tx).with_attr("tx_ch", &tx_ch.to_string()),
            None => e.with_attr("tx", ""),
        }
    }
}

/// The routes the Node keeps (`routes.xml`, §A.2), by receiver name.
#[doc(hidden)]
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RouteTable {
    by_receiver: BTreeMap<String, BTreeMap<u16, (String, u16)>>,
    /// Receivers' latencies set by the site (§A.12), kept with or without routes.
    latency: BTreeMap<String, u32>,
}

impl RouteTable {
    /// The table in `path`, or an empty one.
    pub fn load(path: &Path) -> RouteTable {
        let mut t = RouteTable::default();
        // (Unreadable or unparsable: set aside and said, not saved over by the next
        // route change.)
        let doc = match idn_common::fsutil::load_saved(path, |b| {
            crate::xml::parse(std::str::from_utf8(b).ok()?)
        }) {
            Ok(Some(d)) => d,
            Ok(None) => return t,
            Err(why) => {
                crate::log::log(&format!("routes: {why}"));
                return t;
            }
        };
        for rx in doc.children_named("routes") {
            let Some(name) = rx.attr("receiver").filter(|n| !n.is_empty()) else {
                continue;
            };
            if let Some(ms) = parse_latency(rx.attr("latency_ms")) {
                t.latency.insert(name.to_owned(), ms);
            }
            let routes = parse_routes(rx);
            if routes.is_empty() {
                continue;
            }
            t.by_receiver.insert(
                name.to_owned(),
                routes
                    .into_iter()
                    .map(|r| (r.ch, (r.tx, r.tx_ch)))
                    .collect(),
            );
        }
        t
    }

    /// Writes the table to `path` (readable by the Node's account only).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut doc = Element::new("idn_routes");
        for name in self.receivers() {
            doc = doc.with_child(routes_element(
                &name,
                &self.routes(&name),
                self.latency(&name),
            ));
        }
        crate::trust::pki::write_private(path, doc.to_xml().as_bytes())
    }

    /// A receiver's routes, by channel.
    pub fn routes(&self, receiver: &str) -> Vec<Route> {
        self.by_receiver
            .get(receiver)
            .map(|m| {
                m.iter()
                    .map(|(&ch, (tx, tx_ch))| Route {
                        ch,
                        tx: tx.clone(),
                        tx_ch: *tx_ch,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The receivers with routes or a latency.
    pub fn receivers(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .by_receiver
            .keys()
            .chain(self.latency.keys())
            .cloned()
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// A receiver's latency set by the site (§A.12).
    pub fn latency(&self, receiver: &str) -> Option<u32> {
        self.latency.get(receiver).copied()
    }

    /// Sets (or with `None` clears) a receiver's latency; whether it changed.
    pub fn set_latency(&mut self, receiver: &str, ms: Option<u32>) -> bool {
        match ms {
            Some(ms) => self.latency.insert(receiver.to_owned(), ms) != Some(ms),
            None => self.latency.remove(receiver).is_some(),
        }
    }

    /// Applies a change; whether anything changed.
    pub fn apply(&mut self, c: &RouteChange) -> bool {
        match &c.to {
            Some(to) => {
                let m = self.by_receiver.entry(c.receiver.clone()).or_default();
                m.insert(c.ch, to.clone()).as_ref() != Some(to)
            }
            None => {
                let Some(m) = self.by_receiver.get_mut(&c.receiver) else {
                    return false;
                };
                let removed = m.remove(&c.ch).is_some();
                if m.is_empty() {
                    self.by_receiver.remove(&c.receiver);
                }
                removed
            }
        }
    }
}
