//! The IDN Site Manager's panel (spec/extensions/idn-node.md §N.11.1): the central page
//! for every machine of the site, served by the Discovery Server that runs the site
//! authority, over HTTPS, opened with one site password.

use idn_common::sync::LockOk as _;
mod updates;

use idn_open_protocol::trust::{Authority, OptionScope};
use idn_open_protocol::web::{self, Logins, Request, Response, Sessions};
use std::collections::VecDeque;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod attention;
mod backups;
mod matrix;
pub mod mqtt_site;
mod netaudio;
mod netaudio_pool;
mod requests;
mod saved;
mod settings;
mod status;
mod sync;
mod tokens;
#[cfg(feature = "voice")]
mod voice;

/// The site's templates (§N.21) in the Site Manager's folder.
const TEMPLATES: &str = "templates.json";
mod commands;
pub mod discovery_server;
mod events;
mod front;
pub mod host;
pub mod host_api;
pub mod presets;
pub mod typed;
pub mod users;
pub mod watch;
/// The `/metrics` token's SHA-256 (hex) in the Site Manager's folder (N5).
const METRICS_TOKEN: &str = "metrics-token";

/// The settings a machine may be sent from the panel's forms, copies and templates
/// (§N.21), checked: known keys only (the Node checks their values). A PIN in clear
/// (`pin`) becomes its hash (`panel_pin`; "none": no PIN; empty: unchanged, left out, as a
/// form posted with its PIN field blank); a hash passes as it is.
fn machine_settings(
    v: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let Some(o) = v.as_object() else {
        return Err("settings: an object of names and values".into());
    };
    let groups = [
        "audio.",
        "capture.",
        "sitelink.",
        "aes67.",
        "listen.",
        "data.",
        "omt.",
        "captions.",
        "netaudio.",
        "apps.",
        "video.",
        // Routers (§N.29): the list and switching one (operators may switch, §N.31).
        "routers.",
    ];
    let single = [
        "require",
        "approve_programs",
        "clock",
        "clock_domain",
        "clock_protocol",
        // The machine's own sample rate; empty: the site's (§N.40).
        "audio_rate",
        "services_off",
        "panel_site_only",
        // Another product installs and updates the machine's IDN Node (§N.23); empty:
        // the Site Manager's updates again.
        "update.managed_by",
    ];
    let mut out = serde_json::Map::new();
    for (k, v) in o {
        let v = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => return Err(format!("{k}: text, a number or true/false")),
        };
        match k.as_str() {
            "pin" => {
                let p = v.trim();
                // Empty: unchanged (a UI posting its whole form with the PIN field blank
                // cleared every machine's PIN: bug run O).
                if p.is_empty() {
                    continue;
                }
                // "none": no PIN (just the button), as the page's forms had it.
                let h = if p.eq_ignore_ascii_case("none") {
                    String::new()
                } else if (4..=32).contains(&p.chars().count()) {
                    web::hash_password(p).map_err(|e| e.to_string())?
                } else {
                    return Err("the PIN: 4 to 32 characters".into());
                };
                out.insert("panel_pin".into(), h.into());
            }
            "panel_pin" if v.is_empty() || v.starts_with("$argon2") => {
                out.insert(k.clone(), v.into());
            }
            // Empty (a template's form with no value): left out, not sent to every Node
            // (which refused the whole message: bug run H).
            "clock_protocol" if v.is_empty() => {}
            "clock_protocol" if idn_open_protocol::ptp::Protocol::parse(&v).is_none() => {
                return Err("clock_protocol: v2, v1 or both".into());
            }
            "audio_rate" if !v.is_empty() && idn_open_protocol::node::rate::parse(&v).is_none() => {
                return Err("audio_rate: 44100, 48000 or 96000 (empty: the site's)".into());
            }
            // (As the Node takes it: a product's name, at most 64 characters, printable.)
            "update.managed_by"
                if v.trim().chars().count() > 64 || v.trim().chars().any(char::is_control) =>
            {
                return Err(
                    "update.managed_by: a product's name, at most 64 characters (empty: none)"
                        .into(),
                );
            }
            _ if single.contains(&k.as_str()) => {
                out.insert(k.clone(), v.into());
            }
            _ if groups.iter().any(|g| {
                k.strip_prefix(g).is_some_and(|r| {
                    !r.is_empty() && r.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                })
            }) =>
            {
                out.insert(k.clone(), v.into());
            }
            _ => return Err(format!("{k}: not a setting the Site Manager sends")),
        }
    }
    Ok(out)
}

