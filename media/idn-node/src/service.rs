//! Registering the Node as the machine's service (spec/extensions/idn-node.md §N.2):
//! a systemd unit on Linux, a service on Windows.

#[allow(unused_imports)] // (its locks are in one platform's code)
use idn_common::sync::LockOk as _;
use std::io;
use std::path::PathBuf;

/// The service's state directory (§N.2).
pub fn system_state_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("IDN Node")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/IDN Node")
    } else {
        // systemd's StateDirectory=idn-node.
        std::env::var_os("STATE_DIRECTORY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/idn-node"))
    }
}

// ---------------------------------------------------------------------------------------
// Linux

#[cfg(target_os = "linux")]
const UNIT: &str = "/etc/systemd/system/idn-node.service";
#[cfg(target_os = "linux")]
const UPDATE_PATH: &str = "/etc/systemd/system/idn-update.path";
#[cfg(target_os = "linux")]
const UPDATE_SERVICE: &str = "/etc/systemd/system/idn-update.service";
/// Virtual screens from the panel (§N.36): the service leaves a request, this path unit
/// starts the root helper that makes it.
#[cfg(target_os = "linux")]
const VSCREEN_PATH: &str = "/etc/systemd/system/idn-vscreen.path";
#[cfg(target_os = "linux")]
const VSCREEN_SERVICE: &str = "/etc/systemd/system/idn-vscreen.service";
#[cfg(target_os = "linux")]
const LIMITS: &str = "/etc/security/limits.d/95-idn.conf";
/// Remote control (R.25): the logged-in user may use `/dev/uinput` (a screen's keyboard
/// and mouse from the site's receivers), as Steam's and Sunshine's rules allow; the module
/// is loaded at boot.
#[cfg(target_os = "linux")]
const UINPUT_RULE: &str = "/etc/udev/rules.d/70-idn-uinput.rules";
#[cfg(target_os = "linux")]
const UINPUT_MODULE: &str = "/etc/modules-load.d/idn-uinput.conf";

/// A system file written, its folder made if the system has none (lab Alpha84 537-C: a
/// machine without udev had no /etc/udev/rules.d), an error naming the file.
#[cfg(target_os = "linux")]
fn write_conf(path: &str, text: &str) -> io::Result<()> {
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
    }
    std::fs::write(p, text).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))
}

#[cfg(target_os = "linux")]
fn sh(cmd: &str, args: &[&str]) -> io::Result<bool> {
    use idn_common::cmd::RunWithin as _;
    Ok(std::process::Command::new(cmd)
        .args(args)
        .status_within(std::time::Duration::from_secs(120))?
        .success())
}

