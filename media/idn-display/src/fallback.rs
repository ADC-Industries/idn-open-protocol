//! What a screen shows while its feed is gone (idn-display.md §D.8.4): the last picture
//! held for `after_s`, then the fallback (the logo by default), until the feed's pictures
//! come again. Pure: given what the receiver has seen, [`decide`] says the state and what
//! is on the screen, so the timing is tested without a window or a network.

use crate::proto::{Fallback, Show, Showing, State};
use std::time::{Duration, Instant};

/// What the screen's receiver has seen.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Seen {
    /// A feed is set.
    pub feed: bool,
    /// Since when the feed's pictures are missing: since it was set (no picture yet), or
    /// since its connection was lost. `None`: they come.
    pub out_for: Option<Duration>,
    /// The feed has shown pictures since it was set (a loss is then "lost", not
    /// "connecting").
    pub played: bool,
    /// A picture is on the screen to hold (this feed's or the one before).
    pub picture: bool,
    /// The reconnect cap was reached.
    pub gave_up: bool,
}

/// The state and what is shown.
pub fn decide(f: &Fallback, s: &Seen) -> (State, Showing) {
    if !s.feed {
        return (State::Idle, Showing::Black);
    }
    let fallback = || {
        let shown = match f.show {
            Show::Last if s.picture => Showing::Last,
            Show::Last | Show::Black => Showing::Black,
            Show::Logo => Showing::Logo,
            Show::Slideshow => Showing::Slideshow,
        };
        (State::Fallback, shown)
    };
    if s.gave_up {
        return fallback();
    }
    match s.out_for {
        None => (State::Playing, Showing::Feed),
        Some(t) if t.as_secs_f64() < f.after_s => (
            if s.played {
                State::Lost
            } else {
                State::Connecting
            },
            if s.picture {
                Showing::Last
            } else {
                Showing::Black
            },
        ),
        Some(_) => fallback(),
    }
}

/// A feed followed over time (the screen's tick, §D.8.4): since when its pictures are
/// missing, whether it has played, whether the reconnect cap was reached.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Track {
    pub out_since: Option<Instant>,
    pub played: bool,
    pub gave_up: bool,
}

impl Track {
    /// A new feed set at `now`: its pictures missing from then.
    pub fn restart(&mut self, now: Instant) {
        *self = Track {
            out_since: Some(now),
            ..Default::default()
        };
    }

    /// What the receiver says now: connected, its last picture's arrival, its failed
    /// attempts since it was last connected. True when the reconnect cap is reached now
    /// (the receiver is then stopped).
    pub fn update(
        &mut self,
        connected: bool,
        last_picture: Option<Instant>,
        attempts: u64,
        f: &Fallback,
        now: Instant,
    ) -> bool {
        let back = connected && last_picture.is_some_and(|t| self.out_since.is_none_or(|o| t >= o));
        if back {
            self.out_since = None;
            self.played = true;
        } else if !connected && self.out_since.is_none() {
            self.out_since = Some(now);
        }
        if !self.gave_up && self.out_since.is_some() && capped(f, attempts) {
            self.gave_up = true;
            return true;
        }
        false
    }

    /// For [`decide`]: with a feed set or not, a picture on the screen or not.
    pub fn seen(&self, feed: bool, picture: bool, now: Instant) -> Seen {
        Seen {
            feed,
            out_for: self.out_since.map(|t| now.saturating_duration_since(t)),
            played: self.played,
            picture,
            gave_up: self.gave_up,
        }
    }
}

/// Whether the reconnect cap is reached after `attempts` failed attempts.
pub fn capped(f: &Fallback, attempts: u64) -> bool {
    f.reconnect_cap > 0 && attempts >= u64::from(f.reconnect_cap)
}

