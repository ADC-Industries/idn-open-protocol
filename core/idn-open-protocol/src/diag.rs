//! A program's log file and crash reports (spec/extensions/idn-node.md §N.16): what the
//! program writes on standard error also goes, with the time, to `<app>.log` in
//! [`log_dir`] (kept under 4 MB, the one before as `<app>.log.1`); a panic writes a crash
//! report beside it (what, where, a backtrace, the build, the last lines logged). With the
//! machine's IDN Node, programs hand their crash reports to it and tell it where their log
//! is ([`crate::node::Node::connect`] does both), so the Site Manager can collect them.

use idn_common::sync::LockOk as _;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// How large a log file grows before it is started again (the old one kept as `.1`).
pub const MAX_LOG: u64 = 4 << 20;
/// Lines kept in memory for a crash report.
const RECENT: usize = 300;

struct State {
    app: String,
    dir: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    size: Mutex<u64>,
    recent: Mutex<VecDeque<String>>,
}

static STATE: OnceLock<State> = OnceLock::new();

/// Where programs keep their logs and crash reports: `logs` in the state directory
/// ([`crate::link::LinkConfig::default_dir`], else `%LOCALAPPDATA%\idn-open-protocol`),
/// else the temporary directory.
pub fn log_dir() -> PathBuf {
    idn_common::paths::state_dir_or_temp().join("logs")
}

/// Starts this program's log file and crash reports in [`log_dir`] (first call wins).
pub fn init(app: &str) {
    init_in(app, &log_dir());
}

/// As [`init`], in `dir` (the IDN Node keeps them in its own state directory).
pub fn init_in(app: &str, dir: &Path) {
    if STATE.get().is_some() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("{app}.log"));
    // A log over the limit from before starts again.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(&path, dir.join(format!("{app}.log.1")));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let size = file
        .as_ref()
        .and_then(|f| f.metadata().ok())
        .map_or(0, |m| m.len());
    let _ = STATE.set(State {
        app: app.to_owned(),
        dir: dir.to_owned(),
        file: Mutex::new(file),
        size: Mutex::new(size),
        recent: Mutex::new(VecDeque::new()),
    });
    write_line(&format!(
        "{app} {} starting (pid {})",
        crate::build_line(),
        std::process::id()
    ));
    tee_stderr();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Whoever read this program's output went away (`| head`, a closed terminal):
        // the next `println!` panics. Not a crash: no report and no message; the
        // program ends as it would have (lab 475-A: idn-audio filed a crash report).
        if output_closed(info) {
            // On the main thread the program was ending (with 101, a panic's status:
            // lab 514-A): ended as a pipe's writer is when its reader leaves (SIGPIPE's
            // 128 + 13 on Unix). Another thread's print ends only that thread, as before.
            if std::thread::current().name() == Some("main") {
                std::process::exit(if cfg!(unix) { 141 } else { 1 });
            }
            return;
        }
        let _ = crash_report(info);
        previous(info);
    }));
    // For trying the path to the Site Manager: a thread of its own panics a second after
    // start (the program goes on), with `IDN_CRASH_TEST` set.
    if std::env::var_os("IDN_CRASH_TEST").is_some_and(|v| !v.is_empty()) {
        let _ = std::thread::Builder::new()
            .name("idn-crash-test".into())
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(1));
                panic!("IDN_CRASH_TEST: a test crash, as asked");
            });
    }
}

/// Whether a panic is `print!`'s on a closed standard output or error.
fn output_closed(info: &std::panic::PanicHookInfo<'_>) -> bool {
    let p = info.payload();
    let text = p
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| p.downcast_ref::<&str>().copied())
        .unwrap_or("");
    is_output_closed(text)
}

/// [`output_closed`] by the panic's text: "failed printing to stdout: Broken pipe (os
/// error 32)" (Windows: os error 232 or 109).
fn is_output_closed(text: &str) -> bool {
    text.starts_with("failed printing to std")
        && ["os error 32)", "os error 232)", "os error 109)"]
            .iter()
            .any(|e| text.ends_with(e))
}