#[cfg(target_os = "linux")]
pub fn install() -> io::Result<()> {
    // SAFETY: no arguments.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("run `sudo idn-node install`"));
    }
    let exe = std::env::current_exe()?;
    // Quoted, `%` doubled: systemd splits ExecStart at spaces and expands `%` specifiers.
    let exe = format!(
        "\"{}\"",
        exe.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    );
    // The group whose members' programs may use the Node (§N.3), and the service's account.
    sh("groupadd", &["-f", "--system", "idn"])?;
    // Real-time audio for the group's programs where RealtimeKit isn't there (a machine
    // without a desktop): the mixer's 2 ms clock and the plugin host's threads ask for it
    // (idn-mixer.md, priorities); members may ask, nothing gets it unasked. From their next
    // login.
    write_conf(
        LIMITS,
        "# IDN: real-time scheduling for the mixer's clock and plugin threads (idn-node install).\n\
         @idn - rtprio 60\n",
    )?;
    write_conf(
        UINPUT_RULE,
        "# IDN: a screen's keyboard and mouse used by the site's receivers (idn-node install).\n\
         KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"\n",
    )?;
    write_conf(UINPUT_MODULE, "uinput\n")?;
    // The unit's extra groups (the GPU's) made where a minimal system has none (lab Alpha84
    // 537-C: the service didn't start without them).
    for g in ["render", "video", "input"] {
        let _ = sh("groupadd", &["-f", "--system", g]);
    }
    let _ = sh("modprobe", &["uinput"]);
    let _ = sh("udevadm", &["control", "--reload"]);
    let _ = sh("udevadm", &["trigger", "--name-match=uinput"]);
    // Asked quietly (lab Alpha85 541(d): "id: 'idn-node': no such user" printed before it
    // was made).
    let has_user = {
        use idn_common::cmd::RunWithin as _;
        std::process::Command::new("id")
            .args(["-u", "idn-node"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status_within(std::time::Duration::from_secs(120))?
            .success()
    };
    if !has_user {
        sh(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--home-dir",
                "/var/lib/idn-node",
                "--gid",
                "idn",
                "--shell",
                "/usr/sbin/nologin",
                "idn-node",
            ],
        )?;
    }
    let unit = format!(
        "[Unit]
Description=IDN Node (the machine's IDN site identity and control panel)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={exe} run
User=idn-node
Group=idn
# The GPU (VA-API) for the site link's encoders (idn-remote.md X.12.1); the screens and
# sound cards for the display service with no desktop (apps.display_boot, §D.8.9).
SupplementaryGroups=render video audio
RuntimeDirectory=idn-node
RuntimeDirectoryMode=0750
StateDirectory=idn-node
StateDirectoryMode=0700
UMask=0007
# The site clock's PTP ports 319 and 320 (below 1024): without this the service always
# fell back to 10319/10320, which AES67 equipment doesn't hear (deep bug run 2026-11-09).
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
# Real-time scheduling for the programs it starts (the desk in a network device's rings):
# limits.d applies at a log-on, not to services (bug run K8-2: the headless desk ran
# without it).
LimitRTPRIO=60
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
# The media folder every user reads (review 2026-11-01 F4: read-only under strict, so
# media sent to the machine was refused); `-`: no failure if it is missing.
ReadWritePaths=-/var/lib/idn-media
# Read-only, not hidden: users' programs keep their logs in their homes
# (~/.local/state/…/logs), which the Site Manager's log snapshots read (§N.16; hidden,
# every one failed on Linux, deep bug run 2026-11-09).
ProtectHome=read-only
PrivateTmp=true

[Install]
WantedBy=multi-user.target
"
    );
    // Made now (as root, group `idn`), so the service can write in it.
    if let Err(e) = crate::media::make_dir(&crate::media::media_dir(std::path::Path::new(""), true))
    {
        eprintln!("idn-node: the media folder: {e}");
    }
    std::fs::write(UNIT, unit)?;
    // Updates: the service can't install, being unprivileged. It
    // leaves a job and the trigger file; this path unit starts the root helper, which
    // checks the job itself (the release's signature, the package's hash) and installs
    // over this program's own folder. The helper waits up to ten minutes for the new
    // Node's check-in, hence no start timeout.
    std::fs::write(
        UPDATE_PATH,
        "[Unit]\nDescription=IDN Node updates (the Node asks; idn-update.service installs)\n\n\
         [Path]\nPathExists=/var/lib/idn-node/updates/apply\nUnit=idn-update.service\n\n\
         [Install]\nWantedBy=multi-user.target\n",
    )?;
    std::fs::write(
        UPDATE_SERVICE,
        format!(
            "[Unit]\nDescription=IDN Node update helper\n\n[Service]\nType=oneshot\n\
             ExecStart={exe} update-root /var/lib/idn-node/updates\nStateDirectory=idn-update\n\
             StateDirectoryMode=0700\nTimeoutStartSec=30min\n"
        ),
    )?;
    std::fs::write(
        VSCREEN_PATH,
        "[Unit]\nDescription=IDN Node virtual screens (the panel asks; idn-vscreen.service makes them)\n\n\
         [Path]\nPathExists=/var/lib/idn-node/vscreen/apply\nUnit=idn-vscreen.service\n\n\
         [Install]\nWantedBy=multi-user.target\n",
    )?;
    std::fs::write(
        VSCREEN_SERVICE,
        format!(
            "[Unit]\nDescription=IDN Node virtual screen helper\n\n[Service]\nType=oneshot\n\
             ExecStart={exe} virtual-screen apply /var/lib/idn-node/vscreen\nTimeoutStartSec=15min\n"
        ),
    )?;
    sh("systemctl", &["daemon-reload"])?;
    if !sh("systemctl", &["enable", "--now", "idn-vscreen.path"])? {
        eprintln!(
            "idn-node: systemctl could not start idn-vscreen.path: the panel can't make virtual screens"
        );
    }
    if !sh("systemctl", &["enable", "--now", "idn-update.path"])? {
        eprintln!(
            "idn-node: systemctl could not start idn-update.path: updates from the Site Manager won't install"
        );
    }
    if !sh("systemctl", &["enable", "--now", "idn-node.service"])? {
        return Err(io::Error::other(
            "systemctl could not start idn-node.service",
        ));
    }
    println!("IDN Node installed and started (systemctl status idn-node).");
    // The user who ran sudo can use it after logging in again.
    if let Ok(u) = std::env::var("SUDO_USER")
        && !u.is_empty()
        && u != "root"
        && sh("usermod", &["-aG", "idn", &u])?
    {
        println!("Added {u} to group idn: log out and in again for programs to reach the Node.");
    }
    println!("Other accounts (a presentation server's service user): usermod -aG idn ACCOUNT");
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> io::Result<()> {
    // SAFETY: no arguments.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("run `sudo idn-node uninstall`"));
    }
    let _ = sh("systemctl", &["disable", "--now", "idn-node.service"]);
    let _ = sh("systemctl", &["disable", "--now", "idn-update.path"]);
    let _ = sh("systemctl", &["disable", "--now", "idn-vscreen.path"]);
    let _ = std::fs::remove_file(VSCREEN_PATH);
    let _ = std::fs::remove_file(VSCREEN_SERVICE);
    let _ = std::fs::remove_file(UNIT);
    let _ = std::fs::remove_file(UPDATE_PATH);
    let _ = std::fs::remove_file(UPDATE_SERVICE);
    let _ = std::fs::remove_file(LIMITS);
    let _ = std::fs::remove_file(UINPUT_RULE);
    let _ = std::fs::remove_file(UINPUT_MODULE);
    let _ = sh("systemctl", &["daemon-reload"]);
    println!(
        "IDN Node stopped and removed. Its state (the machine's key and site membership) stays in {}.",
        system_state_dir().display()
    );
    Ok(())
}

#[cfg(not(windows))]
pub fn run_as_service() -> io::Result<()> {
    Err(io::Error::other(
        "`idn-node service` is for the Windows service manager; use `idn-node run`",
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn install() -> io::Result<()> {
    Err(io::Error::other(
        "not available on this system yet (macOS: later)",
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn uninstall() -> io::Result<()> {
    install()
}

// ---------------------------------------------------------------------------------------
// Windows

#[cfg(windows)]
const NAME: &str = "idn-node";

#[cfg(windows)]
pub fn install() -> io::Result<()> {
    use std::ffi::OsString;
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let m = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| io::Error::other(format!("{e} (run as administrator)")))?;
    let dir = system_state_dir();
    std::fs::create_dir_all(&dir)?;
    // The state directory holds the machine's keys: SYSTEM and administrators only (a
    // link or junction there refused: installing goes no further).
    idn_common::fsutil::lock_down_dir(&dir)?;
    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from("IDN Node"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    // Installed before (an upgrade, or this program moved by the installer): the service
    // now runs this copy.
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START;
    let (s, updated) = match m.open_service(NAME, access) {
        Ok(s) => {
            stop_and_wait(&s)?;
            s.change_config(&info)
                .map_err(|e| io::Error::other(e.to_string()))?;
            (s, true)
        }
        Err(_) => (
            m.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
                .map_err(|e| io::Error::other(e.to_string()))?,
            false,
        ),
    };
    let _ = s.set_description(
        "The machine's IDN site identity and control panel (http://127.0.0.1:29200/).",
    );
    // Started again after a failure (a crash, or a failed start reported as one), as
    // Linux's Restart= and the Industrial services do: 5 s, three times a day.
    {
        use windows_service::service::{
            ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod,
        };
        let restart = ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: std::time::Duration::from_secs(5),
        };
        let _ = s.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(std::time::Duration::from_secs(86_400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart.clone(), restart.clone(), restart]),
        });
        let _ = s.set_failure_actions_on_non_crash_failures(true);
    }
    s.start::<&str>(&[])
        .map_err(|e| io::Error::other(e.to_string()))?;
    if updated {
        println!(
            "IDN Node updated and started (it runs {}).",
            info.executable_path.display()
        );
    } else {
        println!("IDN Node installed and started. Open the panel with `idn-node open`.");
    }
    Ok(())
}

/// Stops the service if it runs, and waits (up to 30 s) until it has.
#[cfg(windows)]
fn stop_and_wait(s: &windows_service::service::Service) -> io::Result<()> {
    use windows_service::service::ServiceState;
    let state = || s.query_status().map(|st| st.current_state);
    if state().is_ok_and(|st| st != ServiceState::Stopped) {
        let _ = s.stop();
        for _ in 0..60 {
            if state().is_ok_and(|st| st == ServiceState::Stopped) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        return Err(io::Error::other(
            "the IDN Node service didn't stop within 30 s",
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> io::Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| io::Error::other(format!("{e} (run as administrator)")))?;
    let s = m
        .open_service(
            NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
    // Stopped first, so the program can be deleted right after (the installer's uninstaller).
    let _ = stop_and_wait(&s);
    s.delete().map_err(|e| io::Error::other(e.to_string()))?;
    println!(
        "IDN Node removed. Its state (the machine's key and site membership) stays in {}.",
        system_state_dir().display()
    );
    Ok(())
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
pub fn run_as_service() -> io::Result<()> {
    windows_service::service_dispatcher::start(NAME, ffi_service_main)
        .map_err(|e| io::Error::other(e.to_string()))
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
    let Ok(h) = service_control_handler::register(NAME, handler) else {
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
    // Its folder's access set right first (as SYSTEM, its owner): files an earlier build
    // left with no access at all are mended here (Alpha61–64).
    let place = crate::host::Place::system();
    let _ = idn_common::fsutil::lock_down_dir(&place.state);
    // (An earlier failure's note gone: a later good start isn't read as failing.)
    let _ = std::fs::remove_file(place.state.join("service-error.txt"));
    let r = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map(|rt| rt.block_on(crate::host::serve(place.clone(), None, None, Some(stop_rx))));
    // A start that failed says so to Windows (the System log's "terminated with
    // service-specific error") and in a file beside the log: it stopped "normally"
    // before, with no line anywhere.
    let failed = match r {
        Ok(Err(e)) | Err(e) => {
            eprintln!("idn-node: {e}");
            let _ = std::fs::write(
                place.state.join("service-error.txt"),
                format!("{}: {e}\n", idn_open_protocol::localtime::now().stamp()),
            );
            true
        }
        Ok(Ok(())) => false,
    };
    let mut st = status(ServiceState::Stopped, ServiceControlAccept::empty());
    if failed {
        st.exit_code = ServiceExitCode::ServiceSpecific(1);
    }
    let _ = h.set_service_status(st);
}
