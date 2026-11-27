//! The installed ASIO drivers (`HKLM\SOFTWARE\ASIO`), a driver listed there under another
//! name (cwASIO's instances, spec/extensions/asio-interface.md §A.6), and one loaded (COM,
//! in-process; its interface id is its class id). The calls to it are loaded.rs's.

use super::*;
use idn_common::win::{reg_sz, wide};
use std::io;
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME, ERROR_NO_MORE_ITEMS,
    ERROR_SUCCESS, RPC_E_CHANGED_MODE,
};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE,
    REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegEnumKeyExW,
    RegOpenKeyExW, RegSetValueExW,
};

/// The installed drivers, by name: every key with a class id, several keys of one class id
/// each its own entry (instances, §A.6; their ids are equal, Windows loads by class id).
pub fn drivers() -> Vec<DriverEntry> {
    let mut out = Vec::new();
    let path = wide(ASIO_KEY);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path; the key is closed below.
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) }
        != ERROR_SUCCESS
    {
        return out;
    }
    for i in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: a buffer and its length in characters; the other outputs are not asked.
        let r = unsafe {
            RegEnumKeyExW(
                key,
                i,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        // The end; a key that can't be read (too long a name) is passed over.
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        if r != ERROR_SUCCESS {
            if i > 4096 {
                break;
            }
            continue;
        }
        let name = String::from_utf16_lossy(&name[..len as usize]);
        let Some(clsid) = reg_sz(key, &name, "CLSID").and_then(|c| Guid::parse(&c)) else {
            continue;
        };
        let description = reg_sz(key, &name, "Description").unwrap_or_else(|| name.clone());
        out.push(DriverEntry {
            name,
            id: DriverId::Clsid(clsid),
            description,
        });
    }
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// Where the drivers are listed, under `HKEY_LOCAL_MACHINE`.
const ASIO_KEY: &str = "SOFTWARE\\ASIO";

/// A name that is one key of the list (cwASIO's rule, §A.6): 1 to 31 characters, ASCII
/// (a host reading key names in its own code page hands the driver the same bytes), no
/// `\`, no NUL. `ERROR_INVALID_NAME` otherwise.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 31 || !name.is_ascii() || name.contains(['\\', '\0']) {
        return Err(io::Error::from_raw_os_error(ERROR_INVALID_NAME as i32));
    }
    Ok(())
}

/// Driver `name` listed in `HKLM\SOFTWARE\ASIO` for class `clsid` (cwASIO's
/// `registerDriver`): the key, its `CLSID` (`{XXXXXXXX-…}`) and, if given, its
/// `Description`. Several names may carry one class id: each is an instance (§A.6).
///
/// Errors as cwASIO's: `ERROR_INVALID_NAME` for a name that can't be a key of the list,
/// `ERROR_ALREADY_EXISTS` when `name` is listed for another class id; else the registry's
/// (`ERROR_ACCESS_DENIED`: not run as administrator).
pub fn register(name: &str, clsid: &Guid, description: Option<&str>) -> io::Result<()> {
    check_name(name)?;
    // The class installed (the IDN installer's): a name for a missing class is a driver
    // every host lists and none can open.
    if !key_exists(
        HKEY_CLASSES_ROOT,
        &format!("CLSID\\{clsid}\\InprocServer32"),
    ) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the driver {clsid} isn't installed (install IDN first)"),
        ));
    }
    let path = format!("{ASIO_KEY}\\{name}");
    // A key there already, with no CLSID or another (another product's, perhaps half
    // removed): taken, never written into.
    if key_exists(HKEY_LOCAL_MACHINE, &path)
        && reg_sz(HKEY_LOCAL_MACHINE, &path, "CLSID").and_then(|h| Guid::parse(&h)) != Some(*clsid)
    {
        return Err(io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
    }
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path alive for the call; the key is closed below.
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(&path).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if r != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let clsid = clsid.to_string();
    let mut values = vec![("CLSID", clsid.as_str())];
    if let Some(d) = description {
        values.push(("Description", d));
    }
    let mut result = Ok(());
    for (field, text) in values {
        let (field, text) = (wide(field), wide(text));
        // SAFETY: an open key; NUL-terminated name and value, the value's size in bytes
        // with its NUL (REG_SZ's convention).
        let r = unsafe {
            RegSetValueExW(
                key,
                field.as_ptr(),
                0,
                REG_SZ,
                text.as_ptr().cast(),
                (text.len() * 2) as u32,
            )
        };
        if r != ERROR_SUCCESS {
            result = Err(io::Error::from_raw_os_error(r as i32));
            break;
        }
    }
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    result
}

