//! This computer's displays, each with an identity that stays when the screen numbers
//! change (Windows renumbers monitors when one is plugged in
//! or the arrangement changes). A screen source keeps the display's `id`; the capture and
//! the remote control's pointer find where that display is now.
//!
//! - Windows: the default adapter's DXGI outputs, in Desktop Duplication's order (what
//!   `ddagrab`'s `output_idx` counts); each one's monitor device path (its EDID maker and
//!   product, and the connector) for the id, the name Windows shows for it (DisplayConfig's
//!   friendly name, from its EDID), and its current mode.
//! - Linux (X11): xrandr's monitors in their order (the primary first), by connector
//!   (`DP-1`), the EDID's name from the kernel where it gives one, and the current mode.
//!
//! An id is `mon:` and 12 hex digits (a hash of the Windows device path) or `x11:` and the
//! connector. A number (`1`, `2`) is still a position, as before.

/// One display, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// Its identity: `mon:…` (Windows), `x11:CONNECTOR` (Linux).
    pub id: String,
    /// What to call it: the monitor's own name where it has one ("LG ULTRAGEAR"),
    /// else its connector.
    pub name: String,
    /// Where it's plugged in (`\\.\DISPLAY2`, `DP-1`).
    pub connector: String,
    /// Its position now, as `--screen N` counts (1 the first).
    pub position: u32,
    /// Its place on the desktop and its size, in pixels.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Its refresh rate (Hz), when known.
    pub hz: Option<u32>,
    pub primary: bool,
}

/// FNV-1a, 64 bits: a short, stable id from a device path.
// (Used on Windows only, for its device paths.)
#[cfg_attr(not(windows), allow(dead_code))]
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

/// This computer's displays now (empty without a desktop session).
pub fn displays() -> Vec<Display> {
    #[cfg(windows)]
    {
        windows_displays().unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        x11_displays()
    }
}

/// The display `which` names: a position (`1`), or an id (`mon:…`, `x11:DP-1`). `None`:
/// no such display now (an id's display unplugged), or not a display's name.
pub fn resolve(which: &str) -> Option<Display> {
    let which = which.trim();
    let all = displays();
    if let Ok(n) = which.parse::<u32>() {
        return all.into_iter().find(|d| d.position == n.max(1));
    }
    all.into_iter().find(|d| d.id.eq_ignore_ascii_case(which))
}

/// Whether `which` is a display's id rather than a position or an X display.
pub fn is_id(which: &str) -> bool {
    let w = which.trim();
    w.starts_with("mon:") || w.starts_with("x11:")
}

// ---- Windows

#[cfg(windows)]
fn wstr(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

#[cfg(windows)]
fn windows_displays() -> windows::core::Result<Vec<Display>> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    // SAFETY: COM calls on interfaces this function owns; the descriptions are copied out.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    // The default adapter's outputs: what ddagrab duplicates (its device is the default
    // adapter's), in its order.
    // SAFETY: as above.
    let adapter = unsafe { factory.EnumAdapters1(0)? };
    let names = friendly_names();
    let mut out = Vec::new();
    let mut j = 0u32;
    // SAFETY: as above.
    while let Ok(o) = unsafe { adapter.EnumOutputs(j) } {
        j += 1;
        // SAFETY: as above.
        let Ok(d) = (unsafe { o.GetDesc() }) else {
            continue;
        };
        if !d.AttachedToDesktop.as_bool() {
            continue;
        }
        let connector = wstr(&d.DeviceName);
        let r = d.DesktopCoordinates;
        let (path, plain) = monitor_path(&connector);
        let (width, height, hz) = current_mode(&connector).unwrap_or((
            (r.right - r.left).max(0) as u32,
            (r.bottom - r.top).max(0) as u32,
            0,
        ));
        let name = names
            .iter()
            .find(|(p, _)| !path.is_empty() && p.eq_ignore_ascii_case(&path))
            .map(|(_, n)| n.clone())
            .filter(|n| !n.is_empty())
            .or(plain.filter(|n| !n.is_empty() && n != "Generic PnP Monitor"))
            .unwrap_or_else(|| format!("Display {j}"));
        out.push(Display {
            id: format!(
                "mon:{}",
                short_hash(if path.is_empty() { &connector } else { &path })
            ),
            name,
            connector,
            position: j,
            x: r.left,
            y: r.top,
            width,
            height,
            hz: (hz > 1).then_some(hz),
            primary: r.left == 0 && r.top == 0,
        });
    }
    Ok(out)
}

/// The monitor's device interface path on `\\.\DISPLAYn` (its identity: maker, product,
/// connector) and the name Windows gives the monitor there.
#[cfg(windows)]
fn monitor_path(device: &str) -> (String, Option<String>) {
    use windows::Win32::Graphics::Gdi::{DISPLAY_DEVICEW, EnumDisplayDevicesW};
    use windows::core::PCWSTR;
    const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;
    let dev: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
    let mut dd = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // SAFETY: a NUL-terminated name and a sized struct.
    let ok = unsafe {
        EnumDisplayDevicesW(
            PCWSTR(dev.as_ptr()),
            0,
            &mut dd,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    if !ok.as_bool() {
        return (String::new(), None);
    }
    (wstr(&dd.DeviceID), Some(wstr(&dd.DeviceString)))
}

/// The current mode of `\\.\DISPLAYn`: width, height, refresh (Hz).
#[cfg(windows)]
fn current_mode(device: &str) -> Option<(u32, u32, u32)> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    use windows::core::PCWSTR;
    let dev: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: as above.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(dev.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) };
    ok.as_bool()
        .then_some((dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency))
}

