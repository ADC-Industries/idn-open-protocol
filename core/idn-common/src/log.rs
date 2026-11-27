//! The one logger every IDN crate's diagnostics go to (`idn_open_protocol::log` is this,
//! with its extras): a program installs it once, and the library's lines and these
//! shared pieces' lines both reach it.

use std::sync::OnceLock;

static LOGGER: OnceLock<fn(&str)> = OnceLock::new();

/// Installs the process-wide logger (first call wins).
pub fn set_logger(f: fn(&str)) {
    let _ = LOGGER.set(f);
}

/// Logs a diagnostic message through the installed logger ([`set_logger`]); nothing when
/// none is installed (a library never writes to stdout or stderr itself). A host may call
/// it for its own lines, to have them beside the library's.
pub fn log(msg: &str) {
    if let Some(f) = LOGGER.get() {
        f(msg);
    }
}

/// [`log`], or stderr when no logger is installed: for the few lines that must not go
/// unsaid in a program that installs none (a real-time clock restarted after a panic, a
/// port range typed wrong).
#[doc(hidden)]
pub fn log_or_stderr(msg: &str) {
    match LOGGER.get() {
        Some(f) => f(msg),
        None => eprintln!("{msg}"),
    }
}
