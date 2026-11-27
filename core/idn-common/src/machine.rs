//! What machine this is, for a site's machine list: its CPU, GPUs, operating system,
//! when it started and its memory. The fixed parts are read once ([`info`]); the free
//! memory at each call ([`memory`]). Everything is best effort: what the system doesn't
//! say is left empty or `None`.

use std::sync::OnceLock;

/// The machine's fixed description ([`info`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineInfo {
    /// The CPU's model name, e.g. `AMD Ryzen 7 7840HS w/ Radeon 780M Graphics`.
    pub cpu: String,
    /// Logical CPUs (hardware threads).
    pub cpus: u32,
    /// Its GPUs' adapter names, in the system's order.
    pub gpus: Vec<String>,
    /// The operating system's name, e.g. `Ubuntu`, `Windows 11 Pro`, `macOS`.
    pub os: String,
    /// Its version, e.g. `24.04`, `24H2`, `15.1`.
    pub os_version: String,
    /// The kernel's release or the build, e.g. `6.8.0-45-generic`, `26100.2314`.
    pub kernel: String,
    /// When the machine started (seconds since 1970).
    pub boot_time: Option<u64>,
    /// Its memory in bytes.
    pub memory_total: Option<u64>,
}

/// The machine's fixed description, read once per process.
pub fn info() -> &'static MachineInfo {
    static INFO: OnceLock<MachineInfo> = OnceLock::new();
    INFO.get_or_init(|| {
        let mut m = imp::read();
        if m.cpus == 0 {
            m.cpus = std::thread::available_parallelism().map_or(0, |n| n.get() as u32);
        }
        m.cpu = m.cpu.split_whitespace().collect::<Vec<_>>().join(" ");
        m
    })
}

/// Its memory now: (total, available) in bytes.
pub fn memory() -> Option<(u64, u64)> {
    imp::memory()
}

