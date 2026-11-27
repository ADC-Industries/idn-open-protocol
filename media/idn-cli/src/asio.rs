//! `idn asio`: the ASIO drivers on this machine, and IDN's own driver listed for ASIO hosts
//! under more names, cwASIO's instances (spec/extensions/asio-interface.md §A.5, §A.6):
//! Linux in cwASIO's list (/etc/cwASIO), Windows in the registry (`HKLM\SOFTWARE\ASIO`,
//! where the installer lists the one instance "IDN").

use std::io;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: idn asio list
       idn asio register [NAME] [--description TEXT] [--library PATH]
       idn asio unregister [NAME | --all] [--library PATH]

  list: the ASIO drivers installed (Linux: cwASIO's list, /etc/cwASIO; Windows: the
  registry, HKLM\\SOFTWARE\\ASIO, one line per name), IDN's own marked.
  register and unregister need root (Linux: sudo) or an administrator's prompt (Windows).
  register: lists IDN's driver for ASIO hosts as NAME (default IDN, at most 31 bytes, no
  '/' on Linux or '\\' on Windows); another NAME is another instance, with its own
  settings (asio.json's \"instances\": {\"NAME\": {...}}) and its own source on the
  network. Its description: --description, else the one it has, else its name. On Windows
  the IDN installer must have installed the driver (the name only points at it).
  unregister: takes NAME (default IDN), or every registration of IDN's driver (--all),
  off the list; a NAME another driver holds is left alone. On Windows --all takes the
  installer's \"IDN\" off too (reinstalling IDN lists it again).
  --library (Linux only): IDN's driver library (default /usr/lib/cwASIO/libidn_asio.so, or
  the one next to this program). IDN_CWASIO_DIR: another list folder than /etc/cwASIO.";

/// The driver's own instance (idn-asio's `DEFAULT_INSTANCE`: not linked in here, its
/// library's exported entry points would come with it).
#[cfg(any(windows, target_os = "linux"))]
const DEFAULT_INSTANCE: &str = "IDN";

/// IDN's driver's class id (idn-asio's `CLSID`, object.rs).
#[cfg(windows)]
const IDN_CLSID: &str = "{35676BB1-B34F-4615-929E-85FC4BD40D3B}";

fn fail(m: &str) -> ! {
    eprintln!("idn asio: {m}");
    std::process::exit(2)
}

pub fn main() -> io::Result<()> {
    let args: Vec<String> = idn_open_protocol::cli::args().collect();
    let Some(cmd) = args.first() else {
        fail(USAGE);
    };
    // Errors said plainly, not as the error's debug form `idn` prints.
    let said = |r: io::Result<()>| {
        if let Err(e) = r {
            eprintln!("idn asio: {e}");
            std::process::exit(1);
        }
        Ok(())
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        "--version" | "version" => {
            println!("idn asio {}", idn_open_protocol::VERSION);
            Ok(())
        }
        "list" => said(list()),
        #[cfg(target_os = "linux")]
        "register" | "unregister" => said(linux::run(cmd == "register", &args[1..])),
        #[cfg(windows)]
        "register" | "unregister" => said(win::run(cmd == "register", &args[1..])),
        #[cfg(not(any(windows, target_os = "linux")))]
        "register" | "unregister" => fail("ASIO drivers are listed on Windows and Linux only"),
        other => fail(&format!("no command {other:?}\n{USAGE}")),
    }
}

/// `register` / `unregister`'s arguments.
#[cfg(any(windows, target_os = "linux"))]
struct Args {
    name: Option<String>,
    library: Option<String>,
    description: Option<String>,
    all: bool,
}

