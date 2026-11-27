//! Shared memory between this machine's programs: a region one program makes under a
//! random name and others open by that name, mapped into each. The plugin host's audio
//! blocks (idn-mixer.md §M.6) and local IDN connections' pictures (idn-extensions.md §E.26)
//! go through one instead of a pipe or the network.
//!
//! Linux: anonymous memory (`memfd_create`), with no name in `/dev/shm` at all. The maker
//! hands it to programs of its own account through a socket of its own in the abstract
//! namespace (no file either), which checks the asker's account (`SO_PEERCRED`). The
//! kernel frees the memory when the last program holding it ends, however it ends: a
//! crash or a kill leaves nothing behind (named regions of killed programs stayed, 374 MiB
//! of them). Other Unix (macOS): POSIX shared memory
//! (`shm_open`), readable by this account only, its name removed when the maker drops it.
//! Windows: a named file mapping in the session's namespace (`Local\`), gone when the last
//! handle closes, crash or not.

use std::io;

/// A mapped region.
pub struct Region {
    name: String,
    ptr: *mut u8,
    len: usize,
    owner: bool,
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

/// Linux: this process's regions, handed to other programs of its account on request.
#[cfg(target_os = "linux")]
mod share {
    use crate::sync::LockOk as _;
    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    /// The server: its id (part of every region's name) and the regions it hands out.
    pub(super) struct Server {
        pub(super) id: String,
        items: Mutex<HashMap<String, (RawFd, usize)>>,
    }

    fn address(id: &str) -> io::Result<SocketAddr> {
        SocketAddr::from_abstract_name(format!("idn-shm-{id}").as_bytes())
    }

