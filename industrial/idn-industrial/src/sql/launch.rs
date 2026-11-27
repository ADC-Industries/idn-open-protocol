//! Starting the SQL worker as another identity on Windows (§I.10.6.3), from the service
//! (LocalSystem holds the rights this needs: SE_TCB for the user's token,
//! SE_ASSIGNPRIMARYTOKEN and SE_INCREASE_QUOTA for `CreateProcessAsUser`).
//!
//! - [`as_user`]: the token of the user logged in at the console (else of the first active
//!   session), the process in that user's session, with that user's environment.
//! - [`as_account`]: `LogonUser` with the account's name and password (an interactive
//!   logon, else a batch logon), the process in its own window station.
//!
//! Only the three pipe ends are inherited (a handle list), and the process has no window.

use super::worker::{Fail, Proc};
use std::ffi::c_void;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::Security::{
    LOGON32_LOGON_BATCH, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, LogonUserW,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, WaitForSingleObject,
};

use idn_common::win::wide;

fn last_error() -> std::io::Error {
    // SAFETY: no arguments.
    std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a handle we own, closed once, here.
            unsafe { CloseHandle(self.0) };
        }
    }
}

impl Handle {
    fn take(mut self) -> HANDLE {
        std::mem::replace(&mut self.0, std::ptr::null_mut())
    }
}

/// The worker process: ended and closed on drop.
struct EndProcess(HANDLE);

// SAFETY: a process handle may be used from any thread.
unsafe impl Send for EndProcess {}

impl Drop for EndProcess {
    fn drop(&mut self) {
        // SAFETY: the process handle is ours, ended and closed once, here.
        unsafe {
            TerminateProcess(self.0, 1);
            WaitForSingleObject(self.0, 5000);
            CloseHandle(self.0);
        }
    }
}

/// A pipe whose child end is inheritable and whose parent end isn't. `(read, write)`.
fn pipe(child_reads: bool) -> std::io::Result<(Handle, Handle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: out-params and `sa` of our own.
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(last_error());
    }
    let (r, w) = (Handle(r), Handle(w));
    let parent = if child_reads { &w } else { &r };
    // SAFETY: `parent` is a live pipe end of ours.
    if unsafe { SetHandleInformation(parent.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(last_error());
    }
    Ok((r, w))
}

/// The session of the user at the console, else the first active one with a user.
fn user_token() -> Result<Handle, Fail> {
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: no arguments; `tok` is ours to fill.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if console != u32::MAX && unsafe { WTSQueryUserToken(console, &mut tok) } != 0 {
        return Ok(Handle(tok));
    }
    // SAFETY: `info` holds `n` entries until WTSFreeMemory; `tok` is ours to fill.
    unsafe {
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut n = 0u32;
        if WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut n) != 0 {
            let list = std::slice::from_raw_parts(info, n as usize);
            let mut found = None;
            for s in list {
                if s.State == WTSActive && WTSQueryUserToken(s.SessionId, &mut tok) != 0 {
                    found = Some(Handle(tok));
                    break;
                }
            }
            WTSFreeMemory(info as *mut c_void);
            if let Some(h) = found {
                return Ok(h);
            }
        }
    }
    Err(Fail::Waiting(
        "no one is logged in to Windows; this connection runs as the logged-in user".into(),
    ))
}

