//! The Node hosted in a process: its state folder and endpoint ([`Place`]), and the Node
//! run until stopped ([`serve`]). The `idn-node` program runs it as the machine's service
//! or a user's; an embedder (roadmap: IDN Media as a server and libraries, the UI in the
//! embedding application) runs it with [`start`].

use crate::server::NodeCore;
/// Other products' releases (the host program's apps) installed by the host: see
/// [`crate::update::on_host_release`].
pub use crate::update::on_host_release;
use crate::{awake, ipc, panel, service};
use idn_common::sync::LockOk as _;
use idn_open_protocol::node;
use std::path::PathBuf;
use std::sync::Arc;

/// Where the Node keeps its state and listens.
#[derive(Debug, Clone)]
pub struct Place {
    pub state: PathBuf,
    pub endpoint: String,
    /// The service's place (group-shared socket) rather than a user's (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub system: bool,
}

impl Place {
    pub fn system() -> Place {
        Place {
            state: service::system_state_dir(),
            endpoint: node::system_endpoint(),
            system: true,
        }
    }

    /// User mode; `state` and `endpoint` given on the command line take the place of the
    /// defaults (which need XDG_RUNTIME_DIR / USERNAME and a home directory).
    pub fn user(state: Option<PathBuf>, endpoint: Option<String>) -> std::io::Result<Place> {
        let endpoint = match endpoint {
            Some(e) => e,
            None => node::user_endpoint().ok_or_else(|| {
                std::io::Error::other(
                    "no user endpoint: XDG_RUNTIME_DIR (Linux) or USERNAME (Windows) is not set; give --endpoint",
                )
            })?,
        };
        if let Some(state) = state {
            return Ok(Place {
                state,
                endpoint,
                system: false,
            });
        }
        let state = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|d| d.join("IDN Node"))
        } else {
            std::env::var_os("XDG_STATE_HOME")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .map(|d| d.join("idn-node"))
        }
        .ok_or_else(|| std::io::Error::other("no user state directory; give --state"))?;
        Ok(Place {
            state,
            endpoint,
            system: false,
        })
    }
}

/// Runs the Node until `stop` fires (the service manager) or Ctrl+C, then sends the stop
/// notices of every output still alive ([`idn_open_protocol::shutdown::run_hooks`]).
pub async fn serve(
    place: Place,
    panel_port: Option<u16>,
    lan_port: Option<u16>,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
) -> std::io::Result<()> {
    let how = How {
        service: place.system,
        diagnostics: true,
        hosted: false,
    };
    let r = serve_with(place, panel_port, lan_port, stop, None, how).await;
    // The process ends next (the service stopped by Windows' service manager, or Ctrl+C):
    // the stop notices of any output still alive go now, so other machines drop its
    // sources at once rather than in two minutes. (Not in `start`: a program hosting the
    // Node goes on running, with outputs of its own.) At most two seconds: a hook that
    // stalls doesn't keep the service stopping.
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("idn-node-stop-notices".into())
        .spawn(move || {
            let _ = std::panic::catch_unwind(idn_open_protocol::shutdown::run_hooks);
            let _ = tx.send(());
        });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    r
}

/// What [`start`] is told once the Node is up: it, and its panel's port.
type Ready = std::sync::mpsc::Sender<(Arc<NodeCore>, u16)>;

/// How the Node runs in its process.
struct How {
    /// As the machine's service (an update stops and starts the service).
    service: bool,
    /// The process's stderr into the Node's log and its panics as crash reports.
    diagnostics: bool,
    /// In another program ([`start`]).
    hosted: bool,
}

