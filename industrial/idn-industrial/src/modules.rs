//! The Node | Industrial's modules (§I.10) and their settings (§I.8): the machine's own
//! files in its state folder (`node.toml`, `modules/*.toml`, `secrets.toml`) and the
//! settings the Site Manager pushes (kept as `site/*.toml`, so they hold while the Site
//! Manager is away). A pushed file can't name actions or make Modbus points writable:
//! commands reach only what the machine's own files allow (§I.13).
//!
//! The commands `modbus.write` and `action.run` are the modules'; every other command goes
//! to the machine's program (the local API). Each write and each action is reported to the
//! Site Manager as a message (`audit.modbus`, `audit.action`).

use crate::actions::Action;
use crate::health::HealthConfig;
use crate::modbus::Device;
use crate::node::{Command, IndustrialNode};
use crate::poll::{Poll, Webhook};
use idn_common::sync::LockOk as _;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Sleeps up to `d`; `true` as soon as `stop` is set.
pub fn wait_or_stop(stop: &AtomicBool, d: Duration) -> bool {
    let end = std::time::Instant::now() + d;
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= end {
            return false;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(250)));
    }
}

/// The local MQTT endpoint (§I.7.4): loopback without a login by default; the plant's
/// network with one.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct MqttConfig {
    /// `false`: no local endpoint.
    pub enabled: Option<bool>,
    /// The loopback address (default `127.0.0.1:1883`).
    pub loopback: Option<String>,
    /// Also on the plant's network, e.g. `0.0.0.0:1884`, with `username` and `password`.
    pub listen: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Local topics sent up (filters; default everything).
    #[serde(default)]
    pub up: Vec<String>,
}

/// Remote support (§I.11): on unless turned off. Roles and notify-or-silent are the Site
/// Manager's to decide, so there is little to set here.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RemoteConfig {
    /// `false`: the machine refuses screen and input sessions (chat still works).
    pub enabled: Option<bool>,
}

/// The outbox (§I.6.3): bounded by size only, set on the machine itself.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct OutboxConfig {
    /// Megabytes the queue may hold before the oldest values, then messages, go
    /// (default 1024).
    pub max_mb: Option<u64>,
}

/// One file's worth (or all of them merged).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Settings {
    pub name: Option<String>,
    pub health: Option<HealthConfig>,
    pub mqtt: Option<MqttConfig>,
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub modbus: Vec<Device>,
    #[serde(default)]
    pub http: Vec<Poll>,
    #[serde(default)]
    pub webhook: Vec<Webhook>,
    #[serde(default)]
    pub action: Vec<Action>,
    pub sql: Option<crate::sql::SqlConfig>,
    pub outbox: Option<OutboxConfig>,
}

