//! A plug-in library kept loaded until its process ends (deep bug run 2026-11-09).
//!
//! The ASIO driver and the DAW plug-ins start threads (QUIC, discovery, the Node's
//! client) that outlive their last object: a host that unloaded the library while they
//! ran crashed in code no longer there. Whatever library this crate is built into is the
//! one pinned (this function's own address is in it).

/// Pins the library this code is in (once; later calls do nothing).
pub fn pin_own_module() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(pin);
}

#[cfg(windows)]
fn pin() {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
        GetModuleHandleExW,
    };
    let mut h = std::ptr::null_mut();
    // SAFETY: an address in this module and a handle to fill (a pinned module's handle
    // needs no release).
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            pin_own_module as *const () as *const u16,
            &mut h,
        )
    };
}

#[cfg(unix)]
fn pin() {
    // SAFETY: an address in this library and a struct for the loader to fill; its name
    // is the loader's own string, valid while the library is loaded (it stays).
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(pin_own_module as *const () as *const libc::c_void, &mut info) == 0
            || info.dli_fname.is_null()
        {
            return;
        }
        // A second reference never closed, marked not to be unloaded.
        libc::dlopen(
            info.dli_fname,
            libc::RTLD_NOW | libc::RTLD_NOLOAD | libc::RTLD_NODELETE,
        );
    }
}

#[cfg(not(any(windows, unix)))]
fn pin() {}