/// A short vendor name from a PCI vendor's full one (`NVIDIA Corporation` → `NVIDIA`,
/// `Advanced Micro Devices, Inc. [AMD/ATI]` → `AMD`).
/// An os-release value without its quotes (double or single) and shell escapes.
#[cfg(any(target_os = "linux", test))]
fn unquote(v: &str) -> String {
    let v = v.trim();
    let inner = match v.as_bytes() {
        [b'"', .., b'"'] | [b'\'', .., b'\''] if v.len() >= 2 => &v[1..v.len() - 1],
        _ => v,
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(any(target_os = "linux", test))]
fn short_vendor(full: &str) -> String {
    if let Some(b) = bracketed(full) {
        return b.split('/').next().unwrap_or(b).trim().to_owned();
    }
    let mut v = full.trim();
    for tail in [" Corporation", ", Inc.", " Inc.", " Co., Ltd.", " Ltd."] {
        v = v.strip_suffix(tail).unwrap_or(v);
    }
    v.to_owned()
}

/// The last `[…]` in a name (a PCI device's marketing name: `GA102 [GeForce RTX 3080]`).
#[cfg(any(target_os = "linux", test))]
fn bracketed(s: &str) -> Option<&str> {
    let open = s.rfind('[')?;
    let close = s[open..].find(']')? + open;
    let inner = s[open + 1..close].trim();
    (!inner.is_empty()).then_some(inner)
}

/// A PCI device's name from a `pci.ids` text: `vendor` and `device` as 4 hex digits.
#[cfg(any(target_os = "linux", test))]
fn pci_name(ids: &str, vendor: &str, device: &str) -> Option<String> {
    let mut lines = ids.lines();
    let vname = lines.by_ref().find_map(|l| {
        let rest = l.strip_prefix(vendor)?;
        rest.starts_with(' ').then(|| rest.trim().to_owned())
    })?;
    let dname = lines
        .take_while(|l| l.starts_with('\t') || l.starts_with('#') || l.is_empty())
        .find_map(|l| {
            let rest = l.strip_prefix('\t')?.strip_prefix(device)?;
            rest.starts_with(' ').then(|| rest.trim().to_owned())
        });
    let v = short_vendor(&vname);
    Some(match dname {
        Some(d) => {
            let d = bracketed(&d).map_or(d.as_str(), |b| b).to_owned();
            if d.starts_with(&v) {
                d
            } else {
                format!("{v} {d}")
            }
        }
        None => format!("{v} device {device}"),
    })
}

#[cfg(target_os = "linux")]
mod imp {
    use super::MachineInfo;
    use std::fs;

    pub(super) fn read() -> MachineInfo {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let field = |k: &str| {
            cpuinfo.lines().find_map(|l| {
                let (key, v) = l.split_once(':')?;
                (key.trim() == k && !v.trim().is_empty()).then(|| v.trim().to_owned())
            })
        };
        // x86 says `model name`; ARM boards `Model` (or nothing but the device tree).
        let cpu = field("model name")
            .or_else(|| field("Model"))
            .or_else(|| {
                fs::read_to_string("/sys/firmware/devicetree/base/model")
                    .ok()
                    .map(|s| s.trim_end_matches('\0').trim().to_owned())
            })
            .unwrap_or_default();
        let cpus = cpuinfo
            .lines()
            .filter(|l| l.split(':').next().is_some_and(|k| k.trim() == "processor"))
            .count() as u32;
        // (`/usr/lib/os-release` where `/etc` has none, as os-release(5) says.)
        let os_release = fs::read_to_string("/etc/os-release")
            .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
            .unwrap_or_default();
        let osr = |k: &str| {
            os_release.lines().find_map(|l| {
                let v = l.strip_prefix(k)?.strip_prefix('=')?;
                Some(super::unquote(v))
            })
        };
        let boot_time = fs::read_to_string("/proc/stat").ok().and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
        });
        MachineInfo {
            cpu,
            cpus,
            gpus: gpus(),
            os: osr("NAME").unwrap_or_else(|| "Linux".into()),
            os_version: osr("VERSION_ID")
                .or_else(|| osr("VERSION"))
                .unwrap_or_default(),
            kernel: fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|s| s.trim().to_owned())
                .unwrap_or_default(),
            boot_time,
            memory_total: memory().map(|m| m.0),
        }
    }

    /// The GPUs the kernel drives (`/sys/class/drm/cardN`, PCI ones), named from the
    /// system's `pci.ids` where it has one, else the driver's own name or the ids.
    fn gpus() -> Vec<String> {
        let ids = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"]
            .iter()
            .find_map(|p| fs::read_to_string(p).ok());
        let Ok(dir) = fs::read_dir("/sys/class/drm") else {
            return Vec::new();
        };
        let mut cards: Vec<(u32, String)> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let n: u32 = name.strip_prefix("card")?.parse().ok()?;
                let dev = e.path().join("device");
                let hex = |f: &str| {
                    fs::read_to_string(dev.join(f))
                        .ok()
                        .map(|s| s.trim().trim_start_matches("0x").to_lowercase())
                };
                let (vendor, device) = (hex("vendor")?, hex("device")?);
                let named = fs::read_to_string(dev.join("product_name"))
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .or_else(|| super::pci_name(ids.as_deref()?, &vendor, &device))
                    .unwrap_or_else(|| {
                        let driver = fs::read_link(dev.join("driver"))
                            .ok()
                            .and_then(|p| p.file_name()?.to_str().map(str::to_owned))
                            .unwrap_or_else(|| "GPU".into());
                        format!("{driver} {vendor}:{device}")
                    });
                Some((n, named))
            })
            .collect();
        cards.sort();
        cards.into_iter().map(|(_, n)| n).collect()
    }

    pub(super) fn memory() -> Option<(u64, u64)> {
        let m = fs::read_to_string("/proc/meminfo").ok()?;
        let kb = |k: &str| -> Option<u64> {
            m.lines().find_map(|l| {
                let v = l.strip_prefix(k)?.strip_prefix(':')?;
                v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()
            })
        };
        Some((kb("MemTotal")? * 1024, kb("MemAvailable")? * 1024))
    }
}

