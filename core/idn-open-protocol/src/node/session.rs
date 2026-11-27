//! The Node's side in a person's desktop session (IDN Media as a server and libraries; the
//! UI is the host program's): what the IDN tray did there besides its icon, as an [`Agent`] a host
//! embeds (a tray item of its own) or `idn session` runs with no window.
//!
//! - This session's services (`session_services`: the IDN Microphone and Speaker, X32,
//!   programs' sound, screens and webcams, the switcher and desk when set to run here),
//!   each an `idn` process kept running, their lines to the Node's log (at most ten a
//!   second), how each is doing reported.
//! - The programs with sound and the displays, when the Node's panel asks; the encoders
//!   that work here, when the Node hasn't them.
//! - VB-CABLE's setup (Windows), when the panel asks for it.
//! - Notices: another computer asking to link (its code to compare); this computer's site
//!   to confirm, or the computer waiting for approval.
//! - One agent a session (Linux: a lock file; Windows: a named mutex): a second one waits
//!   while the first runs.
//! - Windows: only the session in front runs the services; Setup's "when you log on" Node
//!   started as this user when none answers.

use crate::node::Node;
use crate::node::bridges::ReportServices;
use crate::xml::Element;
use idn_common::cmd::RunWithin as _;
use idn_common::supervisor::Supervisor;
use idn_common::sync::LockOk as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the agent asks the Node how things are.
pub const POLL: Duration = Duration::from_secs(3);

/// A callback of the host's.
pub type Callback<T> = Arc<dyn Fn(T) + Send + Sync>;

/// How the host runs the agent ([`Agent::start`]).
#[derive(Clone)]
pub struct Config {
    /// The name it connects to the Node as.
    pub app: String,
    /// The Node's endpoint (a host's own Node, `idn_node::host`); default the machine's
    /// (`IDN_NODE`, else the service's, else this user's). The services and probes it runs
    /// use the same one (their `IDN_NODE`).
    pub endpoint: Option<String>,
    /// IDN's programs folder (where `idn` is), for a host not beside IDN's programs
    /// (default: this program's folder, or what the host set already).
    pub programs_dir: Option<PathBuf>,
    /// Whether this session runs the services now (default: Windows, the session in front;
    /// elsewhere always). Another session in front: what this one ran stops.
    pub runs_here: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Called while no Node answers (default: Windows with Setup's "when you log on",
    /// this user's Node started; elsewhere nothing).
    pub no_node: Arc<dyn Fn() + Send + Sync>,
    /// A notice for the person (title, text): another computer asking to link, this
    /// computer's site to confirm, waiting for approval (default: printed).
    pub notice: Callback<(String, String)>,
    /// Each status the Node gave (a host's icon or menu follows it; default nothing).
    pub status: Callback<Element>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            app: "IDN session".into(),
            endpoint: None,
            programs_dir: None,
            runs_here: Arc::new(runs_here),
            no_node: Arc::new(no_node),
            notice: Arc::new(|(title, text)| {
                idn_common::log::log_or_stderr(&format!("{title}: {text}"))
            }),
            status: Arc::new(|_| {}),
        }
    }
}

/// The agent running ([`Agent::start`]); stopped (its services with it) by
/// [`Agent::stop`] or when dropped.
pub struct Agent {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Agent {
    /// Starts it on a thread of its own.
    pub fn start(cfg: Config) -> std::io::Result<Agent> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("idn-session".into())
                .spawn(move || run(cfg, &stop))?
        };
        Ok(Agent {
            stop,
            thread: Some(thread),
        })
    }

    /// Stops it and the services it ran.
    pub fn stop(mut self) {
        self.end();
    }

    fn end(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // (Stopped from its own `notice` or `status` callback, on its thread: not waited
        // for, which would never end; it returns at its next look at `stop`.)
        if let Some(t) = self.thread.take()
            && t.thread().id() != std::thread::current().id()
        {
            let _ = t.join();
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.end();
    }
}

