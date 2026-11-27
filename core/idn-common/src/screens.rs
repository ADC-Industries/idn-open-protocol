//! The monitors of an X11 desktop, as `xrandr --listmonitors` lists them: screen 1 is the
//! first (the primary, which xrandr lists first), 2, 3, … the others. The screen capture
//! grabs one, and remote control (roadmap R.25) places the pointer on it, from the same
//! list. Elsewhere (Windows has its own; Wayland's portal chooses) the list is empty.

use crate::cmd::RunWithin as _;
/// One monitor: where it lies on the desktop, in pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Its output (connector): `DP-1`.
    pub name: String,
    /// The primary monitor (`*`).
    pub primary: bool,
}

/// The X11 desktop's monitors, in xrandr's order; empty without xrandr or a display.
pub fn x11_monitors() -> Vec<Monitor> {
    if !cfg!(all(unix, not(target_os = "macos"))) {
        return Vec::new();
    }
    std::process::Command::new("xrandr")
        .arg("--listmonitors")
        .output_within(std::time::Duration::from_secs(5))
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// The desktop's size: the monitors' bounding box (from the origin).
pub fn desktop(monitors: &[Monitor]) -> Option<(u32, u32)> {
    let w = monitors.iter().map(|m| m.x.max(0) as u32 + m.width).max()?;
    let h = monitors
        .iter()
        .map(|m| m.y.max(0) as u32 + m.height)
        .max()?;
    Some((w, h))
}

/// `xrandr --listmonitors`: ` 0: +*DP-1 1920/527x1080/296+0+0  DP-1`.
fn parse(text: &str) -> Vec<Monitor> {
    text.lines()
        .filter_map(|l| {
            let geometry = l.split_whitespace().nth(2)?;
            let (w, rest) = geometry.split_once('/')?;
            let (_, rest) = rest.split_once('x')?;
            let (h, rest) = rest.split_once('/')?;
            // The offsets keep their signs: "+1920+0", "-1920+0".
            let off = &rest[rest.find(['+', '-'])?..];
            let (x, y) = signed_pair(off)?;
            let flags = l.split_whitespace().nth(1)?;
            Some(Monitor {
                x,
                y,
                width: w.parse().ok()?,
                height: h.parse().ok()?,
                name: l
                    .split_whitespace()
                    .nth(3)
                    .map(str::to_owned)
                    .unwrap_or_else(|| flags.trim_start_matches(['+', '*']).to_owned()),
                primary: flags.contains('*'),
            })
        })
        .collect()
}

/// `+X+Y` (either sign each) as numbers.
fn signed_pair(s: &str) -> Option<(i32, i32)> {
    let second = s[1..].find(['+', '-'])? + 1;
    Some((s[..second].parse().ok()?, s[second..].parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrandr_s_list_is_read() {
        let t = "Monitors: 2\n 0: +*DP-1 1920/527x1080/296+0+0  DP-1\n 1: +HDMI-1 2560/597x1440/336+1920+0  HDMI-1\n";
        let m = parse(t);
        assert_eq!(
            m,
            [
                Monitor {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    name: "DP-1".into(),
                    primary: true,
                },
                Monitor {
                    x: 1920,
                    y: 0,
                    width: 2560,
                    height: 1440,
                    name: "HDMI-1".into(),
                    primary: false,
                },
            ]
        );
        assert_eq!(desktop(&m), Some((4480, 1440)));
        assert_eq!(signed_pair("-1920+0"), Some((-1920, 0)));
    }
}