/// The merged settings and what was wrong with them (for the log and the panel).
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub settings: Settings,
    pub warnings: Vec<String>,
    pub secrets: Arc<BTreeMap<String, String>>,
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Reads and merges the machine's files and the pushed ones.
pub fn load(state: &Path) -> Loaded {
    let mut warnings = Vec::new();
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    if state.join("node.toml").exists() {
        files.push((state.join("node.toml"), true));
    }
    files.extend(
        toml_files(&state.join("modules"))
            .into_iter()
            .map(|p| (p, true)),
    );
    files.extend(
        toml_files(&state.join("site"))
            .into_iter()
            .map(|p| (p, false)),
    );
    let mut all = Settings::default();
    for (path, local) in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let mut s: Settings = match toml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let file = path
            .file_name()
            .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
        if !local {
            if !s.action.is_empty() {
                warnings.push(format!(
                    "site/{file}: actions only come from this machine's own files; ignored"
                ));
                s.action.clear();
            }
            for d in &mut s.modbus {
                for p in &mut d.points {
                    if p.writable {
                        warnings.push(format!(
                            "site/{file}: {}/{} can be made writable only in this machine's own files",
                            d.name, p.name
                        ));
                        p.writable = false;
                    }
                }
            }
            if let Some(q) = &mut s.sql {
                // Connections (who a query runs as, and its credentials) are the machine's
                // own; a pushed file may add read-only queries on them and set cadences.
                if !q.connection.is_empty() {
                    warnings.push(format!(
                        "site/{file}: SQL connections only come from this machine's own files; ignored"
                    ));
                    q.connection.clear();
                }
            }
            // A pushed poll may not carry this machine's secrets (`${secret:…}` in its
            // headers) to wherever its URL points: as the SQL connections above, a pushed
            // file would send a local password to another server.
            s.http.retain(|p| {
                let secret = p.headers.values().any(|v| v.contains("${secret:"));
                if secret {
                    warnings.push(format!(
                        "site/{file}: {}: polls with ${{secret:…}} only come from this machine's own files; ignored",
                        p.url
                    ));
                }
                !secret
            });
            if s.outbox.take().is_some() {
                warnings.push(format!(
                    "site/{file}: [outbox] is set on the machine itself"
                ));
            }
            if s.mqtt.take().is_some() {
                warnings.push(format!("site/{file}: [mqtt] is set on the machine itself"));
            }
            if s.name.take().is_some() {
                warnings.push(format!(
                    "site/{file}: the name is the Site Manager's own setting"
                ));
            }
        }
        all.name = all.name.or(s.name);
        all.mqtt = all.mqtt.or(s.mqtt);
        all.outbox = all.outbox.or(s.outbox);
        // [remote] only toggles a capability the Site Manager already governs, so a
        // pushed file may set it (a machine's own file wins).
        all.remote = all.remote.or(s.remote);
        if let Some(h) = s.health {
            let mut cur = all.health.take().unwrap_or_default();
            cur.processes.extend(h.processes);
            cur.services.extend(h.services);
            for (k, v) in h.files {
                cur.files.entry(k).or_insert(v);
            }
            cur.every_s = match (cur.every_s, h.every_s) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            all.health = Some(cur);
        }
        let mut add = |kind: &str, name: &str, taken: &mut Vec<String>| {
            if taken.iter().any(|n| n == name) {
                warnings.push(format!("{file}: a second {kind} \"{name}\"; ignored"));
                false
            } else {
                taken.push(name.to_owned());
                true
            }
        };
        let mut names: Vec<String> = all.modbus.iter().map(|d| d.name.clone()).collect();
        for d in s.modbus {
            if add("modbus device", &d.name, &mut names) {
                all.modbus.push(d);
            }
        }
        let mut names: Vec<String> = all.http.iter().map(|d| d.name.clone()).collect();
        for d in s.http {
            if add("http poll", &d.name, &mut names) {
                all.http.push(d);
            }
        }
        let mut names: Vec<String> = all.webhook.iter().map(|d| d.name.clone()).collect();
        for d in s.webhook {
            if add("webhook", &d.name, &mut names) {
                all.webhook.push(d);
            }
        }
        let mut names: Vec<String> = all.action.iter().map(|d| d.name.clone()).collect();
        for d in s.action {
            if add("action", &d.name, &mut names) {
                all.action.push(d);
            }
        }
        if let Some(q) = s.sql {
            let mut cur = all.sql.take().unwrap_or_default();
            cur.zone = cur.zone.or(q.zone);
            let mut names: Vec<String> = cur.connection.iter().map(|d| d.name.clone()).collect();
            for d in q.connection {
                if add("SQL connection", &d.name, &mut names) {
                    cur.connection.push(d);
                }
            }
            let mut names: Vec<String> = cur.query.iter().map(|d| d.name.clone()).collect();
            for d in q.query {
                if add("SQL query", &d.name, &mut names) {
                    cur.query.push(d);
                }
            }
            for (k, v) in q.every {
                cur.every.entry(k).or_insert(v);
            }
            all.sql = Some(cur);
        }
    }
    // The Linux service: actions only from root's file (E7-11), none from the files its
    // own user can write.
    if let Some(root) = crate::actions::root_file() {
        if !all.action.is_empty() {
            warnings.push(format!(
                "actions in this machine's settings files aren't used by the service: put them in {} (root's; sudo nano)",
                root.display()
            ));
        }
        all.action = match crate::actions::load_root_file(&root) {
            Ok(a) => a,
            Err(e) => {
                warnings.push(e);
                Vec::new()
            }
        };
    }
    let secrets = crate::settings::read_secrets(state).unwrap_or_else(|e| {
        warnings.push(e.to_string());
        BTreeMap::new()
    });
    Loaded {
        settings: all,
        warnings,
        secrets: Arc::new(secrets),
    }
}