async fn serve_with(
    place: Place,
    panel_port: Option<u16>,
    lan_port: Option<u16>,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
    ready: Option<Ready>,
    how: How,
) -> std::io::Result<()> {
    // Its log and crash reports in its state directory; its own crash reports from before
    // join the programs' for the Site Manager (§N.16). (Process-wide: stderr and the panic
    // hook; a host program may keep its own, bug run O-L7.)
    if how.diagnostics {
        idn_open_protocol::diag::init_in("idn-node", &place.state.join("logs"));
    }
    if how.hosted {
        crate::update::set_hosted();
    }
    let crashes = place.state.join("crashes");
    for p in idn_open_protocol::diag::crash_reports() {
        if let Some(name) = p.file_name() {
            let _ = std::fs::create_dir_all(&crashes);
            if std::fs::copy(&p, crashes.join(name)).is_ok() {
                idn_open_protocol::diag::mark_reported(&p);
            }
        }
    }
    // An update staged for the next start (§N.23): its helper installs it, then starts the
    // Node again. The machine's service only when it is one (bug run O-H1: every hosted
    // Node counted as the service).
    let service = how.service;
    if crate::update::at_start(&place.state, service, &idn_open_protocol::build_line()) {
        idn_common::log::log_or_stderr("idn-node: installing the update staged for this start");
        return Ok(());
    }
    let core = NodeCore::start(&place.state)?;
    core.set_service(service);
    // Ports given on the command line win for this run.
    if let Some(p) = panel_port {
        core.set_panel_port(p);
    }
    if let Some(p) = lan_port {
        core.set_lan_port(p);
    }
    let listener = ipc::listen(&place)?;
    core.log(&format!("listening on {}", place.endpoint));
    let panel = panel::Panel::start(core.clone())?;
    // A free port asked for (0): the one it got, for the panel's links (bug run O-L8).
    if core.settings().panel_port == 0 {
        core.set_panel_port(panel.port());
    }
    core.log(&format!("panel on http://127.0.0.1:{}/", panel.port()));
    if let Some(r) = ready {
        let _ = r.send((core.clone(), panel.port()));
    }
    let ipc_core = core.clone();
    let ipc_task = tokio::spawn(async move { ipc::accept(listener, ipc_core).await });
    // The site link to another location and AES67 streams run here, with the service
    // (idn-remote.md X.12.1, idn-node.md §N.14.5): up at boot, no one logged in.
    let link_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let link_thread = {
        let (core, stop, endpoint) = (core.clone(), link_stop.clone(), place.endpoint.clone());
        std::thread::Builder::new()
            .name("idn-node-services".into())
            .spawn(move || node_services(core, stop, endpoint))?
    };
    let stopped = async {
        match stop {
            Some(rx) => {
                let _ = rx.await;
            }
            // Ctrl+C, or (Unix) SIGTERM: a Node started by hand and stopped with `kill`
            // stops its services too, rather than leaving them running.
            #[cfg(unix)]
            None => match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                Ok(mut term) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                }
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            },
            #[cfg(not(unix))]
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    };
    tokio::select! {
        _ = stopped => {}
        r = ipc_task => {
            if let Ok(Err(e)) = r {
                core.log(&format!("listener ended: {e}"));
            }
        }
    }
    core.log("stopping");
    // The panels first: their ports free and their push channels ended once it returns.
    drop(panel);
    link_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = link_thread.join();
    core.shutdown();
    ipc::cleanup(&place);
    Ok(())
}

