//! Installing an update and coming back from one (G10, idn-node.md §N.23), for any IDN
//! product's programs: IDN Media's Node and IDN Industrial's (each a [`Product`]). How the
//! update arrives (the Site Manager's offer, the package's pieces) is the product's; this
//! is what follows. A helper (`<program> update-apply JOB`, a copy of the program as it
//! was) does the install, outside the service so that stopping the service doesn't stop
//! it: it stops the program, keeps the programs as they were, installs, starts it again,
//! and waits for the new one to check in with its site (or, with no site, to run a
//! minute). No check-in within ten minutes: it puts the programs back.
//!
//! Files in the program's state, `updates/`: `download/` (the package, fetched by
//! [`fetch_package`]); `staged.json` (installed at the next start); `pending.json` (the
//! helper's work); `healthy` (the new program's check-in); `result.json` (how it went);
//! `rollback/` (the programs as they were).

use crate::release;
use idn_common::cmd::RunWithin as _;
use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What differs between the products' programs.
#[derive(Debug, Clone, Copy)]
pub struct Product {
    /// The release product it installs (`release::MEDIA`, `release::INDUSTRIAL`).
    pub release: &'static str,
    /// Its Windows service and systemd unit (without `.service`).
    pub service: &'static str,
    /// The helper's file name in `updates/` (`.exe` added on Windows).
    pub helper: &'static str,
    /// On Linux the service runs unprivileged: a root helper (`idn-update.path`) installs.
    pub linux_root_helper: bool,
}

/// How long the new Node has to check in before the programs are put back.
pub const CHECK_IN: Duration = Duration::from_secs(600);

fn bad(m: impl std::fmt::Display) -> io::Error {
    idn_common::err::other(format!("update: {m}"))
}

/// A file the Node and its helper keep in the updates folder: none when it isn't there; a
/// damaged one set aside (`NAME.bad-SECS-PID-N`), said in the log, and read as none.
fn load(path: &Path) -> Option<Value> {
    idn_common::fsutil::load_json(path).unwrap_or_else(|e| {
        crate::log::log(&format!("update: {e}"));
        None
    })
}

/// The updates folder in a Node's state.
pub fn dir(state: &Path) -> PathBuf {
    state.join("updates")
}

/// Where an update is, for the report and the panels (`<update state=… version=… error=…
/// progress=…/>`): `downloading`, `staged` (at the next start), `installing`, `checking`
/// (the new Node running, not checked in yet), `done`, `rolled_back`, `failed`, or none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// `downloading`, `staged`, `installing`, `checking`, `done`, `rolled_back`, `failed`.
    pub state: String,
    /// The release's version.
    pub version: String,
    /// Why it failed or was rolled back.
    pub error: String,
    /// Downloaded, percent.
    pub progress: u8,
}

/// The state as the files say it (between restarts): what the helper left.
pub fn from_files(state: &Path) -> Option<Progress> {
    let d = dir(state);
    let read = |n: &str| load(&d.join(n));
    if let Some(p) = read("pending.json") {
        return Some(Progress {
            state: if p["installed"].as_bool() == Some(true) {
                "checking"
            } else {
                "installing"
            }
            .into(),
            version: p["version"].as_str().unwrap_or("").into(),
            ..Default::default()
        });
    }
    if let Some(s) = read("staged.json") {
        return Some(Progress {
            state: "staged".into(),
            version: s["version"].as_str().unwrap_or("").into(),
            ..Default::default()
        });
    }
    let r = read("result.json")?;
    Some(Progress {
        state: r["state"].as_str().unwrap_or("").into(),
        version: r["version"].as_str().unwrap_or("").into(),
        error: r["error"].as_str().unwrap_or("").into(),
        progress: 100,
    })
}

/// How the Node runs, for the helper to stop and start it: as the system's service, or by
/// hand (its command line, to start it again).
fn how_it_runs(service: bool) -> Value {
    json!({
        "service": service,
        "argv": std::env::args().collect::<Vec<_>>(),
        "pid": std::process::id(),
    })
}

/// Keeps `package` to install at the next start.
pub fn stage(state: &Path, version: &str, package: &Path, proof: &Value) -> io::Result<()> {
    let d = dir(state);
    let _ = std::fs::remove_file(d.join("result.json"));
    idn_common::fsutil::write(
        d.join("staged.json"),
        json!({"version": version, "package": package, "proof": proof}).to_string(),
    )
}

/// Starts installing `package` now: the helper, outside the service. The Node is stopped
/// by it.
pub fn install(
    product: &Product,
    state: &Path,
    version: &str,
    package: &Path,
    service: bool,
    build: &str,
    proof: &Value,
) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    install_from(
        product, &exe, state, version, package, service, build, proof,
    )
}