/// This program's name and log file, once [`init`] ran.
pub fn current() -> Option<(String, PathBuf)> {
    STATE
        .get()
        .map(|s| (s.app.clone(), s.dir.join(format!("{}.log", s.app))))
}

fn now_text() -> String {
    let t = crate::localtime::now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// One line into the log file (with the time) and the recent lines.
fn write_line(line: &str) {
    let Some(st) = STATE.get() else {
        return;
    };
    {
        let mut r = st.recent.lock_ok();
        r.push_back(line.to_owned());
        if r.len() > RECENT {
            r.pop_front();
        }
    }
    let stamped = format!("{} {line}\n", now_text());
    let mut f = st.file.lock_ok();
    let mut size = st.size.lock_ok();
    if *size + stamped.len() as u64 > MAX_LOG {
        let path = st.dir.join(format!("{}.log", st.app));
        *f = None;
        let _ = std::fs::rename(&path, st.dir.join(format!("{}.log.1", st.app)));
        *f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        *size = 0;
    }
    if let Some(file) = f.as_mut()
        && file.write_all(stamped.as_bytes()).is_ok()
    {
        *size += stamped.len() as u64;
    }
}

/// The lines logged most recently (up to 300).
pub fn recent_lines() -> Vec<String> {
    STATE
        .get()
        .map(|s| s.recent.lock_ok().iter().cloned().collect())
        .unwrap_or_default()
}

/// Writes a crash report for this panic: its path.
fn crash_report(info: &std::panic::PanicHookInfo<'_>) -> std::io::Result<PathBuf> {
    let st = STATE.get().ok_or(std::io::ErrorKind::NotFound)?;
    let t = crate::localtime::now();
    let name = format!(
        "crash-{}-{:04}{:02}{:02}-{:02}{:02}{:02}-{}.txt",
        st.app,
        t.year,
        t.month,
        t.day,
        t.hour,
        t.minute,
        t.second,
        std::process::id()
    );
    let what = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".into());
    let at = info.location().map_or("(unknown)".into(), |l| {
        format!("{}:{}:{}", l.file(), l.line(), l.column())
    });
    let thread = std::thread::current()
        .name()
        .unwrap_or("(unnamed)")
        .to_owned();
    let mut text = format!(
        "{} crashed\nwhen: {}\nbuild: {}\nversion: {}\nsystem: {} {}\npid: {}\nthread: {thread}\nwhat: {what}\nwhere: {at}\n\nbacktrace:\n{}\n\nthe last lines logged:\n",
        st.app,
        now_text(),
        crate::build_line(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        std::backtrace::Backtrace::force_capture(),
    );
    for l in recent_lines() {
        text.push_str(&l);
        text.push('\n');
    }
    let path = st.dir.join(&name);
    std::fs::write(&path, text)?;
    write_line(&format!("crashed: {what} at {at} (report {name})"));
    Ok(path)
}

/// Crash reports in `dir` not yet handed on (no `.reported` beside them), oldest first.
pub fn crash_reports_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".txt"))
                && !p.with_extension("reported").exists()
        })
        .collect();
    v.sort();
    v
}

/// This program's crash reports not yet handed on.
pub fn crash_reports() -> Vec<PathBuf> {
    STATE
        .get()
        .map(|s| crash_reports_in(&s.dir))
        .unwrap_or_default()
}

/// Marks a crash report as handed on (to the IDN Node).
pub fn mark_reported(report: &Path) {
    let _ = std::fs::write(report.with_extension("reported"), b"");
}