fn run(cfg: Config, stop: &AtomicBool) {
    if let Some(d) = &cfg.programs_dir {
        idn_common::supervisor::set_programs_dir(d.clone());
    }
    // One agent a session runs the services: wait while another does.
    let Some(_guard) = session_guard(stop) else {
        return;
    };
    // The Node the services' lines go to, while there is one.
    let log_node: Arc<Mutex<Option<Arc<Node>>>> = Arc::default();
    let mut sup = Supervisor::new(Supervisor::idn_next_to_me());
    if let Some(e) = &cfg.endpoint {
        sup = sup.env("IDN_NODE", e);
    }
    let mut sup = sup.log_to({
        let to_node = to_node(log_node.clone());
        move |args, l| {
            let line = format!("{}: {l}", args.join(" "));
            idn_common::log::log_or_stderr(&line);
            // Handed to one thread that passes them on (never waited for here: this is the
            // thread reading the service's output).
            let _ = to_node.try_send(line);
        }
    });
    let mut node: Option<Arc<Node>> = None;
    // The link code last told of: each told once.
    let mut told = String::new();
    // The Node's state last seen (`state`: "confirm", "pending", …): a notice when it
    // becomes one of those.
    let mut last_state: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        if node.is_none() {
            node = match &cfg.endpoint {
                Some(e) => Node::connect_to(e, &cfg.app),
                None => Node::connect(&cfg.app),
            }
            .ok()
            .map(Arc::new);
        }
        *log_node.lock_ok() = node.clone();
        let status = node.as_ref().map(|n| n.status());
        let here = (cfg.runs_here)();
        match (&node, &status) {
            (Some(n), Some(Ok(s))) => {
                (cfg.status)(s.clone());
                if here {
                    let services = crate::node::bridges::session_services(s);
                    let want: Vec<Vec<String>> = services.iter().map(|(_, a)| a.clone()).collect();
                    sup.supervise(&want);
                    let _ = sup.report(n, &services);
                    if s.child("audio_apps").and_then(|a| a.attr("wanted")) == Some("true") {
                        list_audio_apps(n.clone(), cfg.endpoint.clone());
                    }
                    if s.child("encoders").is_some_and(|e| e.attr("at").is_none()) {
                        report_encoders(n.clone(), cfg.endpoint.clone());
                    }
                } else {
                    sup.stop_all();
                }
                #[cfg(windows)]
                if let Some(r) = s.attr("vbcable_request").filter(|r| !r.is_empty()) {
                    static DONE: Mutex<String> = Mutex::new(String::new());
                    let mut done = DONE.lock_ok();
                    if *done != r {
                        *done = r.to_owned();
                        run_vbcable_setup();
                    }
                }
                // Another computer asks to link (§N.32): who and the code, to compare and
                // answer in the Node's panel.
                let asked = s
                    .child("pairs")
                    .and_then(|p| p.child("incoming"))
                    .and_then(|w| Some((w.attr("code")?.to_owned(), w.attr("by")?.to_owned())));
                match asked {
                    Some((code, by)) if code != told => {
                        let cut = 3.min(code.len());
                        (cfg.notice)((
                            format!("{by} asks to link to this computer"),
                            format!(
                                "Code {} {}. If {by} shows the same code, accept in the IDN Node panel (Linked computers); otherwise reject it. Linked computers may use this computer's screens with Remote control on.",
                                &code[..cut],
                                &code[cut..]
                            ),
                        ));
                        told = code;
                    }
                    Some(_) => {}
                    None => told.clear(),
                }
                let state = s.attr("state").unwrap_or("").to_owned();
                if last_state.as_deref() != Some(state.as_str()) {
                    if let Some(n) = state_notice(s) {
                        (cfg.notice)(n);
                    }
                    last_state = Some(state);
                }
            }
            _ => {
                // The Node gone (it may have restarted): connect again, keep what runs.
                node = None;
                (cfg.no_node)();
                if here {
                    sup.tend();
                } else {
                    sup.stop_all();
                }
            }
        }
        let until = Instant::now() + POLL;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    sup.stop_all();
}

/// The services' lines for the Node's log: at most ten a second, the rest counted and
/// said (a chatty service filled the Node's log; queued here, its output pipe filled and
/// a capture stalled).
fn to_node(node: Arc<Mutex<Option<Arc<Node>>>>) -> std::sync::mpsc::SyncSender<String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(64);
    let _ = std::thread::Builder::new()
        .name("idn-session-log".into())
        .spawn(move || {
            let (mut second, mut sent, mut left_out) = (Instant::now(), 0u32, 0u32);
            let send = |text: &str| {
                let n = node.lock_ok().clone();
                if let Some(n) = n {
                    let _ = n.request(Element::new("log").with_attr("text", text));
                }
            };
            while let Ok(line) = rx.recv() {
                if second.elapsed() >= Duration::from_secs(1) {
                    if left_out > 0 {
                        send(&format!(
                            "… {left_out} more lines from the services not sent here"
                        ));
                    }
                    (second, sent, left_out) = (Instant::now(), 0, 0);
                }
                if sent < 10 {
                    sent += 1;
                    send(&line);
                } else {
                    left_out += 1;
                }
            }
        });
    tx
}

