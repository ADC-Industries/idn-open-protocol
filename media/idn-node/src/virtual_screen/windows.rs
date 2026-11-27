//! Virtual screens on Windows (plan C; idn-node.md §N.36):
//! the bundled Virtual Display Driver (drivers.rs) makes as many screens as its settings
//! file says (`vdd_settings.xml`: `<monitors><count>`), each offering the modes listed
//! there (`<resolutions>`, the global refresh rates replicated over them). The Node
//! keeps that file, installs the driver with the first screen, and tells a running
//! driver to read it again through the driver's pipe (`RELOAD_DRIVER`); the last screen
//! removed takes the driver out again. Windows remembers each screen's mode and place.
//!
//! The file is where the driver reads it: the folder in `HKLM\SOFTWARE\MikeTheTech\
//! VirtualDisplayDriver` `VDDPATH` when set (its own control app sets it), else
//! `C:\VirtualDisplayDriver`. A file there already (made by that app) is kept and edited.

#![cfg_attr(not(windows), allow(dead_code))]

use std::io;
use std::path::PathBuf;

/// The driver's pipe for commands.
#[cfg_attr(not(windows), allow(dead_code))]
const PIPE: &str = r"\\.\pipe\MTTVirtualDisplayPipe";

/// `<count>` in `<monitors>`.
pub fn count_of(xml: &str) -> Option<u32> {
    let m = xml.find("<monitors>")?;
    let rest = &xml[m..];
    let a = rest.find("<count>")? + "<count>".len();
    let b = rest[a..].find("</count>")? + a;
    rest[a..b].trim().parse().ok()
}

/// `xml` with `<monitors><count>` set to `n`.
pub fn with_count(xml: &str, n: u32) -> Option<String> {
    let m = xml.find("<monitors>")?;
    let a = xml[m..].find("<count>")? + m + "<count>".len();
    let b = xml[a..].find("</count>")? + a;
    Some(format!("{}{n}{}", &xml[..a], &xml[b..]))
}

/// The modes listed (width, height, refresh).
pub fn modes_of(xml: &str) -> Vec<(u32, u32, u32)> {
    let num = |s: &str, tag: &str| -> Option<u32> {
        let a = s.find(&format!("<{tag}>"))? + tag.len() + 2;
        let b = s[a..].find(&format!("</{tag}>"))? + a;
        s[a..b].trim().parse().ok()
    };
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(a) = rest.find("<resolution>") {
        let Some(b) = rest[a..].find("</resolution>") else {
            break;
        };
        let r = &rest[a..a + b];
        if let (Some(w), Some(h)) = (num(r, "width"), num(r, "height")) {
            out.push((w, h, num(r, "refresh_rate").unwrap_or(60)));
        }
        rest = &rest[a + b..];
    }
    out
}

/// `xml` with the mode listed first (Windows takes the first as the screen's preferred
/// mode), and its rate among the global ones; unchanged when it is first already.
pub fn with_mode(xml: &str, (w, h, hz): (u32, u32, u32)) -> Option<String> {
    let mut x = xml.to_owned();
    if modes_of(&x).first() != Some(&(w, h, hz)) {
        let a = x.find("<resolutions>")? + "<resolutions>".len();
        let entry = format!(
            "\r\n        <resolution>\r\n            <width>{w}</width>\r\n            <height>{h}</height>\r\n            <refresh_rate>{hz}</refresh_rate>\r\n        </resolution>"
        );
        x.insert_str(a, &entry);
    }
    let rate = format!("<g_refresh_rate>{hz}</g_refresh_rate>");
    if !x.contains(&rate)
        && let Some(a) = x.find("<global>")
    {
        let a = a + "<global>".len();
        x.insert_str(a, &format!("\r\n\t\t{rate}"));
    }
    Some(x)
}

