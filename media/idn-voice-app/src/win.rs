//! Windows: the Voice page in WebView2, the talk key from a low-level keyboard hook (any
//! program in front), the tray icon, the start at log-in (HKCU Run).

use crate::settings::{self, Settings};
use crate::{BRIDGE, FromPage};
use idn_common::sync::LockOk as _;
use idn_common::win::wide;
use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Duration;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    PermissionRequestedEventHandler, ServerCertificateErrorDetectedEventHandler,
    WebMessageReceivedEventHandler, take_pwstr,
};
use windows::core::{HRESULT, Interface, PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyNameTextW, VK_ESCAPE};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "IDNVoiceApp";
const WM_TRAY: u32 = WM_APP + 1;
/// The talk key went down (WPARAM 1) or up (0).
const WM_TALK: u32 = WM_APP + 2;
/// A key was pressed to be the talk key (WPARAM: its code, 0 cancelled; LPARAM: its scan
/// code, + 0x100 if extended).
const WM_LEARNED: u32 = WM_APP + 3;
/// A certificate to ask about (LPARAM: a `Box<Cert>`).
const WM_CERT: u32 = WM_APP + 4;
/// Another start of the program: the window comes forward.
const WM_SHOW: u32 = WM_APP + 5;
/// A call rings.
const WM_RING: u32 = WM_APP + 6;
/// A setup page message (LPARAM: a `Box<FromPage>`), handled outside WebView2's event.
const WM_PAGE: u32 = WM_APP + 7;
const CMD_OPEN: usize = 1;
const CMD_SETTINGS: usize = 2;
const CMD_AUTOSTART: usize = 3;
const CMD_QUIT: usize = 4;
/// NIN_SELECT | NINF_KEY: the icon chosen with the keyboard (not in windows-sys).
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_NAME: &str = "IDN Voice";

/// The talk key (0: none), whether it reaches other programs too, whether the next key
/// pressed becomes it, whether it is held now; read by the keyboard hook.
static KEY: AtomicU32 = AtomicU32::new(0);
static PASS: AtomicBool = AtomicBool::new(false);
static LEARN: AtomicBool = AtomicBool::new(false);
static DOWN: AtomicBool = AtomicBool::new(false);
static MAIN: AtomicIsize = AtomicIsize::new(0);

struct Cert {
    host_port: String,
    fingerprint: String,
    changed: bool,
}

/// The app's state (the window's thread only).
struct App {
    hwnd: HWND,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    settings: Settings,
    /// The setup page is showing (its messages are taken; the talk key isn't sent).
    on_setup: bool,
    /// A certificate question is open or pending: a failed load then isn't "unreachable".
    asking: bool,
    /// The origin the WebView was started trusting as secure (an http server: the
    /// microphone needs a secure page); a different one needs a new start.
    insecure_origin: Option<String>,
    quitting: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// `f` on the app; not called while WebView2 runs code that may call back.
fn with<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

fn fill(field: &mut [u16], s: &str) {
    let mut n = 0;
    for u in s.encode_utf16().take(field.len() - 1) {
        field[n] = u;
        n += 1;
    }
    field[n] = 0;
}

fn message(hwnd: HWND, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let (t, c) = (wide(text), wide("IDN Voice"));
    // SAFETY: NUL-ended strings that outlive the call.
    unsafe { MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), style | MB_SETFOREGROUND) }
}