/// A Node report's settings for the panel's forms (§N.11.3): each group's attributes —
/// `audio`, `capture`, `sitelink`, `aes67`, `listen`, `data`, `apps`, `panel` (the simple-mode PIN:
/// set or not, the site's or not; never the hash), `services` (`off`) — and `approve_programs`.
fn settings_json(e: &idn_open_protocol::xml::Element) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for g in [
        "audio", "capture", "sitelink", "aes67", "listen", "data", "omt", "captions", "netaudio",
        "routers", "apps", "panel", "services", "update",
    ] {
        // A machine older than updates reports none: null, not empty (the page says so).
        if g == "update" && e.child(g).is_none() {
            m.insert(g.into(), serde_json::Value::Null);
            continue;
        }
        let o: serde_json::Map<String, serde_json::Value> = e
            .child(g)
            .map(|c| {
                c.attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect()
            })
            .unwrap_or_default();
        m.insert(g.into(), serde_json::Value::Object(o));
    }
    m.insert(
        "approve_programs".into(),
        serde_json::Value::Bool(e.attr("approve_programs") == Some("true")),
    );
    // Every service these settings make, with its switch (§N.14.4): as the machine
    // reports them (`runs` is its own: the display service from boot is Linux's alone).
    let list: Vec<serde_json::Value> = e
        .children_named("service")
        .filter_map(|s| {
            let flag = |k: &str| s.attr(k) == Some("true");
            Some(
                serde_json::json!({"id": s.attr("id")?, "kind": s.attr("kind").unwrap_or(""),
                "title": s.attr("title").unwrap_or(""), "on": flag("on"), "auto": flag("auto"),
                "runs": s.attr("runs").unwrap_or("session")}),
            )
        })
        .collect();
    m.insert("services_list".into(), list.into());
    serde_json::Value::Object(m)
}

/// What a site's backup leaves out (idn-node.md §N.17): crash reports, logs, alerts and the
/// audit, releases, and a restore waiting for the next start.
pub const BACKUP_SKIP: &[&str] = &[
    "reports",
    "node-logs",
    idn_open_protocol::trust::ALERTS,
    users::AUDIT_FILE,
    "releases",
    // The site's MQTT broker's store (§N.34): held open while the panel runs (a copy then
    // could be half-written); its sessions and retained messages come back by themselves,
    // its logins are added again after a restore (mqtt.json, on/off and the address, is
    // kept).
    "mqtt.redb",
    idn_open_protocol::backup::STAGED,
    // The scheduled backups themselves (§N.42).
    backups::DEFAULT_FOLDER,
];

/// The panel's default port (§N.11.1).
pub const PANEL_PORT: u16 = idn_common::ports::SITE_MANAGER;

/// A tokio runtime the Site Manager owns (Voice's, the MQTT broker's), shut down safely
/// wherever it is dropped: waited for (a second at most, its ports then free) from a
/// plain thread; let go to end by itself from inside an async context (a host's `Handle`
/// dropped or stopped in a task, or the last reference going on another runtime's
/// thread), where tokio panics on waiting ("Cannot drop a runtime in a context where
/// blocking is not allowed").
pub(crate) struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl OwnedRuntime {
    pub(crate) fn new(rt: tokio::runtime::Runtime) -> OwnedRuntime {
        OwnedRuntime(Some(rt))
    }
}

impl std::ops::Deref for OwnedRuntime {
    type Target = tokio::runtime::Runtime;
    fn deref(&self) -> &tokio::runtime::Runtime {
        self.0.as_ref().expect("taken only when dropped")
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(std::time::Duration::from_secs(1));
            }
        }
    }
}

/// The file in the authority's directory that holds the site password's hash.
const PASSWORD_FILE: &str = web::SITE_PASSWORD_FILE;