/// The settings folder (the driver's choice: its registry value, else the default).
#[cfg(windows)]
pub fn settings_dir() -> PathBuf {
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    if let Some(p) = idn_common::win::reg_sz(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\MikeTheTech\VirtualDisplayDriver",
        "VDDPATH",
    ) {
        return PathBuf::from(p);
    }
    PathBuf::from(r"C:\VirtualDisplayDriver")
}

#[cfg(not(windows))]
pub fn settings_dir() -> PathBuf {
    PathBuf::from("/nonexistent/VirtualDisplayDriver")
}

fn settings_file() -> PathBuf {
    settings_dir().join("vdd_settings.xml")
}

/// The settings file there, from the bundled package when missing (no screen yet:
/// count 0 until one is added).
pub fn ensure_settings() -> io::Result<()> {
    // The Node (SYSTEM) writes here, a folder at the drive's root that any user may make
    // first: not through a link or junction a user put there (deep bug run 2026-11-09).
    for p in [settings_dir(), settings_file()] {
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::other(format!(
                "{}: a link or junction, not used (remove it)",
                p.display()
            )));
        }
    }
    // The folder taken over (owner SYSTEM, full control SYSTEM and administrators, read
    // for users and the driver): one a user made first stayed theirs, and they could swap
    // what SYSTEM reads and writes there between its check and its write (bug run B).
    #[cfg(windows)]
    {
        std::fs::create_dir_all(settings_dir())?;
        idn_common::fsutil::lock_down_dir(&settings_dir())?;
        let _ = std::process::Command::new("icacls")
            .arg(settings_dir())
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)RX"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let f = settings_file();
    if f.is_file() {
        return Ok(());
    }
    let pkg = crate::drivers::package(&crate::drivers::DISPLAY)
        .and_then(|inf| inf.parent().map(|d| d.join("vdd_settings.xml")))
        .ok_or_else(|| {
            io::Error::other("the Virtual Display Driver's package isn't beside the programs")
        })?;
    let xml = std::fs::read_to_string(pkg)?;
    let xml = with_count(&xml, 0).unwrap_or(xml);
    std::fs::create_dir_all(settings_dir())?;
    std::fs::write(f, xml)
}

fn read() -> io::Result<String> {
    ensure_settings()?;
    std::fs::read_to_string(settings_file())
}

/// Tells the running driver to read its settings again (its pipe).
fn reload() -> io::Result<()> {
    use std::io::Write;
    let mut p = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE)?;
    let msg: Vec<u8> = "RELOAD_DRIVER"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    p.write_all(&msg)
}

/// What the panel and `status` show.
#[derive(Debug, Clone)]
pub struct State {
    pub driver: String,
    pub installed: bool,
    pub screens: u32,
    pub modes: Vec<(u32, u32, u32)>,
}

impl State {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "driver": self.driver,
            "installed": self.installed,
            "screens": self.screens,
            "modes": self.modes.iter().map(|(w, h, hz)| format!("{w}x{h}@{hz}")).collect::<Vec<_>>(),
        })
    }
}

pub fn state() -> State {
    let d = &crate::drivers::DISPLAY;
    let installed = crate::drivers::win::installed(d);
    let xml = std::fs::read_to_string(settings_file()).unwrap_or_default();
    State {
        driver: crate::drivers::status_line(d),
        installed,
        screens: if installed {
            count_of(&xml).unwrap_or(0)
        } else {
            0
        },
        modes: modes_of(&xml),
    }
}

/// One more screen, `mode` its preferred one: how many there are now, and whether
/// Windows wants a restart first (the driver just installed). The settings are put back
/// as they were when the driver can't take them.
pub fn add(mode: (u32, u32, u32)) -> io::Result<(u32, bool)> {
    let old = read()?;
    let installed = crate::drivers::win::installed(&crate::drivers::DISPLAY);
    let n = if installed {
        count_of(&old).unwrap_or(0) + 1
    } else {
        1
    };
    let xml = with_mode(&old, mode)
        .and_then(|x| with_count(&x, n))
        .ok_or_else(|| io::Error::other("vdd_settings.xml: not the layout expected"))?;
    std::fs::write(settings_file(), xml)?;
    let r = if installed {
        reload().map(|()| false)
    } else {
        crate::drivers::win::install(&crate::drivers::DISPLAY)
    };
    match r {
        Ok(reboot) => Ok((n, reboot)),
        Err(e) => {
            let _ = std::fs::write(settings_file(), old);
            Err(e)
        }
    }
}