/// A notice for the Node's state, when it is one the person acts on: this computer's site
/// to confirm (its code compared with the Site Manager's), or waiting for approval.
fn state_notice(s: &Element) -> Option<(String, String)> {
    let code = crate::trust::show_code(s.attr("code").unwrap_or(""));
    let name = s.attr("name").filter(|n| !n.is_empty());
    let node = name.map_or_else(|| "IDN Node".to_owned(), |n| format!("IDN Node {n}"));
    match s.attr("state")? {
        "confirm" => Some((
            "Confirm this computer's site".into(),
            format!(
                "{node}: approved, code {code}. If the Site Manager shows the same code, confirm it in the IDN Node panel; otherwise say it doesn't."
            ),
        )),
        "pending" => Some((
            "This computer waits for approval".into(),
            format!("{node}: waiting for approval, code {code}. Approve it on the Site Manager."),
        )),
        _ => None,
    }
}

/// The right to run this session's services, held while the returned guard lives: waits
/// (saying so once) while another agent holds it; `None` when stopped meanwhile.
fn session_guard(stop: &AtomicBool) -> Option<SessionGuard> {
    let mut said = false;
    loop {
        if let Some(g) = SessionGuard::take() {
            return Some(g);
        }
        if !std::mem::replace(&mut said, true) {
            idn_common::log::log_or_stderr(
                "idn-session: another program runs this session's services; waiting while it does",
            );
        }
        let until = Instant::now() + POLL;
        while Instant::now() < until {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// [`session_guard`]'s: Unix, the session lock file (`idn_common::supervisor::session_lock`,
/// shared with older programs); Windows, the named mutex `Local\IDN session` (owned by
/// the agent's thread, released when dropped or when the thread ends).
struct SessionGuard {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(windows)]
    mutex: windows_sys::Win32::Foundation::HANDLE,
}

impl SessionGuard {
    #[cfg(unix)]
    fn take() -> Option<SessionGuard> {
        idn_common::supervisor::session_lock().map(|f| SessionGuard { _file: f })
    }

    #[cfg(windows)]
    fn take() -> Option<SessionGuard> {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let name: Vec<u16> = "Local\\IDN session".encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated name alive for the call; the handle is closed unless kept.
        unsafe {
            let m = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if m.is_null() {
                // No mutex to be had: run rather than wait forever.
                return Some(SessionGuard { mutex: m });
            }
            match WaitForSingleObject(m, 0) {
                WAIT_OBJECT_0 | WAIT_ABANDONED => Some(SessionGuard { mutex: m }),
                _ => {
                    CloseHandle(m);
                    None
                }
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn take() -> Option<SessionGuard> {
        Some(SessionGuard {})
    }
}

#[cfg(windows)]
impl Drop for SessionGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;
        if !self.mutex.is_null() {
            // SAFETY: the mutex this thread owns (taken and dropped on the agent's thread).
            unsafe {
                ReleaseMutex(self.mutex);
                CloseHandle(self.mutex);
            }
        }
    }
}

/// `idn …` with no window (Windows), its output within 10 s; the agent's Node, when set.
fn idn_output(endpoint: Option<&str>, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = std::process::Command::new(Supervisor::idn_next_to_me());
    cmd.args(args);
    if let Some(e) = endpoint {
        cmd.env("IDN_NODE", e);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdin(std::process::Stdio::null())
        .output_within(Duration::from_secs(10))
        .ok()
}

/// The programs with sound open in this session and the displays (the panel's "Programs'
/// sound" and "Local video sources"), told to the Node; at most every 5 s.
fn list_audio_apps(node: Arc<Node>, endpoint: Option<String>) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock_ok();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        *last = Some(Instant::now());
    }
    std::thread::spawn(move || {
        let Some(out) = idn_output(endpoint.as_deref(), &["audio", "apps", "--tsv"]) else {
            return;
        };
        let mut e = Element::new("audio_apps");
        for line in String::from_utf8_lossy(&out.stdout).lines().take(200) {
            let mut f = line.split('\t');
            let (Some(exe), Some(name), playing) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            e = e.with_child(
                Element::new("app")
                    .with_attr("exe", exe)
                    .with_attr("name", name)
                    .with_attr(
                        "playing",
                        if playing == Some("1") {
                            "true"
                        } else {
                            "false"
                        },
                    ),
            );
        }
        let _ = node.request(e);
        let Some(out) = idn_output(endpoint.as_deref(), &["capture", "screens", "--tsv"]) else {
            return;
        };
        let mut e = Element::new("displays");
        for line in String::from_utf8_lossy(&out.stdout).lines().take(32) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 8 {
                continue;
            }
            e = e.with_child(
                Element::new("display")
                    .with_attr("id", f[0])
                    .with_attr("name", f[1])
                    .with_attr("width", f[2])
                    .with_attr("height", f[3])
                    .with_attr("hz", f[4])
                    .with_attr("primary", if f[5] == "1" { "true" } else { "false" })
                    .with_attr("position", f[6])
                    .with_attr("connector", f[7]),
            );
        }
        let _ = node.request(e);
    });
}

/// The encoders that work in this session (`idn capture encoders --tsv`), told to the
/// Node: probed once (each encoder opened), told again whenever the Node hasn't the list.
fn report_encoders(node: Arc<Node>, endpoint: Option<String>) {
    static LIST: Mutex<Option<Vec<(String, String, String)>>> = Mutex::new(None);
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        let cached = LIST.lock_ok().clone();
        let list = cached.unwrap_or_else(|| {
            let out = idn_output(endpoint.as_deref(), &["capture", "encoders", "--tsv"])
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
            let ran = out.is_some();
            let l: Vec<(String, String, String)> = out
                .unwrap_or_default()
                .lines()
                .take(32)
                .filter_map(|line| {
                    let f: Vec<&str> = line.split('\t').collect();
                    (f.len() >= 3).then(|| (f[0].to_owned(), f[1].to_owned(), f[2].to_owned()))
                })
                .collect();
            // Kept only from a probe that finished: a failed one is tried again next time.
            if ran {
                *LIST.lock_ok() = Some(l.clone());
            }
            l
        });
        let mut e = Element::new("encoders");
        for (p, c, by) in &list {
            e = e.with_child(
                Element::new("encoder")
                    .with_attr("processor", p)
                    .with_attr("codec", c)
                    .with_attr("by", by),
            );
        }
        let _ = node.request(e);
        BUSY.store(false, Ordering::Relaxed);
    });
}

