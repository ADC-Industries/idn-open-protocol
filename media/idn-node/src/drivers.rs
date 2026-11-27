//! Drivers bundled for Windows (spec/extensions/idn-node.md
//! §N.37): VirtualDrivers' Virtual Display Driver (IddCx, a screen with nothing plugged
//! in, §N.36) and Virtual Audio Driver (IDN's virtual sound card on Windows 11: a speaker
//! and a microphone, the IDN Microphone's way into Teams, Zoom and browsers). Both are
//! MIT-licensed and signed (SignPath Foundation), so they install with test mode off;
//! shipped as they are, in `drivers\display` and `drivers\audio` beside the programs.
//!
//! Each is a root-enumerated device: made here (SetupAPI: a device node with the driver's
//! hardware id, as devcon makes one), then given its driver from the package
//! (`UpdateDriverForPlugAndPlayDevicesW`). Removing takes the device and the driver
//! package out of the driver store again. The Node's service (LocalSystem) does it; from
//! a command line, as administrator.

#![cfg_attr(not(windows), allow(dead_code))]

use std::io;
use std::path::PathBuf;

/// One bundled driver.
pub struct Driver {
    /// Its name on the command line and in the panel.
    pub key: &'static str,
    /// What it is, for people.
    pub what: &'static str,
    /// Its folder under `drivers\` beside the programs, and its INF there.
    pub folder: &'static str,
    pub inf: &'static str,
    /// The root device's hardware id (the INF's).
    pub hwid: &'static str,
    /// Its setup class, by name and GUID.
    pub class_name: &'static str,
    pub class: u128,
    /// The first Windows build its INF installs on (the audio driver's is for Windows 11).
    pub min_build: u32,
}

pub const DISPLAY: Driver = Driver {
    key: "display",
    what: "Virtual Display Driver (virtual screens)",
    folder: "display",
    inf: "MttVDD.inf",
    hwid: "Root\\MttVDD",
    class_name: "Display",
    class: 0x4d36e968_e325_11ce_bfc1_08002be10318,
    min_build: 0,
};

pub const AUDIO: Driver = Driver {
    key: "audio",
    what: "Virtual Audio Driver (IDN's virtual sound card)",
    folder: "audio",
    inf: "VirtualAudioDriver.inf",
    hwid: "ROOT\\VirtualAudioDriver",
    class_name: "MEDIA",
    class: 0x4d36e96c_e325_11ce_bfc1_08002be10318,
    min_build: 22000,
};

pub const ALL: [&Driver; 2] = [&DISPLAY, &AUDIO];

pub fn by_key(k: &str) -> Option<&'static Driver> {
    ALL.into_iter().find(|d| d.key.eq_ignore_ascii_case(k))
}

/// The driver's package beside the programs.
pub fn package(d: &Driver) -> Option<PathBuf> {
    let p = idn_common::supervisor::programs_dir()?
        .join("drivers")
        .join(d.folder)
        .join(d.inf);
    p.is_file().then_some(p)
}

/// A hardware-id list (REG_MULTI_SZ, as UTF-16 bytes) holds `id` (case ignored).
pub fn multi_sz_has(bytes: &[u8], id: &str) -> bool {
    let words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    words
        .split(|w| *w == 0)
        .filter(|s| !s.is_empty())
        .any(|s| String::from_utf16_lossy(s).eq_ignore_ascii_case(id))
}

