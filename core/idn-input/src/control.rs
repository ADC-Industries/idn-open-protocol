//! IDN remote control's events (R.25, idn-extensions.md §E.25:
//! [`idn_open_protocol::remote::Control`]) as the injector's: the pointer's place in the
//! picture mapped onto the part of the desktop the picture shows, the buttons by name,
//! the wheel's 120ths gathered into whole notches (a touchpad's small turns add up).

use crate::Event;
use idn_open_protocol::remote::Control;

/// Turns a source's control events into injector events.
#[derive(Debug, Clone)]
pub struct ControlMapper {
    /// The part of the injector's desktop the picture shows: x, y, width, height as
    /// fractions of it ((0, 0, 1, 1): all of it).
    pub area: (f64, f64, f64, f64),
    /// The wheel's turn not yet a whole notch, 120ths (x, y).
    wheel: (i32, i32),
}

impl Default for ControlMapper {
    fn default() -> Self {
        ControlMapper::new((0.0, 0.0, 1.0, 1.0))
    }
}

impl ControlMapper {
    /// A mapper for a picture that shows `area` of the desktop ([`screen_area`]).
    pub fn new(area: (f64, f64, f64, f64)) -> ControlMapper {
        ControlMapper {
            area,
            wheel: (0, 0),
        }
    }

    /// The injector's event for `c`, if it makes one now.
    pub fn map(&mut self, c: &Control) -> Option<Event> {
        match c {
            Control::Move { x, y } => {
                let (ax, ay, aw, ah) = self.area;
                let to = |f: f64| (f.clamp(0.0, 1.0) * 65535.0).round() as u32;
                Some(Event::Move {
                    x: to(ax + *x as f64 * aw),
                    y: to(ay + *y as f64 * ah),
                })
            }
            Control::Button { button, down } => Some(Event::Button {
                button: ["left", "right", "middle", "back", "forward"]
                    .get(*button as usize)?
                    .to_string(),
                down: *down,
            }),
            Control::Wheel { x, y } => {
                // (A viewer's numbers: added without overflowing.)
                self.wheel.0 = self.wheel.0.saturating_add(*x);
                self.wheel.1 = self.wheel.1.saturating_add(*y);
                let (dx, dy) = (self.wheel.0 / 120, self.wheel.1 / 120);
                self.wheel.0 -= dx * 120;
                self.wheel.1 -= dy * 120;
                (dx != 0 || dy != 0).then_some(Event::Wheel { dx, dy })
            }
            Control::Key { code, down } => Some(Event::Key {
                code: code.clone(),
                down: *down,
            }),
            Control::SecureAttention => Some(Event::Cad),
            Control::Clipboard(text) => Some(Event::ClipboardSet { text: text.clone() }),
            Control::Rel { dx, dy } => {
                (*dx != 0 || *dy != 0).then_some(Event::Rel { dx: *dx, dy: *dy })
            }
            Control::Pad { id, buttons, axes } => Some(Event::Pad {
                id: *id,
                buttons: *buttons,
                axes: *axes,
            }),
            // Answered by the capture (its next picture), not the input.
            Control::Probe(_) => None,
        }
    }
}

/// Where screen `index` (from 0, as the capture's `--screen N` counts from 1) lies on the
/// injector's desktop, as fractions of it: on Windows, its monitor within the virtual
/// desktop (monitors in their enumeration order, as the capture lists them); on X11, its
/// monitor as xrandr lists them (the capture grabs the same one; Linux's virtual device
/// spans the whole desktop); else all of it.
pub fn screen_area(index: usize) -> (f64, f64, f64, f64) {
    #[cfg(windows)]
    if let Some(a) = windows_area(index) {
        return a;
    }
    let monitors = idn_common::screens::x11_monitors();
    if let (Some(m), Some((w, h))) = (monitors.get(index), idn_common::screens::desktop(&monitors))
        && monitors.len() > 1
    {
        let (w, h) = (w as f64, h as f64);
        return (
            m.x.max(0) as f64 / w,
            m.y.max(0) as f64 / h,
            m.width as f64 / w,
            m.height as f64 / h,
        );
    }
    (0.0, 0.0, 1.0, 1.0)
}