/// Starts `exe sql-worker` with `token`. `desktop`: `winsta0\default` in the user's
/// session, or empty (a window station of the logon session's own).
fn launch(exe: &Path, token: &Handle, desktop: &str) -> std::io::Result<Proc> {
    let (in_r, in_w) = pipe(true)?;
    let (out_r, out_w) = pipe(false)?;
    let (err_r, err_w) = pipe(false)?;
    let mut cmd = wide(&format!("\"{}\" sql-worker", exe.display()));
    let mut desk = wide(desktop);
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: the attribute list lives in `attrs` (u64s, so pointer-aligned), sized as
    // the first call asked and deleted after use; the handles listed and the strings live through
    // CreateProcessAsUserW; the parent's pipe ends are taken from their `Handle`s once, into the
    // Files that own them.
    unsafe {
        if CreateEnvironmentBlock(&mut env, token.0, 0) == 0 {
            env = std::ptr::null_mut();
        }
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        // u64s: 8-aligned by the type, not by the allocator.
        let mut attrs = vec![0u64; size.div_ceil(8)];
        let list = attrs.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        let free_env = |env: *mut c_void| {
            if !env.is_null() {
                DestroyEnvironmentBlock(env);
            }
        };
        if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
            let e = last_error();
            free_env(env);
            return Err(e);
        }
        let inherit = [in_r.0, out_w.0, err_w.0];
        let ok = windows_sys::Win32::System::Threading::UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherit.as_ptr() as *const c_void,
            std::mem::size_of_val(&inherit),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if ok == 0 {
            let e = last_error();
            DeleteProcThreadAttributeList(list);
            free_env(env);
            return Err(e);
        }
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = in_r.0;
        si.StartupInfo.hStdOutput = out_w.0;
        si.StartupInfo.hStdError = err_w.0;
        si.StartupInfo.lpDesktop = desk.as_mut_ptr();
        si.lpAttributeList = list;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessAsUserW(
            token.0,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            env,
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        );
        let e = last_error();
        DeleteProcThreadAttributeList(list);
        free_env(env);
        if ok == 0 {
            return Err(e);
        }
        CloseHandle(pi.hThread);
        // The child's ends close here (in_r, out_w, err_w drop); the parent's are kept.
        drop((in_r, out_w, err_w));
        let input = std::fs::File::from_raw_handle(in_w.take());
        let output = std::fs::File::from_raw_handle(out_r.take());
        let errors = std::fs::File::from_raw_handle(err_r.take());
        Ok(Proc::new(
            Box::new(input),
            Box::new(output),
            Some(Box::new(errors)),
            Box::new(EndProcess(pi.hProcess)),
        ))
    }
}

/// The worker as the logged-in user.
pub fn as_user(exe: &Path) -> Result<Proc, Fail> {
    let token = user_token()?;
    launch(exe, &token, "winsta0\\default").map_err(|e| {
        Fail::Error(format!(
            "starting the SQL worker as the logged-in user: {e}"
        ))
    })
}

/// `DOMAIN\name` → (name, Some(domain)); `name@domain` → (UPN, None); `name` → (name, ".").
fn split_account(account: &str) -> (String, Option<String>) {
    if let Some((d, n)) = account.split_once('\\') {
        (n.to_owned(), Some(d.to_owned()))
    } else if account.contains('@') {
        (account.to_owned(), None)
    } else {
        (account.to_owned(), Some(".".to_owned()))
    }
}

/// The worker as a named Windows account.
pub fn as_account(exe: &Path, account: &str, password: &str) -> Result<Proc, Fail> {
    let (name, domain) = split_account(account);
    let (n, p) = (wide(&name), wide(password));
    let d = domain.as_deref().map(wide);
    let mut tok: HANDLE = std::ptr::null_mut();
    let mut err = None;
    // Interactive first (ordinary accounts have it on a workstation), then batch (service
    // accounts given "log on as a batch job").
    for kind in [LOGON32_LOGON_INTERACTIVE, LOGON32_LOGON_BATCH] {
        // SAFETY: NUL-terminated name, domain (or null) and password; `tok` is ours to fill.
        let ok = unsafe {
            LogonUserW(
                n.as_ptr(),
                d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                p.as_ptr(),
                kind,
                LOGON32_PROVIDER_DEFAULT,
                &mut tok,
            )
        };
        if ok != 0 {
            err = None;
            break;
        }
        err = Some(last_error());
    }
    if let Some(e) = err {
        return Err(Fail::Refused(format!("signing in as {account}: {e}")));
    }
    let token = Handle(tok);
    launch(exe, &token, "")
        .map_err(|e| Fail::Error(format!("starting the SQL worker as {account}: {e}")))
}
