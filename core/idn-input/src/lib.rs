//! Injecting mouse, keyboard and clipboard on this machine: IDN Industrial's remote
//! support (idn-industrial.md §I.11.4) and IDN remote control (R.25, idn-extensions.md
//! §E.25, through [`control`]). Keys travel as the browser's `KeyboardEvent.code` (a
//! physical key, so layouts don't matter) and are sent as PS/2 scan codes; the mouse as
//! an absolute position in 0..=65535 of the desktop.
//!
//! Windows uses `SendInput`, with `SendSAS` for Ctrl+Alt+Del; Linux a uinput virtual
//! device (X11 and Wayland alike; `/dev/uinput` needs the user's access: `idn-node
//! install` adds the udev rule). Elsewhere, or when it can't open, a recording backend
//! counts events without acting; it is also what the tests use.

use idn_common::sync::LockOk as _;
pub mod clipboard;
pub mod control;

use serde::{Deserialize, Serialize};
use std::io;

/// One input event from a viewer. The browser sends these as JSON over its WebSocket;
/// the Site Manager forwards them to the machine as a `remote.input` command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    /// Absolute pointer position, each axis 0..=65535 of the picture.
    Move { x: u32, y: u32 },
    /// A mouse button (`left`, `right`, `middle`, `back`, `forward`) going down or up.
    Button { button: String, down: bool },
    /// Wheel, in notches (positive up / right).
    Wheel { dx: i32, dy: i32 },
    /// A key by its `KeyboardEvent.code`, down or up.
    Key { code: String, down: bool },
    /// Ctrl+Alt+Del (SendSAS on Windows).
    Cad,
    /// Put text on the station's clipboard.
    ClipboardSet { text: String },
    /// Read the station's clipboard (the answer carries the text).
    ClipboardGet,
    /// The pointer moved by this many counts (a game's mouse; R.25 stage 2).
    Rel { dx: i32, dy: i32 },
    /// A gamepad's state: pad 0–3, buttons (`remote::PAD_BUTTONS`' bit order), axes (left
    /// x, y, right x, y, left and right triggers).
    Pad {
        id: u8,
        buttons: u32,
        axes: [i16; 6],
    },
}

/// What viewers have held down on this machine (keys, mouse buttons, gamepads), to let go
/// of when they go: a viewer whose connection dropped while holding Backspace or Ctrl left
/// it held here, the key repeating or every click a shortcut (bug round E, E3-1).
#[derive(Debug, Default)]
pub struct Held {
    keys: std::collections::BTreeSet<String>,
    buttons: std::collections::BTreeSet<String>,
    pads: std::collections::BTreeSet<u8>,
}

impl Held {
    /// Notes an event injected.
    pub fn note(&mut self, e: &Event) {
        match e {
            // (At most 256 held: a viewer's ever-new codes grew the set, and the release
            // at its end; bug run 2026-11-25.)
            Event::Key { code, down: true } => {
                if self.keys.len() < 256 && code.len() <= 64 {
                    self.keys.insert(code.clone());
                }
            }
            Event::Key { code, down: false } => {
                self.keys.remove(code);
            }
            Event::Button { button, down: true } => {
                self.buttons.insert(button.clone());
            }
            Event::Button {
                button,
                down: false,
            } => {
                self.buttons.remove(button);
            }
            Event::Pad { id, buttons, axes } => {
                if *buttons == 0 && axes.iter().all(|a| *a == 0) {
                    self.pads.remove(id);
                } else {
                    self.pads.insert(*id);
                }
            }
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && self.pads.is_empty()
    }

    /// The events that let go of everything held (and forgets it).
    pub fn release(&mut self) -> Vec<Event> {
        let mut out: Vec<Event> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|code| Event::Key { code, down: false })
            .collect();
        out.extend(
            std::mem::take(&mut self.buttons)
                .into_iter()
                .map(|button| Event::Button {
                    button,
                    down: false,
                }),
        );
        out.extend(
            std::mem::take(&mut self.pads)
                .into_iter()
                .map(|id| Event::Pad {
                    id,
                    buttons: 0,
                    axes: [0; 6],
                }),
        );
        out
    }
}

/// The most clipboard text taken or given, bytes (as a control message carries).
pub const MAX_CLIPBOARD: usize = idn_open_protocol::remote::MAX_CLIPBOARD;