/// Runs the services the Node runs itself (the site link, AES67 streams) until `stop`,
/// reporting how each is doing.
fn node_services(core: Arc<NodeCore>, stop: Arc<std::sync::atomic::AtomicBool>, endpoint: String) {
    use crate::supervisor::Supervisor;
    use idn_open_protocol::node::bridges::Runs;
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;
    // A bridge's messages in the log under its service's title (by its arguments).
    let titles: Arc<std::sync::Mutex<BTreeMap<Vec<String>, String>>> = Arc::default();
    let (log_core, log_titles) = (core.clone(), titles.clone());
    // The Site Manager's folder (§N.12).
    let site_manager_dir = core.site_manager_dir();
    let mut sup = Supervisor::new(Supervisor::idn_next_to_me())
        .env("IDN_NODE", &endpoint)
        .env("IDN_SITE_MANAGER_DIR", &site_manager_dir.to_string_lossy())
        // IDN Data's panel opens only with a token from this Node's.
        .env("IDN_DATA_PANEL_KEY", &core.data_panel_key())
        // This machine, for a restore of its Site Manager (F6-4).
        .env("IDN_SITE_HOST_SPKI", &core.device_spki_hex())
        .env("IDN_SITE_HOST_NAME", &core.settings().name)
        .log_to(move |args, l| {
            let t = log_titles
                .lock_ok()
                .get(args)
                .cloned()
                .unwrap_or_else(|| args.join(" "));
            log_core.log(&format!("{t}: {l}"));
        });
    let mut said: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    let mut awake = awake::Awake::default();
    while !stop.load(Ordering::Relaxed) {
        // A restore of this machine's Site Manager: its certificate from the restored keys.
        core.take_host_issued(&site_manager_dir);
        // The machine's rate worked out now, before the commands carry it (bug run J1-3:
        // at the start they had 48 kHz, then started again at the real rate).
        core.audio_rate();
        let services: Vec<_> = core
            .settings()
            .services()
            .into_iter()
            .filter(|s| s.runs == Runs::Node)
            .collect();
        let want: BTreeMap<String, Vec<String>> = services
            .iter()
            .filter(|s| s.on)
            .filter_map(|s| Some((s.id.clone(), s.args.clone()?)))
            .collect();
        if want != said {
            let label = |s: &idn_open_protocol::node::bridges::Service| match s.kind {
                "sitelink" => "site link".to_owned(),
                "aes67_send" => format!("AES67 send {:?}", s.title),
                "aes67_recv" => format!("AES67 receive {:?}", s.title),
                "listen" => "listening on phones".to_owned(),
                _ => s.id.clone(),
            };
            for s in &services {
                labels.insert(s.id.clone(), label(s));
            }
            for (id, args) in &want {
                if said.get(id) != Some(args) {
                    core.log(&format!("{}: idn {}", labels[id], args.join(" ")));
                }
            }
            for id in said.keys().filter(|id| !want.contains_key(*id)) {
                core.log(&format!("{}: off", labels.get(id).unwrap_or(id)));
            }
            *titles.lock_ok() = services
                .iter()
                .filter_map(|s| Some((s.args.clone()?, label(s))))
                .collect();
            said = want.clone();
        }
        sup.supervise(&want.values().cloned().collect::<Vec<_>>());
        // Awake while any service runs, here or in someone's session (unless turned off).
        let settings = core.settings();
        let busy = settings.keep_awake
            && settings
                .services()
                .iter()
                .any(|s| s.on && s.args.is_some() && s.unsupported.is_none());
        awake.hold(busy, &|l| core.log(l));
        core.set_node_services(
            want.iter()
                .filter_map(|(id, a)| Some((id.clone(), sup.state(a)?)))
                .collect(),
        );
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    sup.stop_all();
}

/// How a program hosting the Node starts it ([`start`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Its state folder and the endpoint IDN's programs reach it on ([`Place::system`] for
    /// the machine's, [`Place::user`] for a user's, or one of the host's own).
    pub place: Place,
    /// IDN's programs folder (`idn`, `idn-aoip`, …) when it isn't the host program's own:
    /// the services the Node runs are started from there.
    pub programs_dir: Option<PathBuf>,
    /// The control panel's port, and the panel's port on the network (default: the
    /// Node's settings').
    pub panel_port: Option<u16>,
    pub lan_panel_port: Option<u16>,
    /// It runs as the machine's IDN Node service (`idn-node`'s own unit or Windows
    /// service): an update stops and starts that service. Normally `place.system`; false,
    /// an update installs into IDN's programs folder with its `idn-node` (none there: no
    /// updates) and ends and starts this program again.
    pub service: bool,
    /// The process's stderr copied into the Node's log and a panic hook writing crash
    /// reports (process-wide); false: the host program keeps its own.
    pub diagnostics: bool,
}

