//! Running a process on the interactive desktop from a Windows service (§I.11.2), and
//! following the desktop as it switches (log on and off, lock, UAC and the secure
//! desktop). This is the reusable base other projects build a session helper on: a
//! service lives in session 0 and can neither capture nor drive the user's screen, so it
//! launches a helper into the active console session on the current input desktop and
//! relaunches it whenever that desktop changes.
//!
//! [`HelperSupervisor::start`] is the whole story for a caller: give it the helper's
//! command line and it keeps one running on the right desktop until it is stopped. The
//! building blocks ([`active_console_session`], [`input_desktop_name`],
//! [`launch_in_session`], [`attach_to_input_desktop`]) are public so a project can
//! assemble its own policy.
//!
//! What a helper then does on that desktop — capture it, inject input, show a chat
//! window — is the helper's business; `idn-helper-industrial` is a minimal example.
//! On platforms other than Windows the supervisor is a no-op (a service there runs in the
//! user's session already, or uses the desktop portal).

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
mod stub {
    /// A no-op on non-Windows: nothing to do, so starting one does nothing.
    pub struct HelperSupervisor;

    impl HelperSupervisor {
        /// Does nothing off Windows.
        pub fn start(_cmdline: String) -> HelperSupervisor {
            HelperSupervisor
        }
        pub fn stop(self) {}
    }
}

#[cfg(not(windows))]
pub use stub::*;