/// Sets the site password (kept as an Argon2id hash in the authority's directory).
pub fn set_password(dir: &Path, password: &str) -> io::Result<()> {
    // 8 characters or more, as a user's (the engines' own LAN passwords take 6).
    if password.chars().count() < 8 {
        return Err(idn_common::err::invalid_input(
            "the site password needs 8 characters or more",
        ));
    }
    web::set_site_password(dir, password)
}

/// The running panel; stops when dropped.
pub struct SiteManagerPanel {
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    /// The certificate's SHA-256 fingerprint, to compare with what browsers show.
    pub fingerprint: String,
    /// The port it listens on.
    pub port: u16,
    /// IDN Voice beside it (HTTPS on the next port), if on.
    #[cfg(feature = "voice")]
    voice: Option<voice::Running>,
    /// Its server's accept thread: dropped, the panel waits for it (the port free, the
    /// push channels ended; bug run O-M5).
    served: Option<std::thread::JoinHandle<()>>,
}

impl SiteManagerPanel {
    /// A typed command run for `user` (their role checked, the change audited as theirs):
    /// what the API answered, or why not.
    pub(crate) fn run(
        &self,
        user: &str,
        caller: &host_api::Caller,
        c: &typed::SiteCommand,
    ) -> Result<serde_json::Value, String> {
        let r = web::Request {
            method: "POST".into(),
            path: format!("/api/v1/{}", c.path()),
            query: String::new(),
            headers: [
                ("x-idn-node".to_owned(), "1".to_owned()),
                ("x-idn-client".to_owned(), caller.client.clone()),
            ]
            .into(),
            body: c.body().to_string().into_bytes(),
            peer: std::net::IpAddr::from([127, 0, 0, 1]),
            tls: true,
        };
        // (The audit's `peer` and `client`: whom the host made it for.)
        let mut who = web::Who::host(user);
        who.peer = caller.address.clone();
        who.client = caller.client.clone();
        let resp = web::engine::handle_as(&self.state, r, who);
        let answer: serde_json::Value = serde_json::from_slice(&resp.body).unwrap_or_default();
        if resp.status.starts_with('2') {
            Ok(answer)
        } else {
            Err(answer["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| String::from_utf8_lossy(&resp.body).into_owned()))
        }
    }
}

impl Drop for SiteManagerPanel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.served.take() {
            let _ = t.join();
        }
    }
}

impl SiteManagerPanel {
    /// Serves the panel for `authority` (whose files are in `dir`, and whose Discovery
    /// Server listens on `ds_port`) on all interfaces at `port`.
    pub fn start(authority: Authority, dir: &Path, ds_port: u16, port: u16) -> io::Result<Self> {
        Self::start_with(authority, dir, ds_port, port, true)
    }