/// The Node running in this process ([`start`]); stopped by [`Handle::stop`] or when
/// dropped. One a process: the Node's log, crash reports and programs folder are the
/// process's.
pub struct Handle {
    core: Arc<NodeCore>,
    endpoint: String,
    panel_port: u16,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl Handle {
    /// The endpoint IDN's programs and clients reach it on: `Node::connect_to` it for its
    /// settings, status and routes, as every IDN program does
    /// (`idn_open_protocol::node::Node`).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The control panel's port (on 127.0.0.1).
    pub fn panel_port(&self) -> u16 {
        self.panel_port
    }

    /// The Node itself (its internal interface: not a stable API).
    #[doc(hidden)]
    pub fn core(&self) -> &Arc<NodeCore> {
        &self.core
    }

    /// Stops it and the services it runs; returns once they have.
    pub fn stop(mut self) -> std::io::Result<()> {
        self.end()
    }

    fn end(&mut self) -> std::io::Result<()> {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        match self.thread.take() {
            Some(t) => t
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("the Node's thread panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// Starts the Node in this process, on a thread of its own, and returns once it is up
/// (its endpoint listening, its panel served) with the services its settings name being
/// started. The program's way to host IDN Media (roadmap: the server and libraries model;
/// the hosting program is the UI).
pub fn start(cfg: Config) -> std::io::Result<Handle> {
    if let Some(d) = cfg.programs_dir.clone()
        && !idn_common::supervisor::set_programs_dir(d.clone())
        && idn_common::supervisor::programs_dir().as_ref() != Some(&d)
    {
        return Err(std::io::Error::other(
            "IDN's programs folder is already set to another in this process",
        ));
    }
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let endpoint = cfg.place.endpoint.clone();
    let thread = std::thread::Builder::new()
        .name("idn-node-host".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("idn-node")
                .enable_all()
                .build()?;
            let how = How {
                service: cfg.service,
                diagnostics: cfg.diagnostics,
                hosted: true,
            };
            rt.block_on(serve_with(
                cfg.place,
                cfg.panel_port,
                cfg.lan_panel_port,
                Some(stop_rx),
                Some(ready_tx),
                how,
            ))
        })?;
    match ready_rx.recv() {
        Ok((core, panel_port)) => Ok(Handle {
            core,
            endpoint,
            panel_port,
            stop: Some(stop_tx),
            thread: Some(thread),
        }),
        // Ended before it was up: why.
        Err(_) => Err(match thread.join() {
            Ok(Err(e)) => e,
            Ok(Ok(())) => std::io::Error::other(
                "the Node ended at its start (an update staged for it was installed: start it again)",
            ),
            Err(_) => std::io::Error::other("the Node's thread panicked at its start"),
        }),
    }
}

/// The machine's site membership as the hosting program (the Launcher) shows it
/// (idn-node.md §N.42), typed: read with [`site_status`].
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct SiteStatus {
    /// `none` (no site), `pending` (waiting for approval, `code` shown), `enrolled`,
    /// `code_needed`, `code_accepted`, `wrong_code`, `code_expired`, `denied`, `revoked`,
    /// `no_authority`, `error`, …
    pub state: String,
    /// In the site and connected to its authority now.
    pub connected: bool,
    /// The site's address, when set.
    pub server: Option<String>,
    /// The code shown while waiting for approval.
    pub code: Option<String>,
    /// A typed code's tries left (`wrong_code`).
    pub code_left: Option<u32>,
    /// How the site takes machines: `approval`, `code_or_approval`, `code_and_approval`.
    pub join_mode: Option<String>,
    /// The site's device count.
    pub site_devices: Option<u32>,
    /// Why, for `error`.
    pub error: Option<String>,
}

/// The membership now, typed (from the Node's status).
pub fn site_status(core: &NodeCore) -> SiteStatus {
    let s = core.status();
    let opt = |k: &str| s.attr(k).filter(|v| !v.is_empty()).map(str::to_owned);
    SiteStatus {
        state: s.attr("state").unwrap_or("none").to_owned(),
        connected: s.attr("connected") == Some("true"),
        server: opt("server"),
        code: opt("code"),
        code_left: opt("code_left").and_then(|v| v.parse().ok()),
        join_mode: opt("join_mode"),
        site_devices: opt("site_devices").and_then(|v| v.parse().ok()),
        error: opt("error"),
    }
}

/// How much a notice matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warning,
    Info,
    /// A problem over.
    Ok,
}

/// One of the Node's typed events (idn-node.md §N.41), for the hosting program's notices.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Notice {
    /// Its name (`clock_lost`, `site_manager_unreachable`, `revoked`, …).
    pub event: String,
    pub level: Level,
    /// Words for people.
    pub text: String,
    /// When (seconds since 1970).
    pub at: u64,
    /// Everything it carries (the fields above among them).
    pub fields: serde_json::Value,
}

impl Notice {
    fn of(v: serde_json::Value) -> Notice {
        let level = match v["level"].as_str().unwrap_or("") {
            "error" => Level::Error,
            "warning" => Level::Warning,
            "ok" => Level::Ok,
            _ => Level::Info,
        };
        Notice {
            event: v["event"].as_str().unwrap_or("").to_owned(),
            level,
            text: v["text"].as_str().unwrap_or("").to_owned(),
            at: v["at"].as_u64().unwrap_or(0),
            fields: v,
        }
    }
}

/// The Node's notices after place `seq` (0: those kept), oldest first, and the place now
/// to ask from next time.
pub fn notices_since(core: &NodeCore, seq: u64) -> (Vec<Notice>, u64) {
    let (l, now) = core.problem_events.since(seq);
    (l.into_iter().map(Notice::of).collect(), now)
}

/// The problems now (each `{problem, level, text, …}`), as notices.
pub fn problems_now(core: &NodeCore) -> Vec<Notice> {
    core.problems()
        .into_iter()
        .map(|mut p| {
            p["event"] = p["problem"].clone();
            Notice::of(p)
        })
        .collect()
}

#[cfg(test)]
mod notice_tests {
    #[test]
    fn notices_are_typed() {
        let n = super::Notice::of(serde_json::json!({
            "type": "event", "event": "clock_lost", "level": "warning",
            "text": "the site clock is lost", "at": 5, "gm": "x"}));
        assert_eq!(n.event, "clock_lost");
        assert_eq!(n.level, super::Level::Warning);
        assert_eq!(n.at, 5);
        assert_eq!(n.fields["gm"], "x");
    }
}