fn shell_open(url: &str) {
    let (verb, file) = (wide("open"), wide(url));
    // SAFETY: NUL-ended strings that outlive the call; null for the rest, as allowed.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

type CreateEnvironment = unsafe extern "system" fn(
    browser_folder: PCWSTR,
    user_data_folder: PCWSTR,
    options: *mut core::ffi::c_void,
    handler: *mut core::ffi::c_void,
) -> HRESULT;

/// `CreateCoreWebView2EnvironmentWithOptions` from WebView2Loader.dll, if it is there.
fn loader() -> Option<CreateEnvironment> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    // SAFETY: a NUL-ended name; the library is never freed, so the address stays good.
    let f = *F.get_or_init(|| unsafe {
        // From the program's own folder by its full path (as installed beside it): by name
        // alone, the search order could find a planted copy first.
        let path = std::env::current_exe()
            .ok()?
            .with_file_name("WebView2Loader.dll");
        let lib = LoadLibraryW(wide(&path.to_string_lossy()).as_ptr());
        if lib.is_null() {
            return None;
        }
        GetProcAddress(
            lib,
            c"CreateCoreWebView2EnvironmentWithOptions".as_ptr().cast(),
        )
        .map(|p| p as usize)
    });
    // SAFETY: the address of `CreateCoreWebView2EnvironmentWithOptions`, whose C signature
    // (stdcall: two PCWSTRs, two interface pointers, an HRESULT) `CreateEnvironment`
    // matches.
    f.map(|p| unsafe { std::mem::transmute::<usize, CreateEnvironment>(p) })
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let hidden = args.iter().any(|a| a == "--hidden");
    let restarted = args.iter().any(|a| a == "--restarted");
    let want_setup = args.iter().any(|a| a == "--setup");
    // SAFETY: plain calls at the start of the main thread.
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        windows_sys::Win32::System::Com::CoInitializeEx(
            std::ptr::null(),
            windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32,
        );
    }
    // One per log-on session: a second start brings the open one forward. Started again
    // by itself (a new server over http): waits for the one before to end.
    let name = wide("Local\\IDN Voice app");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: a NUL-ended name; the handle is kept for the process's life (the
        // "running" mark); the error read right after.
        let m = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            break;
        }
        // SAFETY: the existing mutex's handle, ours to close.
        unsafe { CloseHandle(m) };
        if !restarted || std::time::Instant::now() > end {
            let class = wide(CLASS);
            // SAFETY: a NUL-ended class name.
            let w = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
            if !w.is_null() {
                // SAFETY: plain calls; a window that is gone only makes PostMessageW fail.
                unsafe {
                    AllowSetForegroundWindow(ASFW_ANY);
                    PostMessageW(w, WM_SHOW, usize::from(want_setup), 0);
                }
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let settings = Settings::load();
    KEY.store(settings.key.unwrap_or(0), Ordering::Relaxed);
    PASS.store(settings.pass_key, Ordering::Relaxed);
    let Some(create) = loader() else {
        message(
            std::ptr::null_mut(),
            "IDN Voice can't open its window: WebView2Loader.dll is missing next to idn-voice.exe. Install IDN again, or open the Voice page in a browser.",
            MB_OK | MB_ICONWARNING,
        );
        return;
    };
    // SAFETY: on the main thread, which runs the window's message loop.
    let Some(hwnd) = (unsafe { make_window() }) else {
        return;
    };
    MAIN.store(hwnd as isize, Ordering::Relaxed);
    let show = !hidden || settings.server.is_empty() || want_setup;
    let insecure_origin = settings::origin(&settings.server).filter(|o| o.starts_with("http://"));
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            hwnd,
            controller: None,
            webview: None,
            settings,
            on_setup: false,
            asking: false,
            insecure_origin: insecure_origin.clone(),
            quitting: false,
        })
    });
    // SAFETY: the window made above, on this thread.
    unsafe {
        add_icon(hwnd);
        if show {
            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(hwnd);
        }
    }
    // The talk key from any program.
    // SAFETY: `keyboard` lives as long as the program; a low-level hook calls it on this
    // thread's message loop.
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    // Sound plays without a click first (the window may start hidden); nothing slows the
    // page down while it is hidden; an http server's page counts as secure (the
    // microphone needs that).
    let mut browser_args = "--autoplay-policy=no-user-gesture-required \
         --disable-background-timer-throttling --disable-renderer-backgrounding \
         --disable-backgrounding-occluded-windows"
        .to_owned();
    if let Some(o) = &insecure_origin {
        browser_args.push_str(&format!(" --unsafely-treat-insecure-origin-as-secure={o}"));
    }
    let options = CoreWebView2EnvironmentOptions::default();
    // SAFETY: options just made and not yet handed to WebView2, so nothing else reads them.
    unsafe { options.set_additional_browser_arguments(browser_args) };
    let options: ICoreWebView2EnvironmentOptions = options.into();
    // The WebView's profile (the Voice session's cookie, the microphone chosen).
    let data = std::env::var("LOCALAPPDATA")
        .map(|d| format!("{d}\\IDN\\voice-app"))
        .unwrap_or_default();
    let data_w = wide(&data);
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result, environment| {
            match (result, environment) {
                (Ok(()), Some(env)) => attach(hwnd, env, want_setup),
                _ => no_webview(hwnd),
            }
            Ok(())
        },
    ));
    // SAFETY: the loader's function (see `loader`); a NUL-ended folder (or null) that
    // outlives the call, and options and a handler WebView2 keeps its own references to.
    let r = unsafe {
        create(
            PCWSTR::null(),
            if data.is_empty() {
                PCWSTR::null()
            } else {
                PCWSTR(data_w.as_ptr())
            },
            options.as_raw(),
            handler.as_raw(),
        )
    };
    if r.is_err() {
        no_webview(hwnd);
    }
    // SAFETY: `MSG` is plain data, all zeros valid; GetMessageW fills it on the window's
    // thread.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: a message GetMessageW just filled.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // SAFETY: the hook set above, checked non-null.
    unsafe {
        if !hook.is_null() {
            UnhookWindowsHookEx(hook);
        }
    }
}