#[cfg(windows)]
mod imp {
    use super::MachineInfo;
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    use windows_sys::Win32::System::SystemInformation::{
        GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
    };
    use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

    fn reg_sz(key: &str, value: &str) -> Option<String> {
        crate::win::reg_sz(HKEY_LOCAL_MACHINE, key, value)
    }

    fn reg_dword(key: &str, value: &str) -> Option<u32> {
        crate::win::reg_dword(HKEY_LOCAL_MACHINE, key, value)
    }

    pub(super) fn read() -> MachineInfo {
        const NT: &str = crate::win::NT_CURRENT_VERSION;
        let build = reg_sz(NT, "CurrentBuildNumber").unwrap_or_default();
        let mut os = reg_sz(NT, "ProductName").unwrap_or_else(|| "Windows".into());
        // Windows 11 still says "Windows 10" there; its builds start at 22000.
        if build.parse::<u32>().is_ok_and(|b| b >= 22000) {
            os = os.replacen("Windows 10", "Windows 11", 1);
        }
        let kernel = match reg_dword(NT, "UBR") {
            Some(ubr) if !build.is_empty() => format!("{build}.{ubr}"),
            _ => build,
        };
        // SAFETY: no arguments to get wrong.
        let up_ms = unsafe { GetTickCount64() };
        let boot_time = crate::epoch::now().as_secs().checked_sub(up_ms / 1000);
        MachineInfo {
            cpu: reg_sz(
                r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                "ProcessorNameString",
            )
            .unwrap_or_default(),
            // SAFETY: no pointers.
            cpus: unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) },
            gpus: gpus(),
            os,
            os_version: reg_sz(NT, "DisplayVersion")
                .or_else(|| reg_sz(NT, "ReleaseId"))
                .unwrap_or_default(),
            kernel,
            boot_time,
            memory_total: memory().map(|m| m.0),
        }
    }

    /// The display adapters present (the display class's devices on the PCI bus: no
    /// adapter removed long ago, no virtual display), by name; two alike listed twice
    /// (bug run 2026-11-24: the class's registry keys listed both).
    fn gpus() -> Vec<String> {
        use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
            DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, SP_DEVINFO_DATA, SPDRP_DEVICEDESC,
            SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID, SetupDiDestroyDeviceInfoList,
            SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW,
        };
        // SAFETY: a class GUID of ours, no enumerator or window; the set is destroyed below.
        let set = unsafe {
            SetupDiGetClassDevsW(
                &GUID_DEVCLASS_DISPLAY,
                std::ptr::null(),
                std::ptr::null_mut(),
                DIGCF_PRESENT,
            )
        };
        if set == -1 {
            return Vec::new();
        }
        // A device's string property (the first string of a list), as UTF-16.
        let prop = |d: &SP_DEVINFO_DATA, p: u32| -> Option<String> {
            let mut buf = vec![0u16; 512];
            let mut kind = 0u32;
            // SAFETY: the buffer and its size in bytes are ours; `d` came from the set.
            let ok = unsafe {
                SetupDiGetDeviceRegistryPropertyW(
                    set,
                    d,
                    p,
                    &mut kind,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 2) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return None;
            }
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let s = String::from_utf16_lossy(&buf[..end]).trim().to_owned();
            (!s.is_empty()).then_some(s)
        };
        let mut out = Vec::new();
        for i in 0.. {
            // SAFETY: zeroed is a valid SP_DEVINFO_DATA; its size set as the call wants.
            let mut d: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
            d.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            // SAFETY: `d` is ours and sized; the set is open.
            if unsafe { SetupDiEnumDeviceInfo(set, i, &mut d) } == 0 {
                break;
            }
            let pci = prop(&d, SPDRP_HARDWAREID)
                .is_some_and(|h| h.to_ascii_uppercase().starts_with("PCI\\"));
            if !pci {
                continue;
            }
            if let Some(n) = prop(&d, SPDRP_FRIENDLYNAME).or_else(|| prop(&d, SPDRP_DEVICEDESC)) {
                out.push(n);
            }
        }
        // SAFETY: made above, not used after.
        unsafe { SetupDiDestroyDeviceInfoList(set) };
        out
    }

    pub(super) fn memory() -> Option<(u64, u64)> {
        // SAFETY: zeroed is a valid MEMORYSTATUSEX; its length set as the call wants.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `m` is ours and sized.
        (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some((m.ullTotalPhys, m.ullAvailPhys))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MachineInfo;
    use std::process::Command;

    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let o = Command::new(cmd).args(args).output().ok()?;
        let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        (o.status.success() && !s.is_empty()).then_some(s)
    }

    pub(super) fn read() -> MachineInfo {
        let cpu = run("sysctl", &["-n", "machdep.cpu.brand_string"]).unwrap_or_default();
        // `{ sec = 1700000000, usec = 0 } Tue Nov 14 …`
        let boot_time = run("sysctl", &["-n", "kern.boottime"]).and_then(|s| {
            s.split("sec = ")
                .nth(1)?
                .split(',')
                .next()?
                .trim()
                .parse()
                .ok()
        });
        MachineInfo {
            // Apple silicon's GPU is part of the chip it names.
            gpus: if cpu.starts_with("Apple ") {
                vec![cpu.clone()]
            } else {
                Vec::new()
            },
            cpu,
            cpus: run("sysctl", &["-n", "hw.logicalcpu"])
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            os: run("sw_vers", &["-productName"]).unwrap_or_else(|| "macOS".into()),
            os_version: run("sw_vers", &["-productVersion"]).unwrap_or_default(),
            kernel: run("uname", &["-r"]).unwrap_or_default(),
            boot_time,
            memory_total: run("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse().ok()),
        }
    }

    pub(super) fn memory() -> Option<(u64, u64)> {
        None
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    use super::MachineInfo;

    pub(super) fn read() -> MachineInfo {
        MachineInfo {
            os: std::env::consts::OS.into(),
            ..MachineInfo::default()
        }
    }

    pub(super) fn memory() -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_names_short_and_marketing() {
        let ids = "\
# comment
10de  NVIDIA Corporation
\t2206  GA102 [GeForce RTX 3080]
\t1234  Plain Chip
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t15bf  Phoenix1
8086  Intel Corporation
\t46a6  Alder Lake-P GT2 [Iris Xe Graphics]
";
        assert_eq!(
            pci_name(ids, "10de", "2206").as_deref(),
            Some("NVIDIA GeForce RTX 3080")
        );
        assert_eq!(
            pci_name(ids, "10de", "1234").as_deref(),
            Some("NVIDIA Plain Chip")
        );
        assert_eq!(
            pci_name(ids, "1002", "15bf").as_deref(),
            Some("AMD Phoenix1")
        );
        assert_eq!(
            pci_name(ids, "8086", "46a6").as_deref(),
            Some("Intel Iris Xe Graphics")
        );
        // A device of another vendor isn't taken; one unknown is named by its id.
        assert_eq!(
            pci_name(ids, "10de", "15bf").as_deref(),
            Some("NVIDIA device 15bf")
        );
        assert_eq!(pci_name(ids, "abcd", "0001"), None);
    }

    #[test]
    fn os_release_values_unquoted() {
        assert_eq!(unquote("\"Ubuntu\""), "Ubuntu");
        assert_eq!(unquote("'Foo Linux'"), "Foo Linux");
        assert_eq!(unquote("24.04"), "24.04");
        assert_eq!(unquote("\"say \\\"hi\\\"\""), "say \"hi\"");
        assert_eq!(unquote("\""), "\"");
    }

    #[test]
    fn this_machine_described() {
        let m = info();
        assert!(m.cpus >= 1, "{m:?}");
        assert!(!m.os.is_empty(), "{m:?}");
        #[cfg(target_os = "linux")]
        {
            assert!(m.boot_time.is_some_and(|b| b > 1_500_000_000), "{m:?}");
            let (total, free) = memory().expect("its memory");
            assert!(total > 0 && free <= total);
            assert_eq!(m.memory_total, Some(total));
        }
    }
}
