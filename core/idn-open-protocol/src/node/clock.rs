//! The site clock as programs see it (spec/extensions/idn-clock.md §C.3): the machine's
//! IDN Node runs PTP ([`crate::ptp`]) and tells its programs how this machine's monotonic
//! clock maps to site time, in a `<clock>` element when they connect and whenever the
//! mapping moves.
//!
//! ```xml
//! <clock state="slave" locked="true" gm="00-1d-…" offset_ns="-120" delay_ns="35000"
//!        domain="0" ports="319/320"><map mono_ns="…" site_ns="…" rate="1.000012"/></clock>
//! ```
//! `state` is also `master`, `listening`, `faulty`, `off` (no clock at all) or `machine`
//! (no PTP: the machine's own clock, its grandmaster `machine-…`; with `error` when PTP's
//! sockets couldn't be opened).

use crate::ptp::{Mapping, Ns, State, Status, identity_str, mono_ns};
use crate::xml::Element;
use idn_common::sync::RwLockOk as _;
use std::sync::RwLock;

/// The clock as a program last heard it from the Node.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockInfo {
    /// `master`, `slave`, `listening`, `faulty`, `off` or `error`.
    pub state: String,
    /// A slave whose offset has been within the limit for 10 s.
    pub locked: bool,
    /// The grandmaster's clock identity, `aa-bb-…`.
    pub grandmaster: String,
    /// The offset from the master the servo sees, ns.
    pub offset_ns: f64,
    /// The path delay to the master, ns.
    pub delay_ns: f64,
    /// Monotonic time to site time.
    pub mapping: Option<Mapping>,
}

impl ClockInfo {
    /// Master, a locked slave, or this machine's own clock (no PTP: programs here share
    /// it), with a mapping: site time can be trusted.
    pub fn usable(&self) -> bool {
        self.mapping.is_some()
            && (self.state == "master"
                || self.state == "machine"
                || (self.state == "slave" && self.locked))
    }

    /// The element for a clock's status (the Node).
    pub fn element(s: &Status, domain: u8, ports: (u16, u16)) -> Element {
        let mut e = Element::new("clock")
            .with_attr("state", s.state.name())
            .with_attr("locked", if s.state.usable() { "true" } else { "false" })
            .with_attr("gm", &identity_str(&s.grandmaster))
            .with_attr("offset_ns", &format!("{:.0}", s.offset_ns))
            .with_attr("delay_ns", &format!("{:.0}", s.delay_ns))
            .with_attr("domain", &domain.to_string())
            .with_attr("ports", &format!("{}/{}", ports.0, ports.1));
        if s.two_domains {
            e = e.with_attr("two_domains", "true");
        }
        if s.state != State::Faulty {
            let m = s.mapping;
            e = e.with_child(
                Element::new("map")
                    .with_attr("mono_ns", &m.mono_ns.to_string())
                    .with_attr("site_ns", &m.site_ns.to_string())
                    .with_attr("rate", &format!("{:.12}", m.rate)),
            );
        }
        e
    }

    /// From a `<clock>` element.
    pub fn from_element(e: &Element) -> ClockInfo {
        let num = |k: &str| e.attr(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let mapping = e.children_named("map").next().and_then(|m| {
            Some(Mapping {
                mono_ns: m.attr("mono_ns")?.parse().ok()?,
                site_ns: m.attr("site_ns")?.parse().ok()?,
                rate: m
                    .attr("rate")?
                    .parse()
                    .ok()
                    .filter(|r: &f64| (0.99..1.01).contains(r))?,
            })
        });
        ClockInfo {
            state: e.attr("state").unwrap_or("").to_owned(),
            locked: e.attr("locked") == Some("true"),
            grandmaster: e.attr("gm").unwrap_or("").to_owned(),
            offset_ns: num("offset_ns"),
            delay_ns: num("delay_ns"),
            mapping,
        }
    }
}

static HEARD: RwLock<Option<ClockInfo>> = RwLock::new(None);

/// Takes a `<clock>` from the Node (at connection and at every change).
pub(crate) fn heard(e: &Element) {
    *HEARD.write_ok() = Some(ClockInfo::from_element(e));
}

/// The clock as last heard from the Node; `None` when this program doesn't use the Node
/// or the Node has no clock (an older one).
pub fn status() -> Option<ClockInfo> {
    HEARD.read_ok().clone()
}

/// Site time now (TAI ns since the PTP epoch), when the machine's Node is master or a
/// locked slave; else `None`. Cheap: no request to the Node.
pub fn site_now() -> Option<Ns> {
    let c = HEARD.read_ok();
    let c = c.as_ref().filter(|c| c.usable())?;
    Some(c.mapping?.site(mono_ns()))
}

/// The grandmaster's identity when site time is usable: senders put it in their
/// metadata (`<x_idn_clock gm="…"/>`) so receivers know two senders share a clock.
pub fn grandmaster() -> Option<String> {
    let c = HEARD.read_ok();
    c.as_ref()
        .filter(|c| c.usable())
        .map(|c| c.grandmaster.clone())
}

/// [`grandmaster`] when it is a PTP clock's (not this machine's own clock, state
/// `machine`): what AES67 and other PTP equipment may be told (`ts-refclk:ptp=…`).
pub fn ptp_grandmaster() -> Option<String> {
    let c = HEARD.read_ok();
    c.as_ref()
        .filter(|c| c.usable() && c.state != "machine")
        .map(|c| c.grandmaster.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_element_round_trips() {
        let s = Status {
            state: State::Slave { locked: true },
            grandmaster: [0, 0x1d, 0, 0, 0, 0, 0, 7],
            offset_ns: -120.0,
            delay_ns: 35_000.0,
            mapping: Mapping {
                mono_ns: 123_456_789,
                site_ns: 1_761_400_000_000_000_000,
                rate: 1.000_012,
            },
            two_domains: false,
        };
        let e = ClockInfo::element(&s, 0, (319, 320));
        let back = ClockInfo::from_element(&crate::xml::parse(&e.to_xml()).unwrap());
        assert!(back.usable());
        assert_eq!(back.grandmaster, "00-1d-00-00-00-00-00-07");
        assert_eq!(back.mapping.unwrap().site_ns, s.mapping.site_ns);
        assert!((back.mapping.unwrap().rate - 1.000_012).abs() < 1e-12);
        heard(&e);
        let t = site_now().unwrap();
        assert!((t - s.mapping.site(mono_ns())).abs() < 1_000_000);
    }
}