/// No WebView2 runtime: said, and the Voice page in the browser instead.
fn no_webview(hwnd: HWND) {
    let server = with(|a| a.settings.server.clone()).unwrap_or_default();
    message(
        hwnd,
        "IDN Voice can't open its window: the Microsoft Edge WebView2 Runtime isn't installed (it comes with Windows 11 and is a free download from Microsoft). The Voice page opens in the browser instead; the talk key works only in this app.",
        MB_OK | MB_ICONWARNING,
    );
    if !server.is_empty() {
        shell_open(&server);
    }
    // SAFETY: on the window's thread; a window already gone only makes the call fail.
    unsafe { DestroyWindow(hwnd) };
}

fn attach(hwnd: HWND, env: ICoreWebView2Environment, want_setup: bool) {
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |result, controller| {
            match (result, controller) {
                (Ok(()), Some(c)) => {
                    if setup(hwnd, &c, want_setup).is_err() {
                        no_webview(hwnd);
                    }
                }
                _ => no_webview(hwnd),
            }
            Ok(())
        },
    ));
    // SAFETY: our window, and a handler WebView2 keeps its own reference to.
    let r = unsafe {
        env.CreateCoreWebView2Controller(windows::Win32::Foundation::HWND(hwnd), &handler)
    };
    if r.is_err() {
        no_webview(hwnd);
    }
}

/// What the page said, and certificates to ask about, waiting for the window's thread:
/// `WM_PAGE` and `WM_CERT` only say there is one (a pointer in the message itself could be
/// posted by any program of the user, with any value).
static PAGE_QUEUE: std::sync::Mutex<std::collections::VecDeque<(usize, FromPage)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static CERT_QUEUE: std::sync::Mutex<std::collections::VecDeque<Cert>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// `m` for the window's thread (`wp`: 1 when the Voice page didn't load).
fn post_page(wp: usize, m: FromPage) {
    PAGE_QUEUE.lock_ok().push_back((wp, m));
    // (Queued and told on the window's thread only, so the newest is this one.)
    if !post(WM_PAGE, 0, 0) {
        PAGE_QUEUE.lock_ok().pop_back();
    }
}

/// Posts to the main window; false when there is none or the post failed.
fn post(msg: u32, wp: usize, lp: isize) -> bool {
    let h = MAIN.load(Ordering::Relaxed) as HWND;
    // SAFETY: posting to a window handle; a stale one only makes the call fail.
    !h.is_null() && unsafe { PostMessageW(h, msg, wp, lp) } != 0
}

