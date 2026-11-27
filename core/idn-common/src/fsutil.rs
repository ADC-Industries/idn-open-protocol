//! Files written whole or not at all: a temporary file of its own beside the target, on
//! disk, then renamed over it. A crash or a power cut leaves the old file or the new one,
//! never half of one (the switcher's inputs and setups were written in place, so a crash
//! mid-write could lose them all).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `data` to `path` whole, creating parent folders. A file that already holds
/// exactly `data` is left alone (each write is a sync to the disk).
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    put(path, data, false, true)
}

/// [`write_atomic`] in `std::fs::write`'s shape, to put in its place.
pub fn write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    put(path.as_ref(), data.as_ref(), false, true)
}

/// A saved file read back for a program's state: `Ok(Some(..))` as `parse` makes it,
/// `Ok(None)` when there is none yet (a fresh start). A file that is there but can't be
/// read, or that `parse` refuses (a power cut mid-write, a newer build's file, the wrong
/// access), is set aside as `NAME.bad-SECS-PID-N` so the program's next save can't write
/// over it — the person's setup kept to look at or put back — and `Err` says so, for the
/// caller to log. (Each of these files read as "none" on any error before: the program
/// started fresh and saved that over the setup, with no line anywhere; bug round
/// 2026-11-09.)
///
/// Set aside only when it is still the file that was read (its length, modified time and,
/// on Unix, inode the same just before the move): one another program saved meanwhile is
/// left where it is, and `Err` says that instead. A set-aside copy never replaces another
/// (a name of its own per process and per call, made by a hard link, else a rename to a
/// name checked free), and the newest [`KEEP_SET_ASIDE`] per file name are kept, the
/// older removed ([`load_saved_keeping`] to keep another number).
///
/// For files the program saves itself. A file only people write (a hand-edited setting
/// the program never saves) is read with [`load_json_readonly`], which never moves it.
pub fn load_saved<T>(
    path: &Path,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> Result<Option<T>, String> {
    load_saved_keeping(path, KEEP_SET_ASIDE, parse)
}

/// How many set-aside copies of one file [`load_saved`] keeps (the newest); a file that
/// keeps failing doesn't fill the disk with them.
pub const KEEP_SET_ASIDE: usize = 5;

/// [`load_saved`], keeping the newest `keep` set-aside copies (at least 1: the one just
/// made) instead of [`KEEP_SET_ASIDE`].
pub fn load_saved_keeping<T>(
    path: &Path,
    keep: usize,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> Result<Option<T>, String> {
    let before = stamp(path);
    let why = match std::fs::read(path) {
        Ok(b) => match parse(&b) {
            Some(t) => return Ok(Some(t)),
            None => "can't be read as a saved file of this build".to_owned(),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => e.to_string(),
    };
    // Still the file that was read? (Saved anew meanwhile, by another program or another
    // thread: that one is good as far as anyone knows, and not moved.)
    if before.is_none() || stamp(path) != before {
        return Err(format!(
            "{}: {why}; it changed while being read (saved meanwhile?): left in place, not set aside",
            path.display()
        ));
    }
    let mut moved = Err(io::Error::from(io::ErrorKind::AlreadyExists));
    for _ in 0..8 {
        let aside = aside_name(path);
        match move_aside(path, &aside) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            r => {
                moved = r.map(|left| (aside, left));
                break;
            }
        }
    }
    Err(match moved {
        Ok((aside, left)) => {
            trim_set_asides(path, &aside, keep);
            match left {
                None => format!(
                    "{}: {why}; set aside as {} (started without it)",
                    path.display(),
                    aside.display()
                ),
                Some(e) => format!(
                    "{}: {why}; copied aside as {} (started without it; the original couldn't be removed: {e})",
                    path.display(),
                    aside.display()
                ),
            }
        }
        Err(e) => format!(
            "{}: {why}; couldn't set it aside ({e}): a save may write over it",
            path.display()
        ),
    })
}

/// A set-aside name for `path` no other has had: `NAME.bad-SECS-PID-N`, `N` counting
/// across the process.
fn aside_name(path: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".bad-{}-{}-{n}",
        crate::epoch::secs(),
        std::process::id()
    ));
    path.with_file_name(name)
}