/// Keeps the Site Manager's pushed files (`{"modules": {"name": "toml"}}`) as
/// `site/<name>.toml`, removing those no longer pushed. `true` if anything changed.
pub fn write_pushed(state: &Path, pushed: &serde_json::Value) -> io::Result<bool> {
    let dir = state.join("site");
    std::fs::create_dir_all(&dir)?;
    let want: BTreeMap<String, String> = pushed
        .get("modules")
        .and_then(|m| m.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let safe: String = k
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c))
                        .collect();
                    (!safe.is_empty())
                        .then(|| (format!("{safe}.toml"), v.as_str().unwrap_or("").to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut changed = false;
    for p in toml_files(&dir) {
        let f = p
            .file_name()
            .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
        if !want.contains_key(&f) {
            std::fs::remove_file(&p)?;
            changed = true;
        }
    }
    for (f, text) in want {
        let p = dir.join(&f);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(text.as_str()) {
            idn_common::fsutil::write_atomic(&p, text.as_bytes())?;
            changed = true;
        }
    }
    Ok(changed)
}

struct Running {
    stop: Arc<AtomicBool>,
    settings: Settings,
    sql: Option<Arc<crate::sql::Engine>>,
}

/// The modules running now, restarted when the settings change.
pub struct Supervisor {
    node: IndustrialNode,
    state: PathBuf,
    running: Mutex<Option<Running>>,
    warnings: Mutex<Vec<String>>,
    /// Remote support, kept across reloads so a settings change doesn't drop a session.
    remote: Arc<crate::remote::Remote>,
    station: Arc<crate::remote::LogStation>,
    /// The settings at the start: what only a restart takes up is compared with them.
    started: Settings,
    /// What was last reported as needing a restart (`node/restart_needed`).
    restart_said: Mutex<Option<Vec<&'static str>>>,
}

impl Supervisor {
    pub fn start(node: IndustrialNode, state: PathBuf) -> Arc<Supervisor> {
        let station = Arc::new(crate::remote::LogStation::default());
        let enabled = load(&state)
            .settings
            .remote
            .and_then(|r| r.enabled)
            .unwrap_or(true);
        let remote = crate::remote::Remote::new(
            node.clone(),
            enabled,
            station.clone() as Arc<dyn crate::remote::Station>,
        );
        let started = load(&state).settings;
        let s = Arc::new(Supervisor {
            node,
            state,
            running: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
            remote,
            station,
            started,
            restart_said: Mutex::new(None),
        });
        s.reload();
        // Updates (§I.17): the new Node's check-in, and where an update is.
        crate::update::watch(
            s.node.clone(),
            s.state.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        // Pushed settings: kept, then everything restarted with them.
        let me = s.clone();
        s.node
            .set_on_config(move |cfg| match write_pushed(&me.state, cfg) {
                Ok(true) => me.reload(),
                Ok(false) => {}
                Err(e) => {
                    idn_open_protocol::log::log(&format!("modules: the pushed settings: {e}"))
                }
            });
        s
    }

    /// Stops the modules and starts them again from the files.
    pub fn reload(&self) {
        let loaded = load(&self.state);
        for w in &loaded.warnings {
            idn_open_protocol::log::log(&format!("modules: {w}"));
        }
        *self.warnings.lock_ok() = loaded.warnings.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s = &loaded.settings;
        if let Some(old) = self.running.lock_ok().take() {
            old.stop.store(true, Ordering::Relaxed);
        }
        crate::health::start(
            self.node.clone(),
            s.health.clone().unwrap_or_default(),
            stop.clone(),
        );
        for d in &s.modbus {
            crate::modbus::start(self.node.clone(), d.clone(), stop.clone());
        }
        for p in &s.http {
            crate::poll::start(
                self.node.clone(),
                p.clone(),
                loaded.secrets.clone(),
                stop.clone(),
            );
        }
        let sql = s.sql.as_ref().filter(|c| !c.query.is_empty()).map(|c| {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("idn-industrial"));
            let workers = crate::sql::Workers::new(c, &loaded.secrets, &exe);
            let e = crate::sql::Engine::new(
                c.clone(),
                loaded.secrets.clone(),
                Arc::new(workers),
                Arc::new(self.node.clone()),
                self.state.join("sql"),
            );
            e.start(stop.clone());
            e
        });
        self.remote
            .set_enabled(s.remote.as_ref().and_then(|r| r.enabled).unwrap_or(true));
        idn_open_protocol::log::log(&format!(
            "modules: health, {} Modbus device(s), {} HTTP poll(s), {} webhook(s), {} action(s), {} SQL quer(ies)",
            s.modbus.len(),
            s.http.len(),
            s.webhook.len(),
            s.action.len(),
            s.sql.as_ref().map_or(0, |q| q.query.len())
        ));
        // What only a restart takes up, said on the panel and to the Site Manager.
        let restart = crate::settings::restart_needed(&self.started, &loaded.settings);
        let mut said = self.restart_said.lock_ok();
        if said.as_ref() != Some(&restart) {
            let _ = self.node.send_values(&[crate::node::Value {
                name: "node/restart_needed".into(),
                value: serde_json::json!(restart),
                time_ms: 0,
                quality: "good".into(),
            }]);
            *said = Some(restart);
        }
        drop(said);
        *self.running.lock_ok() = Some(Running {
            stop,
            settings: loaded.settings,
            sql,
        });
    }

    /// The state folder (the settings files are there).
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// What changed in the files that only a restart of the Node takes up.
    pub fn restart_needed(&self) -> Vec<&'static str> {
        crate::settings::restart_needed(&self.started, &load(&self.state).settings)
    }

    /// The SQL module's problems with its settings (query or connection → why).
    pub fn sql_problems(&self) -> Vec<(String, String)> {
        self.sql().map(|e| e.problems()).unwrap_or_default()
    }

    /// Remote support (§I.11), for the local API and the dispatcher.
    pub fn remote(&self) -> &Arc<crate::remote::Remote> {
        &self.remote
    }

    /// The chat messages waiting for the operator, cleared as they are taken.
    pub fn pending_chat(&self) -> Vec<crate::remote::ChatMessage> {
        self.station.take_pending()
    }

    /// The settings in force.
    pub fn settings(&self) -> Settings {
        self.running
            .lock_ok()
            .as_ref()
            .map(|r| r.settings.clone())
            .unwrap_or_default()
    }

    /// The SQL module's queries and their states (§I.10.6), for the panel.
    pub fn sql_states(&self) -> BTreeMap<String, crate::sql::QueryState> {
        self.sql().map(|e| e.states()).unwrap_or_default()
    }

    fn sql(&self) -> Option<Arc<crate::sql::Engine>> {
        self.running.lock_ok().as_ref().and_then(|r| r.sql.clone())
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock_ok().clone()
    }

    /// A command for the modules, answered here; anything else back to the caller.
    pub async fn handle(&self, c: Command) -> Option<Command> {
        // Remote support (screen, input, chat) first.
        if let Some((status, payload)) = self.remote.handle(&c).await {
            c.respond(status, payload);
            return None;
        }
        let body: serde_json::Value = serde_json::from_slice(&c.payload).unwrap_or_default();
        // "Send me the logs" (§I.12): the Node's recent logs, its crash reports and the
        // update helper's log, as text.
        if c.kind == "idn.logs" {
            let state = self.state.clone();
            let v = tokio::task::spawn_blocking(move || recent_logs(&state))
                .await
                .unwrap_or_default();
            c.answer(true, serde_json::to_vec(&v).unwrap_or_default());
            return None;
        }
        // Updates from the Site Manager (§I.17).
        if c.kind == "idn.update.offer" || c.kind == "idn.update.rollback" {
            let (node, state) = (self.node.clone(), self.state.clone());
            let kind = c.kind.clone();
            let r = tokio::task::spawn_blocking(move || {
                if kind == "idn.update.offer" {
                    crate::update::offer(&node, &state, &body)
                } else {
                    crate::update::rollback(&state)
                }
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            match r {
                Ok(v) => c.answer(true, serde_json::to_vec(&v).unwrap_or_default()),
                Err(e) => c.answer(false, e.into_bytes()),
            }
            return None;
        }
        // A read on request (§I.10.6.2): answered with the rows, not audited (it can't
        // change anything).
        if c.kind == "sql.query" {
            let Some(e) = self.sql() else {
                c.answer(false, b"no SQL queries are set on this machine".to_vec());
                return None;
            };
            let id = c.id.clone();
            let r = tokio::task::spawn_blocking(move || e.request(&id, &body))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            match r {
                Ok(v) => c.answer(true, serde_json::to_vec(&v).unwrap_or_default()),
                Err(e) => c.answer(false, e.into_bytes()),
            }
            return None;
        }
        let s = self.settings();
        let (kind, r) = match c.kind.as_str() {
            "modbus.write" => (
                "audit.modbus",
                crate::modbus::write(&s.modbus, &body)
                    .await
                    .map(serde_json::Value::String),
            ),
            "action.run" => {
                let actions = s.action.clone();
                let b = body.clone();
                let r = tokio::task::spawn_blocking(move || crate::actions::run(&actions, &b))
                    .await
                    .unwrap_or_else(|e| Err(idn_common::err::other(e)));
                ("audit.action", r)
            }
            _ => return Some(c),
        };
        let audit = serde_json::json!({
            "command": c.kind,
            "id": c.id,
            "request": body,
            "ok": r.is_ok(),
            "result": r.as_ref().ok(),
            "error": r.as_ref().err().map(|e| e.to_string()),
        });
        let node = self.node.clone();
        let _ = tokio::task::spawn_blocking(move || {
            node.send(kind, serde_json::to_vec(&audit).unwrap_or_default())
        })
        .await;
        match r {
            Ok(v) => c.answer(true, serde_json::to_vec(&v).unwrap_or_default()),
            Err(e) => c.answer(false, e.to_string().into_bytes()),
        }
        None
    }
}

/// The Node's logs for the Site Manager's "logs" request: the end of each log file in its
/// log folder (512 KiB each), its crash reports, and the update helper's log, 4 MiB in
/// all at most.
pub fn recent_logs(state: &Path) -> serde_json::Value {
    let dir = idn_open_protocol::diag::current()
        .and_then(|(_, p)| p.parent().map(Path::to_owned))
        .unwrap_or_else(idn_open_protocol::diag::log_dir);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "log"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths.extend(idn_open_protocol::diag::crash_reports_in(&dir));
    // Only IDN Industrial's (the folder may be shared with other programs').
    paths.retain(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().contains("industrial"))
    });
    paths.push(crate::update::log_path(state));
    let mut files = serde_json::Map::new();
    let mut total = 0usize;
    for p in paths {
        if total >= 4 << 20 {
            break;
        }
        if let Ok((text, cut)) = idn_open_protocol::diag::tail(&p, 512 * 1024) {
            total += text.len();
            files.insert(
                p.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                serde_json::json!({ "text": text, "cut": cut }),
            );
        }
    }
    serde_json::json!({
        "machine": sysinfo::System::host_name().ok(),
        "at_ms": crate::wire::now_ms(),
        "files": files,
    })
}

