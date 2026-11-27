//! The window a plugin's own interface goes in: a window of this process, either a
//! top-level one (titled, closable) or a child of a window the application passes (its
//! X11 window id or HWND: the plugin then shows inside the application, e.g. a mixing
//! desk's UI). X11 on Linux (also under Wayland, through XWayland), Win32 on Windows.

/// Where the window goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parent {
    /// A window of its own.
    None,
    /// Inside this X11 window.
    X11(u32),
    /// Inside this Win32 window.
    Hwnd(isize),
}

impl Parent {
    /// `x11:ID` or `hwnd:ID` (decimal or 0x hex); nothing: a window of its own.
    pub fn parse(s: Option<&str>) -> Result<Parent, String> {
        let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Parent::None);
        };
        let num = |v: &str| -> Option<u64> {
            match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                Some(h) => u64::from_str_radix(h, 16).ok(),
                None => v.parse().ok(),
            }
        };
        let bad = || format!("parent {s:?}: x11:WINDOW-ID or hwnd:HWND");
        match s.split_once(':') {
            Some(("x11", v)) => num(v)
                .and_then(|n| u32::try_from(n).ok())
                .map(Parent::X11)
                .ok_or_else(bad),
            Some(("hwnd", v)) => num(v).map(|n| Parent::Hwnd(n as isize)).ok_or_else(bad),
            _ => Err(bad()),
        }
    }
}

pub use imp::{HostWindow, PLATFORM, pump};

#[cfg(target_os = "linux")]
mod imp {
    use super::Parent;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    /// The plugin interfaces' window system here.
    pub const PLATFORM: &str = "x11";

    pub struct HostWindow {
        conn: RustConnection,
        win: u32,
        wm_delete: u32,
        closed: bool,
    }

    fn e(x: impl std::fmt::Display) -> String {
        format!("X11: {x}")
    }

    impl HostWindow {
        pub fn open(parent: Parent, title: &str, w: u32, h: u32) -> Result<HostWindow, String> {
            let (conn, screen_num) = x11rb::connect(None).map_err(|x| {
                format!("no X display ({x}): plugin windows need X11 or XWayland (DISPLAY)")
            })?;
            let screen = conn.setup().roots[screen_num].clone();
            let parent_id = match parent {
                Parent::X11(p) => p,
                Parent::Hwnd(_) => return Err("an HWND parent on Linux".into()),
                Parent::None => screen.root,
            };
            let win = conn.generate_id().map_err(e)?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                parent_id,
                0,
                0,
                w.clamp(1, 8192) as u16,
                h.clamp(1, 8192) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(screen.black_pixel)
                    .event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(e)?;
            let atom = |n: &[u8]| {
                conn.intern_atom(false, n)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.atom)
                    .unwrap_or(0)
            };
            let (protocols, wm_delete, net_name, utf8) = (
                atom(b"WM_PROTOCOLS"),
                atom(b"WM_DELETE_WINDOW"),
                atom(b"_NET_WM_NAME"),
                atom(b"UTF8_STRING"),
            );
            let _ = conn.change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                title.as_bytes(),
            );
            let _ = conn.change_property8(PropMode::REPLACE, win, net_name, utf8, title.as_bytes());
            let _ = conn.change_property32(
                PropMode::REPLACE,
                win,
                protocols,
                AtomEnum::ATOM,
                &[wm_delete],
            );
            conn.map_window(win).map_err(e)?;
            conn.flush().map_err(e)?;
            Ok(HostWindow {
                conn,
                win,
                wm_delete,
                closed: false,
            })
        }

        /// The window's id, for the plugin.
        pub fn handle(&self) -> u64 {
            self.win as u64
        }

        pub fn resize(&self, w: u32, h: u32) {
            let _ = self.conn.configure_window(
                self.win,
                &ConfigureWindowAux::new()
                    .width(w.clamp(1, 8192))
                    .height(h.clamp(1, 8192)),
            );
            let _ = self.conn.flush();
        }

        /// Whether the user closed it (or its parent went away).
        pub fn closed(&mut self) -> bool {
            while let Ok(Some(ev)) = self.conn.poll_for_event() {
                match ev {
                    Event::ClientMessage(m) if m.data.as_data32()[0] == self.wm_delete => {
                        self.closed = true
                    }
                    Event::DestroyNotify(d) if d.window == self.win => self.closed = true,
                    _ => {}
                }
            }
            self.closed
        }
    }

    impl Drop for HostWindow {
        fn drop(&mut self) {
            let _ = self.conn.destroy_window(self.win);
            let _ = self.conn.flush();
        }
    }

    /// Nothing to pump: each window reads its own connection.
    pub fn pump() {}
}