/// The oldest set-aside copies of `path` removed, all but the newest `keep` (at least 1),
/// `just_made` never among them.
fn trim_set_asides(path: &Path, just_made: &Path, keep: usize) {
    let all = set_aside_copies(path);
    let extra = all.len().saturating_sub(keep.max(1));
    for old in all.iter().filter(|p| p.as_path() != just_made).take(extra) {
        let _ = std::fs::remove_file(old);
    }
}

/// A copy of `path` set aside beside it (its name as [`load_saved`]'s, the file itself
/// left in place), for a file read only in part (entries this build can't read left out):
/// what the next save drops is kept. Never over another copy (made new, with the file's
/// own mode on Unix, so a private file's copy is private too); the newest
/// [`KEEP_SET_ASIDE`] kept.
pub fn copy_aside(path: &Path) -> io::Result<PathBuf> {
    let data = std::fs::read(path)?;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        o.mode(mode);
    }
    for _ in 0..8 {
        let aside = aside_name(path);
        let mut f = match o.open(&aside) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            r => r?,
        };
        f.write_all(&data)?;
        trim_set_asides(path, &aside, KEEP_SET_ASIDE);
        return Ok(aside);
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

/// A file's length, modified time and (Unix) inode: whether it is still the one read.
fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>, u64)> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some((m.len(), m.modified().ok(), ino))
}

/// `path` moved to `aside`, never over a file there: a hard link made (refused if the
/// name is taken) and the original removed (`Ok(Some(e))` when it couldn't be: the copy
/// is safe all the same, as each save replaces the file rather than writing into it).
/// Where hard links can't be made (FAT, some shares), a rename to a name checked free.
fn move_aside(path: &Path, aside: &Path) -> io::Result<Option<io::Error>> {
    match std::fs::hard_link(path, aside) {
        Ok(()) => Ok(std::fs::remove_file(path).err()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if std::fs::symlink_metadata(aside).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(path, aside).map(|()| None)
        }
    }
}

/// The set-aside copies of `path` ([`load_saved`]'s `NAME.bad-SECS-PID-N`, and the
/// older builds' `NAME.bad-SECS`), oldest first.
pub fn set_aside_copies(path: &Path) -> Vec<PathBuf> {
    let Some(base) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let prefix = format!("{base}.bad-");
    let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from("."),
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<((u64, u64, u64), PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let rest = name.to_str()?.strip_prefix(&prefix)?;
            let mut parts = rest.split('-').map(|p| p.parse::<u64>().ok());
            let secs = parts.next()??;
            let pid = parts.next().unwrap_or(Some(0))?;
            let n = parts.next().unwrap_or(Some(0))?;
            if parts.next().is_some() {
                return None;
            }
            Some(((secs, n, pid), e.path()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

/// Removes every set-aside copy of `path` ([`set_aside_copies`]); how many went. For a
/// file holding a secret that was just removed (a sign-out, a password cleared): a
/// damaged copy set aside earlier mustn't keep it on the disk.
pub fn remove_set_asides(path: &Path) -> usize {
    set_aside_copies(path)
        .iter()
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// A saved JSON file read back as `T`: [`load_saved`] with serde_json, so a file that is
/// there but damaged (or another build's shape) is set aside as `NAME.bad-SECS-PID-N`,
/// never read as "none" and then saved over. `Ok(None)` when there is none yet; `Err`
/// (`InvalidData`) says what was set aside, for the caller to log before it starts
/// without it. Use it for every file a program reads its state from and saves.
pub fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    load_saved(path, |b| serde_json::from_slice(b).ok()).map_err(crate::err::invalid_data)
}

/// A JSON file only people write, which the program reads but never saves (a hand-edited
/// setting such as the ASIO driver's `asio.json`): read as `T` and never moved, as there
/// is no save of the program's to protect it from, and the person's file stays where
/// they look for it. `Ok(None)` when there is none; `Err` names the file and, for a
/// damaged one (`InvalidData`), the line and column the reading stopped at, for the
/// caller to log before it goes on with its defaults.
///
/// Which to use: a file the program writes, even now and then (a panel's or the
/// program's own save) → [`load_json`]; a file nothing of ours ever writes →
/// this.
pub fn load_json_readonly<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    // (serde's message ends "at line L column C".)
    serde_json::from_slice(&b).map(Some).map_err(|e| {
        crate::err::invalid_data(format_args!(
            "{}: {e} (the file left as it is)",
            path.display()
        ))
    })
}

/// As [`write`], without waiting for the disk: for a file rewritten every second or so
/// (a download's progress), whole when read but possibly the one before after a power
/// cut.
pub fn write_quick(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    put(path.as_ref(), data.as_ref(), false, false)
}

/// As [`write_atomic`], readable by this user only from the first byte (keys, hashes).
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    put(path, data, true, true)
}

/// Folder `dir` made if missing and kept to this user: on Unix narrowed to mode 0700 when
/// this user owns it and its mode is wider (a folder made earlier with a wider mode).
/// A link there (a symlink to another disk, systemd's `StateDirectory` under
/// `DynamicUser`, a Windows junction putting a ProgramData folder on D:) is followed and
/// its target worked on; a link whose target is missing is an error. A folder another
/// account owns is left as it is (its mode isn't this user's to change, and trying was an
/// error on a folder this user may write but doesn't own): `Ok(false)`, for the caller to
/// log if it matters; `Ok(true)` otherwise. Windows: only made, `Ok(true)` (a folder's
/// own list inherits from the user's profile or the service's state folder;
/// [`lock_down_dir`] is the machine-wide lock-down for ProgramData).
pub fn private_dir(dir: &Path) -> io::Result<bool> {
    let target = if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::canonicalize(dir).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{}: a link whose target can't be found: {e}", dir.display()),
            )
        })?
    } else {
        dir.to_path_buf()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&target)?;
        let m = std::fs::metadata(&target)?;
        // SAFETY: geteuid has no preconditions and can't fail.
        if m.uid() != unsafe { libc::geteuid() } {
            return Ok(false);
        }
        // (Made earlier with a wider mode: narrowed; left alone when already narrow, so
        // no change, and no error, on every call.)
        if m.mode() & 0o077 != 0 {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&target)?;
        Ok(true)
    }
}