#[cfg(windows)]
mod imp {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_TIMEOUT};
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_QUERY, TokenPrimary,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, OpenProcess,
        OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, STARTUPINFOW,
        TerminateProcess, WaitForSingleObject,
    };

    fn last_error() -> io::Error {
        // SAFETY: no arguments.
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    use idn_common::win::wide;

    /// A closed-on-drop Windows handle.
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle we own, closed once, here.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The active console session, or `None` when no one is at the physical console
    /// (`0xFFFFFFFF`).
    pub fn active_console_session() -> Option<u32> {
        // SAFETY: no arguments.
        let s = unsafe { WTSGetActiveConsoleSessionId() };
        (s != 0xFFFF_FFFF).then_some(s)
    }

    /// The name of the desktop receiving input now — `Default` for the normal desktop,
    /// `Winlogon` on the secure desktop (the login screen, UAC, Ctrl+Alt+Del).
    pub fn input_desktop_name() -> Option<String> {
        // SAFETY: the desktop handle is checked, then closed here; `buf` holds the byte count
        // given.
        unsafe {
            // 0x0001 = DESKTOP_READOBJECTS: enough to read the name.
            let d = OpenInputDesktop(0, 0, 0x0001);
            if d.is_null() {
                return None;
            }
            let mut buf = [0u16; 256];
            let mut len = 0u32;
            let ok = GetUserObjectInformationW(
                d as HANDLE,
                UOI_NAME,
                buf.as_mut_ptr() as *mut _,
                (buf.len() * 2) as u32,
                &mut len,
            );
            CloseDesktop(d);
            if ok == 0 {
                return None;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..n]))
        }
    }

    /// Makes the current thread's desktop the input desktop — a helper calls this so its
    /// windows and input land where the user is looking. Call it again after a switch.
    pub fn attach_to_input_desktop() -> io::Result<()> {
        // SAFETY: the desktop handle is checked before use; it is left open as the thread's
        // desktop.
        unsafe {
            // GENERIC_ALL so a helper can both read and write (draw, inject).
            let d = OpenInputDesktop(0, 1, 0x1000_0000);
            if d.is_null() {
                return Err(last_error());
            }
            if SetThreadDesktop(d) == 0 {
                let e = last_error();
                CloseDesktop(d);
                return Err(e);
            }
            // The desktop stays current for the thread; the handle can close.
            Ok(())
        }
    }

    /// A token good for `session`: the logged-in user's if someone is logged in, else
    /// `winlogon`'s (SYSTEM, so the helper can reach the secure desktop and login screen).
    fn session_token(session: u32) -> io::Result<Handle> {
        let mut tok: HANDLE = std::ptr::null_mut();
        // SAFETY: `tok` is ours to fill.
        if unsafe { WTSQueryUserToken(session, &mut tok) } != 0 {
            return Ok(Handle(tok));
        }
        winlogon_token(session)
    }

    /// A primary token duplicated from `winlogon.exe` in `session` (SYSTEM).
    fn winlogon_token(session: u32) -> io::Result<Handle> {
        let pid = find_process("winlogon.exe", session)
            .ok_or_else(|| io::Error::other("no winlogon in the session"))?;
        // SAFETY: each handle is checked before use and owned by a `Handle` after; out-params of
        // our own.
        unsafe {
            let proc = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
            if proc.is_null() {
                return Err(last_error());
            }
            let proc = Handle(proc);
            let mut tok: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(
                proc.0,
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &mut tok,
            ) == 0
            {
                return Err(last_error());
            }
            let tok = Handle(tok);
            let mut dup: HANDLE = std::ptr::null_mut();
            if DuplicateTokenEx(
                tok.0,
                0,
                std::ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut dup,
            ) == 0
            {
                return Err(last_error());
            }
            Ok(Handle(dup))
        }
    }

    /// The id of a process named `name` (case-insensitive) in `session`, if any.
    fn find_process(name: &str, session: u32) -> Option<u32> {
        // SAFETY: the snapshot handle is checked and owned by `snap`; `e` has its dwSize set, as
        // Process32FirstW/NextW require.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() {
                return None;
            }
            let snap = Handle(snap);
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap.0, &mut e);
            while ok != 0 {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]);
                if exe.eq_ignore_ascii_case(name) {
                    let mut sid = 0u32;
                    if ProcessIdToSessionId(e.th32ProcessID, &mut sid) != 0 && sid == session {
                        return Some(e.th32ProcessID);
                    }
                }
                ok = Process32NextW(snap.0, &mut e);
            }
            None
        }
    }

    /// A launched helper process; killed and closed on drop.
    pub struct Child {
        process: HANDLE,
        thread: HANDLE,
    }

    // SAFETY: the handles are owned and only used here; Windows handles are movable.
    unsafe impl Send for Child {}

    impl Child {
        /// Whether it is still running.
        pub fn running(&self) -> bool {
            // SAFETY: `process` is ours until `Drop`.
            unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
        }
        /// Its exit code once it has ended (`None` while it runs).
        pub fn exit_code(&self) -> Option<u32> {
            if self.running() {
                return None;
            }
            let mut c = 0u32;
            // SAFETY: `process` is ours until `Drop`; `c` is ours to fill.
            (unsafe {
                windows_sys::Win32::System::Threading::GetExitCodeProcess(self.process, &mut c)
            } != 0)
                .then_some(c)
        }
        /// Ends it.
        pub fn kill(&self) {
            // SAFETY: `process` is ours until `Drop`.
            unsafe {
                TerminateProcess(self.process, 1);
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            self.kill();
            // SAFETY: both handles ours, closed once, here.
            unsafe {
                CloseHandle(self.thread);
                CloseHandle(self.process);
            }
        }
    }

    /// Starts `cmdline` in `session` on `desktop` (a name like `Default` or `Winlogon`,
    /// under `winsta0`), as the session's user or SYSTEM. The service must hold
    /// `SE_INCREASE_QUOTA` and `SE_ASSIGNPRIMARYTOKEN` (a LocalSystem service does).
    pub fn launch_in_session(session: u32, cmdline: &str, desktop: &str) -> io::Result<Child> {
        let token = session_token(session)?;
        let mut cmd = wide(cmdline);
        let mut desk = wide(&format!("winsta0\\{desktop}"));
        // SAFETY: `token` is a live primary token; `cmd` and `desk` are NUL-terminated and
        // writable, alive through the call; `si` and `pi` are ours.
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desk.as_mut_ptr();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            // The user's own environment (their profile, TEMP, APPDATA): with the
            // service's, the helper wrote into SYSTEM's profile (deep bug run 2026-11-09).
            // None to be had (no profile loaded): the service's, as before.
            use windows_sys::Win32::System::Environment::{
                CreateEnvironmentBlock, DestroyEnvironmentBlock,
            };
            let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
            if CreateEnvironmentBlock(&mut env, token.0, 0) == 0 {
                env = std::ptr::null_mut();
            }
            let ok = CreateProcessAsUserW(
                token.0,
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                // (No console window: one stood on the station's screen, closing it
                // killed the helper and it came back; bug run B.)
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                env,
                std::ptr::null(),
                &si,
                &mut pi,
            );
            let err = (ok == 0).then(last_error);
            if !env.is_null() {
                DestroyEnvironmentBlock(env);
            }
            if let Some(e) = err {
                return Err(e);
            }
            Ok(Child {
                process: pi.hProcess,
                thread: pi.hThread,
            })
        }
    }

    /// Keeps a helper running on the interactive desktop. Start it once; it runs a
    /// background thread that relaunches the helper when the console session or the input
    /// desktop changes, or the helper exits, until [`HelperSupervisor::stop`].
    pub struct HelperSupervisor {
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl HelperSupervisor {
        pub fn start(cmdline: String) -> HelperSupervisor {
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let join = std::thread::Builder::new()
                .name("idn-session-helper".into())
                .spawn(move || supervise(&s, &cmdline))
                .ok();
            HelperSupervisor { stop, join }
        }

        pub fn stop(mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    impl Drop for HelperSupervisor {
        /// Stops the supervising thread and waits for it, so the helper is ended (its
        /// `Child` dropped) before the service's process exits: without the wait the
        /// process ended first and the helper outlived the service, holding its program
        /// (wide bug round 2026-11-09, 1-7; the installer stopped it until now).
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    /// The exit codes a helper ends with when the session's input desktop changed under it
    /// (it can see that; this service, in session 0, can't): the desktop to start the next
    /// one on (bug run B: the supervisor never followed the login screen, UAC or a lock).
    pub const EXIT_TO_DEFAULT: u32 = 20;
    pub const EXIT_TO_WINLOGON: u32 = 21;

    /// Whether a user is logged on in `session` (its token can be had).
    fn user_logged_on(session: u32) -> bool {
        let mut tok: HANDLE = std::ptr::null_mut();
        // SAFETY: `tok` is ours to fill, closed below when given.
        let ok = unsafe { WTSQueryUserToken(session, &mut tok) } != 0;
        if ok {
            // SAFETY: a handle just given.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(tok) };
        }
        ok
    }

    /// A helper that ran at least this long before it ended is started again after the
    /// shortest wait.
    const RAN_LONG: Duration = Duration::from_secs(30);

    fn supervise(stop: &AtomicBool, cmdline: &str) {
        use std::time::Instant;
        // (session, desktop, launched with a user's token, the helper)
        let mut current: Option<(u32, String, bool, Child)> = None;
        let mut started = Instant::now();
        let mut desktop = String::from("Default");
        // A helper that ends by itself (or won't start) is started again after a wait
        // that grows while it keeps ending soon: 1 s up to a minute. The wait holds for
        // the same session, desktop and user; any change starts one at once.
        let mut waits =
            idn_common::backoff::Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let mut not_before: Option<(Instant, (u32, String, bool))> = None;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            let Some(session) = active_console_session() else {
                current = None; // dropping it kills the helper
                continue;
            };
            // The desktop as the last helper said when it left (its exit code), or as
            // this service can tell (rarely: it sees session 0's).
            if let Some((_, _, _, c)) = &current {
                match c.exit_code() {
                    Some(EXIT_TO_WINLOGON) => desktop = "Winlogon".into(),
                    Some(EXIT_TO_DEFAULT) => desktop = "Default".into(),
                    _ => {}
                }
            }
            if let Some(d) = input_desktop_name() {
                desktop = d;
            }
            let user = user_logged_on(session);
            // Again when a user logs on: a helper started with SYSTEM's token for the
            // login screen went on in the user's session (bug run B).
            let restart = match &current {
                Some((s, d, u, c)) => *s != session || *d != desktop || *u != user || !c.running(),
                None => true,
            };
            if restart {
                let here = (session, desktop.clone(), user);
                let ran = started.elapsed();
                if current.is_some() && ran >= RAN_LONG {
                    waits.reset();
                }
                // Ended by itself, where it was started.
                if let Some((s, d, u, c)) = &current
                    && (*s, d.as_str(), *u) == (session, desktop.as_str(), user)
                {
                    let wait = waits.next();
                    idn_open_protocol::log::log(&format!(
                        "session helper: ended (exit code {}) after {} s; started again in {} s",
                        c.exit_code()
                            .map_or("unknown".to_owned(), |c| c.to_string()),
                        ran.as_secs(),
                        wait.as_secs()
                    ));
                    not_before = Some((Instant::now() + wait, here.clone()));
                }
                current = None;
                if not_before
                    .as_ref()
                    .is_some_and(|(t, k)| *k == here && Instant::now() < *t)
                {
                    continue;
                }
                not_before = None;
                match launch_in_session(session, cmdline, &desktop) {
                    Ok(child) => {
                        current = Some((session, desktop.clone(), user, child));
                        started = Instant::now();
                    }
                    Err(e) => {
                        // A failing launch waits too, so that it doesn't spin.
                        let wait = waits.next();
                        idn_open_protocol::log::log(&format!(
                            "session helper: couldn't start on {desktop} (session {session}): {e}; trying again in {} s",
                            wait.as_secs()
                        ));
                        not_before = Some((Instant::now() + wait, here));
                    }
                }
            }
        }
    }
}