/// Each active monitor's device path and the name Windows shows for it (from its EDID).
#[cfg(windows)]
fn friendly_names() -> Vec<(String, String)> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
        GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    let mut out = Vec::new();
    // SAFETY: buffers of the sizes Windows asks for; each request sized and typed.
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
        .is_err()
        {
            return out;
        }
        for p in paths.iter().take(np as usize) {
            let mut t = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            t.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            t.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            t.header.adapterId = p.targetInfo.adapterId;
            t.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut t.header) == 0 {
                out.push((
                    wstr(&t.monitorDevicePath),
                    wstr(&t.monitorFriendlyDeviceName),
                ));
            }
        }
    }
    out
}

// ---- Linux (X11)

#[cfg(not(windows))]
fn x11_displays() -> Vec<Display> {
    use idn_common::cmd::RunWithin as _;
    let monitors = idn_common::screens::x11_monitors();
    let current = std::process::Command::new("xrandr")
        .arg("--current")
        .output_within(std::time::Duration::from_secs(5))
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let connector = m.name.clone();
            Display {
                id: format!("x11:{connector}"),
                name: edid_name(&connector).unwrap_or_else(|| connector.clone()),
                connector: connector.clone(),
                position: i as u32 + 1,
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                hz: refresh_of(&current, &connector),
                primary: m.primary,
            }
        })
        .collect()
}

/// The refresh rate of `connector`'s current mode, from `xrandr --current` (the mode
/// line under it marked `*`).
#[cfg(not(windows))]
fn refresh_of(text: &str, connector: &str) -> Option<u32> {
    let mut under = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            under = line.split_whitespace().next() == Some(connector);
            continue;
        }
        if under && let Some(r) = line.split_whitespace().skip(1).find(|w| w.contains('*')) {
            return r
                .trim_end_matches(['*', '+'])
                .parse::<f64>()
                .ok()
                .map(|f| f.round() as u32);
        }
    }
    None
}

/// The monitor's name from its EDID (the kernel's copy, `/sys/class/drm/card*-CONNECTOR`).
#[cfg(not(windows))]
fn edid_name(connector: &str) -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    for e in dir.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        // card0-DP-1; xrandr may say DP-1 or DP1.
        let Some((_, c)) = n.split_once('-') else {
            continue;
        };
        if !c.eq_ignore_ascii_case(connector) && c.replace('-', "") != connector.replace('-', "") {
            continue;
        }
        let edid = std::fs::read(e.path().join("edid")).ok()?;
        return edid_monitor_name(&edid);
    }
    None
}

/// An EDID's monitor name descriptor (tag 0xFC), if it has one.
pub fn edid_monitor_name(edid: &[u8]) -> Option<String> {
    if edid.len() < 128 {
        return None;
    }
    for k in 0..4 {
        let d = &edid[54 + k * 18..54 + (k + 1) * 18];
        if d[0] == 0 && d[1] == 0 && d[3] == 0xFC {
            let t: String = d[5..18]
                .iter()
                .take_while(|&&b| b != 0x0A && b != 0)
                .map(|&b| b as char)
                .collect();
            let t = t.trim().to_owned();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// The resolution and rate in a few words: `2560 × 1440 at 144 Hz`.
pub fn mode_text(d: &Display) -> String {
    match d.hz {
        Some(hz) => format!("{} \u{00d7} {} at {hz} Hz", d.width, d.height),
        None => format!("{} \u{00d7} {}", d.width, d.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_and_stable() {
        let p = r"\\?\DISPLAY#GSM5B7F#5&2a1c6e7&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(short_hash(p), short_hash(&p.to_uppercase()));
        assert_eq!(short_hash(p).len(), 12);
        assert_ne!(short_hash(p), short_hash(&p.replace("UID4353", "UID4354")));
        assert!(is_id("mon:0123456789ab") && is_id("x11:DP-1") && !is_id("2") && !is_id(":0"));
    }

    #[test]
    fn an_edid_names_its_monitor() {
        let mut e = vec![0u8; 128];
        let d = 54 + 18; // the second descriptor
        e[d + 3] = 0xFC;
        e[d + 5..d + 5 + 13].copy_from_slice(b"LG ULTRAGEAR\n");
        assert_eq!(edid_monitor_name(&e).as_deref(), Some("LG ULTRAGEAR"));
        assert_eq!(edid_monitor_name(&e[..100]), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn xrandr_s_current_mode_gives_the_rate() {
        let t = "Screen 0: minimum 320 x 200\nDP-1 connected primary 2560x1440+0+0 (normal) 597mm x 336mm\n   2560x1440     59.95 +  143.97*\n   1920x1080     60.00\nHDMI-1 connected 1920x1080+2560+0\n   1920x1080     60.00*+  50.00\n";
        assert_eq!(refresh_of(t, "DP-1"), Some(144));
        assert_eq!(refresh_of(t, "HDMI-1"), Some(60));
        assert_eq!(refresh_of(t, "VGA-1"), None);
    }
}