#[cfg(windows)]
fn win_clipboard_get() -> Option<String> {
    win::get_clipboard()
}

#[cfg(windows)]
fn win_clipboard_set(text: &str) -> io::Result<()> {
    win::set_clipboard(text)
}

/// Ctrl+Alt+Del on this machine (Windows' SendSAS, from `sas.dll`): works from a service
/// (the IDN Node) where the policy allows services to (`SoftwareSASGeneration`, set by the
/// installer). Elsewhere an error: Linux takes Ctrl+Alt+Del as keys.
pub fn send_sas() -> io::Result<()> {
    #[cfg(windows)]
    {
        win::WinInjector::default().send_sas()
    }
    #[cfg(not(windows))]
    Err(io::Error::other("Ctrl+Alt+Del by SendSAS is Windows'"))
}

/// Injects input on this machine.
pub trait Injector: Send {
    /// Acts on one event; for [`Event::ClipboardGet`], returns the clipboard text.
    fn inject(&mut self, e: &Event) -> io::Result<Option<String>>;
}

/// The injector for this machine: `SendInput` on Windows, a uinput device on Linux, else
/// a recording backend. If the real one can't open (no `/dev/uinput`, no permission),
/// every event answers an error saying so (the viewer is told; it went nowhere without a
/// word before; bug round F, F3-10), and the rest of remote support still works.
pub fn injector() -> Box<dyn Injector> {
    // (Tests: recorded, whatever this machine has.)
    if idn_common::switches::on("IDN_INPUT_RECORD") {
        return Box::new(Recording::default());
    }
    #[cfg(windows)]
    {
        Box::new(win::WinInjector::default())
    }
    #[cfg(target_os = "linux")]
    {
        match uinput::Uinput::open() {
            Ok(u) => Box::new(u),
            Err(e) => {
                let why =
                    format!("input can't reach this machine's screen: no uinput device ({e})");
                idn_open_protocol::log::log(&format!("remote: {why}"));
                Box::new(Unavailable(why))
            }
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Box::new(Recording::default())
    }
}

/// An injector that can't inject: each event answers why.
#[cfg(target_os = "linux")]
struct Unavailable(String);

#[cfg(target_os = "linux")]
impl Injector for Unavailable {
    fn inject(&mut self, _: &Event) -> io::Result<Option<String>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, self.0.clone()))
    }
}

/// The Linux evdev keycode for a `KeyboardEvent.code`. The base block's codes equal the
/// PS/2 set-1 scan codes ([`scancode`]); the extended (grey) keys have their own.
#[cfg(target_os = "linux")]
pub fn linux_keycode(code: &str) -> Option<u16> {
    match scancode(code) {
        Some((c, false)) => Some(c),
        Some((_, true)) => Some(match code {
            "NumpadEnter" => 96,
            "ControlRight" => 97,
            "NumpadDivide" => 98,
            "AltRight" => 100,
            "Home" => 102,
            "ArrowUp" => 103,
            "PageUp" => 104,
            "ArrowLeft" => 105,
            "ArrowRight" => 106,
            "End" => 107,
            "ArrowDown" => 108,
            "PageDown" => 109,
            "Insert" => 110,
            "Delete" => 111,
            "MetaLeft" => 125,
            "MetaRight" => 126,
            "ContextMenu" => 127,
            _ => return None,
        }),
        None => None,
    }
}

/// Counts events without acting (tests; platforms with no real injector). Holds the last
/// clipboard text set, and returns it for a get.
#[derive(Default)]
pub struct Recording {
    pub events: std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    clipboard: String,
}

impl Injector for Recording {
    fn inject(&mut self, e: &Event) -> io::Result<Option<String>> {
        self.events.lock_ok().push(e.clone());
        match e {
            Event::ClipboardSet { text } => {
                self.clipboard = text.clone();
                Ok(None)
            }
            Event::ClipboardGet => Ok(Some(self.clipboard.clone())),
            _ => Ok(None),
        }
    }
}