    /// [`start`](Self::start), with IDN Voice beside it or not (`voice`; without the
    /// `voice` feature it never is).
    pub fn start_with(
        authority: Authority,
        dir: &Path,
        ds_port: u16,
        port: u16,
        voice: bool,
    ) -> io::Result<Self> {
        let (tls, fingerprint) = web::self_signed(dir, "IDN Site Manager")?;
        let l = TcpListener::bind(("0.0.0.0", port)).map_err(|e| {
            io::Error::new(e.kind(), format!("Site Manager panel port {port}: {e}"))
        })?;
        let port = l.local_addr()?.port();
        let state = Arc::new(State {
            authority,
            dir: dir.to_owned(),
            ds_port,
            sessions: Sessions::new(Duration::from_secs(12 * 3600)),
            password_seen: Mutex::new(std::fs::read(dir.join(PASSWORD_FILE)).ok()),
            logins: Logins::default(),
            log: Mutex::new(VecDeque::new()),
            sources: Mutex::new(None),
            updates: updates::Updates::new(dir),
            port,
            watch: watch::Watch::start(ds_port),
            users: users::Users::new(dir),
            mqtt: mqtt_site::SiteMqtt::new(dir)
                .map_err(|e| idn_open_protocol::log::log(&format!("site MQTT: {e}")))
                .ok(),
            http_seq: Mutex::default(),
            keys_lock: Mutex::default(),
            status_seen: Mutex::new((0, 0)),
            status_cache: Mutex::new(None),
        });
        // The Site Manager's own typed events (§N.41) into the site's log, with the
        // machines' alerts (a minute apart at most per event and subject).
        {
            let a = state.authority.clone();
            state.watch.events.log.set_sink(move |l| {
                let fp = l.detail["fingerprint"].as_str().unwrap_or("").to_owned();
                let _ = a.add_event(&fp, "site manager", &l.level, &l.event, &l.text, &l.detail);
            });
        }
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = state.clone();
        // A release comes in pieces of up to 4 MiB (§N.23); a backup to restore whole, up
        // to 64 MiB (a site's presets and templates went past the
        // 64 KiB allowed elsewhere, and couldn't be restored).
        const LARGE: &[(&str, usize)] = &[
            (
                "/api/v1/release/",
                idn_open_protocol::release::Store::PIECE + 64 * 1024,
            ),
            ("/api/v1/restore", 64 << 20),
        ];
        // Those large bodies only in a session.
        let s3 = state.clone();
        let gate: web::Gate = Arc::new(move |r: &web::Request| {
            s3.sessions_follow_password();
            s3.sessions.user(r.cookie("idn_site")).is_some()
        });
        #[cfg(feature = "voice")]
        let tls_voice = tls.clone();
        // (Handsets pin it, §V.13: the panel's certificate is Voice's too.)
        #[cfg(feature = "voice")]
        let fp_voice = fingerprint.clone();
        let served = web::serve_until(
            l,
            Some(tls),
            Arc::new(move |r| {
                // The push channel: only from this page's own origin (no other site's page
                // can open it with the session's cookie); then the shared front.
                if r.method == "GET" && r.path == "/api/v1/events" && !r.same_origin() {
                    return web::Response::text("403 Forbidden", "another site's page");
                }
                s2.serve(r)
            }),
            stop.clone(),
            LARGE,
            Some(gate),
        )?;
        // A staged rollout's next step, as the first machine reports.
        {
            let (s3, stop) = (Arc::downgrade(&state), stop.clone());
            std::thread::Builder::new()
                .name("idn-site-updates".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(2));
                        let Some(s) = s3.upgrade() else { return };
                        s.updates.tick(&s.authority, s.port);
                    }
                })?;
        }
        // The site's sync groups (§A.14): every 2 s, receivers of the same transmitters
        // kept in line.
        {
            let (s5, stop) = (Arc::downgrade(&state), stop.clone());
            std::thread::Builder::new()
                .name("idn-site-sync".into())
                .spawn(move || {
                    let mut sync = sync::Sync::default();
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(2));
                        let Some(s) = s5.upgrade() else { return };
                        sync.tick(&s.authority);
                    }
                })?;
        }
        // Site monitoring's round (N5): every 5 s.
        {
            let (s4, stop) = (Arc::downgrade(&state), stop.clone());
            std::thread::Builder::new()
                .name("idn-site-watch".into())
                .spawn(move || {
                    let mut rounds = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(5));
                        let Some(s) = s4.upgrade() else { return };
                        s.watch.tick(&s.authority);
                        // The network audio pools' unused channels let go (K.12), every 30 s.
                        rounds = rounds.wrapping_add(1);
                        // A scheduled backup when one is due (§N.42), once a minute.
                        if rounds.is_multiple_of(12) {
                            s.backup_tick();
                        }
                        if rounds.is_multiple_of(6) {
                            // Saved presets' routes count as in use (M5-3).
                            let held: Vec<(String, u16)> = s
                                .presets()
                                .iter()
                                .flat_map(|p| {
                                    p["receivers"].as_array().cloned().unwrap_or_default()
                                })
                                .flat_map(|rx| presets::saved_routes(&rx).into_values())
                                .collect();
                            netaudio_pool::sweep_later(&s.authority, &s.dir, held);
                        }
                        // The site's machines on its MQTT broker (§N.34), where they changed.
                        if let Some(m) = &s.mqtt {
                            m.publish_machines(&s.status_for("", None));
                        }
                    }
                })?;
        }
        #[cfg(feature = "voice")]
        let voice = voice
            .then(|| voice::start(&state, tls_voice, &fp_voice, port))
            .flatten();
        #[cfg(not(feature = "voice"))]
        let _ = voice;
        Ok(SiteManagerPanel {
            state,
            stop,
            fingerprint,
            port,
            #[cfg(feature = "voice")]
            voice,
            served: Some(served),
        })
    }

    /// IDN Voice's HTTPS port (§V.1), if it runs.
    pub fn voice_port(&self) -> Option<u16> {
        #[cfg(feature = "voice")]
        return self.voice.as_ref().map(|v| v.port);
        #[cfg(not(feature = "voice"))]
        None
    }

    /// A line for the panel's log (the console's lines, as they are printed).
    pub fn log(&self, line: &str) {
        self.state.log(line);
    }

    /// Ends every session (the password changed).
    pub fn logout_all(&self) {
        self.state.sessions.end_all();
    }
}