/// One screen fewer; the last one takes the driver out. The settings are put back as
/// they were when the driver can't take them.
pub fn remove() -> io::Result<u32> {
    let old = read()?;
    let n = count_of(&old).unwrap_or(0).saturating_sub(1);
    let out =
        with_count(&old, n).ok_or_else(|| io::Error::other("vdd_settings.xml: no <count>"))?;
    std::fs::write(settings_file(), out)?;
    let r = if n == 0 {
        crate::drivers::win::remove(&crate::drivers::DISPLAY)
    } else {
        reload()
    };
    if let Err(e) = r {
        let _ = std::fs::write(settings_file(), old);
        return Err(e);
    }
    Ok(n)
}

/// `idn-node virtual-screen add WxH[@HZ] | remove | status` on Windows.
pub fn command(mut args: idn_open_protocol::cli::Args) -> io::Result<()> {
    match args.next().as_deref() {
        Some("add") => {
            let m = args
                .next()
                .unwrap_or_else(|| args.fail("virtual-screen add WxH[@HZ]"));
            let mode = idn_common::edid::parse_mode(&m)
                .ok_or_else(|| io::Error::other(format!("{m}: a mode is WxH or WxH@HZ")))?;
            let (n, reboot) = add(mode)?;
            println!(
                "virtual screens: {n} ({}x{} at {} Hz offered first). {}",
                mode.0,
                mode.1,
                mode.2,
                if reboot {
                    "Restart Windows to finish installing the driver; the screen comes then."
                } else {
                    "Windows shows the new one in Display settings."
                }
            );
            Ok(())
        }
        Some("remove") => {
            let n = remove()?;
            println!("virtual screens: {n}");
            Ok(())
        }
        Some("status") | None => {
            let s = state();
            println!("driver: {}", s.driver);
            println!("virtual screens: {}", s.screens);
            let m: Vec<String> = s
                .modes
                .iter()
                .map(|(w, h, hz)| format!("{w}x{h}@{hz}"))
                .collect();
            println!("modes offered: {}", m.join(", "));
            Ok(())
        }
        _ => args.fail("virtual-screen add WxH[@HZ] | remove | status"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = "<?xml version='1.0' encoding='utf-8'?>\r\n<vdd_settings>\r\n    <monitors>\r\n        <count>1</count>\r\n    </monitors>\r\n\t<global>\r\n\t\t<g_refresh_rate>60</g_refresh_rate>\r\n\t</global>\r\n    <resolutions>\r\n        <resolution>\r\n            <width>800</width>\r\n            <height>600</height>\r\n            <refresh_rate>30</refresh_rate>\r\n        </resolution>\r\n    </resolutions>\r\n</vdd_settings>\r\n";

    #[test]
    fn settings_edited() {
        assert_eq!(count_of(XML), Some(1));
        let x = with_count(XML, 3).unwrap();
        assert_eq!(count_of(&x), Some(3));
        assert_eq!(modes_of(XML), vec![(800, 600, 30)]);
        let x = with_mode(&x, (3840, 2160, 120)).unwrap();
        assert_eq!(modes_of(&x), vec![(3840, 2160, 120), (800, 600, 30)]);
        assert!(x.contains("<g_refresh_rate>120</g_refresh_rate>"));
        assert!(x.contains("<g_refresh_rate>60</g_refresh_rate>"));
        // Already first: unchanged.
        assert_eq!(with_mode(&x, (3840, 2160, 120)).unwrap(), x);
        assert_eq!(count_of(&x), Some(3));
    }
}