/// The PS/2 set-1 scan code for a `KeyboardEvent.code`, and whether it is an extended
/// key (the `E0` prefix: the grey navigation keys, the right-hand modifiers, the keypad
/// Enter and slash). Unknown codes return `None`.
pub fn scancode(code: &str) -> Option<(u16, bool)> {
    let s = |c| Some((c, false));
    let e = |c| Some((c, true));
    match code {
        "Escape" => s(0x01),
        "Digit1" => s(0x02),
        "Digit2" => s(0x03),
        "Digit3" => s(0x04),
        "Digit4" => s(0x05),
        "Digit5" => s(0x06),
        "Digit6" => s(0x07),
        "Digit7" => s(0x08),
        "Digit8" => s(0x09),
        "Digit9" => s(0x0A),
        "Digit0" => s(0x0B),
        "Minus" => s(0x0C),
        "Equal" => s(0x0D),
        "Backspace" => s(0x0E),
        "Tab" => s(0x0F),
        "KeyQ" => s(0x10),
        "KeyW" => s(0x11),
        "KeyE" => s(0x12),
        "KeyR" => s(0x13),
        "KeyT" => s(0x14),
        "KeyY" => s(0x15),
        "KeyU" => s(0x16),
        "KeyI" => s(0x17),
        "KeyO" => s(0x18),
        "KeyP" => s(0x19),
        "BracketLeft" => s(0x1A),
        "BracketRight" => s(0x1B),
        "Enter" => s(0x1C),
        "ControlLeft" => s(0x1D),
        "KeyA" => s(0x1E),
        "KeyS" => s(0x1F),
        "KeyD" => s(0x20),
        "KeyF" => s(0x21),
        "KeyG" => s(0x22),
        "KeyH" => s(0x23),
        "KeyJ" => s(0x24),
        "KeyK" => s(0x25),
        "KeyL" => s(0x26),
        "Semicolon" => s(0x27),
        "Quote" => s(0x28),
        "Backquote" => s(0x29),
        "ShiftLeft" => s(0x2A),
        "Backslash" => s(0x2B),
        "KeyZ" => s(0x2C),
        "KeyX" => s(0x2D),
        "KeyC" => s(0x2E),
        "KeyV" => s(0x2F),
        "KeyB" => s(0x30),
        "KeyN" => s(0x31),
        "KeyM" => s(0x32),
        "Comma" => s(0x33),
        "Period" => s(0x34),
        "Slash" => s(0x35),
        "ShiftRight" => s(0x36),
        "NumpadMultiply" => s(0x37),
        "AltLeft" => s(0x38),
        "Space" => s(0x39),
        "CapsLock" => s(0x3A),
        "F1" => s(0x3B),
        "F2" => s(0x3C),
        "F3" => s(0x3D),
        "F4" => s(0x3E),
        "F5" => s(0x3F),
        "F6" => s(0x40),
        "F7" => s(0x41),
        "F8" => s(0x42),
        "F9" => s(0x43),
        "F10" => s(0x44),
        "NumLock" => s(0x45),
        "ScrollLock" => s(0x46),
        "Numpad7" => s(0x47),
        "Numpad8" => s(0x48),
        "Numpad9" => s(0x49),
        "NumpadSubtract" => s(0x4A),
        "Numpad4" => s(0x4B),
        "Numpad5" => s(0x4C),
        "Numpad6" => s(0x4D),
        "NumpadAdd" => s(0x4E),
        "Numpad1" => s(0x4F),
        "Numpad2" => s(0x50),
        "Numpad3" => s(0x51),
        "Numpad0" => s(0x52),
        "NumpadDecimal" => s(0x53),
        "F11" => s(0x57),
        "F12" => s(0x58),
        // Extended (E0) keys.
        "NumpadEnter" => e(0x1C),
        "ControlRight" => e(0x1D),
        "NumpadDivide" => e(0x35),
        "AltRight" => e(0x38),
        "Home" => e(0x47),
        "ArrowUp" => e(0x48),
        "PageUp" => e(0x49),
        "ArrowLeft" => e(0x4B),
        "ArrowRight" => e(0x4D),
        "End" => e(0x4F),
        "ArrowDown" => e(0x50),
        "PageDown" => e(0x51),
        "Insert" => e(0x52),
        "Delete" => e(0x53),
        "MetaLeft" => e(0x5B),
        "MetaRight" => e(0x5C),
        "ContextMenu" => e(0x5D),
        _ => None,
    }
}

