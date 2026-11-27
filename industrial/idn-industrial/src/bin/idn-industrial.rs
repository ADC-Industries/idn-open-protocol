//! IDN Node | Industrial (spec/extensions/idn-industrial.md §I.8): the machine's link to
//! its company's IDN Site Manager | Industrial, its health readings, and the local API
//! its programs use (§I.9). Runs as a service (Windows: `idn-industrial service`,
//! installed by its installer; Linux: a systemd unit) or in a console.
//!
//!   idn-industrial [run] [--state DIR] [--listen 127.0.0.1:29208]
//!   idn-industrial enrol IDNI1-…      (joins; through the running Node if there is one)
//!   idn-industrial status
//!   idn-industrial token --new       (a new api.token on purpose: the old one stops working)
//!   idn-industrial polkit-check reboot | restart UNIT   (the polkit rule's question, E7-11)
//!
//! Settings: `node.toml` and `modules/*.toml` in the state folder (and what the Site
//! Manager pushes, kept in `site/`; see `idn_industrial::modules`), e.g.:
//!
//! ```toml
//! name = "Plant 1 batch"        # else the computer's name
//! [health]
//! processes = ["batching.exe"]     # should be running
//! services = ["MSSQLSERVER"]
//! ```

#[cfg(windows)]
use idn_common::sync::LockOk as _;
use idn_industrial::node::{IndustrialNode, NodeConfig};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

#[cfg(windows)]
const SERVICE: &str = "idn-industrial";
const LISTEN: &str = "127.0.0.1:29208";

/// The state folder: `%ProgramData%\IDN Node Industrial` on Windows; on Linux
/// `/var/lib/idn-industrial` for the service, else `~/.local/state/idn-industrial`.
fn default_state() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
        base.join("IDN Node Industrial")
    }
    #[cfg(not(windows))]
    {
        let base = if unsafe_is_root() {
            PathBuf::from("/var/lib")
        } else {
            std::env::var_os("HOME")
                .map_or_else(|| PathBuf::from("."), PathBuf::from)
                .join(".local/state")
        };
        base.join("idn-industrial")
    }
}

#[cfg(not(windows))]
fn unsafe_is_root() -> bool {
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid() == 0
        })
        .unwrap_or(false)
}

struct Args {
    cmd: String,
    rest: Vec<String>,
    state: PathBuf,
    listen: SocketAddr,
}

fn args() -> io::Result<Args> {
    let mut it = std::env::args().skip(1);
    let mut cmd = String::from("run");
    let mut rest = Vec::new();
    let mut state = default_state();
    let mut listen: SocketAddr = LISTEN.parse().unwrap();
    let mut first = true;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--state" => {
                state = it
                    .next()
                    .ok_or_else(|| io::Error::other("--state DIR"))?
                    .into()
            }
            "--listen" => {
                listen = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| io::Error::other("--listen ADDRESS:PORT"))?
            }
            "-h" | "--help" => {
                cmd = "help".into();
            }
            _ if first => cmd = a,
            _ => rest.push(a),
        }
        first = false;
    }
    Ok(Args {
        cmd,
        rest,
        state,
        listen,
    })
}

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    // The SQL module's worker (§I.10.6.3), started by the Node as a connection's identity:
    // jobs on stdin, answers on stdout, its log lines on stderr (to the Node's log).
    if std::env::args().nth(1).as_deref() == Some("sql-worker") {
        std::process::exit(idn_industrial::sql::worker::serve_stdio());
    }
    // An update's helper (§I.17): a copy of the Node as it was, outside the service.
    if std::env::args().nth(1).as_deref() == Some("update-apply") {
        let job = std::env::args().nth(2).unwrap_or_default();
        if let Err(e) = idn_industrial::update::run_helper(std::path::Path::new(&job)) {
            eprintln!("idn-industrial update-apply: {e}");
            std::process::exit(1);
        }
        return;
    }
    // The polkit rule's question (Linux service, E7-11): may the service's user reboot,
    // or restart UNIT? Yes (exit 0) only when root's actions file has an action for it.
    // Asked as polkit's own user: reads that file only, touches no state, logs nothing.
    if std::env::args().nth(1).as_deref() == Some("polkit-check") {
        let a: Vec<String> = std::env::args().skip(2).collect();
        let file = idn_industrial::actions::root_file()
            .unwrap_or_else(|| PathBuf::from("/etc/idn-industrial/actions.toml"));
        // A service named by an alias: its unit's own name, as systemd says (only the
        // names in root's file are looked up; `--` so a name can't be an option).
        let canonical = |name: &str| {
            std::process::Command::new("systemctl")
                .args(["show", "-p", "Id", "--value", "--", name])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let ok = idn_industrial::actions::load_root_file(&file).is_ok_and(|actions| {
            match a.as_slice() {
                [w] => idn_industrial::actions::allowed(&actions, w, None),
                [w, unit] => {
                    idn_industrial::actions::allowed_with(&actions, w, Some(unit), canonical)
                }
                _ => false,
            }
        });
        std::process::exit(if ok { 0 } else { 1 });
    }
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-industrial");
    let r = args().and_then(|a| match a.cmd.as_str() {
        "run" => run(a, None),
        "service" => service(a),
        "enrol" | "enroll" => enrol(a),
        "status" => status(a),
        "token" => new_token(a),
        "help" => {
            println!(
                "idn-industrial [run|service|enrol TOKEN|status|token --new] [--state DIR] [--listen {LISTEN}]"
            );
            Ok(())
        }
        other => Err(io::Error::other(format!(
            "unknown command {other:?} (run, enrol, status, token --new)"
        ))),
    });
    if let Err(e) = r {
        eprintln!("idn-industrial: {e}");
        std::process::exit(1);
    }
}