struct State {
    authority: Authority,
    /// One change to the named API keys' file at a time (a key's use noted while one is
    /// made dropped the new key: read, changed and written back from the same old list).
    keys_lock: Mutex<()>,
    dir: PathBuf,
    ds_port: u16,
    sessions: Sessions,
    /// When the password file was last seen changed: the Node sets the site password in
    /// it too (its settings), and the sessions went on (deep bug run 2026-11-09).
    password_seen: Mutex<Option<Vec<u8>>>,
    logins: Logins,
    log: Mutex<VecDeque<String>>,
    /// The Discovery Server's source names, and when they were asked for (the routing
    /// matrix's transmitters not on a Node machine).
    sources: Mutex<Option<(std::time::Instant, Vec<String>)>>,
    /// Releases and their rollout (§N.23).
    updates: updates::Updates,
    /// This panel's port (machines fetch their packages from it).
    port: u16,
    /// Site monitoring (N5): clocks, links, streams, alerts, metrics.
    watch: Arc<watch::Watch>,
    /// Users, roles and the audit (N4, §N.31).
    users: users::Users,
    /// The site's MQTT broker (§N.34); none if its store couldn't be opened.
    mqtt: Option<mqtt_site::SiteMqtt>,
    /// The shared front's per-control sequence numbers over HTTP (`crate::front`).
    http_seq: Mutex<web::events::Sequencer>,
    /// The status's version, and the hash of the status it was given for.
    status_seen: Mutex<(u64, u64)>,
    /// The site's part of the status last made, when, and its version (`status.rs`).
    status_cache: Mutex<Option<(std::time::Instant, serde_json::Value, u64)>>,
}