/// Whether this session runs the services now: Windows, the session in use (connected,
/// at the console or by Remote Desktop), not one switched away from; elsewhere always.
pub fn runs_here() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::RemoteDesktop::{
            WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSActive, WTSConnectState,
            WTSFreeMemory, WTSQuerySessionInformationW,
        };
        let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: this session's connect state into a buffer the call allocates, read as
        // the WTS_CONNECTSTATE_CLASS it is and freed with WTSFreeMemory.
        unsafe {
            if WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER_HANDLE,
                WTS_CURRENT_SESSION,
                WTSConnectState,
                &mut buf,
                &mut len,
            ) == 0
                || buf.is_null()
            {
                return true;
            }
            let state = *(buf as *const i32);
            WTSFreeMemory(buf as *mut _);
            state == WTSActive
        }
    }
    #[cfg(not(windows))]
    true
}

/// No Node answers: Windows with Setup's "when you log on" (HKLM\Software\ADC
/// Industries\IDN, NodeMode = logon), this user's Node started (`idn-node run --user`, no
/// window; at most every 10 s); elsewhere nothing.
pub fn no_node() {
    #[cfg(windows)]
    if at_logon() {
        start_user_node();
    }
}

/// Setup's choice "when you log on": no service; the Node runs as each user.
#[cfg(windows)]
pub fn at_logon() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (key, value) = (wide("Software\\ADC Industries\\IDN"), wide("NodeMode"));
        let mut buf = [0u16; 16];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: NUL-terminated key and value names; `buf` holds the `len` bytes given.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        r == 0 && String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]) == "logon"
    })
}

/// This user's Node (`idn-node run --user`, no window), at most every 10 s.
#[cfg(windows)]
pub fn start_user_node() -> bool {
    use std::os::windows::process::CommandExt;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock_ok();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            return false;
        }
        *last = Some(Instant::now());
    }
    let Some(node) = idn_common::supervisor::programs_dir().map(|d| d.join("idn-node.exe")) else {
        return false;
    };
    std::process::Command::new(node)
        .args(["run", "--user"])
        .creation_flags(0x0800_0000)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// VB-Audio's VB-CABLE setup, from beside the programs, as an administrator (UAC asks):
/// silent but for Windows' question to trust VB-Audio's driver.
#[cfg(windows)]
fn run_vbcable_setup() {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Some(setup) = idn_common::supervisor::programs_dir()
        .map(|d| d.join("vbcable").join("VBCABLE_Setup_x64.exe"))
        .filter(|p| p.is_file())
    else {
        return;
    };
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file, args) = (w("runas"), w(&setup.to_string_lossy()), w("-i -h"));
    // SAFETY: NUL-terminated strings, alive for the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            args.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}