fn setup(hwnd: HWND, c: &ICoreWebView2Controller, want_setup: bool) -> windows::core::Result<()> {
    // SAFETY: WebView2 calls on the window's thread, on objects WebView2 handed us; strings
    // passed are NUL-ended and outlive each call, and those it returns are freed by
    // `take_pwstr`.
    unsafe {
        let webview: ICoreWebView2 = c.CoreWebView2()?;
        let s = webview.Settings()?;
        s.SetAreDevToolsEnabled(false)?;
        s.SetIsStatusBarEnabled(false)?;
        s.SetIsZoomControlEnabled(true)?;
        webview.AddScriptToExecuteOnDocumentCreated(
            PCWSTR(wide(BRIDGE).as_ptr()),
            &AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(|_, _| Ok(()))),
        )?;
        let mut token = 0i64;
        // Links that open a new window: the browser.
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    args.SetHandled(true)?;
                    if uri.starts_with("https://") || uri.starts_with("http://") {
                        shell_open(&uri);
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // The microphone: allowed for the Voice server without asking each time.
        webview.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
                    args.PermissionKind(&mut kind)?;
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    let server = with(|a| a.settings.server.clone()).unwrap_or_default();
                    if kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE
                        && !server.is_empty()
                        && settings::same_origin(&uri, &server)
                    {
                        args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // A call rings; the setup page's choices.
        webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut text = PWSTR::null();
                    if args.TryGetWebMessageAsString(&mut text).is_ok() {
                        let text = take_pwstr(text);
                        let on_setup = with(|a| a.on_setup).unwrap_or(false);
                        match FromPage::parse(&text, on_setup) {
                            Some(FromPage::Ring) => {
                                post(WM_RING, 0, 0);
                            }
                            Some(m) => post_page(0, m),
                            None => {}
                        }
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // The Voice page didn't load: the setup page, saying so.
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut ok = windows::core::BOOL::default();
                    args.IsSuccess(&mut ok)?;
                    let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                    args.WebErrorStatus(&mut status)?;
                    let (on_setup, asking) = with(|a| (a.on_setup, a.asking)).unwrap_or((true, true));
                    if !ok.as_bool()
                        && !on_setup
                        && !asking
                        && status != COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED
                    {
                        let server = with(|a| a.settings.server.clone()).unwrap_or_default();
                        post_page(
                            1,
                            FromPage::Save {
                                server: format!("Couldn't reach the Voice server at {server}. Check the address, and that this computer is on the site's network."),
                                pass_key: false,
                                autostart: false,
                            },
                        );
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // The Site Manager's own certificate: trusted once the user compared it.
        if let Ok(w14) = webview.cast::<ICoreWebView2_14>() {
            w14.add_ServerCertificateErrorDetected(
                &ServerCertificateErrorDetectedEventHandler::create(Box::new(|_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut uri = PWSTR::null();
                    args.RequestUri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    let cert = args.ServerCertificate()?;
                    let mut pem = PWSTR::null();
                    cert.ToPemEncoding(&mut pem)?;
                    let pem = take_pwstr(pem);
                    let fp = settings::pem_der(&pem)
                        .map(|d| settings::fingerprint(&d))
                        .unwrap_or_default();
                    let hp = settings::host_port(&uri).unwrap_or_default();
                    let (server, trusted, asking) = with(|a| {
                        (
                            a.settings.server.clone(),
                            a.settings.trusted.get(&hp).cloned(),
                            a.asking,
                        )
                    })
                    .unwrap_or_default();
                    if !fp.is_empty() && trusted.as_deref() == Some(fp.as_str()) {
                        args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW)?;
                        return Ok(());
                    }
                    args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_CANCEL)?;
                    // Asked about the Voice server only, once at a time.
                    if !fp.is_empty() && !asking && settings::same_origin(&uri, &server) {
                        with(|a| a.asking = true);
                        let c = Cert {
                            host_port: hp,
                            fingerprint: fp,
                            changed: trusted.is_some(),
                        };
                        CERT_QUEUE.lock_ok().push_back(c);
                        if !post(WM_CERT, 0, 0) {
                            CERT_QUEUE.lock_ok().pop_back();
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }
        fit(hwnd, c);
        c.SetIsVisible(true)?;
        with(|a| {
            a.controller = Some(c.clone());
            a.webview = Some(webview.clone());
        });
        let server = with(|a| a.settings.server.clone()).unwrap_or_default();
        if server.is_empty() || want_setup {
            show_setup("");
        } else {
            go(&server);
        }
    }
    Ok(())
}

/// The Voice page.
fn go(url: &str) {
    let Some(w) = with(|a| {
        a.on_setup = false;
        a.webview.clone()
    })
    .flatten() else {
        return;
    };
    let u = wide(url);
    // SAFETY: a NUL-ended URL that outlives the call.
    let _ = unsafe { w.Navigate(PCWSTR(u.as_ptr())) };
}

/// The app's own page (the server, the talk key), `error` said above it.
fn show_setup(error: &str) {
    let Some((w, page)) = with(|a| {
        a.on_setup = true;
        a.webview
            .clone()
            .map(|w| (w, crate::setup_page(&a.settings, error)))
    })
    .flatten() else {
        return;
    };
    let p = wide(&page);
    // SAFETY: a NUL-ended page that outlives the call.
    let _ = unsafe { w.NavigateToString(PCWSTR(p.as_ptr())) };
    show_window();
}

fn run_script(js: &str) {
    if let Some(w) = with(|a| a.webview.clone()).flatten() {
        let s = wide(js);
        // SAFETY: a NUL-ended script that outlives the call.
        let _ = unsafe {
            w.ExecuteScript(
                PCWSTR(s.as_ptr()),
                &ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(()))),
            )
        };
    }
}

fn show_window() {
    let Some(hwnd) = with(|a| a.hwnd) else { return };
    // SAFETY: our window, on its thread.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd);
    }
}

/// A call: the window forward, and flashing until looked at (Windows may keep another
/// program in front).
fn ring() {
    show_window();
    let Some(hwnd) = with(|a| a.hwnd) else { return };
    let f = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: a FLASHWINFO filled in, with its size.
    unsafe { FlashWindowEx(&f) };
}

/// The setup page's Connect.
fn save(server: &str, pass_key: bool, autostart: bool) {
    let Some(url) = settings::server_url(server) else {
        show_setup(
            "That isn't an address: type the Site Manager's name or address, or the whole Voice link.",
        );
        return;
    };
    set_autostart(autostart);
    let restart = with(|a| {
        a.settings.server = url.clone();
        a.settings.pass_key = pass_key;
        a.settings.autostart = autostart;
        let _ = a.settings.save();
        let wanted = settings::origin(&url).filter(|o| o.starts_with("http://"));
        wanted.is_some() && wanted != a.insecure_origin
    })
    .unwrap_or(false);
    PASS.store(pass_key, Ordering::Relaxed);
    if restart {
        // A new server over http: the WebView starts again trusting it.
        if let Ok(me) = std::env::current_exe()
            && std::process::Command::new(me)
                .arg("--restarted")
                .spawn()
                .is_ok()
        {
            quit();
            return;
        }
    }
    go(&url);
}

fn set_autostart(on: bool) {
    let (key, name) = (wide(RUN_KEY), wide(RUN_NAME));
    // SAFETY: NUL-ended key, name and value; the value's size in bytes, its NUL included.
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let cmd = wide(&format!("\"{}\" --hidden", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                cmd.as_ptr().cast(),
                (cmd.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
        }
    }
}

fn quit() {
    let Some(hwnd) = with(|a| {
        a.quitting = true;
        a.hwnd
    }) else {
        return;
    };
    // SAFETY: our window, on its thread.
    unsafe { DestroyWindow(hwnd) };
}

/// The talk key's name as Windows says it ("F13", "Right Ctrl"), from its scan code.
fn key_name(vk: u32, scan: u32) -> String {
    let mut buf = [0u16; 64];
    let lp = (((scan & 0xFF) << 16) | (u32::from(scan & 0x100 != 0) << 24)) as i32;
    // SAFETY: the buffer and its length in u16s.
    let n = unsafe { GetKeyNameTextW(lp, buf.as_mut_ptr(), buf.len() as i32) };
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        format!("key {vk}")
    }
}

/// Every key pressed anywhere, for the talk key: a quick look, the work on the window's
/// thread.
unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, Windows passes a KBDLLHOOKSTRUCT in `lp`, valid for the
        // call.
        let k = unsafe { &*(lp as *const KBDLLHOOKSTRUCT) };
        let msg = wp as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if down && LEARN.swap(false, Ordering::Relaxed) {
            let vk = if k.vkCode == u32::from(VK_ESCAPE) {
                0
            } else {
                k.vkCode
            };
            let scan = k.scanCode
                | if k.flags & LLKHF_EXTENDED != 0 {
                    0x100
                } else {
                    0
                };
            post(WM_LEARNED, vk as usize, scan as isize);
            return 1;
        }
        let key = KEY.load(Ordering::Relaxed);
        if key != 0 && k.vkCode == key && (down || up) {
            // Held keys repeat their key-down: one talk.
            if down && !DOWN.swap(true, Ordering::Relaxed) {
                post(WM_TALK, 1, 0);
            } else if up && DOWN.swap(false, Ordering::Relaxed) {
                post(WM_TALK, 0, 0);
            }
            if !PASS.load(Ordering::Relaxed) {
                return 1;
            }
        }
    }
    // SAFETY: the hook's own arguments, passed on as Windows asks.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wp, lp) }
}

fn fit(hwnd: HWND, c: &ICoreWebView2Controller) {
    // SAFETY: `RECT` is plain integers, all zeros valid; our window and the WebView's
    // controller, on the window's thread.
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        GetClientRect(hwnd, &mut r);
        let _ = c.SetBounds(windows::Win32::Foundation::RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        });
    }
}

