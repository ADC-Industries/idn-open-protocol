//! IDN Site Manager | Industrial (spec/extensions/idn-industrial.md §I.12), configured by
//! environment variables, as a container is:
//!
//! - `IDN_DATA`: the volume: the site CA and keys, devices, tokens, machines, what
//!   machines sent. Back it up. The container sets `/data`; the Linux package's service
//!   and the default elsewhere are `/var/lib/idn-site-manager-industrial` (Windows:
//!   `%ProgramData%\IDN Site Manager Industrial`).
//! - `IDN_HTTP`: the HTTP address (default `0.0.0.0:8080`): `/healthz`, the WebSocket
//!   link at `/idn/link`, the API. What a Cloudflare Tunnel points at.
//! - `IDN_QUIC`: the UDP address for QUIC links (default `0.0.0.0:29206`; `off`: none).
//! - `IDN_PUBLIC_URLS`: the addresses tokens give machines, comma separated, e.g.
//!   `wss://hub.example.com/idn/link,idn://hub.example.com:29206`.
//! - `IDN_API_KEY`: the application's API key; without it one is made and kept in
//!   `<IDN_DATA>/api.key`.
//! - `IDN_ADMIN_PASSWORD`: sets the web UI's administrator password at start (10+
//!   characters); without it, the first visit to the web UI sets one with the API key.
//! - `IDN_MQTT`: TCP for MQTT clients (default `0.0.0.0:1883`; `off`: only MQTT over the
//!   WebSocket at `/mqtt`). Logins are made with the API (`POST /api/mqtt-users`).
//!
//! `backup FILE` and `restore FILE` (idn-node.md §N.17): the volume in one file, locked
//! with a passphrase (`IDN_BACKUP_PASSPHRASE`, else asked); with the server stopped, as its
//! databases are held open while it runs: `docker compose stop site-manager && docker
//! compose run --rm site-manager backup /backup/site.idnsite`. A restore goes into an
//! empty volume (`restore FILE --replace` over one).

use idn_common::switches;
#[cfg(windows)]
use idn_common::sync::LockOk as _;
use idn_industrial::server::{Config, SiteManager};
use std::net::SocketAddr;

/// The volume: `IDN_DATA`, else the service's folder.
fn data_dir() -> std::path::PathBuf {
    if let Some(d) = switches::value("IDN_DATA") {
        return d.into();
    }
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData")
            .map_or_else(|| r"C:\ProgramData".into(), std::path::PathBuf::from)
            .join("IDN Site Manager Industrial")
    }
    #[cfg(not(windows))]
    {
        "/var/lib/idn-site-manager-industrial".into()
    }
}

/// Setting `k` (a registered switch), else `d`.
fn env(k: &str, d: &str) -> String {
    switches::value(k)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| d.to_owned())
}

/// A listening address setting: `off` (or `none`) for none.
fn address(k: &str, d: &str) -> std::io::Result<Option<SocketAddr>> {
    match env(k, d).trim() {
        "none" => Ok(None),
        v if switches::says_off(v) => Ok(None),
        a => a
            .parse()
            .map(Some)
            .map_err(|e| idn_common::err::other(format_args!("{k}: {e}"))),
    }
}

/// `<data>/settings.env` (`KEY=value` lines, `#` comments): the settings of a service
/// that has no environment of its own (Windows; the installer writes it). A variable
/// already set wins.
fn settings_file() {
    let Ok(text) = std::fs::read_to_string(data_dir().join("settings.env")) else {
        return;
    };
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if k.starts_with("IDN_") && k != "IDN_DATA" && std::env::var_os(k).is_none() {
                // SAFETY: at the start, before any other thread runs.
                unsafe { std::env::set_var(k, v) };
            }
        }
    }
}

