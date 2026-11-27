//! Where IDN programs keep their state: `IDN_STATE_DIR`, else `$XDG_STATE_HOME`
//! `/idn-open-protocol`, else `~/.local/state/idn-open-protocol`, else (Windows)
//! `%LOCALAPPDATA%\idn-open-protocol`. Each program has a folder of its own in it.

use std::path::PathBuf;

/// The state folder, when one can be named (Unix-style variables only: the library's
/// link keys use exactly this, so it stays as it was).
pub fn state_dir() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    env("IDN_STATE_DIR")
        .or_else(|| env("XDG_STATE_HOME").map(|d| d.join("idn-open-protocol")))
        .or_else(|| env("HOME").map(|h| h.join(".local/state/idn-open-protocol")))
}

/// The state folder, with `%LOCALAPPDATA%` on Windows and the temporary folder at the
/// last: always somewhere.
pub fn state_dir_or_temp() -> PathBuf {
    state_dir()
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("idn-open-protocol"))
        })
        .unwrap_or_else(std::env::temp_dir)
}

/// Program `name`'s own folder in the state folder (`switcher`, `mixer`).
pub fn program_dir(name: &str) -> PathBuf {
    state_dir_or_temp().join(name)
}
