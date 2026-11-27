//! When to open a lost sound card again (the desk's and the switcher's cards): after it
//! was lost for a while, not while a card that hasn't given sound yet may still be
//! starting, and less and less often while it stays away.

use idn_common::backoff::Backoff;
use std::time::{Duration, Instant};

/// Lost this long before it is opened again.
pub const LOST_FOR: Duration = Duration::from_secs(3);
/// A card that hasn't moved any sound yet is given this long to start (a USB interface
/// waking, a driver waiting for its clock): it was closed again every 4 s and never ran.
pub const START_GRACE: Duration = Duration::from_secs(15);
/// The longest wait between tries.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// One card's tries.
#[derive(Debug)]
pub struct Reopen {
    lost_since: Option<Instant>,
    opened: Instant,
    next: Instant,
    wait: Backoff,
    said: String,
}

impl Default for Reopen {
    fn default() -> Self {
        let now = Instant::now();
        Reopen {
            lost_since: None,
            opened: now,
            next: now,
            wait: Backoff::new(LOST_FOR, MAX_WAIT),
            said: String::new(),
        }
    }
}

impl Reopen {
    /// Whether to open it again now: `lost` as its status says, `moved` once it gave or
    /// took any sound since it was opened. Backs off 3, 6, 12 … 60 s between tries.
    pub fn due(&mut self, lost: bool, moved: bool) -> bool {
        let now = Instant::now();
        if !lost {
            self.lost_since = None;
            self.wait.reset();
            return false;
        }
        if !moved && now.duration_since(self.opened) < START_GRACE {
            return false;
        }
        let since = *self.lost_since.get_or_insert(now);
        if now.duration_since(since) < LOST_FOR || now < self.next {
            return false;
        }
        self.next = now + self.wait.next();
        true
    }

    /// It was opened (again) just now: its start grace begins.
    pub fn opened(&mut self) {
        self.opened = Instant::now();
        self.lost_since = None;
    }

    /// `line` for the log, once while it stays the same (a card away overnight wrote a
    /// line every 3 s).
    pub fn say(&mut self, line: &str) -> bool {
        if self.said == line {
            return false;
        }
        self.said = line.to_owned();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_card_is_tried_after_a_while_then_less_often() {
        let mut r = Reopen::default();
        // Healthy: never.
        assert!(!r.due(false, true));
        // A card that never gave sound: not within its start grace.
        assert!(!r.due(true, false));
        // One that ran: once lost for LOST_FOR (not at once).
        assert!(!r.due(true, true));
        r.lost_since = Some(Instant::now() - LOST_FOR);
        assert!(r.due(true, true));
        // Then not again until its wait (6 s next).
        assert!(!r.due(true, true));
        assert_eq!(r.wait.current(), Some(LOST_FOR));
        assert_eq!(r.wait.clone().next(), LOST_FOR * 2);
        // The log line once while it stays the same.
        assert!(r.say("gone"));
        assert!(!r.say("gone"));
        assert!(r.say("back"));
    }
}
