//! Library diagnostics. The library never writes to stdout or stderr itself; an
//! application that wants these messages installs a logger. It is `idn_common::log`'s
//! one logger, so the shared pieces below the library log to the same place.

pub use idn_common::log::{log, set_logger};

/// A logger that prints to stdout (used by the command-line tools).
pub fn print_logger(msg: &str) {
    println!("{msg}");
}

/// A message that may repeat many times a second (a device's errors, refused peers): the
/// first logged at once, then at most one line per period with how many came since the
/// last line (and how long ago that was).
#[derive(Debug, Clone)]
pub struct Throttle {
    every: std::time::Duration,
    said: Option<std::time::Instant>,
    since: u64,
    /// The last message held back, and what they are called, for [`flush`](Self::flush).
    held: Option<(String, String)>,
}

impl Throttle {
    /// At most one line per `every`.
    pub const fn new(every: std::time::Duration) -> Throttle {
        Throttle {
            every,
            said: None,
            since: 0,
            held: None,
        }
    }

    /// One more `msg`: logged if due, with " (N more `what` in the last … s)" when others
    /// were held back. Whether it was logged.
    pub fn log(&mut self, msg: &str, what: &str) -> bool {
        let now = std::time::Instant::now();
        if self
            .said
            .is_some_and(|t| now.duration_since(t) < self.every)
        {
            self.since += 1;
            self.held = Some((msg.to_owned(), what.to_owned()));
            return false;
        }
        self.say(msg, what, now);
        true
    }

    /// The last message held back, if any, logged now with its count (before the throttle
    /// is dropped: else they went unsaid).
    pub fn flush(&mut self) {
        if let Some((msg, what)) = self.held.take() {
            self.since -= 1;
            self.say(&msg, &what, std::time::Instant::now());
        }
    }

    fn say(&mut self, msg: &str, what: &str, now: std::time::Instant) {
        match self.said {
            Some(t) if self.since > 0 => log(&format!(
                "{msg} ({} more {what} in the last {} s)",
                self.since,
                now.duration_since(t).as_secs().max(1)
            )),
            _ => log(msg),
        }
        (self.said, self.since, self.held) = (Some(now), 0, None);
    }
}