/// [`install`], with `program` (the product's own program, `idn-node`) as the helper and
/// its folder as the programs' (a Node hosted in another program: not the host's).
#[allow(clippy::too_many_arguments)]
pub fn install_from(
    product: &Product,
    program: &Path,
    state: &Path,
    version: &str,
    package: &Path,
    service: bool,
    build: &str,
    proof: &Value,
) -> io::Result<()> {
    let d = dir(state);
    std::fs::create_dir_all(&d)?;
    let exe = program.to_owned();
    let install_dir = exe.parent().ok_or_else(|| bad("where the programs are"))?;
    // The helper is this Node as it is now, out of the folder the install changes.
    let helper = d.join(helper_name(product));
    own_folder(&d, &helper, service)?;
    std::fs::copy(&exe, &helper)?;
    let pending = d.join("pending.json");
    for f in ["staged.json", "result.json", "healthy"] {
        let _ = std::fs::remove_file(d.join(f));
    }
    idn_common::fsutil::write(
        &pending,
        json!({
            "version": version, "package": package, "install_dir": install_dir, "from_build": build,
            "at": idn_common::epoch::secs(), "node": how_it_runs(service),
            "check_s": check_in_s(), "proof": proof,
        })
        .to_string(),
    )?;
    // A job no helper took isn't left to read as `installing` for ever.
    spawn_helper(product, &helper, &pending, service).inspect_err(|_| {
        let _ = std::fs::remove_file(&pending);
    })
}

/// The updates folder made, and, for a service on Windows, SYSTEM's and administrators'
/// only (a Node run by hand, as its user, keeps its own folder its user's): the helper
/// copied there runs as SYSTEM, and ProgramData lets ordinary users create files, so a
/// file put there (or swapped in) by one would run as SYSTEM. A helper file found there is
/// removed before the copy (it might be someone else's).
fn own_folder(d: &Path, helper: &Path, service: bool) -> io::Result<()> {
    std::fs::create_dir_all(d)?;
    #[cfg(not(windows))]
    let _ = service;
    #[cfg(windows)]
    if service {
        // (Its owner made SYSTEM too: a folder a user made first stayed theirs.)
        if idn_common::fsutil::lock_down_dir(d).is_err() {
            return Err(bad("couldn't make the updates folder the system's own"));
        }
    }
    let _ = std::fs::remove_file(helper);
    Ok(())
}

fn helper_name(product: &Product) -> String {
    if cfg!(windows) {
        format!("{}.exe", product.helper)
    } else {
        product.helper.to_owned()
    }
}

/// The check-in time (`IDN_UPDATE_CHECK_S` for tests), seconds.
fn check_in_s() -> u64 {
    idn_common::switches::value("IDN_UPDATE_CHECK_S")
        .and_then(|v| v.parse().ok())
        .unwrap_or(CHECK_IN.as_secs())
}

/// Puts the programs back as they were before the last update (the Site Manager's "Roll
/// back").
pub fn rollback(product: &Product, state: &Path, service: bool) -> io::Result<()> {
    rollback_from(product, &std::env::current_exe()?, state, service)
}

/// [`rollback`], with `program` as the helper and its folder as the programs' (as
/// [`install_from`]).
pub fn rollback_from(
    product: &Product,
    program: &Path,
    state: &Path,
    service: bool,
) -> io::Result<()> {
    let d = dir(state);
    // (Linux, the service: the root helper keeps them, out of this account's sight.)
    let roots = cfg!(target_os = "linux") && service && product.linux_root_helper;
    if !roots && !d.join("rollback").is_dir() {
        return Err(bad("no earlier programs kept here"));
    }
    let exe = program.to_owned();
    let helper = d.join(helper_name(product));
    own_folder(&d, &helper, service)?;
    std::fs::copy(&exe, &helper)?;
    let pending = d.join("rollback.json");
    idn_common::fsutil::write(
        &pending,
        json!({"rollback": true, "install_dir": exe.parent(), "node": how_it_runs(service)})
            .to_string(),
    )?;
    // A job no helper took isn't left to read as `installing` for ever.
    spawn_helper(product, &helper, &pending, service).inspect_err(|_| {
        let _ = std::fs::remove_file(&pending);
    })
}

fn spawn_helper(product: &Product, helper: &Path, job: &Path, service: bool) -> io::Result<()> {
    // Linux, the service: it runs unprivileged and can't install. It leaves the job and
    // a trigger; `idn-update.path` (installed by `idn-node install`) starts the root
    // helper, which checks the job itself (systemd-run was refused
    // by polkit, and tar and systemctl need root anyway).
    if cfg!(target_os = "linux") && service && product.linux_root_helper {
        let d = job.parent().ok_or_else(|| bad("the job's folder"))?;
        return std::fs::write(d.join(TRIGGER), "");
    }
    // Linux, a service running as root (IDN Industrial's): the helper in a unit of its own,
    // so stopping the service (its whole control group) doesn't stop it.
    let mut c = if cfg!(target_os = "linux") && service {
        let mut c = Command::new("systemd-run");
        c.arg("--collect")
            .arg(format!(
                "--unit={}-update-{}",
                product.service,
                std::process::id()
            ))
            .arg(helper);
        c
    } else {
        Command::new(helper)
    };
    c.arg("update-apply").arg(job);
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: not the Node's to end.
        // SAFETY: the child runs only setsid, which is async-signal-safe.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    c.spawn().map(drop)
}

