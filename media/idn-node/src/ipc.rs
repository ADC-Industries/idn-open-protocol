//! The Node's endpoint and who may connect (spec/extensions/idn-node.md §N.3): a Unix
//! socket `0660` for the service account and group `idn`, or a named pipe for SYSTEM,
//! administrators and interactive users.

use crate::host::Place;
use crate::server::{NodeCore, Peer};
use std::io;
use std::sync::Arc;

#[cfg(unix)]
pub struct Listener(tokio::net::UnixListener);

#[cfg(unix)]
pub fn listen(place: &Place) -> io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::path::Path::new(&place.endpoint);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A socket file left by a Node that didn't stop cleanly is removed; one that answers
    // belongs to a running Node.
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another IDN Node is running on {}", place.endpoint),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let l = tokio::net::UnixListener::bind(path)?;
    if place.system {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        if let Some(gid) = idn_common::fsutil::group_id("idn") {
            // SAFETY: a valid NUL-terminated path; uid -1 leaves the owner as it is.
            let c = std::ffi::CString::new(place.endpoint.as_bytes())?;
            if unsafe { libc::chown(c.as_ptr(), u32::MAX, gid) } != 0 {
                idn_common::log::log_or_stderr(&format!(
                    "idn-node: can't give the socket to group idn: {}",
                    io::Error::last_os_error()
                ));
            }
        } else {
            idn_common::log::log_or_stderr(
                "idn-node: no group \"idn\": only this account and root can connect",
            );
        }
    } else {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(Listener(l))
}

#[cfg(unix)]
fn user_name(uid: u32) -> String {
    // SAFETY: passwd is integers and pointers; all zeros (nulls) is valid until getpwuid_r fills
    // it.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: buffers sized as given; `out` is null or points into `pw`.
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r == 0 && !out.is_null() {
        // SAFETY: pw_name points into `buf`, NUL-terminated.
        unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }
            .to_string_lossy()
            .into_owned()
    } else {
        format!("uid {uid}")
    }
}

/// A process's executable (§N.8.1); `None` when the system won't say.
#[cfg(target_os = "linux")]
fn exe_of(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn exe_of(_pid: i32) -> Option<String> {
    None
}

#[cfg(unix)]
pub async fn accept(l: Listener, core: Arc<NodeCore>) -> io::Result<()> {
    loop {
        // A failed accept (out of descriptors, say) is that connection's, not the Node's:
        // returning here stopped the whole Node.
        let s = match l.0.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                core.log(&format!("program connection not taken: {e}"));
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let peer = match s.peer_cred() {
            Ok(c) => Peer {
                pid: c.pid().map(|p| p as u32),
                user: user_name(c.uid()),
                exe: c.pid().and_then(exe_of),
                // root, or the account the Node runs as (it owns the Node's files
                // anyway: a Node run by hand, its user's own programs).
                // SAFETY: geteuid takes nothing and can't fail.
                admin: c.uid() == 0 || c.uid() == unsafe { libc::geteuid() },
                uid: Some(c.uid()),
                pipe: None,
            },
            Err(_) => Peer::default(),
        };
        let core = core.clone();
        tokio::spawn(async move {
            if let Err(e) = core.clone().serve(s, peer).await {
                core.log(&format!("program connection: {e}"));
            }
        });
    }
}

#[cfg(unix)]
pub fn cleanup(place: &Place) {
    let _ = std::fs::remove_file(&place.endpoint);
}

// ---------------------------------------------------------------------------------------
// Windows

#[cfg(windows)]
pub struct Listener {
    name: String,
    sddl: Vec<u16>,
    /// The first instance, created at start so no other process can take the name.
    first: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
}

/// Network logons denied; SYSTEM and administrators all rights; interactive users read
/// and write only (§N.3): `0x12019b` is read and write without FILE_CREATE_PIPE_INSTANCE
/// (FILE_APPEND_DATA's bit on a pipe), WRITE_DAC or WRITE_OWNER. With GENERIC_ALL a
/// standard user could make server instances of this pipe's name (the owner check still
/// passing: instances share the descriptor) and take connections meant for the Node, a
/// passphrase or the Site Manager's password with them (bug round 2026-11-09).
#[cfg(windows)]
const SDDL: &str = "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;IU)";

/// A Node not run as SYSTEM (the Node at log-on, or one run by hand): its own user (the
/// owner) all rights, SYSTEM too, nobody else. Under [`SDDL`] it couldn't make its own
/// next pipe instance (a standard user has no FILE_CREATE_PIPE_INSTANCE there, and an
/// administrators' entry is deny-only unelevated), so it served one program and no more;
/// and every other user logged on could drive this user's Node (bug run B).
#[cfg(windows)]
const USER_SDDL: &str = "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;OW)";

#[cfg(windows)]
pub fn listen(place: &Place) -> io::Result<Listener> {
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    // The service's pipe is SYSTEM's own (`O:SY`): programs check the pipe's owner, which
    // they may read (a standard user may not read the service's process token). Only
    // SYSTEM can make it so; anyone else (a Node run by hand) keeps the default owner.
    let mut l = Listener {
        name: place.endpoint.clone(),
        sddl: wide(&format!("O:SY{SDDL}")),
        first: None,
    };
    // The first instance: fails if another Node (or anyone) has the name.
    l.first = Some(match l.instance(true) {
        Ok(p) => p,
        Err(_) => {
            l.sddl = wide(USER_SDDL);
            l.instance(true)?
        }
    });
    Ok(l)
}

#[cfg(windows)]
impl Listener {
    fn instance(
        &self,
        first: bool,
    ) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string; `sd` receives a LocalAlloc'd descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                self.sddl.as_ptr(),
                1, // SDDL_REVISION_1
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        // SAFETY: `sa` and the descriptor it points at live through the call, which reads them.
        let r = unsafe {
            tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    &self.name,
                    &mut sa as *mut SECURITY_ATTRIBUTES as *mut std::ffi::c_void,
                )
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(sd as _) };
        r.map_err(|e| {
            if first {
                io::Error::new(
                    e.kind(),
                    format!("{}: {e} (another IDN Node running?)", self.name),
                )
            } else {
                e
            }
        })
    }
}

