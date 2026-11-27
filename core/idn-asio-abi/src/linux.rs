//! The installed ASIO drivers on Linux, listed as cwASIO lists them, and one loaded.
//!
//! - The list is a folder, `/etc/cwASIO`: each driver is a folder in it named after the
//!   driver (case matters), holding a `driver` file (the library's full path, which is
//!   also what loads it) and optionally a `description`. Each file is one line of text.
//! - Loading: `dlopen` the library, then its `instantiateDriver()`, which hands over the
//!   driver object with one reference held. The object's own `release` lets it go.
//! - The library is never closed: a driver's threads may outlive its object (IDN's own
//!   do), and cwASIO's own host keeps it loaded too.
//!
//! The writing side ([`register`], [`unregister`]) is what a driver's `registerDriver` and
//! `unregisterDriver` do (IDN's own driver, media/idn-asio).

use super::*;
use idn_common::sync::LockOk as _;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Where cwASIO lists the drivers.
pub const REGISTRY: &str = "/etc/cwASIO";

/// The list's folder: [`REGISTRY`], or `IDN_CWASIO_DIR` when set (a test's or a user's own
/// list).
pub fn registry_dir() -> PathBuf {
    PathBuf::from(
        idn_common::switches::value("IDN_CWASIO_DIR").unwrap_or_else(|| REGISTRY.to_owned()),
    )
}

/// The installed drivers, by name.
pub fn drivers() -> Vec<DriverEntry> {
    drivers_in(&registry_dir())
}

/// The drivers listed in `dir`, by name: its folders not starting with '.' that have a
/// `driver` file; anything else is passed over.
pub fn drivers_in(dir: &Path) -> Vec<DriverEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        // (A name that isn't UTF-8 can't be told to the driver as it is: passed over.)
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let sub = e.path();
        // (A link to a folder counts, as it would for cwASIO's own reader.)
        if !sub.is_dir() {
            continue;
        }
        let Some(library) = line(&sub.join("driver")) else {
            continue;
        };
        // A full path, as cwASIO lists it (a bare name would be looked for on the library
        // search path).
        if !library.starts_with('/') {
            continue;
        }
        let description = line(&sub.join("description")).unwrap_or_else(|| name.clone());
        out.push(DriverEntry {
            id: DriverId::Library {
                name: name.clone(),
                path: PathBuf::from(library),
            },
            name,
            description,
        });
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// A list file's text as cwASIO reads it: at most 1023 bytes, cut at the first newline (a
/// Windows line end or a trailing NUL cut too). None: missing, unreadable or empty.
fn line(path: &Path) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    std::fs::File::open(path)
        .ok()?
        .take(1023)
        .read_to_end(&mut buf)
        .ok()?;
    let end = buf.iter().position(|b| *b == b'\n').unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]);
    let s = s.trim_end_matches(['\r', '\0']);
    (!s.is_empty()).then(|| s.to_owned())
}

/// Driver `name` listed in `dir` (cwASIO's `registerDriver`): its folder (mode 0775) and
/// its `driver` file (0664) naming `library`, and a `description` if given.
///
/// Errors as cwASIO's: `EINVAL` for a name that can't be a folder of the list, `EEXIST`
/// when `name` is listed for another library; else what the file system says.
pub fn register(
    dir: &Path,
    name: &str,
    library: &Path,
    description: Option<&str>,
) -> io::Result<()> {
    check_name(name)?;
    // (At most 31 bytes: what a host's instance name and `getDriverName` can carry, §A.6;
    // a longer one was listed, then refused by the driver as an instance.)
    if name.len() > 31 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let lib = library.as_os_str().as_bytes();
    if lib.is_empty() || lib.contains(&b'\n') || lib.contains(&0) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let sub = dir.join(name);
    let file = sub.join("driver");
    if let Some(have) = line(&file)
        && !same_library(Path::new(&have), library)
    {
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o775)
        .create(&sub)?;
    // (No newline: the whole file is the line, whatever the reader.)
    write_file(&file, lib)?;
    if let Some(d) = description {
        let d: String = d
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        write_file(&sub.join("description"), d.as_bytes())?;
    }
    Ok(())
}