#[cfg(windows)]
mod imp {
    use super::Parent;
    use idn_common::win::wide;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    pub const PLATFORM: &str = "win32";

    thread_local! {
        static CLOSED: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
    }

    pub struct HostWindow {
        hwnd: HWND,
        child: bool,
    }

    unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if msg == WM_CLOSE {
            // Closed by the user: the chain takes the plugin's interface down, then the window.
            CLOSED.with(|c| c.borrow_mut().insert(hwnd as isize));
            return 0;
        }
        // SAFETY: the arguments Windows gave this window procedure, passed on unchanged.
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }

    const CLASS: &str = "IDNPluginWindow";

    fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        // SAFETY: the class name lives through the call (Windows copies it); the procedure is ours
        // and the rest of the struct may be zero.
        ONCE.call_once(|| unsafe {
            let class = wide(CLASS);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc_),
                hInstance: GetModuleHandleW(std::ptr::null()),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassExW(&wc);
        });
    }

    const TOP: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE;

    /// The outer size for a client area of `w` × `h`.
    fn outer(w: u32, h: u32, child: bool) -> (i32, i32) {
        if child {
            return (w as i32, h as i32);
        }
        let mut r = RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        // SAFETY: a RECT on the stack.
        unsafe { AdjustWindowRectEx(&mut r, TOP, 0, 0) };
        (r.right - r.left, r.bottom - r.top)
    }

    impl HostWindow {
        pub fn open(parent: Parent, title: &str, w: u32, h: u32) -> Result<HostWindow, String> {
            register();
            let (parent_hwnd, child) = match parent {
                Parent::Hwnd(p) => (p as HWND, true),
                Parent::X11(_) => return Err("an X11 parent on Windows".into()),
                Parent::None => (std::ptr::null_mut(), false),
            };
            let (ow, oh) = outer(w, h, child);
            let (class, t) = (wide(CLASS), wide(title));
            // SAFETY: `class` and `t` are NUL-ended and alive for the call; a parent HWND that is
            // not (or no longer) a window only makes the call fail.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    t.as_ptr(),
                    if child { WS_CHILD | WS_VISIBLE } else { TOP },
                    if child { 0 } else { CW_USEDEFAULT },
                    if child { 0 } else { CW_USEDEFAULT },
                    ow,
                    oh,
                    parent_hwnd,
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err("the window couldn't be made".into());
            }
            Ok(HostWindow { hwnd, child })
        }

        pub fn handle(&self) -> u64 {
            self.hwnd as u64
        }

        pub fn resize(&self, w: u32, h: u32) {
            let (ow, oh) = outer(w, h, self.child);
            // SAFETY: a window this thread made (HostWindow is not Send); a stale handle only
            // fails.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    ow,
                    oh,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }

        pub fn closed(&mut self) -> bool {
            let h = self.hwnd as isize;
            // SAFETY: IsWindow takes any value.
            CLOSED.with(|c| c.borrow().contains(&h)) || unsafe { IsWindow(self.hwnd) } == 0
        }
    }

    impl Drop for HostWindow {
        fn drop(&mut self) {
            let h = self.hwnd as isize;
            CLOSED.with(|c| c.borrow_mut().remove(&h));
            // SAFETY: as in `resize`: our window, on the thread that made it.
            unsafe { DestroyWindow(self.hwnd) };
        }
    }

    /// The thread's window messages (plugins' windows live on the chain's thread).
    pub fn pump() {
        // SAFETY: a zeroed MSG is a valid one to fill; this thread's own queue.
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::Parent;

    pub const PLATFORM: &str = "none";

    pub struct HostWindow;

    impl HostWindow {
        pub fn open(_: Parent, _: &str, _: u32, _: u32) -> Result<HostWindow, String> {
            Err("plugin windows aren't supported on this system".into())
        }
        pub fn handle(&self) -> u64 {
            0
        }
        pub fn resize(&self, _: u32, _: u32) {}
        pub fn closed(&mut self) -> bool {
            true
        }
    }

    pub fn pump() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents() {
        assert_eq!(Parent::parse(None), Ok(Parent::None));
        assert_eq!(
            Parent::parse(Some("x11:0x3a00007")),
            Ok(Parent::X11(0x3a00007))
        );
        assert_eq!(Parent::parse(Some("hwnd:132456")), Ok(Parent::Hwnd(132456)));
        assert!(Parent::parse(Some("cocoa:1")).is_err());
    }
}