/// The slide shown `elapsed` into the fallback, of `n` slides each `slide_s`.
pub fn slide(n: usize, slide_s: u32, elapsed: Duration) -> Option<usize> {
    (n > 0).then(|| (elapsed.as_secs() / u64::from(slide_s.max(1))) as usize % n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: f64) -> Option<Duration> {
        Some(Duration::from_secs_f64(s))
    }

    #[test]
    fn the_last_picture_is_held_then_the_logo_shows_until_the_feed_is_back() {
        let f = Fallback::default();
        let lost = |t| Seen {
            feed: true,
            out_for: at(t),
            played: true,
            picture: true,
            gave_up: false,
        };
        assert_eq!(
            decide(
                &f,
                &Seen {
                    out_for: None,
                    ..lost(0.0)
                }
            ),
            (State::Playing, Showing::Feed)
        );
        assert_eq!(decide(&f, &lost(0.1)), (State::Lost, Showing::Last));
        assert_eq!(decide(&f, &lost(4.9)), (State::Lost, Showing::Last));
        assert_eq!(decide(&f, &lost(5.0)), (State::Fallback, Showing::Logo));
        assert_eq!(decide(&f, &lost(600.0)), (State::Fallback, Showing::Logo));
        // A new feed that never comes: connecting (black, nothing to hold), then the logo.
        let new = Seen {
            feed: true,
            out_for: at(1.0),
            ..Default::default()
        };
        assert_eq!(decide(&f, &new), (State::Connecting, Showing::Black));
        // The picture of the feed before is held while the new one connects (no gap).
        let swap = Seen {
            picture: true,
            ..new
        };
        assert_eq!(decide(&f, &swap), (State::Connecting, Showing::Last));
        assert_eq!(
            decide(
                &f,
                &Seen {
                    out_for: at(7.0),
                    ..swap
                }
            ),
            (State::Fallback, Showing::Logo)
        );
        // No feed: black, idle.
        assert_eq!(decide(&f, &Seen::default()), (State::Idle, Showing::Black));
    }

    #[test]
    fn each_fallback_and_the_reconnect_cap() {
        let mut f = Fallback {
            after_s: 2.0,
            ..Default::default()
        };
        let s = Seen {
            feed: true,
            out_for: at(3.0),
            played: true,
            picture: true,
            gave_up: false,
        };
        for (show, shown) in [
            (Show::Last, Showing::Last),
            (Show::Black, Showing::Black),
            (Show::Logo, Showing::Logo),
            (Show::Slideshow, Showing::Slideshow),
        ] {
            f.show = show;
            assert_eq!(decide(&f, &s), (State::Fallback, shown), "{show:?}");
        }
        // "Last" with nothing to hold: black.
        f.show = Show::Last;
        assert_eq!(
            decide(
                &f,
                &Seen {
                    picture: false,
                    ..s
                }
            ),
            (State::Fallback, Showing::Black)
        );
        // Given up: the fallback at once, whatever the time.
        f.show = Show::Logo;
        let given_up = Seen {
            out_for: at(0.5),
            gave_up: true,
            ..s
        };
        assert_eq!(decide(&f, &given_up), (State::Fallback, Showing::Logo));
        assert!(!capped(&f, 1_000_000), "0: for ever");
        f.reconnect_cap = 3;
        assert!(!capped(&f, 2) && capped(&f, 3));
        // Slides in turn.
        assert_eq!(slide(0, 8, Duration::from_secs(9)), None);
        assert_eq!(slide(3, 8, Duration::from_secs(7)), Some(0));
        assert_eq!(slide(3, 8, Duration::from_secs(9)), Some(1));
        assert_eq!(slide(3, 8, Duration::from_secs(25)), Some(0));
    }
}

/// A feed that isn't there, received for real (no window): connecting with the screen
/// black, the fallback once `after_s` has passed, the receiver's failed attempts counted
/// (`InputStats::retries`) and the cap reached.
#[cfg(test)]
mod absent {
    use super::*;
    use crate::proto::{Show, Showing, State};
    use crate::receive::{self, Latest, Link};
    use idn_common::sync::LockOk as _;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn an_absent_feed_falls_back_and_gives_up_at_its_cap() {
        // Nothing listens on the discard port here: each attempt is refused at once.
        let (latest, link, stop) = (
            Arc::new(Latest::default()),
            Arc::new(Link::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let t = receive::run(
            receive::Config {
                source: "127.0.0.1:9".into(),
                receiver_name: "test".into(),
                audio: None,
                secure: None,
                discovery_server: None,
                gpu: None,
                game: false,
                sync: None,
            },
            latest,
            link.clone(),
            stop.clone(),
            || {},
        );
        let f = Fallback {
            after_s: 1.0,
            show: Show::Logo,
            reconnect_cap: 2,
            ..Default::default()
        };
        let start = Instant::now();
        let mut track = Track::default();
        track.restart(start);
        let mut seen_states = Vec::new();
        let mut capped_after = None;
        while start.elapsed() < Duration::from_secs(8) && capped_after.is_none() {
            let now = Instant::now();
            let s = link.seen.lock_ok().clone();
            let attempts = link
                .input
                .lock_ok()
                .as_ref()
                .map_or(0, |i| i.stats().retries as u64);
            if track.update(s.connected, s.last_picture, attempts, &f, now) {
                capped_after = Some((start.elapsed(), attempts));
            }
            let (state, showing) = decide(&f, &track.seen(true, false, now));
            if seen_states.last() != Some(&(state, showing)) {
                seen_states.push((state, showing));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = t.join();
        assert_eq!(
            seen_states[..2],
            [
                (State::Connecting, Showing::Black),
                (State::Fallback, Showing::Logo)
            ],
            "{seen_states:?}"
        );
        let (when, attempts) = capped_after.expect("the cap reached");
        assert!(attempts >= 2, "{attempts}");
        assert!(when < Duration::from_secs(8), "{when:?}");
        assert!(track.gave_up);
        assert!(!link.seen.lock_ok().connected);
        // Given up: the fallback stays, whatever the time.
        let later = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            decide(&f, &track.seen(true, false, later)),
            (State::Fallback, Showing::Logo)
        );
    }
}
