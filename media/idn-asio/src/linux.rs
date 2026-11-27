//! The library's entry points on Linux (libidn_asio.so), as cwASIO's hosts and installers
//! call them (spec/extensions/asio-interface.md §A.3):
//!
//! - `instantiateDriver` (the driver object, object.rs, one reference the host's; its own
//!   `release` lets it go) and `releaseDriver`;
//! - `registerDriver` / `unregisterDriver`, which list the driver in cwASIO's list
//!   (`/etc/cwASIO/<name>/`: `driver` = this library's path, `description`).
//!
//! The settings are the same as on Windows: `asio.json` in the IDN state folder (on Linux
//! `$XDG_STATE_HOME/idn-open-protocol` or `~/.local/state/idn-open-protocol`).
#![allow(non_snake_case)] // cwASIO's names

use idn_asio_abi::Driver as AsioDriver;
use std::ffi::{CStr, c_char, c_int};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Its description in the list, as on Windows (`HKLM\SOFTWARE\ASIO\IDN`'s).
const DESCRIPTION: &str = "IDN";

/// A new driver object, one reference the host's (null: it couldn't be made).
#[unsafe(no_mangle)]
pub extern "C" fn instantiateDriver() -> *mut AsioDriver {
    // Never unloaded once used: its threads (the desk's link, discovery) outlive the
    // driver object, and a host may close the library once its objects are gone.
    idn_common::module::pin_own_module();
    // (A panic kept inside: unwinding out of an extern function aborts the host.)
    std::panic::catch_unwind(crate::object::new_object).unwrap_or(std::ptr::null_mut())
}

/// cwASIO's count of the library's users, let go of after the object's own `release`.
/// Nothing to do: the library is never unloaded (see [`instantiateDriver`]), and the object
/// is let go by its `release`, not here.
#[unsafe(no_mangle)]
pub extern "C" fn releaseDriver(_drv: *mut AsioDriver) {}

/// The name a host passes, if it is one (not null, UTF-8).
///
/// # Safety
/// `name` is null or a NUL-terminated string.
unsafe fn name_of<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the caller's NUL-terminated string.
    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

/// Lists this library as driver `name` in cwASIO's list: 0, or an error number (`EINVAL`:
/// no name; `EEXIST`: the name is another library's).
///
/// # Safety
/// `name` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn registerDriver(name: *const c_char) -> c_int {
    // SAFETY: the caller's contract.
    let Some(name) = (unsafe { name_of(name) }) else {
        return libc::EINVAL;
    };
    register_in(&idn_asio_abi::registry_dir(), name)
}

/// Takes driver `name` off cwASIO's list: 0, or an error number (`EINVAL`: no name;
/// `ENODEV`: not listed; `EACCES`: listed for another library).
///
/// # Safety
/// `name` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unregisterDriver(name: *const c_char) -> c_int {
    // SAFETY: the caller's contract.
    let Some(name) = (unsafe { name_of(name) }) else {
        return libc::EINVAL;
    };
    unregister_in(&idn_asio_abi::registry_dir(), name)
}

/// `registerDriver`'s work in list `dir` (tests use their own folder).
pub fn register_in(dir: &Path, name: &str) -> c_int {
    let Some(lib) = own_path() else {
        return libc::ENOENT;
    };
    errno(idn_asio_abi::register(dir, name, &lib, Some(DESCRIPTION)))
}

/// `unregisterDriver`'s work in list `dir`.
pub fn unregister_in(dir: &Path, name: &str) -> c_int {
    let Some(lib) = own_path() else {
        return libc::ENOENT;
    };
    errno(idn_asio_abi::unregister(dir, name, &lib))
}

fn errno(r: std::io::Result<()>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => e.raw_os_error().unwrap_or(libc::EIO),
    }
}

/// The file this code is in (the library; in a test, the test program), its full path:
/// what the list names and what loads it.
pub fn own_path() -> Option<PathBuf> {
    // SAFETY: an address in this file and a struct for the loader to fill; its name is the
    // loader's own string, copied at once.
    let name = unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(own_path as *const () as *const libc::c_void, &mut info) == 0
            || info.dli_fname.is_null()
        {
            return None;
        }
        CStr::from_ptr(info.dli_fname).to_bytes().to_vec()
    };
    if name.is_empty() {
        return None;
    }
    let p = PathBuf::from(std::ffi::OsString::from_vec(name));
    Some(std::fs::canonicalize(&p).unwrap_or(p))
}

/// The ASIO settings button: the IDN Node's panel (`idn-node open`, on the PATH as the
/// packages install it). Nothing if it isn't there.
pub(crate) fn open_node_panel() {
    if let Ok(mut child) = std::process::Command::new("idn-node")
        .arg("open")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // Waited for on a thread of its own (no zombie left in the host).
        let _ = std::thread::Builder::new()
            .name("idn-asio-panel".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

/// The system time hosts read for sample positions: the monotonic clock, in ns (what
/// Linux's audio APIs time by).
pub(crate) fn monotonic_ns() -> u64 {
    // SAFETY: plain numbers (all zero is a value); then a timespec to fill, and
    // CLOCK_MONOTONIC is always there.
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    (t.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(t.tv_nsec as u64)
}
