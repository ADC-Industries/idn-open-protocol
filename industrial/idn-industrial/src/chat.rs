//! The chat popup on the station (§I.11.5): a small always-on-top window the helper opens
//! when the Site Manager (or the application server) sends the operator a message, showing
//! the conversation and letting them answer. It runs on its own thread with a Win32
//! message loop; incoming messages are posted to it, and the operator's replies go back
//! through a callback (the helper sends them to the Node, which relays them to the Site
//! Manager).
//!
//! Off Windows this is a no-op: Linux/Wayland chat UI is an open item, and the helper only
//! runs on Windows.

#[allow(unused_imports)] // (its locks are in one platform's code)
use idn_common::sync::LockOk as _;
#[cfg(windows)]
pub use imp::ChatWindow;

#[cfg(not(windows))]
pub struct ChatWindow;

#[cfg(not(windows))]
impl ChatWindow {
    pub fn open(_reply: impl Fn(String) + Send + 'static) -> ChatWindow {
        ChatWindow
    }
    pub fn show(&self, _from: &str, _text: &str) {}
}

#[cfg(windows)]
mod imp {
    use idn_common::sync::LockOk as _;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
        ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, ES_WANTRETURN, GWLP_USERDATA, GetClientRect,
        GetMessageW, GetWindowLongPtrW, MSG, MoveWindow, PostMessageW, RegisterClassW, SW_SHOW,
        SendMessageW, SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage,
        WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CREATE, WM_SIZE, WNDCLASSW, WS_CHILD,
        WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
    };

    // A pushbutton, and the notification that it was clicked.
    const BS_PUSHBUTTON: u32 = 0x0000_0000;
    const BN_CLICKED: u16 = 0;
    const ID_SEND: usize = 101;
    // Our "a message arrived" signal to the window thread.
    const WM_CHAT_INCOMING: u32 = WM_APP + 1;
    // Window/edit messages windows-sys doesn't re-export here.
    const WM_GETTEXTLENGTH: u32 = 0x000E;
    const WM_GETTEXT: u32 = 0x000D;
    const EM_SETSEL: u32 = 0x00B1;
    const EM_REPLACESEL: u32 = 0x00C2;

    /// The child-window styles as a `WINDOW_STYLE` (the `ES_*` flags are `i32` in
    /// windows-sys, so widen them to the `u32` the flags are).
    fn edit_style(extra: u32) -> u32 {
        WS_CHILD | WS_VISIBLE | extra
    }

    use idn_common::win::wide;

    struct State {
        convo: HWND,
        input: HWND,
        send: HWND,
        queue: Arc<Mutex<VecDeque<String>>>,
        reply: Box<dyn Fn(String) + Send>,
    }

    /// A handle to the chat window running on its own thread.
    pub struct ChatWindow {
        hwnd: Arc<Mutex<Option<usize>>>,
        queue: Arc<Mutex<VecDeque<String>>>,
    }

    // SAFETY: the HWND is only touched across threads through PostMessage, which is
    // thread-safe; the queue is behind a mutex.
    unsafe impl Send for ChatWindow {}
    unsafe impl Sync for ChatWindow {}

    impl ChatWindow {
        /// Opens the window on a dedicated thread. `reply` is called with the operator's
        /// text each time they send.
        pub fn open(reply: impl Fn(String) + Send + 'static) -> ChatWindow {
            let queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
            let hwnd: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
            let (q, h) = (queue.clone(), hwnd.clone());
            std::thread::Builder::new()
                .name("idn-chat-window".into())
                .spawn(move || run(q, h, Box::new(reply)))
                .ok();
            ChatWindow { hwnd, queue }
        }

        /// Shows a message from `from` and brings the window up.
        pub fn show(&self, from: &str, text: &str) {
            self.queue.lock_ok().push_back(format!("{from}: {text}"));
            if let Some(h) = *self.hwnd.lock_ok() {
                // SAFETY: no pointers; a closed window's post just fails.
                unsafe {
                    PostMessageW(h as HWND, WM_CHAT_INCOMING, 0, 0);
                }
            }
        }
    }

    fn run(
        queue: Arc<Mutex<VecDeque<String>>>,
        hwnd_out: Arc<Mutex<Option<usize>>>,
        reply: Box<dyn Fn(String) + Send>,
    ) {
        // SAFETY: Win32 window creation on this thread, which runs its loop; the strings live
        // through the calls; `state` goes to the window and is kept (forgotten) for its life.
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class = wide("IdnChatWindow");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: (16isize) as _, // COLOR_BTNFACE+1
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            // The state is handed to WM_CREATE through the create params.
            let mut state = Box::new(State {
                convo: std::ptr::null_mut(),
                input: std::ptr::null_mut(),
                send: std::ptr::null_mut(),
                queue,
                reply,
            });
            let title = wide("Remote support — message");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST as WINDOW_EX_STYLE,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                420,
                340,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                state.as_mut() as *mut State as *mut _,
            );
            if hwnd.is_null() {
                return;
            }
            // Keep the state alive for the window's life.
            std::mem::forget(state);
            *hwnd_out.lock_ok() = Some(hwnd as usize);
            ShowWindow(hwnd, SW_SHOW);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: called by Windows for this class's windows: WM_CREATE's `lparam` is its
        // CREATESTRUCTW, whose create params are the `State` from `run`; after that GWLP_USERDATA
        // is that `State` (null before), alive for the window's life and used on this thread only.
        unsafe {
            match msg {
                WM_CREATE => {
                    let cs = lparam as *const CREATESTRUCTW;
                    let state = (*cs).lpCreateParams as *mut State;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                    create_controls(hwnd, &mut *state);
                    0
                }
                WM_SIZE => {
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
                    if !state.is_null() {
                        layout(hwnd, &*state);
                    }
                    0
                }
                WM_CHAT_INCOMING => {
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
                    if !state.is_null() {
                        drain(&*state);
                    }
                    0
                }
                WM_COMMAND => {
                    let id = wparam & 0xffff;
                    let note = ((wparam >> 16) & 0xffff) as u16;
                    if id == ID_SEND && note == BN_CLICKED {
                        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
                        if !state.is_null() {
                            send_reply(&*state);
                        }
                    }
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    unsafe fn create_controls(parent: HWND, state: &mut State) {
        // SAFETY: `parent` is the window being created; the strings live through the calls.
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let edit = wide("EDIT");
            let button = wide("BUTTON");
            let empty = wide("");
            let send_label = wide("Send");
            state.convo = CreateWindowExW(
                0,
                edit.as_ptr(),
                empty.as_ptr(),
                edit_style(
                    WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
                ),
                0,
                0,
                0,
                0,
                parent,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            state.input = CreateWindowExW(
                0,
                edit.as_ptr(),
                empty.as_ptr(),
                edit_style(ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_WANTRETURN as u32),
                0,
                0,
                0,
                0,
                parent,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            state.send = CreateWindowExW(
                0,
                button.as_ptr(),
                send_label.as_ptr(),
                edit_style(BS_PUSHBUTTON),
                0,
                0,
                0,
                0,
                parent,
                ID_SEND as _,
                hinst,
                std::ptr::null(),
            );
            layout(parent, state);
        }
    }

    unsafe fn layout(parent: HWND, state: &State) {
        // SAFETY: `parent` and the controls are this thread's windows; RECT is plain data.
        unsafe {
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(parent, &mut r);
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let margin = 8;
            let input_h = 52;
            let send_w = 80;
            let convo_h = (h - input_h - margin * 3).max(40);
            MoveWindow(state.convo, margin, margin, w - margin * 2, convo_h, 1);
            let input_y = margin * 2 + convo_h;
            MoveWindow(
                state.input,
                margin,
                input_y,
                w - send_w - margin * 3,
                input_h,
                1,
            );
            MoveWindow(state.send, w - send_w - margin, input_y, send_w, input_h, 1);
        }
    }

    /// Appends the queued incoming lines to the conversation box.
    unsafe fn drain(state: &State) {
        // SAFETY: `state.convo` is this thread's edit control.
        unsafe {
            let lines: Vec<String> = state.queue.lock_ok().drain(..).collect();
            for line in lines {
                append(state.convo, &line);
            }
        }
    }

    /// Reads the input box, sends it as a reply, and echoes it.
    unsafe fn send_reply(state: &State) {
        // SAFETY: `state`'s controls are this thread's windows; the wide string lives through the
        // call.
        unsafe {
            let text = window_text(state.input);
            if text.trim().is_empty() {
                return;
            }
            (state.reply)(text.clone());
            append(state.convo, &format!("You: {text}"));
            SetWindowTextW(state.input, wide("").as_ptr());
        }
    }

    /// Appends one line at the end of a multiline edit.
    unsafe fn append(edit: HWND, line: &str) {
        // SAFETY: `edit` is an edit control; `text` is NUL-terminated and lives through the call.
        unsafe {
            let end = SendMessageW(edit, WM_GETTEXTLENGTH, 0, 0) as WPARAM;
            SendMessageW(edit, EM_SETSEL, end, end as LPARAM);
            let text = wide(&format!("{line}\r\n"));
            SendMessageW(edit, EM_REPLACESEL, 0, text.as_ptr() as LPARAM);
        }
    }

    /// The text of a window/control.
    unsafe fn window_text(h: HWND) -> String {
        // SAFETY: `buf` holds `len + 1` u16s, the count given; WM_GETTEXT returns at most `len`.
        unsafe {
            let len = SendMessageW(h, WM_GETTEXTLENGTH, 0, 0) as usize;
            if len == 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len + 1];
            // WM_GETTEXT = 0x000D.
            let n = SendMessageW(
                h,
                WM_GETTEXT,
                (len + 1) as WPARAM,
                buf.as_mut_ptr() as LPARAM,
            ) as usize;
            String::from_utf16_lossy(&buf[..n])
        }
    }
}