/// Driver `name` taken off the list in `dir` (cwASIO's `unregisterDriver`): its files and
/// its folder.
///
/// Errors as cwASIO's: `EINVAL` for a bad name, `ENODEV` when it isn't listed, `EACCES`
/// when it is listed for another library.
pub fn unregister(dir: &Path, name: &str, library: &Path) -> io::Result<()> {
    check_name(name)?;
    let sub = dir.join(name);
    let Some(have) = line(&sub.join("driver")) else {
        return Err(io::Error::from_raw_os_error(libc::ENODEV));
    };
    if !same_library(Path::new(&have), library) {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    std::fs::remove_file(sub.join("driver"))?;
    match std::fs::remove_file(sub.join("description")) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // The folder too; another file left in it (not the list's) keeps it, but without its
    // `driver` file the driver is no longer listed.
    let _ = std::fs::remove_dir(&sub);
    Ok(())
}

/// A name that is one folder of the list: not empty, no '/', no NUL, not hidden.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\0']) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(())
}

/// The same library: the same path, or the same file once links are followed.
pub fn same_library(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `bytes` as the whole of `path`, swapped in whole (a new file renamed over it: a host
/// reading the list meanwhile never finds it empty or cut, a link there is replaced, not
/// its target written). Nothing written when it holds that already.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        && std::fs::read(path).is_ok_and(|b| b == bytes)
    {
        return Ok(());
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".new-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o664)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// cwASIO's `struct cwASIODriver *instantiateDriver(void)`.
type Instantiate = unsafe extern "C" fn() -> *mut Driver;

/// The libraries opened, by path: each opened once and never closed.
fn libraries() -> &'static Mutex<HashMap<PathBuf, Instantiate>> {
    static LIBS: OnceLock<Mutex<HashMap<PathBuf, Instantiate>>> = OnceLock::new();
    LIBS.get_or_init(Default::default)
}

/// The loader's last error, as text.
fn dl_error() -> String {
    // SAFETY: the loader's own message for this thread, copied at once (null: none).
    unsafe {
        let e = libc::dlerror();
        if e.is_null() {
            "unknown error".into()
        } else {
            CStr::from_ptr(e).to_string_lossy().into_owned()
        }
    }
}

/// `library`'s `instantiateDriver`, the library opened the first time.
fn instantiate_of(library: &Path) -> io::Result<Instantiate> {
    let mut libs = libraries().lock_ok();
    if let Some(f) = libs.get(library) {
        return Ok(*f);
    }
    let path = CString::new(library.as_os_str().as_bytes())
        .map_err(|_| idn_common::err::invalid_input("the driver's path has a NUL in it"))?;
    // SAFETY: a NUL-terminated path. The handle is never closed (see the module's notes).
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LOCAL | libc::RTLD_NOW) };
    if h.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the driver {} didn't load: {}",
                library.display(),
                dl_error()
            ),
        ));
    }
    // SAFETY: an open library and a NUL-terminated name.
    let sym = unsafe { libc::dlsym(h, c"instantiateDriver".as_ptr()) };
    if sym.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} isn't an ASIO driver (no instantiateDriver)",
                library.display()
            ),
        ));
    }
    // SAFETY: cwASIO's entry point, of exactly this type.
    let f = unsafe { std::mem::transmute::<*mut c_void, Instantiate>(sym) };
    libs.insert(library.to_owned(), f);
    Ok(f)
}

/// Nothing is kept per driver on Linux (the library stays loaded for good).
pub(crate) struct Hold;