#[cfg(any(windows, target_os = "linux"))]
fn parse(register: bool, args: &[String]) -> Args {
    let mut a = Args {
        name: None,
        library: None,
        description: None,
        all: false,
    };
    let mut it = args.iter();
    while let Some(s) = it.next() {
        match s.as_str() {
            "--library" => {
                a.library = Some(
                    it.next()
                        .unwrap_or_else(|| fail("--library takes a path"))
                        .clone(),
                )
            }
            "--description" if register => {
                a.description = Some(
                    it.next()
                        .unwrap_or_else(|| fail("--description takes a text"))
                        .clone(),
                )
            }
            "--all" if !register => a.all = true,
            s if !s.starts_with('-') && a.name.is_none() => a.name = Some(s.to_owned()),
            other => fail(&format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if a.all && a.name.is_some() {
        fail("a NAME or --all, not both");
    }
    a
}

/// Whether a library's file is IDN's driver (`libidn_asio.so`, versioned too), by its
/// name or, through links, the file's.
#[cfg(target_os = "linux")]
fn is_idn_library(p: &Path) -> bool {
    let named = |p: &Path| {
        p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n == "libidn_asio.so" || n.starts_with("libidn_asio.so.")
        })
    };
    named(p) || std::fs::canonicalize(p).is_ok_and(|c| named(&c))
}

#[cfg(target_os = "linux")]
fn list() -> io::Result<()> {
    let dir = idn_asio_abi::registry_dir();
    let all = idn_asio_abi::drivers_in(&dir);
    if all.is_empty() {
        println!("no ASIO drivers listed in {}", dir.display());
    }
    for d in all {
        let idn_asio_abi::DriverId::Library { path, .. } = &d.id else {
            continue;
        };
        let mark = if is_idn_library(path) { "  (IDN)" } else { "" };
        println!(
            "{}{mark}\n  {}\n  {}",
            d.name,
            d.description,
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn list() -> io::Result<()> {
    let all = idn_asio_abi::drivers();
    if all.is_empty() {
        println!("no ASIO drivers registered");
    }
    // IDN's own by its class id; every name is a line, instances sharing one class id too.
    let idn = idn_asio_abi::Guid::parse(IDN_CLSID).map(idn_asio_abi::DriverId::Clsid);
    for d in all {
        let mark = if Some(&d.id) == idn.as_ref() {
            "  (IDN)"
        } else {
            ""
        };
        println!("{}{mark}\n  {}\n  {}", d.name, d.description, d.id);
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn list() -> io::Result<()> {
    fail("ASIO drivers are listed on Windows and Linux only")
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// Where the package puts IDN's driver (cwASIO's suggested folder).
    const INSTALLED: &str = "/usr/lib/cwASIO/libidn_asio.so";

    /// IDN's driver library: the one given, the package's, or the one next to this program.
    fn library(given: Option<PathBuf>) -> PathBuf {
        if let Some(p) = given {
            if !p.is_file() {
                fail(&format!("{}: no such file", p.display()));
            }
            return std::fs::canonicalize(&p).unwrap_or(p);
        }
        let beside = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("libidn_asio.so")));
        [Some(PathBuf::from(INSTALLED)), beside]
            .into_iter()
            .flatten()
            .find(|p| p.is_file())
            .unwrap_or_else(|| {
                fail(&format!(
                    "IDN's ASIO driver isn't installed ({INSTALLED}): give --library PATH"
                ))
            })
    }

    pub(super) fn run(register: bool, args: &[String]) -> io::Result<()> {
        let Args {
            name,
            library: lib,
            description,
            all,
        } = parse(register, args);
        let lib = lib.map(PathBuf::from);
        let dir = idn_asio_abi::registry_dir();
        if register {
            let name = name.unwrap_or_else(|| DEFAULT_INSTANCE.to_owned());
            let lib = library(lib);
            // (A description it has is kept: an update registering again changes nothing.)
            let description = description
                .or_else(|| (!dir.join(&name).join("description").exists()).then(|| name.clone()));
            idn_asio_abi::register(&dir, &name, &lib, description.as_deref())
                .map_err(|e| explain(e, &name, &dir))?;
            println!(
                "IDN's ASIO driver listed as {name:?} ({}) in {}",
                lib.display(),
                dir.display()
            );
            return Ok(());
        }
        // Unregistering: by the library each registration names (the given one, else any
        // of IDN's), so a package's update that moved the file still takes them off.
        let ours = |p: &Path| {
            lib.as_deref()
                .map_or_else(|| is_idn_library(p), |l| idn_asio_abi::same_library(p, l))
        };
        let listed: Vec<(String, PathBuf)> = idn_asio_abi::drivers_in(&dir)
            .into_iter()
            .filter_map(|d| match d.id {
                idn_asio_abi::DriverId::Library { name, path } => Some((name, path)),
                _ => None,
            })
            .collect();
        let names: Vec<(String, PathBuf)> = if all {
            listed.into_iter().filter(|(_, p)| ours(p)).collect()
        } else {
            let name = name.unwrap_or_else(|| DEFAULT_INSTANCE.to_owned());
            match listed.into_iter().find(|(n, _)| *n == name) {
                Some((n, p)) if ours(&p) => vec![(n, p)],
                Some((n, p)) => fail(&format!(
                    "{n:?} is another driver's ({}): left alone",
                    p.display()
                )),
                None => fail(&format!("no driver {name:?} in {}", dir.display())),
            }
        };
        for (n, p) in names {
            idn_asio_abi::unregister(&dir, &n, &p).map_err(|e| explain(e, &n, &dir))?;
            println!("{n:?} taken off {}", dir.display());
        }
        Ok(())
    }

    /// cwASIO's error numbers as words.
    fn explain(e: io::Error, name: &str, dir: &Path) -> io::Error {
        let why = match e.raw_os_error() {
            Some(libc::EINVAL) => format!(
                "{name:?} can't be a driver's name (1 to 31 bytes, no '/', not starting with '.')"
            ),
            Some(libc::EEXIST) => format!(
                "{name:?} is another driver's already (its own: sudo idn asio unregister \
                 {name:?} --library ITS-PATH, then register again)"
            ),
            Some(libc::ENODEV) => format!("no driver {name:?}"),
            Some(libc::EACCES) | Some(libc::EPERM) => {
                format!("not allowed to change {} (sudo?)", dir.display())
            }
            _ => return e,
        };
        io::Error::new(e.kind(), why)
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use idn_asio_abi::{DriverId, Guid};

    /// Where the names are, as said to the user.
    const LIST: &str = "HKLM\\SOFTWARE\\ASIO";

    pub(super) fn run(register: bool, args: &[String]) -> io::Result<()> {
        let Args {
            name,
            library,
            description,
            all,
        } = parse(register, args);
        if library.is_some() {
            fail(
                "--library doesn't apply on Windows: the names point at IDN's driver by its \
                 class id (the installer's)",
            );
        }
        let clsid = Guid::parse(IDN_CLSID).expect("IDN's class id");
        let listed = idn_asio_abi::drivers();
        // (Key names are compared as Windows does: without case.)
        let find = |n: &str| listed.iter().find(|d| d.name.eq_ignore_ascii_case(n));
        if register {
            let name = name.unwrap_or_else(|| DEFAULT_INSTANCE.to_owned());
            // (A description it has is kept: registering again changes nothing.)
            let description = description.or_else(|| find(&name).is_none().then(|| name.clone()));
            idn_asio_abi::register(&name, &clsid, description.as_deref())
                .map_err(|e| explain(e, &name))?;
            // As the registry keeps it (a name already there in other case: that one; the
            // host names the instance by it, and asio.json's "instances" must too).
            let kept = idn_asio_abi::drivers()
                .into_iter()
                .find(|d| d.name.eq_ignore_ascii_case(&name))
                .map_or(name.clone(), |d| d.name);
            println!("IDN's ASIO driver listed as {kept:?} ({IDN_CLSID}) in {LIST}");
            return Ok(());
        }
        let ours = DriverId::Clsid(clsid);
        let names: Vec<String> = if all {
            listed
                .iter()
                .filter(|d| d.id == ours)
                .map(|d| d.name.clone())
                .collect()
        } else {
            let name = name.unwrap_or_else(|| DEFAULT_INSTANCE.to_owned());
            match find(&name) {
                Some(d) if d.id == ours => vec![d.name.clone()],
                Some(d) => fail(&format!(
                    "{:?} is another driver's ({}): left alone",
                    d.name, d.id
                )),
                None => fail(&format!("no driver {name:?} in {LIST}")),
            }
        };
        if names.is_empty() {
            println!("IDN's driver isn't listed in {LIST}");
        }
        for n in names {
            idn_asio_abi::unregister(&n, &clsid).map_err(|e| explain(e, &n))?;
            println!("{n:?} taken off {LIST}");
        }
        Ok(())
    }

    /// The registry's error numbers as words.
    fn explain(e: io::Error, name: &str) -> io::Error {
        // ERROR_FILE_NOT_FOUND, ERROR_ACCESS_DENIED, ERROR_INVALID_NAME, ERROR_ALREADY_EXISTS.
        let why = match e.raw_os_error() {
            Some(123) => {
                format!("{name:?} can't be a driver's name (1 to 31 ASCII characters, no '\\')")
            }
            Some(183) => format!(
                "{name:?} is another driver's already (idn asio list shows whose; another \
                 NAME, or take that driver's off first)"
            ),
            Some(2) => format!("no driver {name:?}"),
            Some(5) => format!(
                "not allowed to change {LIST}: run it from a terminal opened as administrator"
            ),
            _ => return e,
        };
        io::Error::new(e.kind(), why)
    }
}