#[cfg(windows)]
const SERVICE: &str = "idn-site-manager-industrial";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the runtime")
}

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    settings_file();
    // `healthcheck`: for Docker's HEALTHCHECK (the image has no curl): 0 if /healthz
    // answers.
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("version" | "--version")
    ) {
        println!(
            "idn-site-manager-industrial {}",
            idn_open_protocol::build_line()
        );
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(if healthy() { 0 } else { 1 });
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [cmd, file, rest @ ..] = args.as_slice()
        && (cmd == "backup" || cmd == "restore")
    {
        let replace = rest.iter().any(|a| a == "--replace");
        match backup_or_restore(cmd == "backup", file, replace) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("idn-site-manager-industrial: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // The web UI's administrator password, once (the installer's step): from
    // IDN_ADMIN_PASSWORD, else the first line typed.
    if std::env::args().nth(1).as_deref() == Some("admin-password") {
        let pw = switches::value("IDN_ADMIN_PASSWORD")
            .or_else(|| {
                let mut l = String::new();
                std::io::stdin().read_line(&mut l).ok()?;
                Some(l.trim_end_matches(['\r', '\n']).to_owned())
            })
            .unwrap_or_default();
        let data = data_dir();
        let r = std::fs::create_dir_all(&data)
            .and_then(|_| idn_industrial::http::set_admin_password(&data, &pw));
        match r {
            Ok(()) => println!("the administrator password is set"),
            Err(e) => {
                eprintln!("idn-site-manager-industrial: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // As a Windows service (installed by the Industrial installer).
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("service") {
        if let Err(e) = windows_service::service_dispatcher::start(SERVICE, ffi_service_main) {
            eprintln!("idn-site-manager-industrial: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = runtime().block_on(run(None)) {
        eprintln!("idn-site-manager-industrial: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
fn service_main(_args: Vec<std::ffi::OsString>) {
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    idn_open_protocol::diag::init("idn-site-manager-industrial");
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
    // Its folder (the site's CA and keys) SYSTEM's and administrators' only, set right at
    // each start (bug round 2026-11-09).
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = idn_common::fsutil::lock_down_dir(&dir);
    let _ = std::fs::remove_file(dir.join("service-error.txt"));
    let mut st = status(ServiceState::Stopped, ServiceControlAccept::empty());
    if let Err(e) = runtime().block_on(run(Some(stop_rx))) {
        idn_open_protocol::log::log(&format!("idn-site-manager-industrial: {e}"));
        let _ = std::fs::write(
            dir.join("service-error.txt"),
            format!("{}: {e}\n", idn_open_protocol::localtime::now().stamp()),
        );
        // (A failure Windows sees: its recovery actions restart it.)
        st.exit_code = ServiceExitCode::ServiceSpecific(1);
    }
    let _ = h.set_service_status(st);
}

async fn run(stop: Option<tokio::sync::oneshot::Receiver<()>>) -> std::io::Result<()> {
    let data = data_dir();
    let http: SocketAddr = env("IDN_HTTP", "0.0.0.0:8080")
        .parse()
        .map_err(|e| idn_common::err::other(format_args!("IDN_HTTP: {e}")))?;
    let quic = address("IDN_QUIC", "0.0.0.0:29206")?;
    let mqtt = address("IDN_MQTT", "0.0.0.0:1883")?;
    let public_urls: Vec<String> = env("IDN_PUBLIC_URLS", "")
        .split(',')
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
        .collect();
    if public_urls.is_empty() {
        eprintln!(
            "idn-site-manager-industrial: IDN_PUBLIC_URLS is empty: tokens won't tell machines where to connect"
        );
    }
    // The volume holds the site CA, token secrets and logins: this user's alone (Windows:
    // `fsutil::lock_down_dir`, the service's at each start). A link there is followed
    // (systemd's `DynamicUser` + `StateDirectory` make the folder a link into
    // /var/lib/private, owned by the service's user: narrowed, no line). A volume another
    // account owns: used as it is, and said so (once per start, and only then).
    match idn_common::fsutil::private_dir(&data) {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "idn-site-manager-industrial: {}: another account's folder: its access left as it is",
            data.display()
        ),
        Err(e) => {
            eprintln!(
                "idn-site-manager-industrial: {}: not made private: {e}",
                data.display()
            );
            std::fs::create_dir_all(&data)?;
        }
    }
    let api_key = match switches::value("IDN_API_KEY").filter(|k| !k.trim().is_empty()) {
        Some(k) => k,
        None => {
            let path = data.join("api.key");
            match std::fs::read_to_string(&path) {
                Ok(k) if !k.trim().is_empty() => k.trim().to_owned(),
                // There but unreadable: an error, not a new key made over the one the
                // applications have.
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(idn_common::err::other(format_args!(
                        "{}: {e}",
                        path.display()
                    )));
                }
                _ => {
                    let k = idn_open_protocol::random::token_hex(32);
                    idn_open_protocol::trust::pki::write_private(&path, k.as_bytes())?;
                    eprintln!(
                        "idn-site-manager-industrial: API key made and kept in {}",
                        path.display()
                    );
                    k
                }
            }
        }
    };
    if let Some(pw) = switches::value("IDN_ADMIN_PASSWORD") {
        idn_industrial::http::set_admin_password(&data, &pw)?;
    }
    let sm = SiteManager::start(Config {
        data_dir: data.clone(),
        quic,
        public_urls: public_urls.clone(),
        mqtt,
    })
    .await?;
    eprintln!(
        "IDN Site Manager | Industrial {}: site CA {}, HTTP {http}, QUIC {}, machines reach it at {}",
        env!("CARGO_PKG_VERSION"),
        sm.ca_fingerprint(),
        quic.map_or("off".into(), |q| q.to_string()),
        public_urls.join(", ")
    );
    // TCP_NODELAY: Voice's packets (§V.8) and the UI's live updates go at once, not held
    // by Nagle until the last one is acknowledged (up to 40 ms, profile round 8).
    use axum::serve::ListenerExt;
    let listener = tokio::net::TcpListener::bind(http).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(
        listener,
        sm.router(api_key)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        match stop {
            Some(rx) => {
                let _ = rx.await;
                eprintln!("idn-site-manager-industrial: stopping");
            }
            None => stopped().await,
        }
    })
    .await
}

/// Ctrl+C, or SIGTERM (what `docker stop` and Kubernetes send).
async fn stopped() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("idn-site-manager-industrial: stopping");
}

fn healthy() -> bool {
    use std::io::{Read, Write};
    let addr = env("IDN_HTTP", "0.0.0.0:8080").replace("0.0.0.0", "127.0.0.1");
    let Ok(a) = addr.parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs(3))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    if s.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.starts_with("HTTP/1.1 200")
}

/// `backup FILE` / `restore FILE` (idn-node.md §N.17), with the server stopped.
fn backup_or_restore(backup: bool, file: &str, replace: bool) -> std::io::Result<String> {
    let data = data_dir();
    let pass = switches::secret_or_ask(
        "IDN_BACKUP_PASSPHRASE",
        "the backup's passphrase (at least 8 characters):",
    )?;
    // The server holds its databases open while it runs; a copy then could be half-written.
    let db = data.join("site.redb");
    if db.exists() {
        redb::Database::open(&db).map(drop).map_err(|e| {
            idn_common::err::other(format_args!("stop the Site Manager first ({e})"))
        })?;
    }
    if backup {
        // Not the releases kept for updates (each tens of MB: a backup over its limit) nor
        // the MQTT broker's queues (deep bug run 2026-11-09).
        let b = idn_open_protocol::backup::make(
            &data,
            "industrial",
            &["releases", "mqtt.redb"],
            &pass,
        )?;
        std::fs::write(file, &b)?;
        Ok(format!(
            "backup of {} written to {file} ({} KB); keep it and its passphrase safe",
            data.display(),
            b.len() / 1024 + 1
        ))
    } else {
        let (m, files) = idn_open_protocol::backup::open(&std::fs::read(file)?, &pass)?;
        if m.kind != "industrial" {
            return Err(idn_common::err::other(format_args!(
                "{file} is a backup of {:?}, not of an Industrial Site Manager",
                m.kind
            )));
        }
        idn_open_protocol::backup::restore(&files, &data, replace)?;
        // Restored by root into a service's folder (the Linux package's): the files
        // belong to the folder's owner, so the service can open them.
        #[cfg(unix)]
        same_owner_as_dir(&data)?;
        Ok(format!(
            "restored {} files of the backup of {} into {}",
            files.len(),
            m.created,
            data.display()
        ))
    }
}

/// Every file under `dir` given `dir`'s own owner (only when root restores: otherwise the
/// files are the caller's, as is the folder).
#[cfg(unix)]
fn same_owner_as_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let me = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(u32::MAX);
    let top = std::fs::metadata(dir)?;
    if me != 0 || top.uid() == 0 {
        return Ok(());
    }
    fn walk(p: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
        for e in std::fs::read_dir(p)? {
            let path = e?.path();
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
            if std::fs::symlink_metadata(&path)?.is_dir() {
                walk(&path, uid, gid)?;
            }
        }
        Ok(())
    }
    walk(dir, top.uid(), top.gid())
}