/// The new Node, at its start and as it runs: once it has checked in with its site (or
/// run a minute with none), the helper is told (`healthy`).
/// `true` when it checked in just now (the update went in: the caller may start again
/// what the install stopped, such as the tray).
pub fn check_in(state: &Path, in_site: bool, connected: bool, up: Duration) -> bool {
    let d = dir(state);
    let Some(p) = load(&d.join("pending.json")) else {
        return false;
    };
    if p["installed"].as_bool() != Some(true) || d.join("healthy").exists() {
        return false;
    }
    if (in_site && connected) || (!in_site && up >= Duration::from_secs(60)) {
        return idn_common::fsutil::write(d.join("healthy"), idn_common::epoch::secs().to_string())
            .is_ok();
    }
    false
}

/// At the Node's start: an update staged for now is installed (the helper started; the
/// Node then ends, `true`).
pub fn at_start(product: &Product, state: &Path, service: bool, build: &str) -> bool {
    match std::env::current_exe() {
        Ok(exe) => at_start_from(product, &exe, state, service, build),
        Err(_) => false,
    }
}

/// [`at_start`], with `program` as the helper and its folder as the programs' (as
/// [`install_from`]).
pub fn at_start_from(
    product: &Product,
    program: &Path,
    state: &Path,
    service: bool,
    build: &str,
) -> bool {
    let d = dir(state);
    let Some(s) = load(&d.join("staged.json")) else {
        return false;
    };
    let _ = std::fs::remove_file(d.join("staged.json"));
    let (Some(v), Some(p)) = (s["version"].as_str(), s["package"].as_str()) else {
        return false;
    };
    install_from(
        product,
        program,
        state,
        v,
        Path::new(p),
        service,
        build,
        &s["proof"],
    )
    .is_ok()
}

/// How [`fetch_package`] fetches a package: the pieces asked for, the tries for each, the
/// pauses between tries, and how often it says how far it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOptions {
    /// Bytes asked for in each piece (at least 1; the server may send fewer, never more).
    pub piece: usize,
    /// Tries for each piece before the download fails (at least 1).
    pub tries: u32,
    /// The pause after a piece's first failed try; each pause after it twice as long, up
    /// to `max_pause`, each up to a quarter shorter at random (machines that failed
    /// together, their Site Manager restarting, don't all ask again at once).
    pub pause: Duration,
    /// The longest pause between tries.
    pub max_pause: Duration,
    /// Percent between progress reports: the next is said once the download is this much
    /// further than the last one said (0: after every piece). 100 is always said, once.
    pub progress_step: u8,
}

impl Default for FetchOptions {
    /// 1 MiB pieces (any Site Manager's limit takes them), 4 tries each, pauses from 2 s
    /// up to 8 s, progress every 5 %.
    fn default() -> FetchOptions {
        FetchOptions {
            piece: 1 << 20,
            tries: 4,
            pause: Duration::from_secs(2),
            max_pause: Duration::from_secs(8),
            progress_step: 5,
        }
    }
}

/// Fetches the package `p` (this system's, from a signed manifest) in pieces into the
/// updates folder (`updates/download/`, under its own file name), checks it, and gives its
/// path. `fetch(offset, len)` gets up to `len` bytes of the package from `offset` (the Site
/// Manager's panel, a link's request: the product's own way); `on_progress` is told the
/// percent done as `opts` says.
///
/// The rules: each piece is tried up to `opts.tries` times (an error or an empty piece is
/// a failed try; the pauses as [`FetchOptions`] says), then the download fails with the
/// last error, its kind kept. A piece longer than asked fails it at once (the server
/// isn't sending this package). The pieces go to `NAME.part`, synced to the disk, then
/// checked against the manifest (size and SHA-256, as read back from the disk) and only
/// then renamed to `NAME`: a download that failed or doesn't match leaves no file behind
/// that looks like a package.
pub fn fetch_package(
    state: &Path,
    p: &release::Package,
    opts: &FetchOptions,
    mut fetch: impl FnMut(u64, usize) -> io::Result<Vec<u8>>,
    mut on_progress: impl FnMut(u8),
) -> io::Result<PathBuf> {
    let d = dir(state).join("download");
    std::fs::create_dir_all(&d)?;
    let path = d.join(package_file_name(p));
    let mut part_name = path.file_name().unwrap_or_default().to_os_string();
    part_name.push(".part");
    let part = path.with_file_name(part_name);
    let done = download(&part, p, opts, &mut fetch, &mut on_progress)
        .and_then(|()| release::check_package(&part, p))
        .and_then(|()| std::fs::rename(&part, &path));
    if let Err(e) = done {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(path)
}

/// The package's file name as its manifest gives it, its last part only (never a folder);
/// `package` when it gives none.
fn package_file_name(p: &release::Package) -> String {
    Path::new(&p.name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && !n.ends_with(".part"))
        .unwrap_or_else(|| "package".into())
}

/// [`fetch_package`]'s pieces, written to `part`.
fn download(
    part: &Path,
    p: &release::Package,
    opts: &FetchOptions,
    fetch: &mut impl FnMut(u64, usize) -> io::Result<Vec<u8>>,
    on_progress: &mut impl FnMut(u8),
) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(part)?;
    let mut pause = idn_common::backoff::Backoff::new(opts.pause, opts.max_pause).jitter(0.25);
    let (mut got, mut said) = (0u64, 0u8);
    while got < p.size {
        let want = (p.size - got).min(opts.piece.max(1) as u64) as usize;
        let piece = fetch_piece(fetch, got, want, opts.tries, &mut pause)?;
        if piece.len() > want {
            return Err(idn_common::err::invalid_data(format!(
                "update: the piece at byte {got} is {} bytes, {want} asked for",
                piece.len()
            )));
        }
        f.write_all(&piece)?;
        got += piece.len() as u64;
        let pc = (got * 100 / p.size) as u8;
        if opts.progress_step == 0
            || pc >= said.saturating_add(opts.progress_step)
            || (pc == 100 && said < 100)
        {
            said = pc;
            on_progress(pc);
        }
    }
    f.sync_all()?;
    // (An empty package: nothing fetched, all of it there.)
    if said < 100 {
        on_progress(100);
    }
    Ok(())
}

