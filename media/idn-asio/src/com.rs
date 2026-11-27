//! The DLL's entry points (Windows): COM's `DllGetClassObject` and `DllCanUnloadNow`
//! (the class factory and the driver object: object.rs), and `regsvr32`'s
//! `DllRegisterServer` / `DllUnregisterServer`, which list the driver for ASIO programs:
//! the COM class (`HKCR\CLSID\{…}\InprocServer32` = this DLL) and the ASIO entry
//! (`HKLM\SOFTWARE\ASIO\IDN`); unregistering takes off the other names it is listed under
//! too (instances, `idn asio register NAME`: spec/extensions/asio-interface.md §A.6).

use idn_common::win::wide;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{E_FAIL, HMODULE, MAX_PATH, S_OK};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows_sys::core::{GUID, HRESULT};

/// The driver's class id (object.rs has the same).
const CLSID: &str = "{35676BB1-B34F-4615-929E-85FC4BD40D3B}";
/// Its name in ASIO programs' driver lists.
const NAME: &str = "IDN";

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    // Never unloaded once used: its threads (the desk's link, discovery) outlive the
    // driver object, and COM unloads a DLL whose objects are all gone.
    idn_common::module::pin_own_module();
    crate::object::class_object(rclsid, riid, ppv)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    crate::object::can_unload()
}

/// This DLL's own path.
fn dll_path() -> Option<std::path::PathBuf> {
    // SAFETY: a stack buffer and its length in u16s; the module is looked up by an address in
    // this DLL, its count not raised, so there is nothing to release.
    unsafe {
        let mut module: HMODULE = std::ptr::null_mut();
        let here = dll_path as *const () as *const u16;
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            here,
            &mut module,
        ) == 0
        {
            return None;
        }
        let mut buf = [0u16; MAX_PATH as usize * 4];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n]).into())
    }
}

/// Writes `values` ((name or "" for the default, text)) under `root\path`.
fn set_key(root: HKEY, path: &str, values: &[(&str, &str)]) -> bool {
    // SAFETY: UTF-16 strings with their NUL, alive for each call (the `wide` results are
    // temporaries of the statement or `v`/`n`); the size is `v`'s in bytes; the key is closed.
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegCreateKeyExW(
            root,
            wide(path).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        ) != 0
        {
            return false;
        }
        let mut ok = true;
        for (name, value) in values {
            let v = wide(value);
            let n = if name.is_empty() {
                None
            } else {
                Some(wide(name))
            };
            ok &= RegSetValueExW(
                key,
                n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                v.as_ptr() as *const u8,
                (v.len() * 2) as u32,
            ) == 0;
        }
        RegCloseKey(key);
        ok
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    let Some(path) = dll_path() else {
        return E_FAIL;
    };
    let path = path.to_string_lossy();
    let clsid = format!("CLSID\\{CLSID}");
    let ok = set_key(HKEY_CLASSES_ROOT, &clsid, &[("", NAME)])
        && set_key(
            HKEY_CLASSES_ROOT,
            &format!("{clsid}\\InprocServer32"),
            // Both: the object is thread-safe; a host in the multithreaded apartment gets it
            // directly (no proxy exists for this interface; bug run 2026-11-25).
            &[("", &path), ("ThreadingModel", "Both")],
        )
        && set_key(
            HKEY_LOCAL_MACHINE,
            &format!("SOFTWARE\\ASIO\\{NAME}"),
            &[("CLSID", CLSID), ("Description", NAME)],
        );
    if ok { S_OK } else { E_FAIL }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    // Every name listing this class (the installer's and `idn asio register`'s), so an
    // uninstall leaves none pointing at a removed DLL; a name another driver holds ("IDN"
    // too) is left alone. (Listed first, then taken off.)
    let ours = idn_asio_abi::DriverId::Clsid(crate::object::CLSID);
    for d in idn_asio_abi::drivers().into_iter().filter(|d| d.id == ours) {
        let _ = idn_asio_abi::unregister(&d.name, &crate::object::CLSID);
    }
    // SAFETY: a NUL-ended UTF-16 path, alive for the call.
    unsafe {
        RegDeleteTreeW(HKEY_CLASSES_ROOT, wide(&format!("CLSID\\{CLSID}")).as_ptr());
    }
    S_OK
}

/// The ASIO settings button: the IDN Node's panel (`idn-node.exe open`, next to this DLL).
pub fn open_node_panel() {
    // Through the session agent's `--open`, as the IDN Node shortcut does: `idn-node open`
    // gets no panel while programs need approval, and the button did nothing (bug run B).
    if let Some(dir) = dll_path().and_then(|p| p.parent().map(|d| d.to_owned())) {
        let agent = dir.join("idn-session.exe");
        let _ = if agent.is_file() {
            std::process::Command::new(agent).arg("--open").spawn()
        } else {
            std::process::Command::new(dir.join("idn-node.exe"))
                .arg("open")
                .spawn()
        };
    }
}