#[cfg(windows)]
mod win {
    use super::{Event, Injector};
    use std::io;
    use windows_sys::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
    };

    /// Injects on Windows. `SendSAS` is loaded from sas.dll the first time Ctrl+Alt+Del
    /// is asked for (it isn't in the import tables).
    #[derive(Default)]
    pub struct WinInjector {
        send_sas: Option<unsafe extern "system" fn(i32)>,
        sas_tried: bool,
    }

    // SAFETY: the function pointer is a plain extern fn from a loaded DLL.
    unsafe impl Send for WinInjector {}

    fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key(scan: u16, extended: bool, up: bool) -> INPUT {
        let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> io::Result<()> {
        // SAFETY: `inputs` is a live slice of `inputs.len()` INPUTs, each of the size given.
        let n = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if n as usize == inputs.len() {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    impl Injector for WinInjector {
        fn inject(&mut self, e: &Event) -> io::Result<Option<String>> {
            match e {
                Event::Move { x, y } => {
                    // Absolute over the whole virtual desktop, 0..=65535.
                    send(&[mouse(
                        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        (*x).min(65535) as i32,
                        (*y).min(65535) as i32,
                        0,
                    )])?;
                }
                Event::Button { button, down } => {
                    let f = match (button.as_str(), down) {
                        ("left", true) => MOUSEEVENTF_LEFTDOWN,
                        ("left", false) => MOUSEEVENTF_LEFTUP,
                        ("right", true) => MOUSEEVENTF_RIGHTDOWN,
                        ("right", false) => MOUSEEVENTF_RIGHTUP,
                        ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
                        ("middle", false) => MOUSEEVENTF_MIDDLEUP,
                        // XBUTTON1 and XBUTTON2 in mouseData.
                        ("back" | "forward", d) => {
                            let x = if button == "back" { 1 } else { 2 };
                            let f = if *d {
                                MOUSEEVENTF_XDOWN
                            } else {
                                MOUSEEVENTF_XUP
                            };
                            send(&[mouse(f, 0, 0, x)])?;
                            return Ok(None);
                        }
                        _ => return Ok(None),
                    };
                    send(&[mouse(f, 0, 0, 0)])?;
                }
                Event::Wheel { dx, dy } => {
                    let mut v = Vec::new();
                    // Positive away from the user, as Windows' own (it was turned round:
                    // the viewers' wheel scrolled the wrong way on Windows).
                    // (Values from the viewer: bounded.)
                    if *dy != 0 {
                        v.push(mouse(MOUSEEVENTF_WHEEL, 0, 0, (*dy).clamp(-100, 100) * 120));
                    }
                    if *dx != 0 {
                        v.push(mouse(
                            MOUSEEVENTF_HWHEEL,
                            0,
                            0,
                            (*dx).clamp(-100, 100) * 120,
                        ));
                    }
                    if !v.is_empty() {
                        send(&v)?;
                    }
                }
                Event::Key { code, down } => {
                    if let Some((scan, ext)) = super::scancode(code) {
                        send(&[key(scan, ext, !*down)])?;
                    }
                }
                Event::Cad => self.send_sas()?,
                Event::ClipboardSet { text } => set_clipboard(text)?,
                Event::ClipboardGet => return Ok(Some(get_clipboard().unwrap_or_default())),
                // Relative: as a mouse moves (Windows' pointer speed applies, as to one).
                Event::Rel { dx, dy } => send(&[mouse(MOUSEEVENTF_MOVE, *dx, *dy, 0)])?,
                // A virtual gamepad needs a signed driver (R.25 stage 2: open).
                Event::Pad { .. } => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "gamepads on Windows need a driver this doesn't have yet",
                    ));
                }
            }
            Ok(None)
        }
    }

    impl WinInjector {
        pub(super) fn send_sas(&mut self) -> io::Result<()> {
            if !self.sas_tried {
                self.sas_tried = true;
                // SAFETY: NUL-terminated names; sas.dll is never freed, so the pointer stays good,
                // and SendSAS is `void SendSAS(BOOL)`, the type of `send_sas`.
                unsafe {
                    let lib = LoadLibraryA(c"sas.dll".as_ptr() as *const u8);
                    if !lib.is_null() {
                        let p = GetProcAddress(lib, c"SendSAS".as_ptr() as *const u8);
                        self.send_sas = p.map(|p| std::mem::transmute(p));
                    }
                }
            }
            match self.send_sas {
                // 0: the SAS goes to the whole desktop (needs the policy the installer sets).
                Some(f) => {
                    // SAFETY: `f` is SendSAS from sas.dll, loaded for good (above).
                    unsafe { f(0) };
                    Ok(())
                }
                None => Err(io::Error::other(
                    "SendSAS not available (sas.dll); the Ctrl+Alt+Del policy may be off",
                )),
            }
        }
    }

    pub(super) fn set_clipboard(text: &str) -> io::Result<()> {
        let wide = idn_common::win::wide(text);
        // SAFETY: the clipboard is opened before use and closed after; `h` holds `bytes` bytes, the
        // `wide.len()` u16s copied in; it is freed here unless the clipboard took it.
        unsafe {
            // Opened for a window of ours (a message-only one): opened for none,
            // EmptyClipboard leaves no owner and SetClipboardData then fails (bug round E,
            // E3-6).
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, HWND_MESSAGE,
            };
            let class = idn_common::win::wide("STATIC");
            let owner = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if OpenClipboard(owner) == 0 {
                let e = io::Error::last_os_error();
                if !owner.is_null() {
                    DestroyWindow(owner);
                }
                return Err(e);
            }
            EmptyClipboard();
            let bytes = wide.len() * 2;
            let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes);
            let mut taken = false;
            if !h.is_null() {
                // The clipboard owns the memory only once it takes it: else it is freed
                // here (it leaked).
                let p = GlobalLock(h) as *mut u16;
                taken = !p.is_null() && {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    GlobalUnlock(h);
                    !SetClipboardData(CF_UNICODETEXT as u32, h as HANDLE).is_null()
                };
                if !taken {
                    windows_sys::Win32::Foundation::GlobalFree(h);
                }
            }
            let e = (!taken).then(io::Error::last_os_error);
            CloseClipboard();
            if !owner.is_null() {
                DestroyWindow(owner);
            }
            // (Said when it didn't take: it reported success, and a paste did nothing.)
            if let Some(e) = e {
                return Err(io::Error::other(format!(
                    "the clipboard didn't take the text: {e}"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn get_clipboard() -> Option<String> {
        // SAFETY: the clipboard is open while its data is read; the text is read only within
        // the memory block's size (another program may leave out the closing NUL).
        unsafe {
            if OpenClipboard(0 as HWND) == 0 {
                return None;
            }
            let h = GetClipboardData(CF_UNICODETEXT as u32);
            let out = if !h.is_null() {
                let p = GlobalLock(h as HGLOBAL) as *const u16;
                if p.is_null() {
                    None
                } else {
                    let room = windows_sys::Win32::System::Memory::GlobalSize(h as HGLOBAL) / 2;
                    let all = std::slice::from_raw_parts(p, room);
                    let len = all.iter().position(|&c| c == 0).unwrap_or(room);
                    let s = String::from_utf16_lossy(&all[..len]);
                    GlobalUnlock(h as HGLOBAL);
                    Some(s)
                }
            } else {
                None
            };
            CloseClipboard();
            out
        }
    }
}

#[cfg(target_os = "linux")]
mod uinput {
    use super::{Event, Injector};
    use std::io;
    use std::os::fd::RawFd;

    // uinput ioctls (x86-64/most arches): _IO('U',1), _IOW('U',100..103, sizeof(int)).
    const UI_DEV_CREATE: libc::c_ulong = 0x5501;
    const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
    const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
    const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
    const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
    // Event types and codes (linux/input-event-codes.h).
    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_REL: u16 = 0x02;
    const EV_ABS: u16 = 0x03;
    const SYN_REPORT: u16 = 0;
    const ABS_X: u16 = 0x00;
    const ABS_Y: u16 = 0x01;
    const REL_HWHEEL: u16 = 0x06;
    const REL_WHEEL: u16 = 0x08;
    const BTN_LEFT: u16 = 0x110;
    const BTN_RIGHT: u16 = 0x111;
    const BTN_MIDDLE: u16 = 0x112;
    const BTN_SIDE: u16 = 0x113;
    const BTN_EXTRA: u16 = 0x114;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_LEFTALT: u16 = 56;
    const KEY_DELETE: u16 = 111;
    const ABS_CNT: usize = 64;
    /// The pointer is absolute over the captured picture, 0..=65535 each axis.
    const ABS_MAX: i32 = 65535;

    #[repr(C)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }

    #[repr(C)]
    struct UinputUserDev {
        name: [libc::c_char; 80],
        id: InputId,
        ff_effects_max: u32,
        absmax: [i32; ABS_CNT],
        absmin: [i32; ABS_CNT],
        absfuzz: [i32; ABS_CNT],
        absflat: [i32; ABS_CNT],
    }

    #[repr(C)]
    struct InputEvent {
        time: libc::timeval,
        type_: u16,
        code: u16,
        value: i32,
    }

    /// A uinput virtual mouse and keyboard (pointing absolutely), with a relative mouse
    /// and gamepads made when first used (R.25 stage 2: kept apart, as a desktop takes a
    /// device with absolute axes for a tablet); destroyed on drop.
    pub struct Uinput {
        fd: RawFd,
        rel: Option<Box<Uinput>>,
        pads: [Option<PadDevice>; 4],
    }

    /// A gamepad device, with the buttons and axes it last sent.
    type PadDevice = (Box<Uinput>, u32, [i16; 6]);

    /// What a uinput device has: its name, keys, relative axes, absolute axes and ranges.
    struct Spec<'a> {
        name: &'a str,
        keys: &'a [u16],
        rels: &'a [u16],
        abs: &'a [(u16, i32, i32)],
    }

    const BTN_SOUTH: u16 = 0x130;
    const BTN_EAST: u16 = 0x131;
    const BTN_NORTH: u16 = 0x133;
    const BTN_WEST: u16 = 0x134;
    const BTN_TL: u16 = 0x136;
    const BTN_TR: u16 = 0x137;
    const BTN_SELECT: u16 = 0x13a;
    const BTN_START: u16 = 0x13b;
    const BTN_MODE: u16 = 0x13c;
    const BTN_THUMBL: u16 = 0x13d;
    const BTN_THUMBR: u16 = 0x13e;
    const BTN_DPAD_UP: u16 = 0x220;
    const BTN_DPAD_DOWN: u16 = 0x221;
    const BTN_DPAD_LEFT: u16 = 0x222;
    const BTN_DPAD_RIGHT: u16 = 0x223;
    const REL_X: u16 = 0x00;
    const REL_Y: u16 = 0x01;
    const ABS_Z: u16 = 0x02;
    const ABS_RX: u16 = 0x03;
    const ABS_RY: u16 = 0x04;
    const ABS_RZ: u16 = 0x05;
    /// A pad's buttons in `remote::PAD_BUTTONS`' order.
    const PAD_KEYS: [u16; 15] = [
        BTN_SOUTH,
        BTN_EAST,
        BTN_WEST,
        BTN_NORTH,
        BTN_TL,
        BTN_TR,
        BTN_SELECT,
        BTN_START,
        BTN_MODE,
        BTN_THUMBL,
        BTN_THUMBR,
        BTN_DPAD_UP,
        BTN_DPAD_DOWN,
        BTN_DPAD_LEFT,
        BTN_DPAD_RIGHT,
    ];
    /// Its axes in `Event::Pad`'s order, with their ranges.
    const PAD_AXES: [(u16, i32, i32); 6] = [
        (ABS_X, -32768, 32767),
        (ABS_Y, -32768, 32767),
        (ABS_RX, -32768, 32767),
        (ABS_RY, -32768, 32767),
        (ABS_Z, 0, 32767),
        (ABS_RZ, 0, 32767),
    ];

    // SAFETY: the fd is owned and only used through &mut self or on drop.
    unsafe impl Send for Uinput {}

    fn set(fd: RawFd, req: libc::c_ulong, val: libc::c_int) -> io::Result<()> {
        if unsafe { libc::ioctl(fd, req, val) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Uinput {
        /// Opens `/dev/uinput` and creates the device (needs write access, i.e. root or a
        /// udev rule).
        pub fn open() -> io::Result<Uinput> {
            // Every keyboard key, and the mouse buttons; the wheel; the pointer absolute.
            let mut keys: Vec<u16> = (1..=255).collect();
            keys.extend([BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA]);
            Uinput::create(&Spec {
                name: "IDN Remote Support",
                keys: &keys,
                rels: &[REL_WHEEL, REL_HWHEEL],
                abs: &[(ABS_X, 0, ABS_MAX), (ABS_Y, 0, ABS_MAX)],
            })
        }

        /// A device as `spec` says.
        fn create(spec: &Spec) -> io::Result<Uinput> {
            let fd =
                // SAFETY: a NUL-terminated path; the descriptor is checked below.
                unsafe { libc::open(c"/dev/uinput".as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let u = Uinput {
                fd,
                rel: None,
                pads: Default::default(),
            };
            u.setup(spec)?;
            Ok(u)
        }

        fn setup(&self, spec: &Spec) -> io::Result<()> {
            let fd = self.fd;
            set(fd, UI_SET_EVBIT, EV_SYN as libc::c_int)?;
            if !spec.keys.is_empty() {
                set(fd, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
            }
            if !spec.rels.is_empty() {
                set(fd, UI_SET_EVBIT, EV_REL as libc::c_int)?;
            }
            if !spec.abs.is_empty() {
                set(fd, UI_SET_EVBIT, EV_ABS as libc::c_int)?;
            }
            for &key in spec.keys {
                set(fd, UI_SET_KEYBIT, key as libc::c_int)?;
            }
            for &rel in spec.rels {
                set(fd, UI_SET_RELBIT, rel as libc::c_int)?;
            }
            for &(abs, ..) in spec.abs {
                set(fd, UI_SET_ABSBIT, abs as libc::c_int)?;
            }
            // SAFETY: uinput_user_dev is integers and arrays; all zeros is valid.
            let mut dev: UinputUserDev = unsafe { std::mem::zeroed() };
            for (i, b) in spec.name.bytes().take(79).enumerate() {
                dev.name[i] = b as libc::c_char;
            }
            dev.id = InputId {
                bustype: 0x03, // USB
                vendor: 0x1d6b,
                product: 0x0001,
                version: 1,
            };
            for &(abs, min, max) in spec.abs {
                dev.absmin[abs as usize] = min;
                dev.absmax[abs as usize] = max;
            }
            // SAFETY: `fd` is ours; one whole `UinputUserDev`, repr(C) as the kernel's
            // uinput_user_dev.
            let n = unsafe {
                libc::write(
                    fd,
                    &dev as *const _ as *const libc::c_void,
                    std::mem::size_of::<UinputUserDev>(),
                )
            };
            if n != std::mem::size_of::<UinputUserDev>() as isize {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is ours, set up above; UI_DEV_CREATE takes no argument.
            if unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn emit(&self, type_: u16, code: u16, value: i32) -> io::Result<()> {
            let ev = InputEvent {
                time: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                type_,
                code,
                value,
            };
            // SAFETY: `self.fd` is ours while `self` lives; one whole repr(C) input_event.
            let n = unsafe {
                libc::write(
                    self.fd,
                    &ev as *const _ as *const libc::c_void,
                    std::mem::size_of::<InputEvent>(),
                )
            };
            if n != std::mem::size_of::<InputEvent>() as isize {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn syn(&self) -> io::Result<()> {
            self.emit(EV_SYN, SYN_REPORT, 0)
        }
    }

    impl Drop for Uinput {
        fn drop(&mut self) {
            // SAFETY: `self.fd` is ours, destroyed and closed once, here.
            unsafe {
                libc::ioctl(self.fd, UI_DEV_DESTROY);
                libc::close(self.fd);
            }
        }
    }

    impl Injector for Uinput {
        fn inject(&mut self, e: &Event) -> io::Result<Option<String>> {
            match e {
                Event::Move { x, y } => {
                    self.emit(EV_ABS, ABS_X, (*x).min(65535) as i32)?;
                    self.emit(EV_ABS, ABS_Y, (*y).min(65535) as i32)?;
                    self.syn()?;
                }
                Event::Button { button, down } => {
                    let code = match button.as_str() {
                        "left" => BTN_LEFT,
                        "right" => BTN_RIGHT,
                        "middle" => BTN_MIDDLE,
                        "back" => BTN_SIDE,
                        "forward" => BTN_EXTRA,
                        _ => return Ok(None),
                    };
                    self.emit(EV_KEY, code, *down as i32)?;
                    self.syn()?;
                }
                Event::Wheel { dx, dy } => {
                    if *dy != 0 {
                        self.emit(EV_REL, REL_WHEEL, *dy)?;
                    }
                    if *dx != 0 {
                        self.emit(EV_REL, REL_HWHEEL, *dx)?;
                    }
                    self.syn()?;
                }
                Event::Key { code, down } => {
                    if let Some(k) = super::linux_keycode(code) {
                        self.emit(EV_KEY, k, *down as i32)?;
                        self.syn()?;
                    }
                }
                Event::Cad => {
                    for (k, v) in [
                        (KEY_LEFTCTRL, 1),
                        (KEY_LEFTALT, 1),
                        (KEY_DELETE, 1),
                        (KEY_DELETE, 0),
                        (KEY_LEFTALT, 0),
                        (KEY_LEFTCTRL, 0),
                    ] {
                        self.emit(EV_KEY, k, v)?;
                    }
                    self.syn()?;
                }
                Event::Rel { dx, dy } => {
                    if self.rel.is_none() {
                        self.rel = Some(Box::new(Uinput::create(&Spec {
                            name: "IDN Remote Mouse",
                            keys: &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
                            rels: &[REL_X, REL_Y],
                            abs: &[],
                        })?));
                    }
                    let m = self.rel.as_ref().expect("made above");
                    if *dx != 0 {
                        m.emit(EV_REL, REL_X, *dx)?;
                    }
                    if *dy != 0 {
                        m.emit(EV_REL, REL_Y, *dy)?;
                    }
                    m.syn()?;
                }
                Event::Pad { id, buttons, axes } => {
                    let slot = &mut self.pads[(*id as usize).min(3)];
                    if slot.is_none() {
                        let name = format!("IDN Gamepad {}", (*id).min(3) + 1);
                        let pad = Uinput::create(&Spec {
                            name: &name,
                            keys: &PAD_KEYS,
                            rels: &[],
                            abs: &PAD_AXES,
                        })?;
                        *slot = Some((Box::new(pad), 0, [0, 0, 0, 0, 0, 0]));
                    }
                    let (pad, last_b, last_a) = slot.as_mut().expect("made above");
                    // Only what changed.
                    for (bit, &key) in PAD_KEYS.iter().enumerate() {
                        let (now, was) = (buttons >> bit & 1, *last_b >> bit & 1);
                        if now != was {
                            pad.emit(EV_KEY, key, now as i32)?;
                        }
                    }
                    for (k, &(abs, min, max)) in PAD_AXES.iter().enumerate() {
                        if axes[k] != last_a[k] {
                            pad.emit(EV_ABS, abs, (axes[k] as i32).clamp(min, max))?;
                        }
                    }
                    pad.syn()?;
                    (*last_b, *last_a) = (*buttons, *axes);
                }
                // The clipboard is the desktop's, not uinput's: through its own tools.
                Event::ClipboardSet { text } => crate::clipboard::set(text)?,
                Event::ClipboardGet => return Ok(crate::clipboard::get()),
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_as_json() {
        let e = Event::Key {
            code: "KeyA".into(),
            down: true,
        };
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&j).unwrap(), e);
        assert!(j.contains("\"t\":\"key\""));
    }

    #[test]
    fn known_scancodes() {
        assert_eq!(scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(scancode("Enter"), Some((0x1C, false)));
        assert_eq!(scancode("ArrowUp"), Some((0x48, true)));
        assert_eq!(scancode("NumpadEnter"), Some((0x1C, true)));
        assert_eq!(scancode("Nonsense"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_keycodes() {
        // The base block equals the PS/2 scan code; extended keys have their own.
        assert_eq!(super::linux_keycode("KeyA"), Some(30)); // KEY_A
        assert_eq!(super::linux_keycode("Enter"), Some(28)); // KEY_ENTER
        assert_eq!(super::linux_keycode("ArrowUp"), Some(103)); // KEY_UP
        assert_eq!(super::linux_keycode("ControlRight"), Some(97)); // KEY_RIGHTCTRL
        assert_eq!(super::linux_keycode("Delete"), Some(111)); // KEY_DELETE
        assert_eq!(super::linux_keycode("Nonsense"), None);
    }

    #[test]
    fn recording_keeps_and_answers_clipboard() {
        let mut inj = Recording::default();
        inj.inject(&Event::ClipboardSet { text: "hi".into() })
            .unwrap();
        assert_eq!(
            inj.inject(&Event::ClipboardGet).unwrap(),
            Some("hi".to_owned())
        );
        inj.inject(&Event::Move { x: 1, y: 2 }).unwrap();
        assert_eq!(inj.events.lock_ok().len(), 3);
    }
}
