//! The sample rate sound runs at on this machine (44.1, 48 and 96 kHz,
//! one rate for the site with a machine's and a program's own override;
//! spec/extensions/idn-node.md §N.40).
//!
//! The Node works out the machine's rate (its own setting, else the site option
//! `audio.rate` for this machine, else 48 kHz) and gives it in its `identity` as
//! `<audio_rate hz="…" from="machine|site|default"/>`. A program takes its own override
//! first ([`for_program`]). Sound between programs carries its rate in every block, and a
//! receiver at another rate converts it, so a program at another rate still plays.

use crate::xml::Element;
use std::sync::atomic::{AtomicU32, Ordering};

/// The rates IDN runs at.
pub const RATES: [u32; 3] = [44_100, 48_000, 96_000];
/// The rate when nothing says otherwise.
pub const DEFAULT: u32 = 48_000;

/// Whether `hz` is one of [`RATES`].
pub fn valid(hz: u32) -> bool {
    RATES.contains(&hz)
}

/// A rate as people write it: `44100`, `44.1`, `44.1k`, `48`, `48 kHz`, `96000` …
pub fn parse(s: &str) -> Option<u32> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.trim_end_matches("hz").trim_end_matches('k').trim();
    let v: f64 = t.parse().ok()?;
    let hz = if v < 1000.0 { v * 1000.0 } else { v };
    let hz = hz.round() as u32;
    valid(hz).then_some(hz)
}

/// `44.1 kHz`, `48 kHz`, `96 kHz`.
pub fn label(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", f64::from(hz) / 1000.0)
    }
}

/// Where a machine's rate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The machine's own setting.
    Machine,
    /// The site option `audio.rate`.
    Site,
    /// Neither: [`DEFAULT`].
    Default,
}

impl Source {
    /// As the Node's `from` attribute says it.
    pub fn name(self) -> &'static str {
        match self {
            Source::Machine => "machine",
            Source::Site => "site",
            Source::Default => "default",
        }
    }
}

/// The machine's rate: its own setting, else the site's, else [`DEFAULT`].
pub fn resolve(machine: Option<u32>, site: Option<&str>) -> (u32, Source) {
    if let Some(hz) = machine.filter(|&h| valid(h)) {
        return (hz, Source::Machine);
    }
    if let Some(hz) = site.and_then(parse) {
        return (hz, Source::Site);
    }
    (DEFAULT, Source::Default)
}

/// The identity's child.
pub fn element(hz: u32, from: Source) -> Element {
    Element::new("audio_rate")
        .with_attr("hz", &hz.to_string())
        .with_attr("from", from.name())
}

/// The rate an identity (or a status) gives, when it says.
pub fn of_identity(identity: &Element) -> Option<u32> {
    identity
        .child("audio_rate")
        .and_then(|e| e.attr("hz"))
        .and_then(|v| v.parse().ok())
        .filter(|&h| valid(h))
}

static MACHINE: AtomicU32 = AtomicU32::new(0);

/// The machine's rate as this program last heard it from the Node ([`DEFAULT`] before it
/// has, or without a Node).
pub fn machine() -> u32 {
    match MACHINE.load(Ordering::Relaxed) {
        0 => DEFAULT,
        hz => hz,
    }
}

/// Whether this program has heard the machine's rate from the Node.
pub fn heard_yet() -> bool {
    MACHINE.load(Ordering::Relaxed) != 0
}

/// Takes the rate in the Node's identity (at connection and every change).
pub(crate) fn heard(identity: &Element) {
    if let Some(hz) = of_identity(identity) {
        MACHINE.store(hz, Ordering::Relaxed);
    }
}

/// Asks `node` for the machine's rate now (and keeps it as heard); [`DEFAULT`] when it
/// doesn't say (an older Node).
pub fn ask(node: &super::Node) -> u32 {
    if let Ok(id) = node.request(Element::new("identity")) {
        heard(&id);
    }
    machine()
}

/// A program's rate: its own override when it has one, else the machine's.
pub fn for_program(own: Option<u32>) -> u32 {
    own.filter(|&h| valid(h)).unwrap_or_else(machine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_as_written_and_where_they_come_from() {
        for (s, hz) in [
            ("44100", Some(44_100)),
            ("44.1", Some(44_100)),
            ("44.1k", Some(44_100)),
            ("48 kHz", Some(48_000)),
            ("48", Some(48_000)),
            ("96000", Some(96_000)),
            ("88200", None),
            ("22050", None),
            ("fast", None),
        ] {
            assert_eq!(parse(s), hz, "{s}");
        }
        assert_eq!(label(44_100), "44.1 kHz");
        assert_eq!(label(96_000), "96 kHz");
        assert_eq!(
            resolve(Some(96_000), Some("44100")),
            (96_000, Source::Machine)
        );
        assert_eq!(resolve(None, Some("44.1")), (44_100, Source::Site));
        assert_eq!(
            resolve(Some(12), Some("nonsense")),
            (DEFAULT, Source::Default)
        );
        let e = Element::new("identity").with_child(element(44_100, Source::Site));
        assert_eq!(of_identity(&e), Some(44_100));
        assert_eq!(for_program(Some(96_000)), 96_000);
    }
}