/// `idn-node driver install|remove|status [display|audio|all]`.
pub fn command(mut args: idn_open_protocol::cli::Args) -> io::Result<()> {
    let usage = "driver install|remove display|audio | driver status";
    let verb = args.next().unwrap_or_else(|| "status".into());
    let which: Vec<&Driver> = match args.next().as_deref() {
        None | Some("all") => ALL.to_vec(),
        Some(k) => vec![by_key(k).unwrap_or_else(|| args.fail(usage))],
    };
    match verb.as_str() {
        "status" => {
            for d in which {
                println!("{}: {}", d.key, status_line(d));
            }
            Ok(())
        }
        "install" => {
            for d in which {
                let reboot = win::install(d)?;
                println!(
                    "{}: installed{}",
                    d.what,
                    if reboot {
                        " (restart the computer to finish)"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        "remove" => {
            // Every one tried (an uninstall takes them all out), the errors said after.
            let mut failed = Vec::new();
            for d in which {
                match win::remove(d) {
                    Ok(()) => println!("{}: removed", d.what),
                    Err(e) => failed.push(format!("{}: {e}", d.what)),
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                Err(io::Error::other(failed.join("; ")))
            }
        }
        _ => args.fail(usage),
    }
}

/// The driver's state, for people.
pub fn status_line(d: &Driver) -> String {
    if !cfg!(windows) {
        return "Windows only".into();
    }
    if !win::supported(d) {
        return "not for this Windows (Windows 11 only)".into();
    }
    match (win::installed(d), package(d).is_some()) {
        (true, _) => "installed".into(),
        (false, true) => "not installed (bundled: idn-node driver install …)".into(),
        (false, false) => "not installed (not bundled with these programs)".into(),
    }
}

#[cfg(windows)]
pub mod win {
    use super::*;
    use idn_common::win::wide as w;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows_sys::Win32::System::Registry::*;
    use windows_sys::core::GUID;

    fn last(what: &str) -> io::Error {
        let e = io::Error::last_os_error();
        io::Error::new(e.kind(), format!("{what}: {e}"))
    }

    /// This Windows' build number (22000 and later: Windows 11).
    pub fn build() -> u32 {
        idn_common::win::windows_build().unwrap_or(0)
    }

    pub fn supported(d: &Driver) -> bool {
        d.min_build == 0 || build() >= d.min_build
    }

    /// The devices whose hardware ids hold the driver's (present ones only, or every one
    /// the system remembers), each handed to `each` with the set it is in.
    fn with_devices(d: &Driver, present: bool, mut each: impl FnMut(HDEVINFO, &SP_DEVINFO_DATA)) {
        let flags = if present {
            DIGCF_ALLCLASSES | DIGCF_PRESENT
        } else {
            DIGCF_ALLCLASSES
        };
        // SAFETY: SetupAPI's own enumeration; the set destroyed at the end.
        unsafe {
            let set = SetupDiGetClassDevsW(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                flags,
            );
            if set == -1 {
                return;
            }
            let mut i = 0;
            loop {
                let mut data = SP_DEVINFO_DATA {
                    cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(set, i, &mut data) == 0 {
                    break;
                }
                i += 1;
                let mut buf = vec![0u8; 2048];
                let mut kind = 0u32;
                let mut need = 0u32;
                if SetupDiGetDeviceRegistryPropertyW(
                    set,
                    &data,
                    SPDRP_HARDWAREID,
                    &mut kind,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut need,
                ) == 0
                {
                    continue;
                }
                buf.truncate(need as usize);
                if multi_sz_has(&buf, d.hwid) {
                    each(set, &data);
                }
            }
            SetupDiDestroyDeviceInfoList(set);
        }
    }

    pub fn installed(d: &Driver) -> bool {
        let mut found = false;
        with_devices(d, true, |_, _| found = true);
        found
    }

    /// The device made (when there is none) and given the bundled driver. Whether
    /// Windows wants a restart.
    pub fn install(d: &Driver) -> io::Result<bool> {
        if !supported(d) {
            return Err(io::Error::other(format!(
                "{}: for Windows 11 (build {} and later); this is build {}",
                d.what,
                d.min_build,
                build()
            )));
        }
        let inf = package(d).ok_or_else(|| {
            io::Error::other(format!(
                "{}: its package isn't beside the programs (drivers\\{})",
                d.what, d.folder
            ))
        })?;
        if d.key == DISPLAY.key {
            // Its settings, where it reads them as it starts.
            crate::virtual_screen::windows::ensure_settings()?;
        }
        let inf_w = idn_common::win::wide_os(inf.as_os_str());
        let hwid = w(d.hwid);
        let class = GUID::from_u128(d.class);
        let mut made: Option<(HDEVINFO, SP_DEVINFO_DATA)> = None;
        if !installed(d) {
            // SAFETY: SetupAPI calls on a set and device made here; the hardware id list
            // (two terminating zeros) outlives the call.
            unsafe {
                let set = SetupDiCreateDeviceInfoList(&class, std::ptr::null_mut());
                if set == -1 {
                    return Err(last("a device list"));
                }
                let mut data = SP_DEVINFO_DATA {
                    cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                let name = w(d.class_name);
                if SetupDiCreateDeviceInfoW(
                    set,
                    name.as_ptr(),
                    &class,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    DICD_GENERATE_ID,
                    &mut data,
                ) == 0
                {
                    let e = last("making the device");
                    SetupDiDestroyDeviceInfoList(set);
                    return Err(e);
                }
                let mut ids: Vec<u16> = d.hwid.encode_utf16().collect();
                ids.extend([0, 0]);
                if SetupDiSetDeviceRegistryPropertyW(
                    set,
                    &mut data,
                    SPDRP_HARDWAREID,
                    ids.as_ptr().cast(),
                    (ids.len() * 2) as u32,
                ) == 0
                    || SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, &data) == 0
                {
                    let e = last("registering the device");
                    SetupDiDestroyDeviceInfoList(set);
                    return Err(e);
                }
                made = Some((set, data));
            }
        }
        let mut reboot = 0;
        // SAFETY: strings of ours, ending in zero.
        let ok = unsafe {
            UpdateDriverForPlugAndPlayDevicesW(
                std::ptr::null_mut(),
                hwid.as_ptr(),
                inf_w.as_ptr(),
                INSTALLFLAG_FORCE,
                &mut reboot,
            )
        };
        let err = (ok == 0).then(|| last("installing the driver"));
        if let Some((set, data)) = made {
            // SAFETY: the set and device made above.
            unsafe {
                if err.is_some() {
                    // No device left without its driver.
                    SetupDiCallClassInstaller(DIF_REMOVE, set, &data);
                }
                SetupDiDestroyDeviceInfoList(set);
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(reboot != 0),
        }
    }

    /// Every device of the driver taken out, and its package out of the driver store.
    pub fn remove(d: &Driver) -> io::Result<()> {
        let mut infs: Vec<Vec<u16>> = Vec::new();
        let mut failed = None;
        with_devices(d, false, |set, data| {
            // SAFETY: SetupAPI's set and device; the key closed here.
            unsafe {
                let key = SetupDiOpenDevRegKey(set, data, DICS_FLAG_GLOBAL, 0, DIREG_DRV, KEY_READ);
                if !key.is_null() && key as isize != -1 {
                    let mut buf = [0u16; 64];
                    let mut len = (buf.len() * 2) as u32;
                    let name = w("InfPath");
                    if RegGetValueW(
                        key,
                        std::ptr::null(),
                        name.as_ptr(),
                        RRF_RT_REG_SZ,
                        std::ptr::null_mut(),
                        buf.as_mut_ptr().cast(),
                        &mut len,
                    ) == 0
                    {
                        let n = (len as usize / 2).saturating_sub(1).min(buf.len());
                        let mut v = buf[..n].to_vec();
                        v.push(0);
                        if !infs.contains(&v) {
                            infs.push(v);
                        }
                    }
                    RegCloseKey(key);
                }
                let mut reboot = 0;
                if DiUninstallDevice(std::ptr::null_mut(), set, data, 0, &mut reboot) == 0 {
                    failed = Some(last("removing the device"));
                }
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        // Packages a removal before couldn't take out of the store (its devices were gone
        // then, so they can't be found through them now): tried again.
        let pending = pending_file(d);
        for name in std::fs::read_to_string(&pending)
            .unwrap_or_default()
            .lines()
        {
            // Only a package that is still this driver's (gone, or another's: dropped).
            let name = name.trim();
            if !still_ours(d, name) {
                continue;
            }
            let v = w(name);
            if !infs.contains(&v) {
                infs.push(v);
            }
        }
        let mut left = Vec::new();
        for inf in infs {
            // SAFETY: an oemNN.inf name ending in zero.
            if unsafe { SetupUninstallOEMInfW(inf.as_ptr(), SUOI_FORCEDELETE, std::ptr::null()) }
                == 0
            {
                failed = Some(last("taking the driver out of Windows' driver store"));
                left.push(String::from_utf16_lossy(&inf[..inf.len() - 1]));
            }
        }
        if left.is_empty() {
            let _ = std::fs::remove_file(&pending);
        } else {
            if let Some(dir) = pending.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&pending, left.join("\n"));
        }
        failed.map_or(Ok(()), Err)
    }

    /// Where the packages a removal couldn't take out are kept for the next one: in the
    /// Node's own folder (SYSTEM and administrators only; a list in a folder users may
    /// write in could name any driver for an elevated removal to delete).
    fn pending_file(d: &Driver) -> PathBuf {
        crate::service::system_state_dir().join(format!("driver-store-{}.txt", d.key))
    }

    /// Whether `name` is an `oemNN.inf` in Windows' INF folder that is still this
    /// driver's (it names the driver's hardware id): an oemNN number is reused once a
    /// package is gone.
    fn still_ours(d: &Driver, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        let Some(n) = lower
            .strip_prefix("oem")
            .and_then(|r| r.strip_suffix(".inf"))
        else {
            return false;
        };
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let win = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let Ok(b) = std::fs::read(win.join("INF").join(&lower)) else {
            return false;
        };
        // ANSI or UTF-16 (little-endian), as INF files are.
        let text = if b.starts_with(&[0xff, 0xfe]) {
            String::from_utf16_lossy(
                &b[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect::<Vec<_>>(),
            )
        } else {
            String::from_utf8_lossy(&b).into_owned()
        };
        text.to_ascii_lowercase()
            .contains(&d.hwid.to_ascii_lowercase())
    }
}

/// (Elsewhere there are none of these drivers.)
#[cfg(not(windows))]
pub mod win {
    use super::*;
    pub fn supported(_: &Driver) -> bool {
        false
    }
    pub fn installed(_: &Driver) -> bool {
        false
    }
    pub fn install(_: &Driver) -> io::Result<bool> {
        Err(io::Error::other("these drivers are for Windows"))
    }
    pub fn remove(_: &Driver) -> io::Result<()> {
        Err(io::Error::other("these drivers are for Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_ids_matched() {
        let mut b = Vec::new();
        for s in ["ROOT\\VirtualAudioDriver", "Other"] {
            for u in s.encode_utf16().chain([0]) {
                b.extend(u.to_le_bytes());
            }
        }
        b.extend([0, 0]);
        assert!(multi_sz_has(&b, AUDIO.hwid));
        assert!(multi_sz_has(&b, "root\\virtualaudiodriver"));
        assert!(!multi_sz_has(&b, DISPLAY.hwid));
        assert_eq!(by_key("Display").map(|d| d.inf), Some("MttVDD.inf"));
    }
}
