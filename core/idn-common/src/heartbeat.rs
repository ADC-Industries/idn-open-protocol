//! Real-time clocks kept running and watched (the desk's audio clock, the switcher's frame
//! clock, the ASIO driver's clock). A panic in one used to end that thread alone: the
//! panel and the API went on reporting a healthy program while its sound or pictures had
//! stopped. [`keep_running`] starts the clock again after a panic (the crash report is
//! still written by the panic hook), and a [`Heartbeat`] it marks every block or frame
//! tells the program's status whether the clock is running at all, also when it is stuck
//! rather than gone.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// When a clock last ran, and how often it was started again.
#[derive(Debug, Default)]
pub struct Heartbeat {
    /// Milliseconds from [`epoch`] at the last beat, plus one (0: never beaten).
    last_ms: AtomicU64,
    restarts: AtomicU32,
}

/// The process's own time base for heartbeats.
fn epoch() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

impl Heartbeat {
    pub const fn new() -> Self {
        Heartbeat {
            last_ms: AtomicU64::new(0),
            restarts: AtomicU32::new(0),
        }
    }

    /// The clock ran now (once a block or frame: a clock read and a store).
    pub fn beat(&self) {
        let ms = epoch().elapsed().as_millis() as u64 + 1;
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// Whether it beat within `within` (false before its first beat).
    pub fn alive(&self, within: Duration) -> bool {
        let last = self.last_ms.load(Ordering::Relaxed);
        last != 0
            && (epoch().elapsed().as_millis() as u64 + 1).saturating_sub(last)
                <= within.as_millis() as u64
    }

    /// Whether it has beaten at all yet (a clock that hasn't started isn't "stopped").
    pub fn started(&self) -> bool {
        self.last_ms.load(Ordering::Relaxed) != 0
    }

    /// Times the clock was started again after a panic.
    pub fn restarts(&self) -> u32 {
        self.restarts.load(Ordering::Relaxed)
    }
}

/// The wait before the first restart, and the most it grows to.
const FIRST_WAIT: Duration = Duration::from_millis(100);
const LONGEST_WAIT: Duration = Duration::from_secs(5);
/// A run this long counts as having recovered: the next restart waits [`FIRST_WAIT`].
const RECOVERED: Duration = Duration::from_secs(10);

/// Runs `clock` (a clock thread's loop) on this thread until it returns, starting it
/// again after a panic, unless `stop` is set by then: a tenth of a second later, and
/// twice as long each time it panics again within 10 s, up to 5 s (a clock that panics
/// at once every time isn't started ten times a second for ever, each with a crash
/// report). `what` names it in the log ("the desk's clock"). The panic itself has been
/// reported by then (the panic hook's crash report).
pub fn keep_running(what: &str, stop: &AtomicBool, beat: &Heartbeat, mut clock: impl FnMut()) {
    let mut wait = FIRST_WAIT;
    loop {
        let started = Instant::now();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut clock));
        if r.is_ok() || stop.load(Ordering::Relaxed) {
            return;
        }
        if started.elapsed() >= RECOVERED {
            wait = FIRST_WAIT;
        }
        let n = beat.restarts.fetch_add(1, Ordering::Relaxed) + 1;
        crate::log::log_or_stderr(&format!(
            "{what} stopped on an error and is started again in {} ms ({n}×; the crash report has it)",
            wait.as_millis()
        ));
        // In short steps, so a program stopping meanwhile isn't held up.
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50).min(until - Instant::now()));
        }
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_that_panics_is_started_again() {
        let stop = AtomicBool::new(false);
        let beat = Heartbeat::new();
        assert!(!beat.started() && !beat.alive(Duration::from_secs(1)));
        let mut runs = 0;
        keep_running("the test's clock", &stop, &beat, || {
            runs += 1;
            beat.beat();
            if runs < 3 {
                panic!("a bug in the clock");
            }
        });
        assert_eq!((runs, beat.restarts()), (3, 2));
        assert!(beat.alive(Duration::from_secs(1)));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!beat.alive(Duration::from_millis(20)), "stale is not alive");
    }

    #[test]
    fn restarts_back_off() {
        let stop = AtomicBool::new(false);
        let beat = Heartbeat::new();
        let mut runs = 0;
        let t = Instant::now();
        keep_running("the test's clock", &stop, &beat, || {
            runs += 1;
            if runs < 4 {
                panic!("at once, every time");
            }
        });
        // 100 + 200 + 400 ms.
        assert!(
            t.elapsed() >= Duration::from_millis(700),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(beat.restarts(), 3);
    }

    #[test]
    fn a_stopping_program_isnt_restarted() {
        let stop = AtomicBool::new(true);
        let beat = Heartbeat::new();
        let mut runs = 0;
        keep_running("the test's clock", &stop, &beat, || {
            runs += 1;
            panic!("while stopping");
        });
        assert_eq!((runs, beat.restarts()), (1, 0));
    }
}
