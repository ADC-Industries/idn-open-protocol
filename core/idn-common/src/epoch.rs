//! The time now as the time since 1970 (the system's clock): one call for what had been
//! written out sixty times.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The time since 1970 now (zero on a clock set before it): `.as_secs()`, `.as_millis()`
/// and so on as needed.
pub fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Whole seconds since 1970 now (Unix time).
pub fn secs() -> u64 {
    now().as_secs()
}

/// Milliseconds since 1970 now.
pub fn millis() -> u64 {
    now().as_millis() as u64
}

#[cfg(test)]
mod tests {
    #[test]
    fn after_2026() {
        assert!(super::now().as_secs() > 1_767_225_600);
        let (s, ms) = (super::secs(), super::millis());
        assert!(ms / 1000 >= s && ms / 1000 - s <= 1, "{s} {ms}");
    }
}
