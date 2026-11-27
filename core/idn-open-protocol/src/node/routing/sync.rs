//! The site's sync groups: receivers of the same transmitters play in line.

use super::*;

// ---------------------------------------------------------------------------------------
// The site's sync groups (roadmap R.30, §A.14).

/// One receiver of the site as its machine reports it, for its sync group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncMember {
    /// Its machine's fingerprint.
    pub fingerprint: String,
    /// Its name.
    pub receiver: String,
    /// What its adapting lanes need (`need_ms`), or the latency the site holds it at
    /// (`latency_ms`): what the group must give it.
    pub need_ms: Option<u32>,
    /// Held at a latency the site set (it doesn't follow a floor).
    pub fixed: bool,
    /// The floor it has now (`floor_ms`).
    pub floor_ms: Option<u32>,
    /// The transmitters routed to it.
    pub txs: Vec<String>,
}

/// The receivers in machines' reports (`audio_rx` with its `route`s).
pub fn sync_members<'a>(
    reports: impl IntoIterator<Item = (&'a str, &'a Element)>,
) -> Vec<SyncMember> {
    let mut out = Vec::new();
    for (fp, report) in reports {
        for x in report.children_named("audio_rx") {
            // A receiver whose program isn't running (only its routes kept in the table):
            // not a member — a closed recorder held at 40 ms floored every live receiver
            // of its transmitters at 40 for good (bug round 2026-11-09).
            if x.attr("app").is_none_or(str::is_empty) {
                continue;
            }
            let num = |k: &str| {
                x.attr(k)
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|&v| v > 0)
            };
            let fixed = num("latency_ms");
            let mut txs: Vec<String> = x
                .children_named("route")
                .filter_map(|r| r.attr("tx").map(str::to_owned))
                .filter(|t| !t.is_empty())
                .collect();
            txs.sort();
            txs.dedup();
            out.push(SyncMember {
                fingerprint: fp.to_owned(),
                receiver: x.attr("name").unwrap_or("").to_owned(),
                need_ms: fixed.or(num("need_ms")),
                fixed: fixed.is_some(),
                floor_ms: num("floor_ms"),
                txs,
            });
        }
    }
    out
}

/// Each member's floor: receivers that share a transmitter (and so, through each other,
/// whole groups) play in line, at the largest need among them. A receiver alone gets
/// none. One held at a latency the site set keeps it and is left out of the groups: it
/// neither joins one nor holds the others back (a recorder pinned at
/// 40 ms would put the desk's live sound 40 ms late).
pub fn group_floors(members: &[SyncMember]) -> Vec<Option<u32>> {
    let n = members.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    let mut first_of: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, m) in members.iter().enumerate() {
        if m.fixed {
            continue;
        }
        for t in &m.txs {
            match first_of.get(t.as_str()) {
                Some(&j) => {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                }
                None => {
                    first_of.insert(t, i);
                }
            }
        }
    }
    let mut need: BTreeMap<usize, (u32, usize)> = BTreeMap::new();
    for (i, m) in members.iter().enumerate() {
        if m.fixed {
            continue;
        }
        let r = find(&mut parent, i);
        let e = need.entry(r).or_insert((0, 0));
        e.1 += 1;
        if let Some(v) = m.need_ms {
            e.0 = e.0.max(v);
        }
    }
    (0..n)
        .map(|i| {
            if members[i].fixed {
                return None;
            }
            let (max, count) = need[&find(&mut parent, i)];
            (count > 1 && max > 0).then_some(max)
        })
        .collect()
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    fn rx(name: &str, need: Option<u32>, fixed: Option<u32>, txs: &[&str]) -> Element {
        let mut e = Element::new("audio_rx")
            .with_attr("name", name)
            .with_attr("app", "a program");
        if let Some(n) = need {
            e = e.with_attr("need_ms", &n.to_string());
        }
        if let Some(f) = fixed {
            e = e.with_attr("latency_ms", &f.to_string());
        }
        for t in txs {
            e = e.with_child(
                Element::new("route")
                    .with_attr("tx", t)
                    .with_attr("ch", "0")
                    .with_attr("tx_ch", "0"),
            );
        }
        e
    }

    /// Three receivers of the desk (one of them also of a laptop), one of the laptop
    /// alone, and one elsewhere: the three automatic ones in one group at the largest
    /// need; the one alone gets none; a held one neither counts nor gets one.
    #[test]
    fn receivers_of_the_same_transmitters_share_a_floor() {
        let a = Element::new("report")
            .with_child(rx("Stage", Some(8), None, &["DESK (Main)"]))
            .with_child(rx(
                "IEM",
                Some(12),
                None,
                &["DESK (Main)", "LAPTOP (Music)"],
            ));
        let b = Element::new("report")
            .with_child(rx("Lobby", Some(30), None, &["LAPTOP (Music)"]))
            .with_child(rx("Record", None, Some(40), &["DESK (Main)"]))
            .with_child(rx("Office", Some(50), None, &["RADIO (Feed)"]))
            // A receiver whose program is closed (its routes kept): not a member, though
            // held at 90 on the desk's main.
            .with_child(
                Element::new("audio_rx")
                    .with_attr("name", "Closed")
                    .with_attr("app", "")
                    .with_attr("latency_ms", "90")
                    .with_child(
                        Element::new("route")
                            .with_attr("tx", "DESK (Main)")
                            .with_attr("ch", "0")
                            .with_attr("tx_ch", "0"),
                    ),
            );
        let m = sync_members([("A", &a), ("B", &b)]);
        assert_eq!(m.len(), 5, "the closed one left out");
        let f = group_floors(&m);
        let by = |n: &str| f[m.iter().position(|x| x.receiver == n).unwrap()];
        // The held recorder (40) doesn't hold the group back: the largest automatic
        // need, the Lobby's 30.
        assert_eq!(by("Stage"), Some(30));
        assert_eq!(by("IEM"), Some(30));
        assert_eq!(by("Lobby"), Some(30));
        assert_eq!(
            by("Record"),
            None,
            "held at its own latency, not in the group"
        );
        assert_eq!(by("Office"), None, "alone");
    }
}
