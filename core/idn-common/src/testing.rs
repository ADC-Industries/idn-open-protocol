//! Helpers for the workspace's tests.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A port free now for both TCP and UDP, for a test to bind ([`crate::net::kept_port`]):
/// a port asked of the system (`bind(…:0)`) and let go could be handed to any other socket
/// before the test bound it again, which tests run side by side did now and then. Ports
/// the tests fix themselves are below 20000.
pub fn test_port() -> u16 {
    crate::net::kept_port()
}

/// Polls `what` every 20 ms until it holds or `within` has passed: whether it held. For a
/// test waiting on another thread or process (a fixed sleep is too short on a loaded
/// machine and too long on an idle one).
pub fn wait_until(within: Duration, mut what: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    loop {
        if what() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// [`wait_until`] that fails the test when `f` hasn't held within `within`, the panic
/// naming `what` (the thing waited for: "the receiver's first picture").
#[track_caller]
pub fn wait_for(what: &str, within: Duration, f: impl FnMut() -> bool) {
    if !wait_until(within, f) {
        panic!("waited {within:?} for {what}");
    }
}

/// A folder of a test's own, empty at the start and removed with all in it when dropped.
/// Unique per call (the process, a counter and `name`), so tests run side by side, or a
/// test run again after one that crashed, never share one; under the system's temporary
/// folder. (Each test file had its own `tmp()`, most leaving their folders behind.)
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// A new folder whose name holds `name` (for finding it when a test is looked at).
    pub fn new(name: &str) -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let d = std::env::temp_dir().join(format!("idn-test-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("a test folder");
        TempDir(d)
    }

    /// The folder.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A path in it.
    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.0.join(p)
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_below_the_ephemeral_range_and_differ() {
        let a: Vec<u16> = (0..50).map(|_| test_port()).collect();
        assert!(a.iter().all(|p| (20_000..32_000).contains(p)));
        let mut b = a.clone();
        b.sort_unstable();
        b.dedup();
        assert!(b.len() > 40, "{a:?}");
    }

    #[test]
    fn temp_dirs_are_unique_and_removed() {
        let a = TempDir::new("x/y");
        let b = TempDir::new("x/y");
        assert_ne!(a.path(), b.path());
        assert!(a.is_dir() && b.is_dir());
        assert!(a.path().starts_with(std::env::temp_dir()));
        std::fs::create_dir_all(a.join("in/deep")).unwrap();
        std::fs::write(a.join("in/deep/f"), b"x").unwrap();
        let kept = a.path().to_owned();
        drop(a);
        assert!(!kept.exists());
    }

    #[test]
    fn waits_count_and_fail_by_name() {
        let mut n = 0;
        assert!(wait_until(Duration::from_secs(5), || {
            n += 1;
            n == 3
        }));
        assert_eq!(n, 3);
        assert!(!wait_until(Duration::from_millis(30), || false));
        wait_for("at once", Duration::from_millis(1), || true);
        let p =
            std::panic::catch_unwind(|| wait_for("the never", Duration::from_millis(30), || false));
        let msg = p.unwrap_err();
        let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("the never"), "{msg}");
    }
}