/// A log file opened to be read by another account's process: not through a link at
/// its end, without waiting (a named pipe hung the reads), and on Windows a disk file only
/// (not a pipe or device), opened at identification level (a server on the other end
/// can't act as the Node).
pub fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK | libc::O_NOFOLLOW);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        o.security_qos_flags(0x0001_0000); // SECURITY_IDENTIFICATION
    }
    let f = o.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the file's own handle.
        let kind =
            unsafe { windows_sys::Win32::Storage::FileSystem::GetFileType(f.as_raw_handle() as _) };
        if kind != windows_sys::Win32::Storage::FileSystem::FILE_TYPE_DISK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a file on a disk",
            ));
        }
    }
    Ok(f)
}

/// Whose a file is: an account by number (Unix), or by its SIDs (Windows: its token's
/// user and default owner; an elevated program's files are the Administrators').
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A Unix account's number.
    Uid(u32),
    /// A Windows account's SIDs (`S-1-5-…`).
    Sids(Vec<String>),
}

impl Owner {
    /// As kept in a text file: the number, or the SIDs comma-separated.
    pub fn to_text(&self) -> String {
        match self {
            Owner::Uid(u) => u.to_string(),
            Owner::Sids(s) => s.join(","),
        }
    }

    /// From [`Owner::to_text`]; `None` for anything else (empty included).
    pub fn parse(t: &str) -> Option<Owner> {
        let t = t.trim();
        if t.starts_with("S-") {
            let s: Vec<String> = t.split(',').map(|x| x.trim().to_owned()).collect();
            return s
                .iter()
                .all(|x| x.starts_with("S-") && x.len() > 3)
                .then_some(Owner::Sids(s));
        }
        t.parse().ok().map(Owner::Uid)
    }
}

/// Whether an opened file is `owner`'s (see [`tail_owned`]); never for the other
/// system's kind of owner.
pub fn owned_by(f: &std::fs::File, m: &std::fs::Metadata, owner: &Owner) -> bool {
    match owner {
        #[cfg(unix)]
        Owner::Uid(u) => {
            let _ = f;
            std::os::unix::fs::MetadataExt::uid(m) == *u
        }
        #[cfg(windows)]
        Owner::Sids(sids) => {
            let _ = m;
            file_owner_sid(f).is_some_and(|s| sids.iter().any(|o| o.eq_ignore_ascii_case(&s)))
        }
        _ => {
            let _ = (f, m);
            false
        }
    }
}

/// An opened file's owner, as a SID's text (`S-1-5-…`).
#[cfg(windows)]
pub fn file_owner_sid(f: &std::fs::File) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
    // SAFETY: the file's own handle; the owner points into the descriptor, which is freed
    // after the owner was turned into text; that text is freed too.
    unsafe {
        let mut sid: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if GetSecurityInfo(
            f.as_raw_handle() as _,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut sid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        ) != 0
        {
            return None;
        }
        // (No owner: a volume without security; its descriptor freed all the same.)
        if sid.is_null() {
            if !sd.is_null() {
                LocalFree(sd.cast());
            }
            return None;
        }
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(sid, &mut text) != 0 && !text.is_null();
        let out = ok.then(|| {
            let n = (0..).take_while(|&i| *text.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
        });
        if !text.is_null() {
            LocalFree(text.cast());
        }
        LocalFree(sd.cast());
        out
    }
}

/// The last `max` bytes of a file, as text (a log can be large).
pub fn tail(path: &Path, max: u64) -> std::io::Result<(String, bool)> {
    tail_owned(path, max, None)
}

/// [`tail`], the file (as opened, not as named) also owned by `owner` when given: an
/// account number on Unix; on Windows the account's SIDs, comma-separated (its user, and
/// its default owner: an elevated program's files are the Administrators'). A folder on
/// the way swapped for a link led elsewhere after the checks (bug run 4); on Windows any
/// program's log path was read with the Node's rights (bug run 2026-11-25).
pub fn tail_owned(path: &Path, max: u64, owner: Option<&Owner>) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    // A regular file only (`open_log`: not through a link, without waiting, on a disk):
    // a named pipe given as a log hung the Node's reports for good; a link led to the
    // Node's key file.
    let mut f = open_log(path)?;
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if let Some(o) = owner
        && !owned_by(&f, &m, o)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not the program's own file",
        ));
    }
    let len = m.len();
    let cut = len > max;
    if cut {
        f.seek(SeekFrom::Start(len - max))?;
    }
    let mut b = Vec::new();
    f.take(max).read_to_end(&mut b)?;
    let mut s = String::from_utf8_lossy(&b).into_owned();
    // From the first whole line.
    if cut && let Some(k) = s.find('\n') {
        s.drain(..=k);
    }
    Ok((s, cut))
}