fn put(path: &Path, data: &[u8], private: bool, sync: bool) -> io::Result<()> {
    // Unchanged: nothing to write (a short file read is far cheaper than a sync). Not for
    // a private file: written again, it is also made private again.
    if !private
        && std::fs::metadata(path).is_ok_and(|m| m.len() == data.len() as u64)
        && std::fs::read(path).is_ok_and(|old| old == data)
    {
        return Ok(());
    }
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    // A temporary name of its own per file and per write: one name per folder let
    // concurrent writes swap contents (server.key and server.crt once).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let written = (|| {
        let mut f = o.open(&tmp)?;
        f.write_all(data)?;
        // On disk before it replaces the old file.
        if sync { f.sync_all() } else { Ok(()) }
    })();
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename itself on disk too (the folder synced): a power cut right after could
    // otherwise bring back the old file, or none.
    #[cfg(unix)]
    if sync
        && let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(dir) = std::fs::File::open(d)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// A folder a service account owns, worked in by a root helper (the update helper, the
/// virtual screens' one): that account could make the folder, or any file in it, a link
/// (symbolic or hard) to anywhere, and root would then read or write there. Opened
/// without following a link and checked; its files opened relative to it
/// (`openat`), never through a link, and new ones made fresh.
#[cfg(unix)]
pub struct OwnedDir {
    dir: std::fs::File,
    /// The folder's owner and group.
    pub owner: (u32, u32),
}