/// Driver `name` taken off `HKLM\SOFTWARE\ASIO` (cwASIO's `unregisterDriver`): its two
/// values, then its key if nothing else is in it (never a tree: what another product put
/// there stays).
///
/// Errors as cwASIO's: `ERROR_INVALID_NAME` for a bad name, `ERROR_FILE_NOT_FOUND` when it
/// isn't listed (no key, or no CLSID in it), `PermissionDenied` when it is listed for another
/// class id; else the registry's (`ERROR_ACCESS_DENIED`: not run as administrator).
pub fn unregister(name: &str, clsid: &Guid) -> io::Result<()> {
    check_name(name)?;
    let path = format!("{ASIO_KEY}\\{name}");
    let Some(have) = reg_sz(HKEY_LOCAL_MACHINE, &path, "CLSID") else {
        return Err(io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32));
    };
    if Guid::parse(&have) != Some(*clsid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name:?} is another driver's ({have})"),
        ));
    }
    let wpath = wide(&path);
    for field in ["CLSID", "Description"] {
        // SAFETY: NUL-terminated path and name alive for the call.
        let r =
            unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, wpath.as_ptr(), wide(field).as_ptr()) };
        if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
    }
    // (Refused while it has subkeys: the name is unlisted all the same, without its CLSID.)
    // SAFETY: as above.
    let _ = unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, wpath.as_ptr()) };
    Ok(())
}

/// Whether `sub` is a key under `root`.
fn key_exists(root: HKEY, sub: &str) -> bool {
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path alive for the call; the key closed at once.
    unsafe {
        if RegOpenKeyExW(root, wide(sub).as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS {
            return false;
        }
        RegCloseKey(key);
    }
    true
}

/// COM started on this thread for a driver (ended after its release: [`Loaded`]'s drop).
pub(crate) struct Hold {
    com: bool,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if self.com {
            // SAFETY: matches the successful CoInitializeEx in `load`, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// Loads driver `id` (a class id) on this thread.
pub(crate) fn load(id: &DriverId) -> io::Result<(*mut Driver, Hold)> {
    let DriverId::Clsid(clsid) = id else {
        return Err(idn_common::err::invalid_input(format!(
            "{id}: not a class id"
        )));
    };
    // SAFETY: COM on this thread; a thread already in another apartment is used as it is.
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let com = hr >= 0;
    if hr < 0 && hr != RPC_E_CHANGED_MODE {
        return Err(idn_common::err::other(format!("COM: {hr:#x}")));
    }
    let mut drv: *mut c_void = std::ptr::null_mut();
    let guid = windows_sys::core::GUID {
        data1: clsid.data1,
        data2: clsid.data2,
        data3: clsid.data3,
        data4: clsid.data4,
    };
    // SAFETY: the class id doubles as the interface id (the interface's convention);
    // `drv` receives the object.
    let hr = unsafe {
        CoCreateInstance(
            &guid,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &guid,
            &mut drv,
        )
    };
    if hr < 0 || drv.is_null() {
        if com {
            // SAFETY: matches the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the driver {clsid} didn't load ({hr:#x})"),
        ));
    }
    Ok((drv.cast(), Hold { com }))
}