/// The machine's name: `name` in its own files, else the computer's.
fn machine_name(state: &std::path::Path) -> String {
    idn_industrial::modules::load(state)
        .settings
        .name
        .or_else(|| sysinfo::System::host_name().ok())
        .unwrap_or_else(|| "machine".into())
}

fn run(a: Args, stop: Option<tokio::sync::oneshot::Receiver<()>>) -> io::Result<()> {
    std::fs::create_dir_all(&a.state)?;
    // As the system's service (Windows: `service`; Linux: started by systemd), so an
    // update's helper stops and starts it as one.
    idn_industrial::update::set_service(
        stop.is_some() || std::env::var_os("INVOCATION_ID").is_some(),
    );
    // An update staged for the next start: installed now (the helper starts the Node
    // again).
    if idn_industrial::update::at_start(&a.state) {
        eprintln!("IDN Node | Industrial: installing the update staged for this start");
        return Ok(());
    }
    let name = machine_name(&a.state);
    let node = IndustrialNode::start(NodeConfig {
        state_dir: a.state.clone(),
        name,
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    // The modules (health, Modbus, HTTP polls, webhooks, actions), restarted when their
    // settings change; their commands taken first, the rest to the local API.
    let sup = idn_industrial::modules::Supervisor::start(node.clone(), a.state.clone());
    let commands = idn_industrial::modules::dispatch(&node, sup.clone());
    let mqtt = idn_industrial::modules::load(&a.state)
        .settings
        .mqtt
        .unwrap_or_default();
    if mqtt.enabled != Some(false) {
        // Never fatal: the link, the outbox and the modules run without it (a Site
        // Manager | Industrial on the same machine has the MQTT port, say).
        match idn_industrial::bridge::start(&node, &a.state, &mqtt) {
            Ok(addr) => eprintln!("IDN Node | Industrial: local MQTT on {addr}"),
            Err(e) => {
                let m = format!(
                    "IDN Node | Industrial: no local MQTT endpoint: {} ({e}). Another program has the \
                 port (a Site Manager | Industrial on this machine?): set [mqtt] loopback = \
                 \"127.0.0.1:1885\" in node.toml, or [mqtt] enabled = false",
                    mqtt.loopback.as_deref().unwrap_or("127.0.0.1:1883")
                );
                eprintln!("{m}");
                idn_open_protocol::log::log(&m);
            }
        }
    }
    // The programs' token: one that is there but can't be read is never replaced on its
    // own (the programs have it), and no longer stops the whole Node either (under
    // Restart=always it started again and again): the link, the modules and MQTT run, the
    // local API and panel don't, and the log says how to mend it.
    let token = match idn_industrial::local_api::token(&a.state) {
        Ok(t) => Some(t),
        Err(e) => {
            let m = format!(
                "IDN Node | Industrial: local API and panel OFF: the API token {} can't be read \
                 (or, the first time, made) ({e}); one there is never replaced on its own. Mend the file's access, or make a new \
                 one on purpose with `idn-industrial token --new` (programs then need the new \
                 one), then restart the Node",
                a.state.join("api.token").display()
            );
            eprintln!("{m}");
            idn_open_protocol::log::log(&m);
            None
        }
    };
    // The session helper's own token, which opens only its endpoint (§I.11.2).
    if let Some(token) = &token
        && let Err(e) = idn_industrial::local_api::write_worker_token(&a.state, token)
    {
        idn_open_protocol::log::log(&format!("the session helper's token: {e}"));
    }
    // (Without the API, the module supervisor and its commands kept all the same.)
    let (router, _idle) = match token {
        Some(t) => (
            Some(idn_industrial::local_api::router_with(
                node.clone(),
                t,
                commands,
                Some(sup),
            )),
            None,
        ),
        None => (None, Some((commands, sup))),
    };
    if router.is_some() {
        eprintln!(
            "IDN Node | Industrial {}: state {}, local API and panel http://{}",
            env!("CARGO_PKG_VERSION"),
            a.state.display(),
            a.listen
        );
    } else {
        eprintln!(
            "IDN Node | Industrial {}: state {}, without the local API",
            env!("CARGO_PKG_VERSION"),
            a.state.display()
        );
    }
    // As a Windows service, keep the session helper running on the interactive desktop so
    // remote support can see and drive it (§I.11.2). `stop` is set only on the service
    // path, so a plain `run` (a console, for testing) doesn't try to launch into a session.
    #[cfg(windows)]
    let _helper = stop.is_some().then(|| start_session_helper(&a.state));
    let rt = idn_industrial::node::runtime();
    let r = rt.block_on(async move {
        let stopped = async move {
            match stop {
                Some(rx) => {
                    let _ = rx.await;
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        };
        let Some(router) = router else {
            stopped.await;
            return Ok(());
        };
        let l = tokio::net::TcpListener::bind(a.listen).await?;
        axum::serve(l, router).with_graceful_shutdown(stopped).await
    });
    node.stop();
    r
}

/// Keeps `idn-helper-industrial.exe` (next to this program) running in the active
/// console session; it stops when the returned supervisor is dropped (the service exits).
#[cfg(windows)]
fn start_session_helper(state: &std::path::Path) -> idn_industrial::session::HelperSupervisor {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("idn-helper-industrial.exe")))
        .unwrap_or_else(|| PathBuf::from("idn-helper-industrial.exe"));
    let cmdline = format!("\"{}\" --state \"{}\"", exe.display(), state.display());
    // A helper left by a service that ended without stopping it (a crash, a kill): ended
    // first, not run twice beside the new one (deep bug run 2026-11-09).
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "idn-helper-industrial.exe"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    idn_industrial::session::HelperSupervisor::start(cmdline)
}

/// Run as root (`sudo idn-industrial status`, `enrol`) over a state folder that is
/// another user's (the service's own, idn-industrial): as that user from here on. As root,
/// links that user had put in its folder were followed: its token file pointing at
/// /etc/shadow was read and sent to the port, a new token written through a link over any
/// file, and the fallback left root's files where the service couldn't use them (bug
/// round F, F3-2).
#[cfg(target_os = "linux")]
fn as_state_owner(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no arguments and can't fail.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let Ok(m) = std::fs::symlink_metadata(dir) else {
        return Ok(());
    };
    if m.uid() == 0 {
        return Ok(());
    }
    if !m.is_dir() {
        return Err(io::Error::other(format!(
            "{} isn't a folder",
            dir.display()
        )));
    }
    // SAFETY: plain calls with no pointers but setgroups' (0 groups, null allowed); glibc
    // applies them to every thread of the process.
    let ok = unsafe {
        libc::setgroups(0, std::ptr::null()) == 0
            && libc::setgid(m.gid()) == 0
            && libc::setuid(m.uid()) == 0
    };
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "couldn't take the state folder's user: {}",
            io::Error::last_os_error()
        )))
    }
}

