//! Windows: the session agent (`idn-session`) started for the user at the screen by the
//! Node's service (SYSTEM), after an update the installer ended it for (deep bug run
//! 2026-11-09).

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Starts `idn-session.exe` (beside this program; the session agent) in the console
/// session as its user. Nobody signed in, or the Node not run as the service (no right to a
/// user's token): nothing. One agent runs a session (its named mutex), so one already
/// there stays alone.
pub fn start_for_console_user() {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("idn-session.exe")))
        .filter(|p| p.is_file())
    else {
        return;
    };
    let mut cmd = idn_common::win::wide(&format!("\"{}\"", exe.display()));
    let mut desk = idn_common::win::wide("winsta0\\default");
    // SAFETY: the session id and a token handle to fill (closed below); the environment
    // block made for that token and destroyed after; `cmd` and `desk` NUL-ended and
    // writable through the call; `si` and `pi` ours, their handles closed.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            return;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if WTSQueryUserToken(session, &mut token) == 0 {
            return;
        }
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        if CreateEnvironmentBlock(&mut env, token, 0) == 0 {
            env = std::ptr::null_mut();
        }
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desk.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessAsUserW(
            token,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT,
            env,
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if !env.is_null() {
            DestroyEnvironmentBlock(env);
        }
        CloseHandle(token);
        if ok != 0 {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            idn_open_protocol::log::log(
                "update: the session agent (idn-session) started again for the user at the screen",
            );
        }
    }
}