/// One piece, tried up to `tries` times with `pause`'s waits between: the last try's error
/// when none gave it.
fn fetch_piece(
    fetch: &mut impl FnMut(u64, usize) -> io::Result<Vec<u8>>,
    offset: u64,
    len: usize,
    tries: u32,
    pause: &mut idn_common::backoff::Backoff,
) -> io::Result<Vec<u8>> {
    let tries = tries.max(1);
    pause.reset();
    let mut last = idn_common::err::other("no try");
    for n in 1..=tries {
        match fetch(offset, len) {
            Ok(b) if !b.is_empty() => return Ok(b),
            Ok(_) => last = idn_common::err::other("an empty piece"),
            Err(e) => last = e,
        }
        if n < tries {
            std::thread::sleep(pause.next());
        }
    }
    Err(io::Error::new(
        last.kind(),
        format!("update: the piece at byte {offset}, {tries} tries: {last}"),
    ))
}

// ---- The helper (`idn-node update-apply JOB`, or `update-root DIR` as root). ----

/// The file the Linux service leaves for `idn-update.path`.
pub const TRIGGER: &str = "apply";

/// Opens `path` for writing without following a link: the folder belongs to the Node's
/// account, and the root helper writes in it (a link there was a write as root anywhere).
/// Written afresh (whatever was there removed, then made new: a hard link to a root file
/// isn't truncated) and given to the folder's owner; appended to only as a plain file
/// with no other link (never waiting on a FIFO).
fn open_nofollow(path: &Path, append: bool) -> io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true);
    if append {
        o.create(true).append(true);
    } else {
        let _ = std::fs::remove_file(path);
        o.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o640);
    }
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = f.metadata()?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(bad("not a plain file"));
        }
        // Made by root in the Node's folder: the folder's owner's, as before.
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if m.uid() == 0
            && let Ok(d) = std::fs::metadata(dir)
            && d.uid() != 0
        {
            let _ = std::os::unix::fs::fchown(&f, Some(d.uid()), Some(d.gid()));
        }
    }
    Ok(f)
}

/// Writes `path` whole: a file of its own beside it (`NAME.new`, opened as
/// [`open_nofollow`] does), then renamed over it (a rename replaces a link, never follows
/// it), so the Node reading the job or the result never finds half of one.
fn write_file(path: &Path, b: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    let tmp = path.with_file_name(name);
    let mut f = open_nofollow(&tmp, false)?;
    f.write_all(b)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Reads a plain file with no other link (at most 1 MB), not through a link and never
/// waiting on a FIFO.
fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o.open(path)?;
    let m = f.metadata()?;
    #[cfg(unix)]
    let one_link = {
        use std::os::unix::fs::MetadataExt;
        m.nlink() == 1
    };
    #[cfg(not(unix))]
    let one_link = true;
    if !m.is_file() || !one_link {
        return Err(bad("not a plain file"));
    }
    let mut b = Vec::new();
    f.take(1 << 20).read_to_end(&mut b)?;
    Ok(b)
}

fn log(d: &Path, m: &str) {
    use std::io::Write;
    if let Ok(mut f) = open_nofollow(&d.join("update.log"), true) {
        let _ = writeln!(f, "{} {m}", idn_common::epoch::secs());
    }
}

/// Runs an update or a rollback `job` (the helper's whole life).
pub fn run_helper(product: &Product, job: &Path) -> io::Result<()> {
    let d = job
        .parent()
        .ok_or_else(|| bad("the job's folder"))?
        .to_owned();
    let j: Value = serde_json::from_slice(&std::fs::read(job)?).map_err(bad)?;
    let install_dir = PathBuf::from(
        j["install_dir"]
            .as_str()
            .ok_or_else(|| bad("install_dir"))?,
    );
    if j["rollback"].as_bool() == Some(true) {
        return run_job(
            product,
            &d,
            job,
            &j,
            &install_dir,
            &d.join("rollback"),
            Path::new(""),
        );
    }
    // Checked here too: on Windows this runs as SYSTEM, and the
    // package's name comes from a file in the Node's folder. The copy goes to a folder of
    // this account's own, inside the updates folder `own_folder` locked down (not the
    // system's temporary folder: a user could make `idn-update-<pid>` there first, a
    // process id being guessable, and as its owner swap the package after its check,
    // bug round 2026-11-09).
    let own = d.join("checked");
    std::fs::create_dir_all(&own)?;
    match verified_package(product, &j, &own) {
        Ok(package) => run_job(
            product,
            &d,
            job,
            &j,
            &install_dir,
            &d.join("rollback"),
            &package,
        ),
        Err(e) => {
            log(&d, &format!("refused: {e}"));
            let _ = std::fs::remove_file(job);
            write_result(
                &d,
                j["version"].as_str().unwrap_or(""),
                "failed",
                &format!("the update didn't check out: {e}"),
            )
        }
    }
}