/// Loads driver `id` (a registration and its library): its object, one reference ours,
/// told its instance's name (cwASIO: a must on Linux, before `init`; a driver without
/// instances refuses it, and is used as it is).
pub(crate) fn load(id: &DriverId) -> io::Result<(*mut Driver, Hold)> {
    let DriverId::Library {
        name,
        path: library,
    } = id
    else {
        return Err(idn_common::err::invalid_input(format!(
            "{id}: not a driver library"
        )));
    };
    let f = instantiate_of(library)?;
    // SAFETY: the driver's own entry point; it hands over one reference (released by
    // `Loaded`'s drop).
    let drv = unsafe { f() };
    if drv.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the driver {} didn't start (instantiateDriver)",
                library.display()
            ),
        ));
    }
    // (At most 31 bytes, §A.6: a driver copies it into 32; refused by `register`.)
    if name.len() > 31 {
        // SAFETY: the reference just handed over, let go.
        unsafe { ((*(*drv).vtbl).release)(drv) };
        return Err(idn_common::err::invalid_input(format!(
            "{name:?}: an instance's name is at most 31 bytes"
        )));
    }
    if let Ok(n) = CString::new(name.as_str()) {
        // SAFETY: a live object's own method; a NUL-terminated name it copies.
        let _ = unsafe {
            ((*(*drv).vtbl).future)(drv, kcwASIOsetInstanceName, n.as_ptr() as *mut c_void)
        };
    }
    Ok((drv, Hold))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn the_list_is_read_as_cwasio_writes_it() {
        let d = idn_common::testing::TempDir::new("cwasio-list");
        put(&d.join("Zed/driver"), b"/usr/lib/cwASIO/libzed.so");
        put(&d.join("Zed/description"), b"Zed interface\nsecond line");
        // A line end, a Windows one, and no description.
        put(&d.join("alpha/driver"), b"/opt/alpha.so\n");
        put(&d.join("Beta/driver"), b"/opt/beta.so\r\n");
        // Passed over: hidden, no driver file, an empty one, a file not a folder.
        put(&d.join(".hidden/driver"), b"/opt/hidden.so");
        std::fs::create_dir_all(d.join("nodriver")).unwrap();
        put(&d.join("empty/driver"), b"");
        put(&d.join("loose"), b"/opt/loose.so");
        // Not a full path: passed over too.
        put(&d.join("rel/driver"), b"librel.so");
        // A long line: cut at 1023 bytes.
        let long = format!("/{}", "x".repeat(2000));
        put(&d.join("long/driver"), long.as_bytes());

        let all = drivers_in(&d);
        let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "long", "Zed"]);
        assert_eq!(
            all[0].id,
            DriverId::Library {
                name: "alpha".into(),
                path: "/opt/alpha.so".into()
            }
        );
        assert_eq!(all[0].description, "alpha");
        assert_eq!(
            all[1].id,
            DriverId::Library {
                name: "Beta".into(),
                path: "/opt/beta.so".into()
            }
        );
        // ("long:" and the path.)
        assert_eq!(all[2].id.to_string().len(), 5 + 1023);
        assert_eq!(all[3].description, "Zed interface");
        assert_eq!(
            all[3].id,
            DriverId::Library {
                name: "Zed".into(),
                path: "/usr/lib/cwASIO/libzed.so".into()
            }
        );
        // No folder: nothing.
        assert!(drivers_in(&d.join("missing")).is_empty());
    }

    #[test]
    fn registering_and_unregistering() {
        let d = idn_common::testing::TempDir::new("cwasio-reg");
        let lib = Path::new("/usr/lib/cwASIO/libidn_asio.so");
        let other = Path::new("/usr/lib/cwASIO/libother.so");
        let errno = |r: io::Result<()>| r.unwrap_err().raw_os_error();

        register(&d, "IDN", lib, Some("IDN")).unwrap();
        let all = drivers_in(&d);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "IDN");
        assert_eq!(
            all[0].id,
            DriverId::Library {
                name: "IDN".into(),
                path: lib.into()
            }
        );
        assert_eq!(all[0].description, "IDN");
        // Again for the same library: fine; for another: taken.
        register(&d, "IDN", lib, None).unwrap();
        assert_eq!(errno(register(&d, "IDN", other, None)), Some(libc::EEXIST));
        // Longer than an instance's name can be (31 bytes).
        assert_eq!(
            errno(register(&d, &"x".repeat(32), lib, None)),
            Some(libc::EINVAL)
        );
        register(&d, &"x".repeat(31), lib, None).unwrap();
        unregister(&d, &"x".repeat(31), lib).unwrap();
        // Names that aren't one folder of the list.
        for bad in ["", ".IDN", "a/b", "..", "a\0b"] {
            assert_eq!(errno(register(&d, bad, lib, None)), Some(libc::EINVAL));
        }
        // Taken off: not by another library, then by its own; then it isn't there.
        assert_eq!(errno(unregister(&d, "IDN", other)), Some(libc::EACCES));
        unregister(&d, "IDN", lib).unwrap();
        assert!(!d.join("IDN").exists());
        assert!(drivers_in(&d).is_empty());
        assert_eq!(errno(unregister(&d, "IDN", lib)), Some(libc::ENODEV));
    }

    #[test]
    fn a_library_that_isnt_there_is_reported_not_found() {
        let e = Loaded::load(&DriverId::Library {
            name: "Nothing".into(),
            path: "/nonexistent/libnothing.so".into(),
        })
        .err()
        .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let e = Loaded::load(&DriverId::Clsid(Guid::default()))
            .err()
            .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }
}