/// Copies what is written on standard error into the log as well, line by line, and
/// still to the console. Programs print their messages there (`eprintln!`); with no
/// console (a service, a program started from the tray) the log is where they go.
fn tee_stderr() {
    #[cfg(unix)]
    {
        let mut fds = [0i32; 2];
        // SAFETY: plain descriptor calls on descriptors this process owns.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return;
            }
            let console = libc::dup(2);
            if console < 0 || libc::dup2(fds[1], 2) < 0 {
                return;
            }
            libc::close(fds[1]);
            let read = fds[0];
            TEE_READ.store(read, std::sync::atomic::Ordering::Relaxed);
            TEE_CONSOLE.store(console, std::sync::atomic::Ordering::Relaxed);
            libc::atexit(drain_at_exit);
            std::thread::Builder::new()
                .name("idn-log".into())
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    let mut line = Vec::new();
                    loop {
                        let n = libc::read(read, buf.as_mut_ptr().cast(), buf.len());
                        if n <= 0 {
                            break;
                        }
                        TEE_BUSY.store(true, std::sync::atomic::Ordering::SeqCst);
                        let chunk = &buf[..n as usize];
                        libc::write(console, chunk.as_ptr().cast(), chunk.len());
                        take_lines(&mut line, chunk);
                        TEE_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                })
                .ok();
        }
    }
    #[cfg(windows)]
    {
        win::tee();
    }
}

