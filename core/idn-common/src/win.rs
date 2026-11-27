//! Windows' own forms, one copy each: strings as the wide (UTF-16) functions take them,
//! and values read from the registry.

use windows_sys::Win32::System::Registry::{HKEY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};

/// `s` as UTF-16 ending in a NUL, for a `…W` function's string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A path or other `OsStr` as UTF-16 ending in a NUL (as [`wide`], for names that may not
/// be Unicode).
pub fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

/// The string value `value` of `subkey` under `hkey` (`HKEY_LOCAL_MACHINE`), trimmed;
/// `None` when it's missing, empty, not a string, or longer than 511 characters (every
/// value read is short: a name, a version, a build number).
// (`hkey` is only handed to the registry, which checks it: a bad one is an error there,
// never dereferenced here.)
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn reg_sz(hkey: HKEY, subkey: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(subkey), wide(value));
    let mut buf = vec![0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its length in bytes are ours; the names end in a 0.
    let r = unsafe {
        RegGetValueW(
            hkey,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    buf.truncate((len as usize / 2).saturating_sub(1));
    let s = String::from_utf16_lossy(&buf).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// The DWORD value `value` of `subkey` under `hkey`.
// (`hkey` is only handed to the registry, which checks it: a bad one is an error there,
// never dereferenced here.)
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn reg_dword(hkey: HKEY, subkey: &str, value: &str) -> Option<u32> {
    let (k, v) = (wide(subkey), wide(value));
    let mut out = 0u32;
    let mut len = 4u32;
    // SAFETY: as in reg_sz, for a u32.
    let r = unsafe {
        RegGetValueW(
            hkey,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut out as *mut u32).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(out)
}

/// Where Windows keeps its version (`HKEY_LOCAL_MACHINE`).
pub const NT_CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// Windows' build number (19045, 22631, 26100: 22000 and up is Windows 11).
pub fn windows_build() -> Option<u32> {
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    reg_sz(HKEY_LOCAL_MACHINE, NT_CURRENT_VERSION, "CurrentBuildNumber")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn wide_ends_in_nul_and_the_build_is_read() {
        assert_eq!(super::wide("ab"), [97, 98, 0]);
        assert_eq!(super::wide_os(std::ffi::OsStr::new("ab")), [97, 98, 0]);
        assert!(super::windows_build().is_some_and(|b| b >= 10240));
    }
}