    /// This process's server, started on first use.
    pub(super) fn server() -> io::Result<&'static Server> {
        static S: OnceLock<Result<&'static Server, String>> = OnceLock::new();
        S.get_or_init(|| {
            let id = super::fresh_name("s").trim_start_matches("s-").to_owned();
            let l = UnixListener::bind_addr(&address(&id).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let server: &'static Server = Box::leak(Box::new(Server {
                id,
                items: Mutex::new(HashMap::new()),
            }));
            std::thread::Builder::new()
                .name("idn-shm-share".into())
                .spawn(move || {
                    for c in l.incoming() {
                        match c {
                            Ok(c) => {
                                let _ = serve(server, c);
                            }
                            // (Out of descriptors: no busy spin, deep bug run 2026-11-09.)
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(server)
        })
        .clone()
        .map_err(io::Error::other)
    }

    impl Server {
        pub(super) fn add(&self, item: &str, fd: RawFd, len: usize) {
            self.items.lock_ok().insert(item.to_owned(), (fd, len));
        }

        /// Region `item` no longer handed out: its descriptor, for the caller to close.
        pub(super) fn take(&self, item: &str) -> Option<(RawFd, usize)> {
            self.items.lock_ok().remove(item)
        }
    }

    /// One request: the asker's account checked, then the region's descriptor.
    fn serve(s: &Server, mut c: UnixStream) -> io::Result<()> {
        // (Short: one at a time, and a program of this account saying nothing held the
        // others up 2 s each; bug run 2026-11-25. A request is 80 bytes, local.)
        c.set_read_timeout(Some(Duration::from_millis(200)))?;
        // SAFETY: ucred is plain integers; all zeros is a valid value.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a ucred's room, as SO_PEERCRED fills.
        let r = unsafe {
            libc::getsockopt(
                c.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut n,
            )
        };
        // Programs of this account only (as the 0600 of a named region).
        // SAFETY: no arguments.
        if r != 0 || cred.uid != unsafe { libc::getuid() } {
            return Ok(());
        }
        let mut item = [0u8; 80];
        let mut got = 0;
        while got < item.len() {
            let k = c.read(&mut item[got..])?;
            if k == 0 {
                break;
            }
            got += k;
            if item[..got].contains(&b'\n') {
                break;
            }
        }
        let item = std::str::from_utf8(&item[..got]).unwrap_or("").trim();
        // Held while it goes: a region dropped meanwhile would close the descriptor, and
        // its number, reused by whatever this process opened next, would be sent instead.
        let items = s.items.lock_ok();
        let Some((fd, _)) = items.get(item).copied() else {
            drop(items);
            return c.write_all(b"no");
        };
        send_fd(&c, fd)
    }

    /// `fd` across `c` (SCM_RIGHTS), with one byte.
    fn send_fd(c: &UnixStream, fd: RawFd) -> io::Result<()> {
        let mut byte = *b"y";
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr() as *mut libc::c_void,
            iov_len: 1,
        };
        // SAFETY: a control buffer sized for one descriptor, filled as cmsg(3) says.
        unsafe {
            let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
            let mut cbuf = vec![0u8; space];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            let cm = libc::CMSG_FIRSTHDR(&msg);
            (*cm).cmsg_level = libc::SOL_SOCKET;
            (*cm).cmsg_type = libc::SCM_RIGHTS;
            (*cm).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::copy_nonoverlapping(
                &fd as *const RawFd as *const u8,
                libc::CMSG_DATA(cm),
                std::mem::size_of::<RawFd>(),
            );
            if libc::sendmsg(c.as_raw_fd(), &msg, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Region `item` from server `id`: its descriptor.
    pub(super) fn fetch(id: &str, item: &str) -> io::Result<OwnedFd> {
        let mut c = UnixStream::connect_addr(&address(id)?)?;
        // A server of this account only: another user's program could serve the name once
        // its maker ended, and hand over memory it then shrinks.
        // SAFETY: ucred is plain integers; all zeros is a valid value.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a ucred's room, as SO_PEERCRED fills.
        let r = unsafe {
            libc::getsockopt(
                c.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut n,
            )
        };
        // SAFETY: no arguments.
        if r != 0 || cred.uid != unsafe { libc::getuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the region's server isn't this account's",
            ));
        }
        c.set_read_timeout(Some(Duration::from_secs(2)))?;
        c.write_all(format!("{item}\n").as_bytes())?;
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr() as *mut libc::c_void,
            iov_len: 1,
        };
        // SAFETY: as in `send_fd`; a descriptor received is ours to own.
        unsafe {
            let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
            let mut cbuf = vec![0u8; space];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            if libc::recvmsg(c.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            let cm = libc::CMSG_FIRSTHDR(&msg);
            if cm.is_null()
                || (*cm).cmsg_level != libc::SOL_SOCKET
                || (*cm).cmsg_type != libc::SCM_RIGHTS
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no such region (or not this account's)",
                ));
            }
            let mut fd: RawFd = -1;
            std::ptr::copy_nonoverlapping(
                libc::CMSG_DATA(cm),
                &mut fd as *mut RawFd as *mut u8,
                std::mem::size_of::<RawFd>(),
            );
            Ok(OwnedFd::from_raw_fd(fd))
        }
    }
}

// SAFETY: the mapping is plain memory; who writes where is the users' protocol (each
// user here keeps writers and readers apart, with sequence stamps).
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

/// A name no other region has and no other program can guess: the prefix and 16 hex
/// digits from the OS's randomness (the standard library's per-process hash seeds).
fn fresh_name(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(crate::epoch::now().as_nanos());
    h.write_u32(std::process::id());
    format!("{prefix}-{:016x}", h.finish())
}

/// A name as this module makes them: short, letters, digits and dashes.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a region's name",
        ));
    }
    Ok(())
}

/// A descriptor this process hands to programs of its account by name, through the same
/// share server as its regions (Linux): GPU memory exported as a descriptor
/// (idn-extensions.md §E.27). Its name is `PREFIX-SERVER-ITEM`. Dropped, it is handed out
/// no more and closed; what others took stays theirs.
#[cfg(target_os = "linux")]
pub struct SharedFd {
    name: String,
    item: String,
}

