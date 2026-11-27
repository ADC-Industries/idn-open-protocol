//! Switchers' outputs as desk inputs by themselves (spec/extensions/idn-mixer.md §M.17):
//! each switcher output with sound on the network (announced as `switcher/1`) is routed,
//! the first time it is seen, onto two free desk inputs from the top, through this
//! machine's IDN Node. It goes on no channel: the user patches it when wanted. Inputs the
//! user takes off stay off; an output gone for an hour whose inputs no channel uses gives
//! them back.

use idn_open_protocol::node::routing::{Route, RouteChange};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a switcher's output announces (idn-switcher.md §S.25).
pub const ROLE: &str = "switcher/1";
/// What a switcher output that takes a desk's feed announces (idn-switcher.md §S.26).
pub const FEED_ROLE: &str = "switcher-feed";
/// What the desk's output announces, so switchers find it (§M.18).
pub const DESK_ROLE: &str = "desk/1";

/// The bus channels (the output patch's `bus:J`, left then right, by the outside numbering
/// [`crate::desk::bus_out`]) a feed's bus is on: `main`, `aux:N`, `group:N`, `matrix:N`
/// (from 1).
pub fn bus_pair(bus: &str) -> Option<usize> {
    use crate::desk::{AUX, GROUPS, MATRIX, bus_out, group_out, matrix_out};
    let (kind, n) = bus.split_once(':').unwrap_or((bus, "1"));
    let n: usize = n.parse().ok().filter(|&n| n >= 1)?;
    let inside = match kind {
        "main" if n == 1 => 0,
        "aux" if n <= AUX => 2 * n,
        "group" if n <= GROUPS => group_out(n - 1),
        "matrix" if n <= MATRIX => matrix_out(n - 1),
        _ => return None,
    };
    Some(bus_out(inside))
}

/// The desk's feeds as the metadata its output sends (idn-switcher.md §S.26): each
/// switcher output's full name and the desk channels (from 0) carrying its mix.
pub fn feeds_xml(feeds: &[(String, [usize; 2])]) -> String {
    let lines: Vec<String> = feeds
        .iter()
        .map(|(name, [l, r])| format!("{}={l},{r}", idn_open_protocol::control::escape_attr(name)))
        .collect();
    format!("<idn_feeds feeds=\"{}\"/>", lines.join("&#10;"))
}
/// How long an output may be gone before its inputs, unused, are given back.
pub const GONE_SECS: u64 = 3600;

/// One switcher output the desk knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Known {
    /// The two desk inputs it is on (from 0, left and right); `None`: taken off by the
    /// user, not brought in again.
    pub inputs: Option<[u16; 2]>,
    /// When it was last on the network (Unix seconds).
    pub seen: u64,
    /// Its routes have been seen in place (so their going later is the user's doing).
    #[serde(default)]
    pub confirmed: bool,
    /// It takes a desk's feed (the program, a mix: `switcher-feed/1`, §M.18).
    #[serde(default)]
    pub takes_feed: bool,
}

/// What is kept (`switcher-inputs.json` in the state directory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    /// Bring switchers' outputs in by themselves.
    pub on: bool,
    /// By the output's full source name.
    pub outputs: BTreeMap<String, Known>,
    /// The bus each switcher output takes its sound from (§M.18), by its full name:
    /// `main`, `aux:N`, `group:N`, `matrix:N`.
    pub feeds: BTreeMap<String, String>,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            on: true,
            outputs: BTreeMap::new(),
            feeds: BTreeMap::new(),
        }
    }
}

/// Two free desk inputs side by side, from the top (the stage box's routes usually
/// start at the bottom): a free pair at an even channel first, `taken` counting as used.
pub fn free_pair(routes: &[Route], channels: u16, taken: &[u16]) -> Option<[u16; 2]> {
    let used = |c: u16| routes.iter().any(|r| r.ch == c) || taken.contains(&c);
    (0..channels.saturating_sub(1))
        .rev()
        .filter(|c| c % 2 == 0)
        .chain((0..channels.saturating_sub(1)).rev().filter(|c| c % 2 == 1))
        .find(|&c| !used(c) && !used(c + 1))
        .map(|c| [c, c + 1])
}

/// The route changes that put `tx`'s left and right on `pair` (or take them off).
pub fn routes_for(receiver: &str, tx: &str, pair: [u16; 2], on: bool) -> Vec<RouteChange> {
    pair.iter()
        .enumerate()
        .map(|(k, &ch)| RouteChange {
            receiver: receiver.to_owned(),
            ch,
            to: on.then(|| (tx.to_owned(), k as u16)),
        })
        .collect()
}