unsafe fn make_window() -> Option<HWND> {
    // SAFETY: a window class and window made on this thread; `WNDCLASSW` is valid all zeros
    // apart from what is set; the class name and title outlive the calls.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            // The program's icon (resource 1, assets/voice.rc).
            hIcon: LoadIconW(instance, 1 as _),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let title = wide("IDN Voice");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return None;
        }
        // 520 × 820 at the window's scale, within the screen, centred.
        let scale = GetDpiForWindow(hwnd).max(96) as i32;
        let (w, h) = (520 * scale / 96, 820 * scale / 96);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let (w, h) = (w.min(sw - 40), h.min(sh - 80));
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            SWP_NOZORDER,
        );
        Some(hwnd)
    }
}

fn taskbar_created() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    // SAFETY: a NUL-ended name that outlives the call.
    *ID.get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) })
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

unsafe fn add_icon(hwnd: HWND) {
    let mut d = icon_data(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    d.uCallbackMessage = WM_TRAY;
    // SAFETY: a NOTIFYICONDATAW filled in with its size, the tip NUL-ended by `fill`; the
    // icon is loaded here (not shared), ours to destroy.
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        d.hIcon = LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            1 as _,
            IMAGE_ICON,
            size,
            size,
            0,
        ) as HICON;
        fill(&mut d.szTip, "IDN Voice");
        Shell_NotifyIconW(NIM_ADD, &d);
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &d);
        if !d.hIcon.is_null() {
            DestroyIcon(d.hIcon);
        }
    }
}