#[cfg(unix)]
impl OwnedDir {
    /// Opens `path` (not a link, a folder, not writable by everyone).
    pub fn open(path: &Path) -> io::Result<OwnedDir> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        let m = dir.metadata()?;
        if !m.is_dir() || m.mode() & 0o002 != 0 {
            return Err(io::Error::other(format!(
                "{}: not a folder of the service's",
                path.display()
            )));
        }
        Ok(OwnedDir {
            dir,
            owner: (m.uid(), m.gid()),
        })
    }

    fn at(&self, name: &str, flags: libc::c_int, mode: libc::mode_t) -> io::Result<std::fs::File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(io::Error::other("a file's name in the folder"));
        }
        let c = std::ffi::CString::new(name).map_err(io::Error::other)?;
        // SAFETY: our folder's descriptor and a NUL-ended name.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just opened, ours alone.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    /// Reads `name` (a plain file with one link, at most 1 MB).
    pub fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let f = self.at(name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        let m = f.metadata()?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(io::Error::other(format!("{name}: not a plain file")));
        }
        let mut b = Vec::new();
        f.take(1 << 20).read_to_end(&mut b)?;
        Ok(b)
    }

    /// Writes `name` afresh (whatever was there, link or file, removed first; made new,
    /// `O_EXCL`), given to the folder's owner.
    pub fn write_new(&self, name: &str, data: &[u8]) -> io::Result<()> {
        self.remove(name);
        let mut f = self.at(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o640)?;
        f.write_all(data)?;
        std::os::unix::fs::fchown(&f, Some(self.owner.0), Some(self.owner.1))
    }

    /// Appends to `name` (made if missing, then the folder owner's; never through a link,
    /// nor one with other links, nor waiting on a FIFO).
    pub fn append(&self, name: &str) -> io::Result<std::fs::File> {
        use std::os::unix::fs::MetadataExt;
        let f = self.at(
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_NONBLOCK,
            0o640,
        )?;
        let m = f.metadata()?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(io::Error::other(format!("{name}: not a plain file")));
        }
        if m.uid() != self.owner.0 {
            std::os::unix::fs::fchown(&f, Some(self.owner.0), Some(self.owner.1))?;
        }
        Ok(f)
    }

    /// Removes `name` (the link itself, if one), if there.
    pub fn remove(&self, name: &str) {
        use std::os::fd::AsRawFd;
        if let Ok(c) = std::ffi::CString::new(name) {
            // SAFETY: our folder's descriptor and a NUL-ended name.
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), c.as_ptr(), 0) };
        }
    }

    /// Makes it the working folder (for helpers that work in it by relative paths).
    pub fn enter(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: a folder this process opened.
        if unsafe { libc::fchdir(self.dir.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Windows: `dir` (and all in it) made SYSTEM's and administrators' only — owner set to
/// SYSTEM first (a folder in ProgramData that a standard user made before stayed theirs,
/// and an owner can give itself access again), then the folder's access list replaced by
/// full control for SYSTEM and administrators, inherited by what is in it, and what is in
/// it set back to inheriting that. For folders that hold a machine's keys or programs
/// that run as SYSTEM. `Err` if the access list couldn't be set.
///
/// (The folder's list alone, not with `/T`: given to every file too, the folder-only
/// flags `(OI)(CI)` were refused on each file after its inherited entries were removed,
/// which left every file in the IDN Node's folder with none: SYSTEM itself was refused
/// its own key and log, and the service didn't start (Alpha61–64). Run
/// as SYSTEM, the owner, this also mends such files.)
#[cfg(windows)]
pub fn lock_down_dir(dir: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let run = |target: &Path, args: &[&str]| {
        Command::new("icacls")
            .arg(target)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // Not through a link or junction (a standard user may make one in ProgramData before
    // the first install: the lock-down would then act on its target, a system folder or
    // one the user can swap out; Rust reports junctions as links).
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other(format!(
            "{}: a link or junction, not a folder: not locked down (remove it, then install again)",
            dir.display()
        )));
    }
    let _ = run(dir, &["/setowner", "*S-1-5-18", "/T", "/C", "/Q"]);
    // The folder's list set whole (`icacls /grant:r` replaced only SYSTEM's and the
    // administrators' entries: a user's own from before stayed, full control of the
    // machine's key included).
    let set = only_system_and_administrators(dir).is_ok();
    // What is in it: its own entries dropped, the folder's inherited (an empty folder
    // has nothing to match: not an error).
    let _ = run(&dir.join("*"), &["/reset", "/T", "/C", "/Q"]);
    if set {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{}: couldn't make it SYSTEM's and administrators' only",
            dir.display()
        )))
    }
}

