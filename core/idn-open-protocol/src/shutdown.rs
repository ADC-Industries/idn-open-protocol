//! Clean exit on Ctrl+C and service stop.
//!
//! When an output stops, it tells the network, so other apps drop it from their source
//! lists at once. A process killed by Ctrl+C (SIGINT) or by a service manager (SIGTERM)
//! runs no destructors, though; nor does one ended by Windows' Ctrl+C, a closed console
//! window, a log-off or a shutdown. Its sources would then stay listed in every NDI app
//! for up to two minutes.
//!
//! Pick one of:
//!
//! - call [`install`] once at start-up, if your application doesn't handle these
//!   signals (Windows: console control events) itself;
//! - call [`run_hooks`] from your own signal handling, before exiting;
//! - drop all outputs before the process exits.
//!
//! ```no_run
//! // First thing in main():
//! idn_open_protocol::shutdown::install();
//! // … then create the publisher and outputs, and run.
//! ```

use idn_common::sync::LockOk as _;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

type Hook = std::sync::Arc<dyn Fn() + Send + Sync>;

fn hooks() -> &'static Mutex<Vec<(u64, Hook)>> {
    static H: OnceLock<Mutex<Vec<(u64, Hook)>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(unix)]
static PIPE_WR: AtomicI32 = AtomicI32::new(-1);

/// Removes its hook when dropped.
#[doc(hidden)]
pub struct Registration(u64);

impl Drop for Registration {
    fn drop(&mut self) {
        hooks().lock_ok().retain(|(id, _)| *id != self.0);
    }
}

/// Runs `hook` on SIGINT/SIGTERM (Windows: console control events and the session's end)
/// if [`install`] was called, until the returned registration is dropped.
#[doc(hidden)]
pub fn register(hook: impl Fn() + Send + Sync + 'static) -> Registration {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    hooks().lock_ok().push((id, std::sync::Arc::new(hook)));
    Registration(id)
}

/// Sends the stop notices of every live output now: mDNS goodbyes and departure
/// messages. For applications with their own signal handling: call it right before the
/// process exits. (To withdraw outputs while the application keeps running, drop them.)
pub fn run_hooks() {
    // Run them without holding the list: a hook that blocks (a stalled receiver) must not
    // stop outputs being created or dropped meanwhile.
    let list: Vec<Hook> = hooks().lock_ok().iter().map(|(_, h)| h.clone()).collect();
    for h in list {
        h();
    }
}