/// Whether `tx`'s left and right are on `pair` now.
pub fn in_place(routes: &[Route], tx: &str, pair: [u16; 2]) -> bool {
    pair.iter().enumerate().all(|(k, &ch)| {
        routes
            .iter()
            .any(|r| r.ch == ch && r.tx == tx && r.tx_ch == k as u16)
    })
}

/// A source's name without its machine: `HOST (Switcher (Monitor))` → `Switcher (Monitor)`.
pub fn short_name(tx: &str) -> &str {
    tx.split_once(" (")
        .and_then(|(_, n)| n.strip_suffix(')'))
        .unwrap_or(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(ch: u16, tx: &str, tx_ch: u16) -> Route {
        Route {
            ch,
            tx: tx.into(),
            tx_ch,
        }
    }

    #[test]
    fn pairs_come_from_the_top() {
        // The stage box on 1–8; the first switcher output on 63–64, then 61–62.
        let mut routes: Vec<Route> = (0..8).map(|c| r(c, "BOX (Stage)", c)).collect();
        assert_eq!(free_pair(&routes, 64, &[]), Some([62, 63]));
        assert_eq!(free_pair(&routes, 64, &[62, 63]), Some([60, 61]));
        // A single route in the way: the pair below.
        routes.push(r(61, "X (Y)", 0));
        assert_eq!(free_pair(&routes, 64, &[62, 63]), Some([58, 59]));
        // Only an odd pair left.
        let full: Vec<Route> = (0..8)
            .filter(|&c| c != 4 && c != 5 && c != 3)
            .map(|c| r(c, "A (B)", c))
            .collect();
        assert_eq!(free_pair(&full, 8, &[4]), None);
        assert_eq!(free_pair(&full, 8, &[]), Some([4, 5]));
        let odd: Vec<Route> = (0..8)
            .filter(|&c| c != 3 && c != 4)
            .map(|c| r(c, "A (B)", c))
            .collect();
        assert_eq!(free_pair(&odd, 8, &[]), Some([3, 4]));
        assert_eq!(free_pair(&[], 1, &[]), None);
    }

    #[test]
    fn feeds_are_buses_and_lines() {
        assert_eq!(bus_pair("main"), Some(0));
        assert_eq!(bus_pair("aux:1"), Some(2));
        assert_eq!(bus_pair("aux:8"), Some(16));
        assert_eq!(bus_pair("group:1"), Some(20));
        assert_eq!(bus_pair("matrix:1"), Some(36));
        assert_eq!(bus_pair("matrix:4"), Some(42));
        // The buses added 2026-11-08 after the 52 there were.
        assert_eq!(bus_pair("aux:9"), Some(52));
        assert_eq!(bus_pair("aux:16"), Some(66));
        assert_eq!(bus_pair("matrix:5"), Some(68));
        assert_eq!(bus_pair("matrix:9"), None);
        assert_eq!(bus_pair("nothing"), None);
        // The labels agree.
        let s = crate::desk::Settings::new(8);
        let l = s.output_labels();
        assert_eq!(
            l[bus_pair("matrix:1").unwrap()],
            format!("{} L", s.matrix[0].name)
        );
        assert_eq!(
            l[bus_pair("group:2").unwrap() + 1],
            format!("{} R", s.groups[1].name)
        );
        let x = feeds_xml(&[
            ("PC (Switcher)".into(), [36, 37]),
            ("PC (A&B)".into(), [0, 1]),
        ]);
        assert_eq!(
            x,
            "<idn_feeds feeds=\"PC (Switcher)=36,37&#10;PC (A&amp;B)=0,1\"/>"
        );
    }

    #[test]
    fn routes_and_names() {
        let ch = routes_for("Mixer", "PC (Switcher (Monitor))", [62, 63], true);
        assert_eq!(ch[0].to, Some(("PC (Switcher (Monitor))".into(), 0)));
        assert_eq!((ch[1].ch, ch[1].to.as_ref().unwrap().1), (63, 1));
        assert!(
            routes_for("Mixer", "x", [0, 1], false)
                .iter()
                .all(|c| c.to.is_none())
        );
        let routes = vec![r(62, "PC (Switcher)", 0), r(63, "PC (Switcher)", 1)];
        assert!(in_place(&routes, "PC (Switcher)", [62, 63]));
        assert!(!in_place(&routes, "PC (Switcher)", [60, 61]));
        assert_eq!(short_name("PC (Switcher (Monitor))"), "Switcher (Monitor)");
        assert_eq!(short_name("PC (Lobby)"), "Lobby");
        assert_eq!(short_name("odd"), "odd");
    }
}