/// `dir`'s access list replaced whole: full control for SYSTEM and administrators,
/// inherited by what is in it, nothing inherited from above (`D:PAI(A;OICI;FA;;;SY)
/// (A;OICI;FA;;;BA)`).
#[cfg(windows)]
fn only_system_and_administrators(dir: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    let wide = crate::win::wide_os;
    let sddl = wide(std::ffi::OsStr::new(
        "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
    ));
    let path = wide(dir.as_os_str());
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; on success `sd` is a descriptor the system
    // allocated, freed with LocalFree below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `sd` is the valid descriptor made above; the out-pointers are ours.
    let ok = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } != 0
        && present != 0;
    let r = if ok {
        // SAFETY: `path` is NUL-terminated; `dacl` points into `sd`, alive until freed
        // below; no owner, group or SACL is passed (null: left as they are).
        let e = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if e == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(e as i32))
        }
    } else {
        Err(io::Error::last_os_error())
    };
    // SAFETY: `sd` was allocated by the conversion above and isn't used after this.
    unsafe { LocalFree(sd as _) };
    r
}

/// A group's id by name (thread-safe: `getgrnam_r`, not getgrnam's shared buffer).
#[cfg(unix)]
pub fn group_id(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: group is integers and pointers; all zeros (nulls) is valid until getgrnam_r fills it.
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: buffers sized as given; `out` is null or points into `grp`.
    let r =
        unsafe { libc::getgrnam_r(c.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut out) };
    (r == 0 && !out.is_null()).then_some(grp.gr_gid)
}

#[cfg(all(test, unix))]
mod owned_tests {
    use super::OwnedDir;