#[cfg(not(target_os = "linux"))]
fn as_state_owner(_: &std::path::Path) -> io::Result<()> {
    Ok(())
}

/// Through the running Node's local API, else here.
fn enrol(a: Args) -> io::Result<()> {
    as_state_owner(&a.state)?;
    let token = a
        .rest
        .first()
        .ok_or_else(|| io::Error::other("enrol IDNI1-…"))?
        .clone();
    if let Some(answer) = local_call(
        &a,
        "POST",
        "/enrol",
        &serde_json::json!({ "token": token }).to_string(),
    )? {
        println!("{answer}");
        return Ok(());
    }
    let node = IndustrialNode::start(NodeConfig {
        state_dir: a.state.clone(),
        name: machine_name(&a.state),
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    let st = node.enrol(&token)?;
    node.stop();
    println!("joined as {}", st.name);
    Ok(())
}

/// `token --new`: the local API's token made anew, on purpose (the Node never replaces
/// one it can't read on its own: the programs have it). The old one stops working; the
/// running Node takes the new one when restarted, and each program needs it.
fn new_token(a: Args) -> io::Result<()> {
    if a.rest.first().map(String::as_str) != Some("--new") {
        return Err(io::Error::other(
            "token --new: makes a new api.token (the old one stops working; restart the Node and give the programs the new one)",
        ));
    }
    as_state_owner(&a.state)?;
    let path = a.state.join("api.token");
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(io::Error::other(format!(
                "{}: the old token couldn't be removed: {e}",
                path.display()
            )));
        }
    }
    idn_industrial::local_api::token(&a.state)?;
    println!(
        "a new API token is in {}: restart the Node (its service) to use it, and give it to the programs that use the local API",
        path.display()
    );
    Ok(())
}