/// The root helper on Linux (`idn-node update-root DIR`, started by `idn-update.path`):
/// the job in the Node's update folder `dir`, trusted for nothing. The programs' folder is
/// this program's own; the package is copied here and checked against the release's
/// signature with this build's keys; the programs kept for a way back stay in root's own
/// folder.
pub fn run_root(product: &Product, dir: &Path) -> io::Result<()> {
    // `dir` is in the Node's own folder: its account could make it a link to any folder,
    // and root would then write and remove files there. Opened without following a link,
    // checked, and worked in from here on by relative paths (the folder itself, whatever
    // its name points to later). (Its parent, /var/lib/<node>, is in root's /var/lib.)
    #[cfg(unix)]
    idn_common::fsutil::OwnedDir::open(dir)
        .and_then(|d| d.enter())
        .map_err(|e| bad(format!("the updates folder: {e}")))?;
    #[cfg(unix)]
    let dir = Path::new(".");
    let _ = std::fs::remove_file(dir.join(TRIGGER));
    let own = std::env::var_os("STATE_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/idn-update"));
    std::fs::create_dir_all(&own)?;
    let install_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| bad("where the programs are"))?
        .to_owned();
    let keep = own.join("rollback");
    // Installed from a Linux package: dpkg's files, updated by the package manager
    // (the Node refuses such offers; root does too, whatever a job file says).
    if cfg!(target_os = "linux")
        && (install_dir.starts_with("/usr/lib") || install_dir.starts_with("/usr/bin"))
    {
        for n in ["rollback.json", "pending.json"] {
            let _ = std::fs::remove_file(dir.join(n));
        }
        return write_result(
            dir,
            "",
            "failed",
            "installed from a package: the package manager updates it",
        );
    }
    let (job, j) = ["rollback.json", "pending.json"]
        .iter()
        .find_map(|n| {
            let p = dir.join(n);
            let j: Value = serde_json::from_slice(&read_file(&p).ok()?).ok()?;
            Some((p, j))
        })
        .ok_or_else(|| bad("no update job"))?;
    // The Node is the system's service, whatever the job says: a pid to kill or a command
    // to start, from a file the Node's account writes, would run as root.
    let mut j = j;
    j["node"] = serde_json::json!({ "service": true });
    if j["rollback"].as_bool() == Some(true) {
        return run_job(product, dir, &job, &j, &install_dir, &keep, Path::new(""));
    }
    let checked = verified_package(product, &j, &own);
    match checked {
        Ok(package) => run_job(product, dir, &job, &j, &install_dir, &keep, &package),
        Err(e) => {
            log(dir, &format!("refused: {e}"));
            let _ = std::fs::remove_file(&job);
            write_result(
                dir,
                j["version"].as_str().unwrap_or(""),
                "failed",
                &format!("the update didn't check out: {e}"),
            )
        }
    }
}