    #[test]
    fn links_in_the_folder_are_not_followed() {
        let d = std::env::temp_dir().join(format!("idn-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let outside = d.with_extension("outside");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("result.json")).unwrap();
        std::fs::hard_link(&outside, d.join("request.json")).unwrap();
        let o = OwnedDir::open(&d).unwrap();
        assert!(o.read("request.json").is_err(), "a hard link isn't read");
        o.write_new("result.json", b"{}").unwrap();
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"keep",
            "the link's target untouched"
        );
        assert_eq!(o.read("result.json").unwrap(), b"{}");
        assert!(o.read("../x").is_err());
        std::os::unix::fs::symlink("/", d.with_extension("link")).unwrap();
        assert!(
            OwnedDir::open(&d.with_extension("link")).is_err(),
            "the folder itself a link"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_file(d.with_extension("link"));
    }
}

/// The folder each engine keeps its daily settings copies in (idn-node.md §N.42).
pub const SETTINGS_BACKUPS: &str = "settings-backups";

/// The date (UTC) of `secs` since 1970, as `YYYY-MM-DD`.
fn utc_date(secs: u64) -> String {
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// [`daily_settings_copy`] of `dir` on a thread of its own, looked at every hour (and at
/// once), keeping 14 days.
pub fn keep_daily_settings_copies(dir: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("idn-settings-copy".into())
        .spawn(move || {
            loop {
                if let Err(e) = daily_settings_copy(&dir, 14) {
                    crate::log::log_or_stderr(&format!("settings copy in {}: {e}", dir.display()));
                }
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        });
}

/// An engine's settings copied once a day (idn-node.md §N.42, the per-machine settings
/// backup): its folder's top-level `.json` and `.txt` files (each under 4 MB) into
/// `settings-backups/YYYY-MM-DD/` (UTC), readable by its own account only; the newest
/// `keep` days kept. Nothing when today's copy is there: the folder made, or `None`.
pub fn daily_settings_copy(dir: &Path, keep: usize) -> io::Result<Option<PathBuf>> {
    let root = dir.join(SETTINGS_BACKUPS);
    let today = root.join(utc_date(crate::epoch::now().as_secs()));
    if today.exists() {
        return Ok(None);
    }
    let files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "json" || e == "txt")
                && std::fs::symlink_metadata(p)
                    .is_ok_and(|m| m.file_type().is_file() && m.len() < 4 << 20)
        })
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    let part = root.join(format!(".{}.part", utc_date(crate::epoch::now().as_secs())));
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part)?;
    // Its own account only (a switcher's destinations hold stream keys); the files
    // themselves are written private on every system.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }
    for p in &files {
        if let (Some(name), Ok(b)) = (p.file_name(), std::fs::read(p)) {
            write_private(&part.join(name), &b)?;
        }
    }
    std::fs::rename(&part, &today)?;
    let mut days: Vec<PathBuf> = std::fs::read_dir(&root)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.len() == 10 && n.as_bytes()[4] == b'-' && n.as_bytes()[7] == b'-'
            })
        })
        .collect();
    days.sort();
    for old in days.iter().take(days.len().saturating_sub(keep.max(1))) {
        let _ = std::fs::remove_dir_all(old);
    }
    Ok(Some(today))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_file_that_cant_be_read_is_set_aside_not_overwritten() {
        let dir = crate::testing::TempDir::new("load-saved");
        let p = dir.join("node.xml");
        let parse = |b: &[u8]| {
            std::str::from_utf8(b)
                .ok()
                .filter(|t| t.starts_with('<'))
                .map(str::to_owned)
        };
        assert_eq!(load_saved(&p, parse), Ok(None), "none yet: a fresh start");
        std::fs::write(&p, "<ok/>").unwrap();
        assert_eq!(load_saved(&p, parse), Ok(Some("<ok/>".to_owned())));
        std::fs::write(&p, "garbage").unwrap();
        let e = load_saved(&p, parse).unwrap_err();
        assert!(e.contains("set aside"), "{e}");
        assert!(!p.exists(), "moved out of the way of the next save");
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(
            std::fs::read(aside[0].path()).unwrap(),
            b"garbage",
            "kept as it was"
        );
    }

    #[test]
    fn set_asides_never_replace_each_other_and_the_oldest_go() {
        let dir = crate::testing::TempDir::new("set-aside-names");
        let p = dir.join("desk.json");
        let asides = || super::set_aside_copies(&p);
        // An older build's name: the oldest, the first to go.
        std::fs::write(dir.join("desk.json.bad-100"), b"old").unwrap();
        // Two in the same second: both kept, under names of their own.
        for text in ["one", "two"] {
            std::fs::write(&p, text).unwrap();
            assert!(load_json::<serde_json::Value>(&p).is_err());
        }
        let a = asides();
        assert_eq!(a.len(), 3, "{a:?}");
        assert_eq!(std::fs::read(&a[1]).unwrap(), b"one");
        assert_eq!(std::fs::read(&a[2]).unwrap(), b"two");
        // More than KEEP_SET_ASIDE: the oldest removed, the newest kept.
        for i in 0..KEEP_SET_ASIDE + 2 {
            std::fs::write(&p, format!("bad {i}")).unwrap();
            assert!(load_json::<serde_json::Value>(&p).is_err());
        }
        let a = asides();
        assert_eq!(a.len(), KEEP_SET_ASIDE, "{a:?}");
        assert!(!dir.join("desk.json.bad-100").exists());
        assert_eq!(
            std::fs::read(a.last().unwrap()).unwrap(),
            format!("bad {}", KEEP_SET_ASIDE + 1).as_bytes()
        );
        // Another file's copies, or names only alike, aren't counted or removed.
        std::fs::write(dir.join("desk.json.bak.bad-1-2-3"), b"x").unwrap();
        std::fs::write(dir.join("desk.json.bad-notes"), b"x").unwrap();
        assert_eq!(asides().len(), KEEP_SET_ASIDE);
        assert_eq!(super::remove_set_asides(&p), KEEP_SET_ASIDE);
        assert!(asides().is_empty());
        assert!(dir.join("desk.json.bad-notes").exists());
    }

    #[test]
    fn a_file_saved_while_it_was_read_is_not_set_aside() {
        let dir = crate::testing::TempDir::new("set-aside-race");
        let p = dir.join("mixes.json");
        std::fs::write(&p, b"half a sav").unwrap();
        // Another program saves it whole between the read and the move.
        let e = load_saved(&p, |_| {
            write_atomic(&p, br#"{"saved":"whole"}"#).unwrap();
            None::<()>
        })
        .unwrap_err();
        assert!(e.contains("left in place"), "{e}");
        assert_eq!(std::fs::read(&p).unwrap(), br#"{"saved":"whole"}"#);
        assert!(super::set_aside_copies(&p).is_empty());
    }

    #[test]
    fn a_hand_edited_file_is_never_moved() {
        let dir = crate::testing::TempDir::new("readonly-json");
        let p = dir.join("asio.json");
        assert!(
            load_json_readonly::<serde_json::Value>(&p)
                .unwrap()
                .is_none()
        );
        std::fs::write(&p, b"{\n  \"buffer\": 64,\n  oops\n}").unwrap();
        let e = load_json_readonly::<serde_json::Value>(&p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let m = e.to_string();
        assert!(m.contains("asio.json") && m.contains("line 3"), "{m}");
        assert!(p.exists(), "left where the person looks for it");
        assert!(super::set_aside_copies(&p).is_empty());
        std::fs::write(&p, br#"{"buffer":64}"#).unwrap();
        let v: serde_json::Value = load_json_readonly(&p).unwrap().unwrap();
        assert_eq!(v["buffer"], 64);
    }

    #[test]
    fn a_json_file_that_cant_be_read_is_set_aside() {
        let dir = crate::testing::TempDir::new("load-json");
        let p = dir.join("simple.json");
        assert!(load_json::<serde_json::Value>(&p).unwrap().is_none());
        std::fs::write(&p, br#"{"on":true}"#).unwrap();
        let v: serde_json::Value = load_json(&p).unwrap().unwrap();
        assert_eq!(v["on"], true);
        std::fs::write(&p, b"{\"on\":tr").unwrap();
        let e = load_json::<serde_json::Value>(&p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("set aside"), "{e}");
        assert!(!p.exists());
        // The wrong shape is damaged too (another build's file), not "none".
        std::fs::write(&p, b"[1,2]").unwrap();
        assert!(load_json::<std::collections::BTreeMap<String, u8>>(&p).is_err());
        assert!(!p.exists());
    }

    #[test]
    fn a_file_is_replaced_whole_and_a_private_one_kept_to_its_user() {
        let dir = crate::testing::TempDir::new("fsutil");
        let p = dir.join("sub").join("state.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let leftovers = std::fs::read_dir(p.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file left");
        let k = dir.join("key");
        write_private(&k, b"secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&k).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn settings_are_copied_once_a_day_and_old_days_go() {
        let d = crate::testing::TempDir::new("daily-copy");
        std::fs::create_dir_all(d.join(super::SETTINGS_BACKUPS).join("2020-01-01")).unwrap();
        std::fs::create_dir_all(d.join(super::SETTINGS_BACKUPS).join("2020-01-02")).unwrap();
        std::fs::write(d.join("desk.json"), b"{}").unwrap();
        std::fs::write(d.join("audit.jsonl"), b"x").unwrap();
        let made = super::daily_settings_copy(&d, 2).unwrap().unwrap();
        assert!(made.join("desk.json").exists());
        assert!(!made.join("audit.jsonl").exists());
        assert!(
            super::daily_settings_copy(&d, 2).unwrap().is_none(),
            "once a day"
        );
        let days: Vec<_> = std::fs::read_dir(d.join(super::SETTINGS_BACKUPS))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(days.len(), 2, "the oldest day gone");
        assert!(!d.join(super::SETTINGS_BACKUPS).join("2020-01-01").exists());
        assert_eq!(super::utc_date(0), "1970-01-01");
        assert_eq!(super::utc_date(1_795_000_000), "2026-11-18");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_folder_is_this_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let t = crate::testing::TempDir::new("private-dir");
        let d = t.join("a/b");
        private_dir(&d).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        // Made earlier with a wider mode: narrowed.
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        // A link (systemd's StateDirectory under DynamicUser, a folder on another disk):
        // followed, and its target, ours, narrowed; the link itself left a link.
        let target = t.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = t.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(private_dir(&link).unwrap());
        assert_eq!(mode(&target), 0o700);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // A link to nothing: an error, nothing made.
        let dangling = t.join("dangling");
        std::os::unix::fs::symlink(t.join("missing"), &dangling).unwrap();
        assert!(private_dir(&dangling).is_err());
        assert!(!t.join("missing").exists());
        // Another account's folder: left as it is, and said so (only as a user who doesn't
        // own `/`; as root it would be narrowed, so not tried).
        use std::os::unix::fs::MetadataExt;
        let root = Path::new("/");
        let before = std::fs::metadata(root).unwrap();
        // SAFETY: geteuid has no preconditions and can't fail.
        let me = unsafe { libc::geteuid() };
        if me != 0 && before.uid() != me {
            assert!(!private_dir(root).unwrap());
            assert_eq!(std::fs::metadata(root).unwrap().mode(), before.mode());
        }
    }
}