/// Runs the hooks on a thread of their own for at most [`HOOK_TIME`]: one that stalls or
/// panics doesn't keep the process alive.
#[cfg(any(unix, windows))]
fn run_hooks_bounded() {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("idn-shutdown-hooks".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(run_hooks);
            let _ = tx.send(());
        });
    let _ = rx.recv_timeout(HOOK_TIME);
}

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    // Only async-signal-safe work here: hand the signal number to the watcher thread.
    let fd = PIPE_WR.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        // SAFETY: write(2) is async-signal-safe; one byte from the stack.
        unsafe {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

/// Handles SIGINT (Ctrl+C) and SIGTERM (service stop): sends every output's stop
/// notices ([`run_hooks`]), then exits the way the signal would have (status 130 or
/// 143). Leaves alone any of the two signals your application already handles. Returns
/// the signals now handled. Calling it again does nothing. Unix (Linux, macOS).
#[cfg(unix)]
pub fn install() -> Vec<i32> {
    static INSTALLED: OnceLock<Vec<i32>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: `fds` has room for the two descriptors.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Vec::new();
            }
            // Not inherited by child processes; the handler's write never blocks.
            // SAFETY: descriptors just made, ours.
            unsafe {
                for fd in fds {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                let fl = libc::fcntl(fds[1], libc::F_GETFL);
                libc::fcntl(fds[1], libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
            let rd = fds[0];
            // The watcher first: if it can't start, the signals stay as they were rather
            // than being caught with nobody to act on them (R1-47).
            let watcher = std::thread::Builder::new()
                .name("idn-shutdown".into())
                .spawn(move || {
                    let mut b = 0u8;
                    loop {
                        let n =
                            // SAFETY: `rd` is the pipe's read end, this thread's; one byte into
                            // `b`.
                            unsafe { libc::read(rd, &mut b as *mut u8 as *mut libc::c_void, 1) };
                        if n == 1 {
                            break;
                        }
                        if n < 0
                            && std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        return;
                    }
                    // A second Ctrl+C or SIGTERM now ends the process at once, whatever
                    // the hooks are doing.
                    // SAFETY: default dispositions, no handler pointers.
                    unsafe {
                        libc::signal(libc::SIGINT, libc::SIG_DFL);
                        libc::signal(libc::SIGTERM, libc::SIG_DFL);
                    }
                    // The hooks get at most HOOK_TIME; one that stalls or panics doesn't
                    // keep the process alive.
                    run_hooks_bounded();
                    // Exit the way the signal would have (status 128 + n for the shell): to
                    // the process, not this thread (which may block it, and then the
                    // signal waited: a mixer with heavy plugins outlived its SIGTERM by
                    // over 10 s); and if that hasn't ended it within half a
                    // second, ended here with the same status.
                    // SAFETY: no pointers.
                    unsafe {
                        libc::kill(libc::getpid(), b as libc::c_int);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    // SAFETY: ends the process without unwinding or destructors, as meant.
                    unsafe {
                        libc::_exit(128 + b as libc::c_int);
                    }
                });
            if watcher.is_err() {
                // SAFETY: both ends ours, never handed out (the watcher didn't start).
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Vec::new();
            }
            PIPE_WR.store(fds[1], Ordering::Relaxed);
            let mut done = Vec::new();
            for sig in [libc::SIGINT, libc::SIGTERM] {
                // SAFETY: sigaction structs of our own (zeroed is valid); `on_signal` is an `extern
                // "C" fn(c_int)`, as a plain sa_handler (no SA_SIGINFO) is called.
                unsafe {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    libc::sigaction(sig, std::ptr::null(), &mut old);
                    if old.sa_sigaction != libc::SIG_DFL {
                        continue;
                    }
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                    libc::sigemptyset(&mut sa.sa_mask);
                    if libc::sigaction(sig, &sa, std::ptr::null_mut()) == 0 {
                        done.push(sig);
                    }
                }
            }
            done
        })
        .clone()
}

/// How long the stop notices may take after a signal before the process exits anyway.
#[cfg(any(unix, windows))]
const HOOK_TIME: std::time::Duration = std::time::Duration::from_secs(2);

/// Handles Windows' console control events (Ctrl+C, Ctrl+Break, the console window
/// closed, log-off, shutdown) and, through a hidden window, the end of the session (log-off
/// or shutdown, which a program that isn't a console one, or one using `user32.dll`, hears
/// only that way): sends every output's stop notices ([`run_hooks`], once, for at most
/// two seconds), then lets the event go on, so that the process ends as it would have
/// (Windows' own handler: exit status `STATUS_CONTROL_C_EXIT` on Ctrl+C). A second Ctrl+C
/// while the notices go ends the process at once. Handlers your application sets after
/// this one come first (one that handles the event keeps the notices from going); one set
/// before it comes after. Log-off is passed on untouched in session 0 (a service: another
/// person logging off doesn't stop it; a service's stop is its own to handle: call
/// [`run_hooks`] on it). Returns the console events now handled (`CTRL_*_EVENT`); calling
/// it again does nothing.
#[cfg(windows)]
pub fn install() -> Vec<i32> {
    static INSTALLED: OnceLock<Vec<i32>> = OnceLock::new();
    INSTALLED.get_or_init(win::install).clone()
}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, ENDSESSION_LOGOFF, GetMessageW, MSG,
        RegisterClassW, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
    };
    use windows_sys::core::BOOL;

    /// The hooks have run (or are running): they run once, and a second event passes on.
    static FIRED: AtomicBool = AtomicBool::new(false);

    /// Runs the hooks unless they ran already.
    fn fire() {
        if !FIRED.swap(true, Ordering::AcqRel) {
            super::run_hooks_bounded();
        }
    }

    /// Whether this process runs in session 0: a service's (no person logs on there).
    fn in_service_session() -> bool {
        let mut id = u32::MAX;
        // SAFETY: a pointer to a local u32.
        (unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) }) != 0 && id == 0
    }

    /// The console control handler, on a thread Windows starts for each event. FALSE: the
    /// event goes on to the next handler (Windows' own ends the process).
    unsafe extern "system" fn on_console(event: u32) -> BOOL {
        match event {
            CTRL_LOGOFF_EVENT if in_service_session() => {}
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT => fire(),
            _ => {}
        }
        0
    }

    /// The hidden window's messages: the session ends (`WM_ENDSESSION` with a true
    /// `wParam`): the notices go before it returns (Windows ends the process after that).
    unsafe extern "system" fn on_window(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                let logoff = (l as u32) & ENDSESSION_LOGOFF != 0;
                if w != 0 && !(logoff && in_service_session()) {
                    fire();
                }
                0
            }
            // SAFETY: the message as given.
            _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
        }
    }

    /// A hidden top-level window on a thread of its own, for `WM_ENDSESSION` (a
    /// message-only window doesn't get it). If it can't be made, the console handler is
    /// still there.
    fn end_session_window() {
        let _ = std::thread::Builder::new()
            .name("idn-shutdown-window".into())
            .spawn(|| {
                let class: Vec<u16> = "IDN shutdown\0".encode_utf16().collect();
                // SAFETY: the class name lives for the thread (the window's whole life); a
                // plain window procedure; the message loop on the window's own thread.
                unsafe {
                    let inst = GetModuleHandleW(std::ptr::null());
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(on_window),
                        hInstance: inst,
                        lpszClassName: class.as_ptr(),
                        ..Default::default()
                    };
                    if RegisterClassW(&wc) == 0 {
                        return;
                    }
                    let hwnd = CreateWindowExW(
                        0,
                        class.as_ptr(),
                        class.as_ptr(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        inst,
                        std::ptr::null(),
                    );
                    if hwnd.is_null() {
                        return;
                    }
                    let mut m = MSG::default();
                    while GetMessageW(&mut m, std::ptr::null_mut(), 0, 0) > 0 {
                        DispatchMessageW(&m);
                    }
                }
            });
    }

    pub(super) fn install() -> Vec<i32> {
        end_session_window();
        // SAFETY: a handler of the right signature, for the whole process life.
        if unsafe { SetConsoleCtrlHandler(Some(on_console), 1) } == 0 {
            return Vec::new();
        }
        [
            CTRL_C_EVENT,
            CTRL_BREAK_EVENT,
            CTRL_CLOSE_EVENT,
            CTRL_LOGOFF_EVENT,
            CTRL_SHUTDOWN_EVENT,
        ]
        .map(|e| e as i32)
        .to_vec()
    }
}

/// Neither Unix nor Windows: nothing is installed (the application calls [`run_hooks`]
/// before exiting). Returns an empty list.
#[cfg(not(any(unix, windows)))]
pub fn install() -> Vec<i32> {
    Vec::new()
}