fn status(a: Args) -> io::Result<()> {
    as_state_owner(&a.state)?;
    match local_call(&a, "GET", "/status", "")? {
        Some(s) => println!("{s}"),
        None => println!("the Node isn't running"),
    }
    Ok(())
}

/// A call to the running Node's local API; `None` if nothing listens.
fn local_call(a: &Args, method: &str, path: &str, body: &str) -> io::Result<Option<String>> {
    use std::io::{Read, Write};
    let Ok(mut s) =
        std::net::TcpStream::connect_timeout(&a.listen, std::time::Duration::from_secs(1))
    else {
        return Ok(None);
    };
    // Linux: the port is the Node's (its state folder's user), not another program's that
    // took it while the service was down; the tokens go only to it (bug round F, F3-13).
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(m), Some(uid)) = (std::fs::metadata(&a.state), listener_uid(a.listen.port()))
            && uid != m.uid()
            && uid != 0
        {
            return Err(io::Error::other(format!(
                "port {} is held by another program (user {uid}), not the IDN Node | Industrial: nothing sent",
                a.listen.port()
            )));
        }
    }
    let token = idn_industrial::local_api::token(&a.state)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(60)))?;
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    if !head.starts_with("HTTP/1.1 2") {
        return Err(io::Error::other(body.trim().to_owned()));
    }
    Ok(Some(body.trim().to_owned()))
}

#[cfg(not(windows))]
fn service(a: Args) -> io::Result<()> {
    // systemd runs `run`; `service` is the Windows entry.
    run(a, None)
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
static SERVICE_ARGS: std::sync::Mutex<Option<Args>> = std::sync::Mutex::new(None);

#[cfg(windows)]
fn service(a: Args) -> io::Result<()> {
    *SERVICE_ARGS.lock_ok() = Some(a);
    windows_service::service_dispatcher::start(SERVICE, ffi_service_main)
        .map_err(idn_common::err::other)
}

#[cfg(windows)]
fn service_main(_args: Vec<std::ffi::OsString>) {
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = std::sync::Mutex::new(Some(stop_tx));
    let handler = move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(t) = stop_tx.lock_ok().take() {
                let _ = t.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(h) = service_control_handler::register(SERVICE, handler) else {
        return;
    };
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    };
    let _ = h.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ));
    // Its folder SYSTEM's and administrators' only, set right at each start (the
    // installer's icacls left other entries in place, bug round 2026-11-09).
    let state = default_state();
    let _ = std::fs::create_dir_all(&state);
    let _ = idn_common::fsutil::lock_down_dir(&state);
    // (Its `session` folder readable by users again: the session helper, which runs as
    // the logged-in user, reads its worker token there; the lock-down's reset took the
    // installer's grant away.)
    let session = state.join("session");
    let _ = std::fs::create_dir_all(&session);
    let _ = std::process::Command::new("icacls")
        .arg(&session)
        .args(["/grant", "*S-1-5-32-545:(OI)(CI)RX", "/Q"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_file(state.join("service-error.txt"));
    let failed = match SERVICE_ARGS.lock_ok().take() {
        None => Some("started by the service manager without its arguments".to_owned()),
        Some(a) => run(a, Some(stop_rx)).err().map(|e| e.to_string()),
    };
    // A failed start says so to Windows and in a file (it stopped "normally" before, with
    // no line anywhere: the IDN Node's failure of 2026-11-09).
    let mut st = status(ServiceState::Stopped, ServiceControlAccept::empty());
    if let Some(e) = failed {
        eprintln!("idn-industrial: {e}");
        idn_open_protocol::log::log(&format!("idn-industrial: {e}"));
        let _ = std::fs::write(
            state.join("service-error.txt"),
            format!("{}: {e}\n", idn_open_protocol::localtime::now().stamp()),
        );
        st.exit_code = ServiceExitCode::ServiceSpecific(1);
    }
    let _ = h.set_service_status(st);
}

/// The user of the socket listening on TCP `port` on this machine (Linux, from
/// /proc/net/tcp and tcp6).
#[cfg(target_os = "linux")]
fn listener_uid(port: u16) -> Option<u32> {
    let want = format!("{port:04X}");
    ["/proc/net/tcp", "/proc/net/tcp6"].iter().find_map(|f| {
        let text = std::fs::read_to_string(f).ok()?;
        text.lines().skip(1).find_map(|l| {
            let c: Vec<&str> = l.split_whitespace().collect();
            let local_port = c.get(1)?.rsplit(':').next()?;
            // 0A: listening.
            (local_port == want && *c.get(3)? == "0A")
                .then(|| c.get(7)?.parse().ok())
                .flatten()
        })
    })
}