/// Where the display at `x, y` (`w` × `h` pixels, on the desktop) lies on the injector's
/// desktop, as fractions of it: the display found by its identity (the capture's), not by
/// a position the two count differently.
pub fn display_area(x: i32, y: i32, w: u32, h: u32) -> (f64, f64, f64, f64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        // SAFETY: plain metric queries.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
            )
        };
        if vw > 0.0 && vh > 0.0 {
            return (
                (x as f64 - vx) / vw,
                (y as f64 - vy) / vh,
                w as f64 / vw,
                h as f64 / vh,
            );
        }
    }
    let monitors = idn_common::screens::x11_monitors();
    if let Some((dw, dh)) = idn_common::screens::desktop(&monitors)
        && monitors.len() > 1
    {
        let (dw, dh) = (dw as f64, dh as f64);
        return (
            x.max(0) as f64 / dw,
            y.max(0) as f64 / dh,
            w as f64 / dw,
            h as f64 / dh,
        );
    }
    (0.0, 0.0, 1.0, 1.0)
}

#[cfg(windows)]
fn windows_area(index: usize) -> Option<(f64, f64, f64, f64)> {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe extern "system" fn cb(_m: HMONITOR, _dc: HDC, r: *mut RECT, lp: LPARAM) -> i32 {
        // SAFETY: `lp` is `&mut rects` from below, alive for the synchronous enumeration; `r` is
        // the monitor's RECT, valid during the call.
        let out = unsafe { &mut *(lp as *mut Vec<RECT>) };
        out.push(unsafe { *r });
        1
    }
    let mut rects: Vec<RECT> = Vec::new();
    // SAFETY: `cb` matches MONITORENUMPROC and is called only during this call, while `rects`
    // lives.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(cb),
            &mut rects as *mut _ as LPARAM,
        );
    }
    let r = rects.get(index)?;
    // SAFETY: no pointers.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
            GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
        )
    };
    (vw > 0.0 && vh > 0.0).then(|| {
        (
            (r.left as f64 - vx) / vw,
            (r.top as f64 - vy) / vh,
            (r.right - r.left) as f64 / vw,
            (r.bottom - r.top) as f64 / vh,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_lands_on_the_screen_the_picture_shows() {
        // The right half of a desktop of two side by side.
        let mut m = ControlMapper::new((0.5, 0.0, 0.5, 1.0));
        assert_eq!(
            m.map(&Control::Move { x: 0.0, y: 0.5 }),
            Some(Event::Move { x: 32768, y: 32768 })
        );
        assert_eq!(
            m.map(&Control::Move { x: 1.0, y: 1.0 }),
            Some(Event::Move { x: 65535, y: 65535 })
        );
    }

    #[test]
    fn buttons_by_name_and_the_wheel_by_whole_notches() {
        let mut m = ControlMapper::default();
        assert_eq!(
            m.map(&Control::Button {
                button: 3,
                down: true
            }),
            Some(Event::Button {
                button: "back".into(),
                down: true
            })
        );
        assert_eq!(
            m.map(&Control::Button {
                button: 9,
                down: true
            }),
            None
        );
        // A touchpad's small turns: nothing until a whole notch, the rest kept.
        assert_eq!(m.map(&Control::Wheel { x: 0, y: 50 }), None);
        assert_eq!(m.map(&Control::Wheel { x: 0, y: 50 }), None);
        assert_eq!(
            m.map(&Control::Wheel { x: 0, y: 50 }),
            Some(Event::Wheel { dx: 0, dy: 1 })
        );
        assert_eq!(
            m.map(&Control::Wheel { x: -240, y: 0 }),
            Some(Event::Wheel { dx: -2, dy: 0 })
        );
    }
}