fn ok() -> Response {
    Response::json(r#"{"ok":true}"#)
}

/// The JSON body's string field.
fn field(body: &[u8], k: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    match v.get(k)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The JSON body's yes/no field: a boolean, or a form's text value (on/off, true/false,
/// yes/no, 1/0; bug run O).
fn field_bool(body: &[u8], k: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    match v.get(k)? {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => idn_common::text::parse_on_off(s),
        _ => None,
    }
}

impl State {
    fn log(&self, line: &str) {
        // One line (names read from the network are in some; bug run N1-6).
        let line: String = line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut l = self.log.lock_ok();
        if l.len() >= 200 {
            l.pop_front();
        }
        l.push_back(line);
    }

    /// Everyone signed out if the password file changed since last looked at (by its
    /// contents: two changes within one tick of the file's time were missed by its time).
    fn sessions_follow_password(&self) {
        let now = std::fs::read(self.dir.join(PASSWORD_FILE)).ok();
        let mut seen = self.password_seen.lock_ok();
        if *seen != now {
            *seen = now;
            drop(seen);
            self.sessions.end_all();
            // The site password is the built-in administrator's: their engine tokens end
            // too (§N.31.5), the first password set included (it was read at the start).
            self.withdraw_tokens(users::ADMIN);
        }
    }

    fn password(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join(PASSWORD_FILE))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| s.starts_with("$argon2"))
    }
}

/// The `ms` of a latency request (idn-routing.md §A.12): a number of ms in range, or
/// null / absent / empty for the program's own.
fn latency_of(v: &serde_json::Value) -> Result<Option<u32>, String> {
    use serde_json::Value;
    let ms = match &v["ms"] {
        Value::Null => return Ok(None),
        Value::String(s) if s.trim().is_empty() => return Ok(None),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        x => x.as_u64().and_then(|n| u32::try_from(n).ok()),
    };
    ms.filter(|ms| idn_open_protocol::node::routing::LATENCY_MS.contains(ms))
        .map(Some)
        .ok_or_else(|| "latency: 1 to 500 ms, or none for the program's own".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machines_apps_and_its_services_as_it_reports_them() {
        use idn_open_protocol::xml::Element;
        let report = Element::new("node")
            .with_child(
                Element::new("apps")
                    .with_attr("display", "true")
                    .with_attr("display_boot", "true"),
            )
            .with_child(
                Element::new("service")
                    .with_attr("id", "display")
                    .with_attr("kind", "display")
                    .with_attr("title", "IDN Display service")
                    .with_attr("runs", "node")
                    .with_attr("on", "true")
                    .with_attr("auto", "true"),
            );
        let v = settings_json(&report);
        assert_eq!(v["apps"]["display"], "true");
        assert_eq!(v["apps"]["display_boot"], "true");
        let list = v["services_list"].as_array().unwrap();
        assert_eq!(list.len(), 1, "the machine's own list: {list:?}");
        assert_eq!(list[0]["runs"], "node");
        assert_eq!(
            (list[0]["on"].as_bool(), list[0]["auto"].as_bool()),
            (Some(true), Some(true))
        );
        // Sent through Apply as text or booleans.
        let s =
            machine_settings(&serde_json::json!({"apps.display": true, "apps.display_boot": "on"}))
                .unwrap();
        assert_eq!(
            (s["apps.display"].as_str(), s["apps.display_boot"].as_str()),
            (Some("true"), Some("on"))
        );
    }

    #[test]
    fn only_machine_settings_are_sent_and_a_pin_goes_as_its_hash() {
        let s = machine_settings(&serde_json::json!({
            "audio.mic": true,
            "listen.channels": "English=MIXER (Main)",
            "video.threads": 4,
            "clock": "on",
            "services_off": "screen:1",
            "pin": "2468",
        }))
        .unwrap();
        assert_eq!(s["audio.mic"], "true");
        assert_eq!(s["video.threads"], "4");
        let h = s["panel_pin"].as_str().unwrap();
        assert!(h.starts_with("$argon2") && web::verify_password(h, "2468"));
        assert!(!s.contains_key("pin"));
        // "none" is no PIN; an empty PIN is unchanged (not sent).
        for none in ["none", "None"] {
            assert_eq!(
                machine_settings(&serde_json::json!({"pin": none})).unwrap()["panel_pin"],
                ""
            );
        }
        for blank in ["", "  "] {
            let s = machine_settings(&serde_json::json!({"pin": blank, "clock": "on"})).unwrap();
            assert!(!s.contains_key("panel_pin") && !s.contains_key("pin"));
        }
        // A yes/no field: a boolean or the schema's text.
        for (body, want) in [
            (r#"{"x": true}"#, Some(true)),
            (r#"{"x": "on"}"#, Some(true)),
            (r#"{"x": "True"}"#, Some(true)),
            (r#"{"x": "off"}"#, Some(false)),
            (r#"{"x": "false"}"#, Some(false)),
            (r#"{"x": "maybe"}"#, None),
            (r#"{"y": true}"#, None),
        ] {
            assert_eq!(field_bool(body.as_bytes(), "x"), want, "{body}");
        }
        // Not sent: the machine's identity and secrets, unknown keys, a PIN in clear as a
        // hash, a short PIN, nested values.
        for bad in [
            serde_json::json!({"name": "X"}),
            serde_json::json!({"server": "1.2.3.4"}),
            serde_json::json!({"passphrase": "x"}),
            serde_json::json!({"lan_password": "x"}),
            serde_json::json!({"audio.": "x"}),
            serde_json::json!({"audio.Mic": "x"}),
            serde_json::json!({"panel_pin": "2468"}),
            serde_json::json!({"pin": "12"}),
            serde_json::json!({"listen.channels": ["a"]}),
            serde_json::json!({"update.managed_by": "x".repeat(65)}),
            serde_json::json!({"update.managed_by": "Host\nApp"}),
            serde_json::json!({"update.track": "beta"}),
        ] {
            assert!(machine_settings(&bad).is_err(), "{bad}");
        }
        // Who updates a machine (§N.23): a product's name, or empty for none.
        for name in ["HostApp", " HostApp ", ""] {
            let s = machine_settings(&serde_json::json!({"update.managed_by": name})).unwrap();
            assert_eq!(s["update.managed_by"], name);
        }
    }
}
