//! Files on disk: [`PathSink`] writing a received file safely (plain files only, resumable,
//! checked), and the file identities that keep an offered file the same file.

use super::*;

impl PathSink {
    /// A sink for `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        PathSink {
            path: path.into(),
            part: None,
        }
    }

    fn part_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".part");
        p.into()
    }

    fn version_path(&self) -> PathBuf {
        let mut p = self.part_path().into_os_string();
        p.push(".version");
        p.into()
    }
}

pub(super) fn file_identity(path: &Path) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        std::fs::File::open(path)
            .ok()
            .and_then(|f| handle_identity(&f))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// Windows: an open file's volume serial number and file index (bug run B: Windows had no
/// identity, so a folder on the way swapped for a junction after `media_send`'s check
/// still sent what it led to, the Node's own key among them).
#[cfg(windows)]
fn handle_identity(f: &std::fs::File) -> Option<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION; the handle is the open file's.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: as above; `info` is ours and sized.
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
    (ok != 0).then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

/// Whether the open file `f` is the one identified as `want` (`None`: not checked).
pub(super) fn is_identified(f: &std::fs::File, want: Option<FileIdentity>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        want.is_none_or(|(dev, ino)| f.metadata().is_ok_and(|m| m.dev() == dev && m.ino() == ino))
    }
    #[cfg(windows)]
    {
        want.is_none_or(|w| handle_identity(f) == Some(w))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (f, want);
        true
    }
}

pub(super) fn open_plain(
    path: &Path,
    o: &mut std::fs::OpenOptions,
    ours: bool,
) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(in_the_way(path));
    }
    let f = o.open(path).map_err(|e| {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            in_the_way(path)
        } else {
            e
        }
    })?;
    let m = f.metadata()?;
    #[cfg(unix)]
    let theirs = {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: no arguments.
        ours && m.uid() != unsafe { libc::geteuid() }
    };
    #[cfg(not(unix))]
    let theirs = {
        let _ = ours;
        false
    };
    if !m.is_file() || theirs {
        return Err(in_the_way(path));
    }
    Ok(f)
}

fn in_the_way(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{}: something else is in the way (a link, or not this program's file)",
            path.display()
        ),
    )
}

/// Reads a small file as text, as [`open_plain`] opens it.
fn read_plain(path: &Path, ours: bool) -> io::Result<String> {
    let mut s = String::new();
    open_plain(path, std::fs::OpenOptions::new().read(true), ours)?
        .take(1024)
        .read_to_string(&mut s)?;
    Ok(s)
}

impl FileSink for PathSink {
    fn held(&mut self) -> io::Result<Option<(u64, String)>> {
        let len = open_plain(
            &self.part_path(),
            std::fs::OpenOptions::new().read(true),
            true,
        )
        .and_then(|f| f.metadata())
        .map(|m| m.len())
        .ok();
        let version = read_plain(&self.version_path(), true).ok();
        Ok(len.zip(version.map(|v| v.trim().to_owned())))
    }

    fn restart(&mut self) -> io::Result<()> {
        self.part = None;
        let _ = std::fs::remove_file(self.part_path());
        let _ = std::fs::remove_file(self.version_path());
        Ok(())
    }

    fn begin(&mut self, info: &FileInfo) -> io::Result<()> {
        self.note_version(&info.version)
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if self.part.is_none() {
            if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let f = open_plain(
                &self.part_path(),
                std::fs::OpenOptions::new().create(true).append(true),
                true,
            )?;
            self.part = Some(f);
        }
        self.part.as_mut().expect("opened above").write_all(data)
    }

    fn finish(&mut self, info: &FileInfo) -> io::Result<()> {
        if let Some(f) = self.part.take() {
            f.sync_all()?;
        }
        let part = self.part_path();
        let got = sha256_file(&part, true)?;
        if got != info.version {
            self.restart()?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: SHA-256 mismatch", info.name),
            ));
        }
        std::fs::rename(&part, &self.path)?;
        let _ = std::fs::remove_file(self.version_path());
        Ok(())
    }
}

impl PathSink {
    /// Records the version being fetched next to the partial file, for resuming.
    pub(super) fn note_version(&self, version: &str) -> io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        open_plain(
            &self.version_path(),
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true),
            true,
        )?
        .write_all(version.as_bytes())
    }
}