/// Takes the node's commands: the modules' answered here, the rest out of the receiver
/// returned (for the local API).
pub fn dispatch(
    node: &IndustrialNode,
    sup: Arc<Supervisor>,
) -> tokio::sync::mpsc::Receiver<Command> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    if let Some(mut from) = node.commands() {
        crate::node::runtime().spawn(async move {
            while let Some(c) = from.recv().await {
                if c.given_up() {
                    continue;
                }
                let sup = sup.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Some(c) = sup.handle(c).await else {
                        return;
                    };
                    // A place in the queue, waited for a while (more than 64 at once come
                    // when a machine reconnects and its held commands arrive: they were
                    // answered "unknown", their answer kept, the commands lost; bug review
                    // 3). No program reading commands, or none taken in time: unknown here.
                    match tokio::time::timeout(Duration::from_secs(30), tx.reserve()).await {
                        Ok(Ok(place)) => place.send(c),
                        _ => c.unknown(),
                    }
                });
            }
        });
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushed_files_cant_add_actions_or_writable_points() {
        let d = idn_common::testing::TempDir::new("modules");
        std::fs::create_dir_all(d.join("modules")).unwrap();
        std::fs::write(
            d.join("modules/plc.toml"),
            r#"
            [[modbus]]
            name = "plc"
            address = "127.0.0.1:502"
            [[modbus.points]]
            name = "setpoint"
            address = 1
            writable = true
            [[action]]
            name = "local-one"
            run = ["true"]
            "#,
        )
        .unwrap();
        let pushed = serde_json::json!({ "modules": { "extra": r#"
            [[modbus]]
            name = "pump"
            address = "127.0.0.1:503"
            [[modbus.points]]
            name = "speed"
            address = 2
            writable = true
            [[action]]
            name = "pushed-one"
            run = ["rm", "-rf", "/"]
            [health]
            processes = ["batching.exe"]
        "# } });
        assert!(write_pushed(&d, &pushed).unwrap());
        assert!(!write_pushed(&d, &pushed).unwrap(), "unchanged");
        let l = load(&d);
        let s = &l.settings;
        assert_eq!(
            s.action.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["local-one"]
        );
        let w = |dev: &str| s.modbus.iter().find(|d| d.name == dev).unwrap().points[0].writable;
        assert!(w("plc"), "the machine's own file decides");
        assert!(!w("pump"), "a pushed file can't");
        assert_eq!(s.health.as_ref().unwrap().processes, ["batching.exe"]);
        assert!(l.warnings.iter().any(|w| w.contains("actions only come")));
        // SQL: a pushed file adds queries and cadences, never connections.
        let pushed = serde_json::json!({ "modules": { "sql": r#"
            [sql]
            [[sql.connection]]
            name = "elsewhere"
            connect = "Driver=x;Server=evil;PWD=${secret:batch}"
            [[sql.query]]
            name = "silos"
            connection = "batch"
            kind = "snapshot"
            type = "scada.silos"
            sql = "SELECT 1"
            [sql.every]
            silos = 30
        "# } });
        std::fs::write(
            d.join("modules/sql.toml"),
            "[sql]\n[[sql.connection]]\nname = \"batch\"\nas = \"system\"\n",
        )
        .unwrap();
        write_pushed(&d, &pushed).unwrap();
        let l = load(&d);
        let q = l.settings.sql.unwrap();
        assert_eq!(
            q.connection
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["batch"]
        );
        assert_eq!(q.query[0].name, "silos");
        assert_eq!(q.every["silos"], 30);
        assert!(
            l.warnings
                .iter()
                .any(|w| w.contains("SQL connections only"))
        );
        std::fs::remove_file(d.join("modules/sql.toml")).unwrap();
        // Withdrawn by the Site Manager: removed.
        assert!(write_pushed(&d, &serde_json::json!({"modules": {}})).unwrap());
        assert!(load(&d).settings.modbus.iter().all(|m| m.name != "pump"));
    }
}