#[cfg(target_os = "linux")]
impl SharedFd {
    pub fn new(prefix: &str, fd: std::os::fd::OwnedFd) -> io::Result<SharedFd> {
        use std::os::fd::IntoRawFd;
        let server = share::server()?;
        let item = fresh_name("r").trim_start_matches("r-").to_owned();
        server.add(&item, fd.into_raw_fd(), 0);
        Ok(SharedFd {
            name: format!("{prefix}-{}-{item}", server.id),
            item,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(target_os = "linux")]
impl Drop for SharedFd {
    fn drop(&mut self) {
        if let Ok(s) = share::server()
            && let Some((fd, _)) = s.take(&self.item)
        {
            // SAFETY: the descriptor given to `new`, ours alone since.
            unsafe { libc::close(fd) };
        }
    }
}

/// The descriptor another program shares as `name` ([`SharedFd`]).
#[cfg(target_os = "linux")]
pub fn fetch_fd(name: &str) -> io::Result<std::os::fd::OwnedFd> {
    check_name(name)?;
    let mut parts = name.rsplitn(3, '-');
    let (item, server) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    share::fetch(server, item)
}

impl Region {
    /// A new region of `len` bytes (zeroed), named after `prefix`.
    pub fn create(prefix: &str, len: usize) -> io::Result<Region> {
        #[cfg(target_os = "linux")]
        {
            // `PREFIX-SERVER-ITEM`: this process's share server, then the region.
            let server = share::server()?;
            let item = fresh_name("r").trim_start_matches("r-").to_owned();
            let name = format!("{prefix}-{}-{item}", server.id);
            // SAFETY: memfd_create, ftruncate and mmap as documented.
            unsafe {
                let fd = libc::memfd_create(
                    c"idn-shm".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                );
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Sealed at its size: no program it is given to can shrink it under the
                // others (they'd fault reading it).
                if libc::ftruncate(fd, len as libc::off_t) != 0
                    || libc::fcntl(
                        fd,
                        libc::F_ADD_SEALS,
                        libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
                    ) != 0
                {
                    let e = io::Error::last_os_error();
                    libc::close(fd);
                    return Err(e);
                }
                let r = Self::map_fd(&name, fd, len, true);
                match &r {
                    Ok(_) => server.add(&item, fd, len),
                    Err(_) => {
                        libc::close(fd);
                    }
                }
                r
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let name = fresh_name(prefix);
            // SAFETY: OS calls as documented; the name is ours alone.
            unsafe { Self::map(&name, len, true) }
        }
    }

    /// The region `name` another program made, `len` bytes of it.
    pub fn open(name: &str, len: usize) -> io::Result<Region> {
        check_name(name)?;
        #[cfg(target_os = "linux")]
        {
            // From the maker's share server (`PREFIX-SERVER-ITEM`).
            let mut parts = name.rsplitn(3, '-');
            let (item, server) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let fd = share::fetch(server, item)?;
            // SAFETY: a descriptor of a memfd, mapped after its size is checked.
            unsafe {
                use std::os::fd::IntoRawFd;
                let fd = fd.into_raw_fd();
                let r = Self::map_fd(name, fd, len, false);
                libc::close(fd);
                r
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: as above.
            unsafe { Self::map(name, len, false) }
        }
    }

    /// Maps `len` bytes of descriptor `fd` (its size checked when not made here).
    #[cfg(target_os = "linux")]
    unsafe fn map_fd(name: &str, fd: i32, len: usize, create: bool) -> io::Result<Region> {
        if !create {
            // SAFETY: `fd` is the caller's open descriptor; a zeroed stat (plain data) for fstat to
            // fill.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 || (st.st_size as usize) < len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the region is shorter",
                ));
            }
            // Sealed against shrinking, or it could be cut under this mapping.
            // SAFETY: a query on the caller's open descriptor.
            let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
            if seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the region isn't sealed",
                ));
            }
        }
        // SAFETY: a new shared mapping of `len` bytes of a file at least that long (sized by the
        // caller when made here, else checked above and sealed against shrinking); unmapped in
        // `Drop`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Region {
            name: name.to_owned(),
            ptr: ptr as *mut u8,
            len,
            owner: create,
        })
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    unsafe fn map(name: &str, len: usize, create: bool) -> io::Result<Region> {
        let path = std::ffi::CString::new(format!("/{name}")).map_err(io::Error::other)?;
        let flags = if create {
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR
        } else {
            libc::O_RDWR
        };
        // SAFETY: a NUL-terminated name of our own.
        let fd = unsafe { libc::shm_open(path.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fail = |e: io::Error| {
            // SAFETY: `fd` is ours and still open (closed only after the last `fail`); the name is
            // unlinked only when this call made it.
            unsafe {
                libc::close(fd);
                if create {
                    libc::shm_unlink(path.as_ptr());
                }
            }
            Err(e)
        };
        // SAFETY: `fd` is ours, opened read-write.
        if create && unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            return fail(io::Error::last_os_error());
        }
        if !create {
            // Never mapped past its end (a short region would fault on access).
            // SAFETY: `fd` is ours; a zeroed stat (plain data) for fstat to fill.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 || (st.st_size as usize) < len {
                return fail(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the region is shorter",
                ));
            }
        }
        // SAFETY: a new shared mapping of `len` bytes of an object at least that long (made so or
        // checked above); unmapped in `Drop`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return fail(io::Error::last_os_error());
        }
        // SAFETY: ours, closed once; the mapping keeps the memory.
        unsafe { libc::close(fd) };
        Ok(Region {
            name: name.to_owned(),
            ptr: ptr as *mut u8,
            len,
            owner: create,
        })
    }

    #[cfg(windows)]
    unsafe fn map(name: &str, len: usize, create: bool) -> io::Result<Region> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW,
            PAGE_READWRITE,
        };
        let wide = crate::win::wide(&format!("Local\\{name}"));
        let handle = if create {
            // SAFETY: a paging-file mapping with a NUL-terminated name of our own.
            let h = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    (len as u64 >> 32) as u32,
                    len as u32,
                    wide.as_ptr(),
                )
            };
            // A name that existed already isn't ours.
            if !h.is_null()
                // SAFETY: no arguments; read right after the call it reports on.
                && unsafe { windows_sys::Win32::Foundation::GetLastError() }
                    == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
            {
                // SAFETY: the handle just made, not kept.
                unsafe { CloseHandle(h) };
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the region's name",
                ));
            }
            h
        } else {
            // SAFETY: a NUL-terminated name of our own.
            unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr()) }
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a live mapping (checked above); a view longer than the mapping fails.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: the mapping's handle, not kept.
            unsafe { CloseHandle(handle) };
            return Err(e);
        }
        let _ = INVALID_HANDLE_VALUE;
        Ok(Region {
            name: name.to_owned(),
            ptr: view.Value as *mut u8,
            len,
            owner: create,
            handle,
        })
    }

    /// Its name, for the other program.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The region's memory. Other programs write it too: what is read is only as good as
    /// the users' protocol keeps it.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Copies `src` in at `at`; false (nothing copied) when it doesn't fit.
    pub fn write(&self, at: usize, src: &[u8]) -> bool {
        match at.checked_add(src.len()) {
            Some(end) if end <= self.len => {
                // SAFETY: within the mapping.
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(at), src.len()) };
                true
            }
            _ => false,
        }
    }

    /// Copies `dst.len()` bytes out from `at`; false when that is past the end.
    pub fn read(&self, at: usize, dst: &mut [u8]) -> bool {
        match at.checked_add(dst.len()) {
            Some(end) if end <= self.len => {
                // SAFETY: within the mapping.
                unsafe {
                    std::ptr::copy_nonoverlapping(self.ptr.add(at), dst.as_mut_ptr(), dst.len())
                };
                true
            }
            _ => false,
        }
    }

    /// The u64 at `at` (8-aligned), as an atomic: sequence stamps between programs.
    pub fn atomic_u64(&self, at: usize) -> Option<&std::sync::atomic::AtomicU64> {
        if !at.is_multiple_of(8) || at.checked_add(8).is_none_or(|e| e > self.len) {
            return None;
        }
        // SAFETY: aligned, within the mapping, and the mapping lives as long as `self`.
        Some(unsafe { &*(self.ptr.add(at) as *const std::sync::atomic::AtomicU64) })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // Linux: no longer handed out; the descriptor closed (the memory goes with the
        // last mapping, here or in another program).
        #[cfg(target_os = "linux")]
        // SAFETY: the mapping made in `map_fd`, unmapped once, here; a descriptor taken back from
        // the share server is ours to close.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            if self.owner
                && let Ok(s) = share::server()
                && let Some(item) = self.name.rsplit('-').next()
                && let Some((fd, _)) = s.take(item)
            {
                libc::close(fd);
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        // SAFETY: the mapping made in `map`, unmapped once, here; the name unlinked only by its
        // maker.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            if self.owner
                && let Ok(p) = std::ffi::CString::new(format!("/{}", self.name))
            {
                libc::shm_unlink(p.as_ptr());
            }
        }
        #[cfg(windows)]
        // SAFETY: the view and handle from `map`, released once, here.
        unsafe {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr as *mut core::ffi::c_void,
            });
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
            let _ = self.owner;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Region;

    #[test]
    fn a_region_made_here_is_seen_through_its_name() {
        let a = Region::create("idn-test", 1 << 16).unwrap();
        let b = Region::open(a.name(), 1 << 16).unwrap();
        assert!(a.write(100, b"hello"));
        let mut got = [0u8; 5];
        assert!(b.read(100, &mut got));
        assert_eq!(&got, b"hello");
        a.atomic_u64(8)
            .unwrap()
            .store(42, std::sync::atomic::Ordering::Release);
        assert_eq!(
            b.atomic_u64(8)
                .unwrap()
                .load(std::sync::atomic::Ordering::Acquire),
            42
        );
        assert!(!a.write((1 << 16) - 2, b"xyz"), "past the end");
        assert!(
            Region::open(a.name(), 1 << 20).is_err() || cfg!(windows),
            "longer than it is"
        );
        assert!(Region::open("../etc", 16).is_err());
        #[cfg(unix)]
        let name = a.name().to_owned();
        drop(a);
        drop(b);
        #[cfg(unix)]
        assert!(Region::open(&name, 16).is_err(), "gone with its maker");
    }

    /// A descriptor shared by name reaches another asker (here, this process) as the same
    /// file, and is handed out no more once dropped.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_shared_descriptor_is_fetched_by_its_name() {
        use std::io::{Read, Seek, Write};
        // SAFETY: a NUL-terminated name; the descriptor is checked before it is owned.
        let fd = unsafe { libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
        // SAFETY: a new descriptor no one else owns.
        let mut f = std::fs::File::from(unsafe {
            use std::os::fd::FromRawFd;
            std::os::fd::OwnedFd::from_raw_fd(fd)
        });
        f.write_all(b"gpu").unwrap();
        let shared = super::SharedFd::new("idn-test", f.try_clone().unwrap().into()).unwrap();
        let mut g = std::fs::File::from(super::fetch_fd(shared.name()).unwrap());
        g.rewind().unwrap();
        let mut got = String::new();
        g.read_to_string(&mut got).unwrap();
        assert_eq!(got, "gpu");
        let name = shared.name().to_owned();
        drop(shared);
        assert!(super::fetch_fd(&name).is_err());
        assert!(super::fetch_fd("../x").is_err());
    }
}