/// A process's executable (§N.8.1); `None` when the system won't say.
#[cfg(windows)]
fn exe_of(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    // SAFETY: the handle is checked and closed; the buffer's length is passed and updated.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Who is on the other end of pipe `pipe` (a server handle whose client has written):
/// its token's user and default owner SIDs, asked by impersonating it for a moment (the
/// connection's own account, not a process id's, which may have been reused).
#[cfg(windows)]
pub fn pipe_client_sids(pipe: usize) -> Option<Vec<String>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSID, RevertToSelf, TOKEN_QUERY, TokenOwner, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
    // SAFETY: the pipe handle is live (its connection is being served); the thread acts as
    // the client only between the impersonation and RevertToSelf, which always runs, with
    // nothing in between but opening its token (checked as the Node: `OpenAsSelf`); the
    // token information is read into our own 8-aligned buffer, whose first field is the
    // SID pointer into it; each handle and text is closed or freed.
    unsafe {
        if ImpersonateNamedPipeClient(pipe as HANDLE) == 0 {
            return None;
        }
        let mut tok: HANDLE = std::ptr::null_mut();
        let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut tok) != 0;
        if RevertToSelf() == 0 {
            // Never go on as the client.
            std::process::abort();
        }
        if !opened {
            return None;
        }
        let sid_of = |class| -> Option<String> {
            let mut buf = vec![0u64; 64];
            let mut len = 0u32;
            if GetTokenInformation(tok, class, buf.as_mut_ptr().cast(), 512, &mut len) == 0 {
                return None;
            }
            let sid = *(buf.as_ptr() as *const PSID);
            if sid.is_null() {
                return None;
            }
            let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut text) == 0 || text.is_null() {
                return None;
            }
            let n = (0..).take_while(|&i| *text.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text.cast());
            Some(s)
        };
        let mut sids: Vec<String> = [TokenUser, TokenOwner]
            .into_iter()
            .filter_map(sid_of)
            .collect();
        CloseHandle(tok);
        sids.dedup();
        (!sids.is_empty()).then_some(sids)
    }
}

/// Whether process `pid` runs elevated (an administrator's with UAC's consent, or SYSTEM).
#[cfg(windows)]
fn elevated(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: each handle is checked and closed; the information's buffer and size are ours.
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return false;
        }
        let mut tok: HANDLE = std::ptr::null_mut();
        let opened = OpenProcessToken(p, TOKEN_QUERY, &mut tok) != 0;
        CloseHandle(p);
        if !opened {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenElevation,
            (&mut e as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        ) != 0;
        CloseHandle(tok);
        ok && e.TokenIsElevated != 0
    }
}

#[cfg(windows)]
pub async fn accept(mut l: Listener, core: Arc<NodeCore>) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mut server = match l.first.take() {
        Some(s) => s,
        None => l.instance(false)?,
    };
    // A failing connect waits before the next instance, growing while it keeps failing
    // (it looped at once, without end, when the failure lasted).
    let mut waits = idn_common::backoff::Backoff::new(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_secs(2),
    );
    loop {
        // A client that came and went before it was taken (ERROR_NO_DATA), or a failure to
        // make the next instance, is that connection's, not the Node's: returning here
        // stopped the whole Node.
        if let Err(e) = server.connect().await {
            core.log(&format!("program connection not taken: {e}"));
            tokio::time::sleep(waits.next()).await;
            server = next_instance(&l, &core).await;
            continue;
        }
        waits.reset();
        let connected = std::mem::replace(&mut server, next_instance(&l, &core).await);
        let mut pid = 0u32;
        // SAFETY: a live pipe handle; `pid` receives the client's process id.
        unsafe {
            windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(
                connected.as_raw_handle() as _,
                &mut pid,
            )
        };
        let peer = Peer {
            pid: (pid != 0).then_some(pid),
            user: "interactive user".into(),
            exe: (pid != 0).then(|| exe_of(pid)).flatten(),
            admin: pid != 0 && elevated(pid),
            uid: None,
            pipe: Some(connected.as_raw_handle() as usize),
        };
        let core = core.clone();
        tokio::spawn(async move {
            if let Err(e) = core.clone().serve(connected, peer).await {
                core.log(&format!("program connection: {e}"));
            }
        });
    }
}

/// A new pipe instance, tried again until there is one.
#[cfg(windows)]
async fn next_instance(
    l: &Listener,
    core: &NodeCore,
) -> tokio::net::windows::named_pipe::NamedPipeServer {
    loop {
        match l.instance(false) {
            Ok(s) => return s,
            Err(e) => {
                core.log(&format!("program connections: no new pipe instance: {e}"));
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}

#[cfg(windows)]
pub fn cleanup(_: &Place) {}