unsafe fn notice(hwnd: HWND, title: &str, text: &str) {
    let mut d = icon_data(hwnd);
    d.uFlags = NIF_INFO;
    fill(&mut d.szInfoTitle, title);
    fill(&mut d.szInfo, text);
    d.dwInfoFlags = NIIF_INFO;
    // SAFETY: a NOTIFYICONDATAW filled in with its size, the texts NUL-ended by `fill`.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
}

unsafe fn show_menu(hwnd: HWND, x: i32, y: i32) {
    let autostart = with(|a| a.settings.autostart).unwrap_or(false);
    // SAFETY: a menu made and destroyed here, on the window's thread; item texts outlive
    // the calls.
    unsafe {
        let menu = CreatePopupMenu();
        let open = wide("Open IDN Voice");
        AppendMenuW(menu, MF_STRING, CMD_OPEN, open.as_ptr());
        SetMenuDefaultItem(menu, CMD_OPEN as u32, 0);
        let settings = wide("Server and talk key\u{2026}");
        AppendMenuW(menu, MF_STRING, CMD_SETTINGS, settings.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        let auto = wide("Start when I log in");
        let check = if autostart { MF_CHECKED } else { MF_UNCHECKED };
        AppendMenuW(menu, MF_STRING | check, CMD_AUTOSTART, auto.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        let q = wide("Quit");
        AppendMenuW(menu, MF_STRING, CMD_QUIT, q.as_ptr());
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
            x,
            y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: Windows calls this on the window's thread with its own handle; no message's
    // parameters are taken as pointers (pages and certificates come from the queues).
    unsafe {
        match msg {
            WM_SIZE => {
                // Not borrowed while WebView2 runs: it may send messages back here.
                if let Some(c) = with(|a| a.controller.clone()).flatten() {
                    fit(hwnd, &c);
                }
                0
            }
            WM_GETMINMAXINFO => {
                let scale = GetDpiForWindow(hwnd).max(96) as i32;
                let mm = &mut *(lp as *mut MINMAXINFO);
                mm.ptMinTrackSize.x = 360 * scale / 96;
                mm.ptMinTrackSize.y = 480 * scale / 96;
                0
            }
            // Closing the window keeps Voice running (calls, the talk key) in the tray.
            WM_CLOSE => {
                ShowWindow(hwnd, SW_HIDE);
                let first = with(|a| {
                    let first = !a.settings.told_tray;
                    if first {
                        a.settings.told_tray = true;
                        let _ = a.settings.save();
                    }
                    first
                })
                .unwrap_or(false);
                if first {
                    notice(
                        hwnd,
                        "IDN Voice is still running",
                        "Calls and your talk key still work. Open it or quit from this icon.",
                    );
                }
                0
            }
            WM_DESTROY => {
                Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
                if let Some(c) = with(|a| a.controller.take()).flatten() {
                    let _ = c.Close();
                }
                MAIN.store(0, Ordering::Relaxed);
                PostQuitMessage(0);
                0
            }
            WM_TRAY => {
                match (lp & 0xFFFF) as u32 {
                    NIN_SELECT | NIN_KEYSELECT => show_window(),
                    WM_CONTEXTMENU => {
                        let (x, y) = (
                            (wp & 0xFFFF) as i16 as i32,
                            ((wp >> 16) & 0xFFFF) as i16 as i32,
                        );
                        show_menu(hwnd, x, y);
                    }
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                match wp & 0xFFFF {
                    CMD_OPEN => show_window(),
                    CMD_SETTINGS => show_setup(""),
                    CMD_AUTOSTART => {
                        let on = with(|a| {
                            a.settings.autostart = !a.settings.autostart;
                            let _ = a.settings.save();
                            a.settings.autostart
                        })
                        .unwrap_or(false);
                        set_autostart(on);
                    }
                    CMD_QUIT => quit(),
                    _ => {}
                }
                0
            }
            WM_TALK => {
                if !with(|a| a.on_setup).unwrap_or(true) {
                    run_script(&crate::key_script(wp == 1));
                }
                0
            }
            WM_LEARNED => {
                let vk = wp as u32;
                let name = if vk == 0 {
                    with(|a| a.settings.key_name.clone()).unwrap_or_default()
                } else {
                    let name = key_name(vk, lp as u32);
                    KEY.store(vk, Ordering::Relaxed);
                    DOWN.store(false, Ordering::Relaxed);
                    with(|a| {
                        a.settings.key = Some(vk);
                        a.settings.key_name = name.clone();
                        let _ = a.settings.save();
                    });
                    name
                };
                run_script(&format!(
                    "window.setKey && window.setKey({})",
                    serde_json::json!(name)
                ));
                0
            }
            WM_PAGE => {
                // (The message only says there is one; nothing waiting: a stray one.)
                let Some((wp, m)) = PAGE_QUEUE.lock_ok().pop_front() else {
                    return 0;
                };
                match m {
                    // (WPARAM 1: the Voice page didn't load; `server` is what to say.)
                    FromPage::Save { server, .. } if wp == 1 => show_setup(&server),
                    FromPage::Save {
                        server,
                        pass_key,
                        autostart,
                    } => save(&server, pass_key, autostart),
                    FromPage::Learn => LEARN.store(true, Ordering::Relaxed),
                    FromPage::ClearKey => {
                        KEY.store(0, Ordering::Relaxed);
                        with(|a| {
                            a.settings.key = None;
                            a.settings.key_name.clear();
                            let _ = a.settings.save();
                        });
                    }
                    FromPage::Cancel => {
                        if let Some(s) =
                            with(|a| a.settings.server.clone()).filter(|s| !s.is_empty())
                        {
                            go(&s);
                        }
                    }
                    FromPage::Ring => ring(),
                }
                0
            }
            WM_RING => {
                ring();
                0
            }
            WM_CERT => {
                let Some(c) = CERT_QUEUE.lock_ok().pop_front() else {
                    return 0;
                };
                show_window();
                let q = crate::certificate_question(&c.host_port, &c.fingerprint, c.changed);
                let icon = if c.changed {
                    MB_ICONWARNING
                } else {
                    MB_ICONQUESTION
                };
                let yes = message(hwnd, &q, MB_YESNO | icon | MB_DEFBUTTON2) == IDYES;
                let server = with(|a| {
                    a.asking = false;
                    if yes {
                        a.settings
                            .trusted
                            .insert(c.host_port.clone(), c.fingerprint.clone());
                        let _ = a.settings.save();
                    }
                    a.settings.server.clone()
                })
                .unwrap_or_default();
                if yes {
                    go(&server);
                } else {
                    show_setup("Not connected: the server's certificate wasn't trusted.");
                }
                0
            }
            WM_SHOW => {
                if wp == 1 {
                    show_setup("");
                } else {
                    show_window();
                }
                0
            }
            m if m == taskbar_created() => {
                add_icon(hwnd);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
