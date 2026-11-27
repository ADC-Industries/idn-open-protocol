//! The site's sync groups (roadmap R.30, idn-routing.md §A.14): receivers of the same
//! transmitters, on any machine, play in line at the largest need among them. Every 2 s
//! the machines' reports are read ([`group_floors`]), and each receiver whose floor should
//! change is told: a rise at once (a member needs more now), a fall once it has held for
//! [`FALL_AFTER`] (the group's need came down, not a moment's dip).

use idn_open_protocol::node::routing::{group_floors, sync_members};
use idn_open_protocol::trust::Authority;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const FALL_AFTER: Duration = Duration::from_secs(10);

/// What was told, and falls waiting.
#[derive(Default)]
pub struct Sync {
    /// (machine, receiver) → (the largest lower floor wanted since the fall began, since
    /// when).
    falling: BTreeMap<(String, String), (Option<u32>, Instant)>,
}

impl Sync {
    /// One round: the floors to send, then sent (each answer waited for, briefly).
    pub fn tick(&mut self, a: &Authority) {
        let reports = a.node_reports();
        let members = sync_members(reports.iter().map(|r| (r.fingerprint.as_str(), &r.report)));
        let floors = group_floors(&members);
        let now = Instant::now();
        let mut keep = std::collections::BTreeSet::new();
        // Machines that didn't answer this tick: their other receivers wait for the next
        // (2 s for each, one after another, held every other machine's rise up for
        // seconds; bug round 2026-11-09).
        let mut stalled = std::collections::BTreeSet::new();
        for (m, want) in members.iter().zip(floors) {
            let key = (m.fingerprint.clone(), m.receiver.clone());
            keep.insert(key.clone());
            let mut want = want;
            let send = match (m.floor_ms, want) {
                (have, want) if have == want => {
                    self.falling.remove(&key);
                    false
                }
                // Up (or a first floor): at once.
                (have, Some(w)) if have.is_none_or(|h| w > h) => {
                    self.falling.remove(&key);
                    true
                }
                // Down (or none): to the largest wanted over the last FALL_AFTER, then
                // again: an adapting need walking down 1 ms each 5 s is followed in
                // steps (restarting the wait at each lower value held it up until the
                // need stopped, then dropped it all at once: a skip on every member).
                _ => {
                    let e = self.falling.entry(key.clone()).or_insert((want, now));
                    e.0 = e.0.max(want);
                    want = e.0;
                    now.duration_since(e.1) >= FALL_AFTER
                }
            };
            if send && !stalled.contains(&m.fingerprint) {
                self.falling.remove(&key);
                if let Err(e) =
                    a.floor_node(&m.fingerprint, &m.receiver, want, Duration::from_secs(2))
                {
                    stalled.insert(m.fingerprint.clone());
                    idn_open_protocol::log::log(&format!(
                        "sync: {} on {}: floor not set ({e})",
                        m.receiver, m.fingerprint
                    ));
                }
            }
        }
        self.falling.retain(|k, _| keep.contains(k));
    }
}