/// The pipe stderr goes through, its reading end, and the console's own stderr; whether
/// the thread copying it is handling what it read.
#[cfg(unix)]
static TEE_READ: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static TEE_CONSOLE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static TEE_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// At exit: stderr straight to the console again, and what is still in the pipe copied
/// first (at most 300 ms). A message just before `exit` (a port in use) was lost with
/// the process: the copying thread hadn't got to it (a second idn-mixer
/// ended without a word).
#[cfg(unix)]
extern "C" fn drain_at_exit() {
    let (read, console) = (
        TEE_READ.load(std::sync::atomic::Ordering::Relaxed),
        TEE_CONSOLE.load(std::sync::atomic::Ordering::Relaxed),
    );
    if read < 0 || console < 0 {
        return;
    }
    // SAFETY: plain descriptor calls on descriptors this process owns.
    unsafe {
        libc::dup2(console, 2);
        let end = std::time::Instant::now() + std::time::Duration::from_millis(300);
        // Idle twice running: a read that just returned may not have marked itself busy.
        let mut idle = 0;
        while idle < 2 && std::time::Instant::now() < end {
            let mut waiting: libc::c_int = 0;
            let empty = libc::ioctl(read, libc::FIONREAD, &mut waiting) != 0 || waiting == 0;
            idle = if empty && !TEE_BUSY.load(std::sync::atomic::Ordering::SeqCst) {
                idle + 1
            } else {
                0
            };
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// Whole lines out of what came, into the log.
fn take_lines(line: &mut Vec<u8>, chunk: &[u8]) {
    take_lines_with(line, chunk, write_line);
}

fn take_lines_with(line: &mut Vec<u8>, chunk: &[u8], write: fn(&str)) {
    for &b in chunk {
        if b == b'\n' {
            let s = String::from_utf8_lossy(line);
            write(s.trim_end_matches('\r'));
            line.clear();
        } else if line.len() < 16 << 10 {
            line.push(b);
        }
    }
}

/// One line into the log as the process ends, if no stopped thread was holding the file
/// (Windows stops the other threads first; waiting for one of them would hang the exit).
#[cfg(windows)]
fn write_line_at_exit(line: &str) {
    let Some(st) = STATE.get() else {
        return;
    };
    if let Some(mut f) = st.file.try_lock_ok()
        && let Some(file) = f.as_mut()
    {
        let _ = file.write_all(format!("{} {line}\n", now_text()).as_bytes());
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicPtr, Ordering};

    type Handle = *mut c_void;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(read: *mut Handle, write: *mut Handle, attrs: *mut c_void, size: u32) -> i32;
        fn GetStdHandle(which: u32) -> Handle;
        fn SetStdHandle(which: u32, h: Handle) -> i32;
        fn ReadFile(h: Handle, buf: *mut u8, len: u32, read: *mut u32, ov: *mut c_void) -> i32;
        fn WriteFile(
            h: Handle,
            buf: *const u8,
            len: u32,
            written: *mut u32,
            ov: *mut c_void,
        ) -> i32;
        fn PeekNamedPipe(
            h: Handle,
            buf: *mut u8,
            len: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
    }

    /// The pipe's reading end and the console's standard error, for `at_exit`.
    static READ: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static CONSOLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    /// Windows calls this as the process ends, after it has stopped the other threads,
    /// the copying one too: a message just before `exit` (a port in use) was still in the
    /// pipe and went with the process (seen on Linux; the same here). A TLS
    /// callback, because `process::exit` ends the process without the C runtime's
    /// `atexit` list.
    #[used]
    #[unsafe(link_section = ".CRT$XLD")]
    static AT_EXIT: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = at_exit;

    unsafe extern "system" fn at_exit(_: *mut c_void, reason: u32, _: *mut c_void) {
        const DLL_PROCESS_DETACH: u32 = 0;
        let read = READ.load(Ordering::Relaxed);
        if reason != DLL_PROCESS_DETACH || read.is_null() {
            return;
        }
        let console = CONSOLE.load(Ordering::Relaxed);
        let mut buf = [0u8; 4096];
        let mut line = Vec::new();
        // SAFETY: Win32 calls with handles this process owns; nothing else runs now.
        unsafe {
            loop {
                let mut waiting = 0u32;
                let peeked = PeekNamedPipe(
                    read,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut waiting,
                    std::ptr::null_mut(),
                );
                if peeked == 0 || waiting == 0 {
                    break;
                }
                let mut n = 0u32;
                let len = waiting.min(buf.len() as u32);
                if ReadFile(read, buf.as_mut_ptr(), len, &mut n, std::ptr::null_mut()) == 0
                    || n == 0
                {
                    break;
                }
                let chunk = &buf[..n as usize];
                if !console.is_null() {
                    let mut wr = 0u32;
                    WriteFile(console, chunk.as_ptr(), n, &mut wr, std::ptr::null_mut());
                }
                super::take_lines_with(&mut line, chunk, super::write_line_at_exit);
            }
        }
        if !line.is_empty() {
            super::write_line_at_exit(&String::from_utf8_lossy(&line));
        }
    }

    struct Raw(Handle);
    // SAFETY: Windows handles may be used from any thread.
    unsafe impl Send for Raw {}

    /// Standard error becomes a pipe whose other end copies to the console (if there is
    /// one) and the log. Rust's standard error asks for the handle on every write.
    pub fn tee() {
        // SAFETY: Win32 calls with handles this process owns.
        unsafe {
            let console = GetStdHandle(STD_ERROR_HANDLE);
            let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut r, &mut w, std::ptr::null_mut(), 0) == 0
                || SetStdHandle(STD_ERROR_HANDLE, w) == 0
            {
                return;
            }
            READ.store(r, Ordering::Relaxed);
            CONSOLE.store(console, Ordering::Relaxed);
            // The linker keeps a section only something refers to.
            std::ptr::read_volatile(&AT_EXIT);
            let (read, console) = (Raw(r), Raw(console));
            std::thread::Builder::new()
                .name("idn-log".into())
                .spawn(move || {
                    let (read, console) = (read, console);
                    let mut buf = [0u8; 4096];
                    let mut line = Vec::new();
                    loop {
                        let mut n = 0u32;
                        if ReadFile(
                            read.0,
                            buf.as_mut_ptr(),
                            buf.len() as u32,
                            &mut n,
                            std::ptr::null_mut(),
                        ) == 0
                            || n == 0
                        {
                            break;
                        }
                        let chunk = &buf[..n as usize];
                        if !console.0.is_null() {
                            let mut wr = 0u32;
                            WriteFile(console.0, chunk.as_ptr(), n, &mut wr, std::ptr::null_mut());
                        }
                        super::take_lines(&mut line, chunk);
                    }
                })
                .ok();
        }
    }
}

/// The space left for this account on the disk holding `path`, and the disk's size, bytes
/// (for recordings' "disk almost full", switcher §S.30).
pub fn free_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs is plain data (zeroed is valid); `c` is NUL-terminated.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let unit = st.f_frsize as u64;
        Some((st.f_bavail as u64 * unit, st.f_blocks as u64 * unit))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                dir: *const u16,
                avail: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let w: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: `w` is NUL-terminated; three u64s of our own to fill.
        (unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0)
            .then_some((avail, total))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn owners_kept_as_text() {
        use super::Owner;
        for o in [
            Owner::Uid(1000),
            Owner::Sids(vec!["S-1-5-21-1-2-3-1001".into(), "S-1-5-32-544".into()]),
        ] {
            assert_eq!(Owner::parse(&o.to_text()), Some(o));
        }
        assert_eq!(Owner::parse(""), None);
        assert_eq!(Owner::parse("S-1,bob"), None);
    }

    use super::*;

    #[test]
    fn a_closed_output_is_not_a_crash() {
        assert!(is_output_closed(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(is_output_closed(
            "failed printing to stderr: The pipe is being closed. (os error 232)"
        ));
        for other in [
            "failed printing to stdout: No space left on device (os error 28)",
            "index out of bounds: the len is 3 but the index is 32)",
            "",
        ] {
            assert!(!is_output_closed(other), "{other}");
        }
    }

    #[test]
    fn crash_reports_are_found_until_reported_and_tails_start_on_a_line() {
        let d = idn_common::testing::TempDir::new("diag");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("crash-a-20261029-101010-1.txt"), "x").unwrap();
        std::fs::write(d.join("crash-b-20261029-111010-2.txt"), "y").unwrap();
        std::fs::write(d.join("a.log"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(crash_reports_in(&d).len(), 2);
        mark_reported(&d.join("crash-a-20261029-101010-1.txt"));
        let left = crash_reports_in(&d);
        assert_eq!(left.len(), 1);
        assert!(left[0].ends_with("crash-b-20261029-111010-2.txt"));
        assert_eq!(
            tail(&d.join("a.log"), 1000).unwrap(),
            ("one\ntwo\nthree\n".into(), false)
        );
        // Cut: from the first whole line.
        assert_eq!(tail(&d.join("a.log"), 9).unwrap(), ("three\n".into(), true));
    }

    #[test]
    fn lines_are_taken_whole() {
        let mut line = Vec::new();
        take_lines(&mut line, b"half");
        assert_eq!(line, b"half");
        take_lines(&mut line, b" done\r\nnext");
        assert_eq!(line, b"next");
    }

    #[test]
    fn free_space_of_a_disk() {
        let (free, total) = free_space(&std::env::temp_dir()).expect("a disk");
        assert!(total > 0 && free <= total, "{free} of {total}");
        assert!(free_space(Path::new("/no/such/place/at/all")).is_none());
    }
}