/// The job's package, copied into `own` (a folder the Node's account can't write) and
/// checked against the release's signature with this build's keys: the copy's path.
fn verified_package(product: &Product, j: &Value, own: &Path) -> io::Result<PathBuf> {
    let b64 = |k: &str| {
        idn_common::text::unbase64_r(j["proof"][k].as_str().unwrap_or(""))
            .map_err(|_| bad(format!("{k}: not base64")))
    };
    let m = release::verify(
        &b64("manifest")?,
        &b64("signature")?,
        &release::trusted_keys(),
    )?;
    if j["version"].as_str() != Some(m.version.as_str()) {
        return Err(bad("the job's version isn't the signed release's"));
    }
    if m.product != product.release {
        return Err(bad(format!(
            "a release of {}, not of {}",
            m.product, product.release
        )));
    }
    // Not an older release than one this helper has installed (its signing time, kept in
    // its own folder): an old signed release, its holes known, put back by whoever can
    // write a job (the Node's account) — deep bug run 2026-11-09. The way back to the
    // programs as they were is the rollback, not an older package.
    // (None yet: any release. Damaged: refused rather than read as none, which let any old
    // release in; written whole, it is damaged only by hand.)
    let newest = own.join("newest-installed");
    let high = match std::fs::read_to_string(&newest) {
        Ok(t) => t.trim().parse::<u64>().map_err(|_| {
            bad(format!(
                "{} is damaged: remove it to take any release again",
                newest.display()
            ))
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };
    if m.created < high {
        return Err(bad(format!(
            "{} is older than a release already installed here: roll back instead",
            m.version
        )));
    }
    let p = m.package(&release::platform()).cloned().ok_or_else(|| {
        bad(format!(
            "the release has nothing for {}",
            release::platform()
        ))
    })?;
    idn_common::fsutil::write(&newest, m.created.to_string())?;
    // A copy of root's own, checked: the Node's account can't change it after.
    let from = PathBuf::from(j["package"].as_str().unwrap_or(""));
    let src = {
        use std::io::Read;
        let mut o = std::fs::OpenOptions::new();
        o.read(true);
        // Not a link, and not blocking on open (a FIFO would hold root's helper for ever):
        // a plain file only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut f = o.open(&from)?;
        if !f.metadata()?.is_file() {
            return Err(bad("the package isn't a file"));
        }
        let mut b = Vec::new();
        f.by_ref().take(p.size + 1).read_to_end(&mut b)?;
        b
    };
    let to = own.join("package");
    let _ = std::fs::remove_file(&to);
    std::fs::write(&to, &src)?;
    release::check_package(&to, &p)?;
    Ok(to)
}

fn run_job(
    product: &Product,
    d: &Path,
    job: &Path,
    j: &Value,
    install_dir: &Path,
    keep: &Path,
    package: &Path,
) -> io::Result<()> {
    let (d, install_dir) = (d.to_owned(), install_dir.to_owned());
    let node = &j["node"];
    if j["rollback"].as_bool() == Some(true) {
        log(&d, "rolling back by hand");
        stop_node(product, node);
        let r = restore(keep, &install_dir);
        start_node(product, node);
        let _ = std::fs::remove_file(job);
        let (state, error) = match r {
            Ok(()) => ("rolled_back", "by hand".to_owned()),
            Err(e) => ("failed", format!("rolling back: {e}")),
        };
        return write_result(&d, "", state, &error);
    }
    let version = j["version"].as_str().unwrap_or("").to_owned();
    log(&d, &format!("installing {version}"));
    stop_node(product, node);
    // The programs as they are, to come back to.
    let _ = std::fs::remove_dir_all(keep);
    if let Err(e) = keep_programs(package, &install_dir, keep) {
        start_node(product, node);
        let _ = std::fs::remove_file(job);
        return write_result(
            &d,
            &version,
            "failed",
            &format!("keeping the programs as they were: {e}"),
        );
    }
    if let Err(e) = put_in(package, &install_dir) {
        log(&d, &format!("install failed: {e}"));
        let _ = restore(keep, &install_dir);
        start_node(product, node);
        let _ = std::fs::remove_file(job);
        return write_result(&d, &version, "failed", &format!("installing: {e}"));
    }
    let mut j2 = j.clone();
    j2["installed"] = json!(true);
    write_file(job, j2.to_string().as_bytes())?;
    start_node(product, node);
    // The new Node's check-in.
    let wait = Duration::from_secs(j["check_s"].as_u64().unwrap_or(CHECK_IN.as_secs()));
    let start = Instant::now();
    while start.elapsed() < wait {
        if d.join("healthy").exists() {
            log(&d, &format!("{version} checked in"));
            let _ = std::fs::remove_file(job);
            let _ = std::fs::remove_file(d.join("healthy"));
            return write_result(&d, &version, "done", "");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    log(
        &d,
        &format!(
            "{version} didn't check in within {} s: putting the programs back",
            wait.as_secs()
        ),
    );
    stop_node(product, node);
    let r = restore(keep, &install_dir);
    start_node(product, node);
    let _ = std::fs::remove_file(job);
    let why = format!(
        "the new version didn't check in within {}{}",
        span(wait),
        r.err()
            .map(|e| format!("; and putting the old back: {e}"))
            .unwrap_or_default()
    );
    write_result(&d, &version, "rolled_back", &why)
}

/// A wait in words: whole minutes as minutes, else seconds.
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        60 => "a minute".into(),
        s if s >= 60 && s.is_multiple_of(60) => format!("{} minutes", s / 60),
        s => format!("{s} s"),
    }
}

/// Writes the helper's `result.json` (how it went), and logs it.
pub fn write_result(d: &Path, version: &str, state: &str, error: &str) -> io::Result<()> {
    log(d, &format!("result: {state} {version} {error}"));
    write_file(
        &d.join("result.json"),
        json!({"version": version, "state": state, "error": error, "at": idn_common::epoch::secs()})
            .to_string()
            .as_bytes(),
    )
}

/// Installs a package over the programs' folder: Windows, the installer (`/S /UPDATE`);
/// elsewhere, a `.tar.gz` of the programs.
fn put_in(package: &Path, install_dir: &Path) -> io::Result<()> {
    let ok = if cfg!(windows) {
        let mut c = Command::new(package);
        c.arg("/S").arg("/UPDATE");
        // NSIS takes `/D=` last and as it is: quoted (a path with spaces, as Program
        // Files), it was ignored.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::raw_arg(
            &mut c,
            format!("/D={}", install_dir.display()),
        );
        c.status()?.success()
    } else {
        // Unlinked first: a running program's file is replaced, not written over.
        Command::new("tar")
            .arg("-xzf")
            .arg(package)
            .arg("--unlink-first")
            .arg("-C")
            .arg(install_dir)
            .status_within(std::time::Duration::from_secs(600))?
            .success()
    };
    if ok {
        Ok(())
    } else {
        Err(bad("the installer failed"))
    }
}

/// Keeps what the install will replace, in `keep`. Linux: the files the package holds,
/// as they are in `install_dir` (a manual install lives in /usr/local/bin, among other
/// programs: copying the whole folder, and putting it all back on a rollback, reverted
/// whatever else changed there). Windows: the installer writes the product's own folder,
/// kept whole.
fn keep_programs(package: &Path, install_dir: &Path, keep: &Path) -> io::Result<()> {
    if cfg!(windows) {
        return copy_dir(install_dir, keep);
    }
    let out = Command::new("tar")
        .arg("-tzf")
        .arg(package)
        .output_within(std::time::Duration::from_secs(600))?;
    if !out.status.success() {
        return Err(bad("the package's list of files"));
    }
    std::fs::create_dir_all(keep)?;
    for name in String::from_utf8_lossy(&out.stdout).lines() {
        let rel = Path::new(name.trim_start_matches("./"));
        // Only plain paths inside the folder (no "..", nothing absolute).
        if rel.as_os_str().is_empty()
            || !rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            continue;
        }
        let src = install_dir.join(rel);
        if src.symlink_metadata().is_ok_and(|m| m.is_file()) {
            let dst = keep.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn restore(keep: &Path, install_dir: &Path) -> io::Result<()> {
    copy_dir(keep, install_dir)
}

/// Copies a folder over another, files replaced rather than written over.
pub fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let t = e.file_type()?;
        if t.is_dir() {
            copy_dir(&src, &dst)?;
        } else if t.is_file() {
            // Replaced, not written over (a program may still run from it).
            let _ = std::fs::remove_file(&dst);
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn stop_node(product: &Product, node: &Value) {
    if node["service"].as_bool() == Some(true) {
        let _ = if cfg!(windows) {
            Command::new("net")
                .args(["stop", product.service])
                .status_within(std::time::Duration::from_secs(120))
        } else {
            Command::new("systemctl")
                .args(["stop", &format!("{}.service", product.service)])
                .status_within(std::time::Duration::from_secs(120))
        };
        return;
    }
    // By hand: ended (it stops its services), then waited for.
    // One process only: 0 and -1 would signal a group or everything the account reaches.
    let Some(pid) = node["pid"]
        .as_u64()
        .filter(|p| (2..=i32::MAX as u64).contains(p))
    else {
        return;
    };
    #[cfg(unix)]
    // SAFETY: kill takes no pointers.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
        for _ in 0..100 {
            if libc::kill(pid as i32, 0) != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status_within(std::time::Duration::from_secs(30));
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn start_node(product: &Product, node: &Value) {
    if node["service"].as_bool() == Some(true) {
        let _ = if cfg!(windows) {
            Command::new("net")
                .args(["start", product.service])
                .status_within(std::time::Duration::from_secs(120))
        } else {
            Command::new("systemctl")
                .args(["start", &format!("{}.service", product.service)])
                .status_within(std::time::Duration::from_secs(120))
        };
        return;
    }
    let argv: Vec<String> = node["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let Some((exe, args)) = argv.split_first() else {
        return;
    };
    let mut c = Command::new(exe);
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the child runs only setsid, which is async-signal-safe.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let _ = c.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_common::testing::TempDir;

    #[test]
    fn where_an_update_is_and_its_check_in() {
        let state = TempDir::new("update");
        let d = dir(&state);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(from_files(&state), None);
        // Staged for the next start.
        stage(&state, "0.3.0", Path::new("/x/pkg.tar.gz"), &Value::Null).unwrap();
        assert_eq!(from_files(&state).unwrap().state, "staged");
        std::fs::remove_file(d.join("staged.json")).unwrap();
        // The helper at work: installing, then the new Node checking in.
        let pending =
            |installed: bool| json!({"version": "0.3.0", "installed": installed}).to_string();
        std::fs::write(d.join("pending.json"), pending(false)).unwrap();
        assert_eq!(from_files(&state).unwrap().state, "installing");
        // Not installed yet: no check-in, even connected.
        check_in(&state, true, true, Duration::from_secs(5));
        assert!(!d.join("healthy").exists());
        std::fs::write(d.join("pending.json"), pending(true)).unwrap();
        assert_eq!(from_files(&state).unwrap().state, "checking");
        // In a site: only once connected to it.
        check_in(&state, true, false, Duration::from_secs(300));
        assert!(!d.join("healthy").exists());
        check_in(&state, true, true, Duration::from_secs(1));
        assert!(d.join("healthy").exists());
        // With no site: after a minute running.
        std::fs::remove_file(d.join("healthy")).unwrap();
        check_in(&state, false, false, Duration::from_secs(30));
        assert!(!d.join("healthy").exists());
        check_in(&state, false, false, Duration::from_secs(61));
        assert!(d.join("healthy").exists());
        // The helper's result.
        std::fs::remove_file(d.join("pending.json")).unwrap();
        write_result(&d, "0.3.0", "rolled_back", "no check-in").unwrap();
        let p = from_files(&state).unwrap();
        assert_eq!(
            (p.state.as_str(), p.version.as_str()),
            ("rolled_back", "0.3.0")
        );
        assert_eq!(span(Duration::from_secs(600)), "10 minutes");
        assert_eq!(span(Duration::from_secs(60)), "a minute");
        assert_eq!(span(Duration::from_secs(30)), "30 s");
        // A folder copied over what is there.
        let (a, b) = (state.join("a"), state.join("b"));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::write(a.join("x"), b"new").unwrap();
        std::fs::write(a.join("sub").join("y"), b"y").unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("x"), b"old").unwrap();
        copy_dir(&a, &b).unwrap();
        assert_eq!(std::fs::read(b.join("x")).unwrap(), b"new");
        assert_eq!(std::fs::read(b.join("sub").join("y")).unwrap(), b"y");
    }

    #[test]
    fn a_damaged_state_file_is_set_aside_and_read_as_none() {
        let state = TempDir::new("update-damaged");
        let d = dir(&state);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("staged.json"), b"{\"version\": \"0.3").unwrap();
        assert_eq!(from_files(&state), None);
        assert!(!d.join("staged.json").exists());
        assert!(std::fs::read_dir(&d).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("staged.json.bad-")
        }));
    }

    /// A package of `n` bytes and its manifest entry.
    fn package(n: usize) -> (Vec<u8>, release::Package) {
        let bytes: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let p = release::Package {
            platform: release::platform(),
            name: "../idn-setup.exe".into(),
            size: n as u64,
            sha256: crate::hash::sha256_hex(&bytes),
        };
        (bytes, p)
    }

    fn quick() -> FetchOptions {
        FetchOptions {
            piece: 1000,
            pause: Duration::from_millis(1),
            max_pause: Duration::from_millis(2),
            ..FetchOptions::default()
        }
    }

    /// Serves `bytes` as a Site Manager would.
    fn serve(bytes: &[u8], offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let at = (offset as usize).min(bytes.len());
        Ok(bytes[at..(at + len).min(bytes.len())].to_vec())
    }

    #[test]
    fn a_package_is_fetched_in_pieces_through_failed_tries() {
        let state = TempDir::new("update-fetch");
        let (bytes, p) = package(3500);
        // Each piece fails twice (an error, then an empty piece), then comes.
        let (mut calls, mut asked) = (0u32, Vec::new());
        let mut said = Vec::new();
        let path = fetch_package(
            &state,
            &p,
            &FetchOptions {
                progress_step: 0,
                ..quick()
            },
            |offset, len| {
                calls += 1;
                match calls % 3 {
                    1 => Err(io::Error::other("the Site Manager restarting")),
                    2 => Ok(Vec::new()),
                    _ => {
                        asked.push((offset, len));
                        serve(&bytes, offset, len)
                    }
                }
            },
            |pc| said.push(pc),
        )
        .unwrap();
        // Its own name only (never a folder), the bytes as served, no part left.
        assert_eq!(path, dir(&state).join("download").join("idn-setup.exe"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            !dir(&state)
                .join("download")
                .join("idn-setup.exe.part")
                .exists()
        );
        assert_eq!(calls, 12);
        // Never more asked for than the package holds.
        assert_eq!(asked, [(0, 1000), (1000, 1000), (2000, 1000), (3000, 500)]);
        assert_eq!(said, [28, 57, 85, 100]);
    }

    #[test]
    fn progress_is_said_every_step_and_100_once() {
        let state = TempDir::new("update-progress");
        let (bytes, p) = package(10_000);
        let mut said = Vec::new();
        let opts = FetchOptions {
            progress_step: 30,
            ..quick()
        };
        fetch_package(
            &state,
            &p,
            &opts,
            |o, l| serve(&bytes, o, l),
            |pc| said.push(pc),
        )
        .unwrap();
        assert_eq!(said, [30, 60, 90, 100]);
        // An empty package: 100 only.
        let (_, empty) = package(0);
        let mut said = Vec::new();
        fetch_package(
            &state,
            &empty,
            &opts,
            |_, _| unreachable!(),
            |pc| said.push(pc),
        )
        .unwrap();
        assert_eq!(said, [100]);
    }

    #[test]
    fn a_fetch_that_keeps_failing_fails_with_its_error() {
        let state = TempDir::new("update-fails");
        let (_, p) = package(3500);
        let mut calls = 0;
        let e = fetch_package(
            &state,
            &p,
            &quick(),
            |_, _| {
                calls += 1;
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "403 Forbidden",
                ))
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(calls, FetchOptions::default().tries);
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("403 Forbidden"), "{e}");
        let download = dir(&state).join("download");
        assert_eq!(
            std::fs::read_dir(&download).unwrap().count(),
            0,
            "nothing left"
        );
    }

    #[test]
    fn a_package_that_doesnt_match_its_release_is_refused() {
        let state = TempDir::new("update-bad-hash");
        let (bytes, mut p) = package(3500);
        let download = dir(&state).join("download");
        // Other bytes than the release signed (its hash), then a piece longer than asked.
        p.sha256 = crate::hash::sha256_hex(b"another package");
        let e =
            fetch_package(&state, &p, &quick(), |o, l| serve(&bytes, o, l), |_| {}).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_dir(&download).unwrap().count(), 0);
        let (_, p) = package(3500);
        let e =
            fetch_package(&state, &p, &quick(), |_, l| Ok(vec![0u8; l + 1]), |_| {}).unwrap_err();
        assert!(e.to_string().contains("asked for"), "{e}");
        assert_eq!(std::fs::read_dir(&download).unwrap().count(), 0);
    }
}
