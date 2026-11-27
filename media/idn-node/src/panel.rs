//! The Node's panel (spec/extensions/idn-node.md §N.6, §N.11.3): on the loopback over HTTP,
//! opened with a one-time token from the IPC; and on the LAN over HTTPS with a password,
//! while one is set.

mod api;
mod changes;
mod commands;
mod front;
mod status_json;

use crate::server::{Change, LAN_PANEL_PORT, NodeCore};
use idn_common::cmd::RunWithin as _;
use idn_common::sync::LockOk as _;
use idn_open_protocol::discovery::{Browser, BrowserConfig};
use idn_open_protocol::web::{self, Logins, Request, Response, Sessions};
use serde_json::{Value, json};
use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INDEX: &str = include_str!("panel/index.html");
/// The loopback session of a token a program asked for while programs need approval
/// (F6-1): its "user".
const LIMITED: &str = "(opened by a program)";
const APP_JS: &str = include_str!("panel/app.js");
/// The pages by the menu's sections (panel/pages/), served as one script: the shared part
/// first, each section, then the end (every page known).
const PAGES_JS: &str = concat!(
    include_str!("panel/pages/common.js"),
    include_str!("panel/pages/machine.js"),
    include_str!("panel/pages/sound.js"),
    include_str!("panel/pages/video.js"),
    include_str!("panel/pages/data.js"),
    include_str!("panel/pages/network.js"),
    include_str!("panel/pages/site.js"),
    include_str!("panel/pages/apps.js"),
    include_str!("panel/pages/advanced.js"),
    include_str!("panel/pages/end.js"),
);
const WIZARD_JS: &str = include_str!("panel/wizard.js");
const APP_CSS: &str = include_str!("panel/app.css");
const LOCKED: &str = include_str!("panel/locked.html");
/// Where a loopback panel's tab keeps its session (F6-9).
const SESSION_KEY: &str = "idn-node-session";

/// The panel's page on 127.0.0.1: before anything else, every request of the page sends
/// the tab's session (`X-IDN-Session`); none (a page opened without the panel's link), or
/// one refused (ended): the "open it again" page.
fn loopback_index() -> String {
    let boot = format!(
        "<script>(function(){{var k='{SESSION_KEY}',s=null;try{{s=sessionStorage.getItem(k)}}catch(e){{}}\
if(!s){{location.replace('/locked');return}}var f=window.fetch.bind(window);\
window.fetch=function(u,o){{o=Object.assign({{}},o||{{}});o.headers=Object.assign({{}},o.headers||{{}},{{'X-IDN-Session':s}});\
return f(u,o).then(function(r){{if(r.status===401||r.status===403){{try{{sessionStorage.removeItem(k)}}catch(e){{}}}}return r}})}}}})();</script>"
    );
    INDEX.replacen("<head>", &format!("<head>\n{boot}"), 1)
}
const LOGIN: web::ui::Login = web::ui::Login {
    title: "IDN Node",
    hint: Some("This machine's IDN settings. Log in with its panel password."),
    label: "Password",
    header: "X-IDN-Node",
    who: "machine",
    console: false,
    user: false,
};

/// How long a panel session lasts.
const SESSION: Duration = Duration::from_secs(12 * 3600);

pub struct Panel {
    port: u16,
    stop: Arc<AtomicBool>,
    /// Its server's and the LAN panel's threads: dropped, it waits for them (the ports
    /// free, the push channels ended).
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

struct State {
    core: Arc<NodeCore>,
    port: u16,
    /// Loopback sessions (from one-time tokens).
    local: Sessions,
    /// LAN sessions (from the password).
    lan: Sessions,
    logins: Logins,
    /// The source lists for the "Network" section, for the current server and interface:
    /// mDNS always, and the Discovery Server when one is set (a browser uses one or the
    /// other).
    browser: Mutex<Option<Browsers>>,
    /// The AES67 streams heard by the last search (§N.14.5).
    aes67_found: Arc<Mutex<Aes67Search>>,
    /// Site-signed tokens for this Node's panel on the LAN (engine `node`; §N.31.5).
    tokens: Arc<web::TokenCheck>,
    /// The panel's changes, who made them and from where (`audit.jsonl`; §N.31.6).
    audit: Arc<web::audit::Audit>,
    /// Per-control sequence numbers over HTTP (`X-IDN-Client`, `X-IDN-Control`,
    /// `X-IDN-Seq`): a change older than one applied to its control is skipped.
    seq: Mutex<web::events::Sequencer>,
    /// The status's version, and the hash of the status it was given for (panel/front.rs).
    status_seen: Mutex<(u64, u64)>,
    /// The status last made, when, and its version.
    status_cache: Mutex<Option<(std::time::Instant, Value, u64)>>,
}

/// A search for announced AES67 streams: `idn audio aes67 list --tsv`, 30 s.
#[derive(Default)]
struct Aes67Search {
    running: bool,
    /// When the last one ended (seconds since the epoch).
    at: Option<u64>,
    streams: Vec<Value>,
    error: Option<String>,
}

/// The browsers for (server, interface), and when they were started.
type Browsers = (
    (Option<String>, Option<String>),
    Vec<Result<Arc<Browser>, String>>,
    Instant,
);

impl Panel {
    pub fn start(core: Arc<NodeCore>) -> io::Result<Panel> {
        let port = core.settings().panel_port;
        let l = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| io::Error::new(e.kind(), format!("panel port 127.0.0.1:{port}: {e}")))?;
        let port = l.local_addr()?.port();
        let st = Arc::new(State {
            core: core.clone(),
            port,
            local: Sessions::new(SESSION),
            lan: Sessions::new(SESSION),
            logins: Logins::default(),
            browser: Mutex::new(None),
            aes67_found: Arc::default(),
            tokens: {
                let core = core.clone();
                web::TokenCheck::new("node", move || web::SiteKeys::of(&*core.enrollment()?))
            },
            audit: web::audit::Audit::for_engine(Some(core.state_dir())),
            seq: Mutex::default(),
            status_seen: Mutex::new((0, 0)),
            status_cache: Mutex::new(None),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = st.clone();
        let served = web::serve_until(
            l,
            None,
            Arc::new(move |r| {
                // The push channel on this machine: the tab's session as a header, or (a
                // browser's WebSocket can't send one) as `?session=`; then the shared front
                // (panel/front.rs: `{"type": "state"}`, events, commands).
                if r.method == "GET" && r.path == "/api/v1/events" {
                    let sid = r
                        .header("x-idn-session")
                        .or(r.param("session"))
                        .unwrap_or("")
                        .to_owned();
                    return match s2.local.user(Some(&sid)) {
                        Some(user) if r.same_origin() => {
                            let who = local_who(&r, &user);
                            // Open only while its session is (ended, the channel went on
                            // with full control; bug run 4).
                            let w = Arc::downgrade(&s2);
                            let still: web::StillAllowed = Arc::new(move || {
                                w.upgrade()
                                    .is_some_and(|s| s.local.user(Some(&sid)).is_some())
                            });
                            web::engine::handle_as(&s2, r, who).while_allowed(still)
                        }
                        _ => Response::text("403 Forbidden", "not logged in"),
                    };
                }
                s2.loopback(r)
            }),
            stop.clone(),
            &[],
            None,
        )?;
        // Start searching now, so the list is there when the panel is first opened.
        let early = st.clone();
        std::thread::spawn(move || {
            let _ = early.sources();
        });
        // The LAN panel, while a password is set (§N.11.3).
        let (s3, stop3) = (st.clone(), stop.clone());
        let lan = std::thread::Builder::new()
            .name("idn-node-lan-panel".into())
            .spawn(move || lan_supervisor(s3, stop3));
        let lan = match lan {
            Ok(t) => t,
            Err(e) => {
                stop.store(true, Ordering::Relaxed);
                let _ = served.join();
                return Err(e);
            }
        };
        Ok(Panel {
            port,
            stop,
            threads: vec![served, lan],
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Starts the LAN panel when a password is set or the machine is in a site (its tokens,
/// §N.31.5), and stops it when neither; a changed password ends the LAN sessions.
fn lan_supervisor(st: Arc<State>, stop: Arc<AtomicBool>) {
    let mut running: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> = None;
    let mut hash: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        let want = st.core.settings().lan_password;
        if want != hash {
            st.lan.end_all();
            hash = want.clone();
        }
        // On with a password, or in a site (tokens checked).
        let on = want.is_some() || st.tokens.available();
        match (on, &running) {
            (true, None) => match start_lan(&st) {
                Ok(s) => running = Some(s),
                Err(e) => {
                    st.core.log(&format!("LAN panel: {e}"));
                    // Tried again in 10 s, the stop looked at meanwhile.
                    for _ in 0..100 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            },
            (false, Some(_)) => {
                if let Some((s, t)) = running.take() {
                    s.store(true, Ordering::Relaxed);
                    let _ = t.join();
                }
                st.core.log("LAN panel off");
            }
            _ => {}
        }
        // A second, the stop looked at more often (the host's stop waits for this).
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some((s, t)) = running {
        s.store(true, Ordering::Relaxed);
        let _ = t.join();
    }
}

/// A loopback session's caller: `local`, or a session a program opened (`LIMITED`).
fn local_who(r: &Request, user: &str) -> web::Who {
    let mut who = web::Who::local(r);
    if user == LIMITED {
        who.user = LIMITED.to_owned();
        who.name = LIMITED.to_owned();
    }
    who
}

fn start_lan(st: &Arc<State>) -> io::Result<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
    // The site's certificate for the machine while it is in one (extensions §E.10.11),
    // else the panel's own.
    let membership: web::lan::Membership = {
        let core = Arc::downgrade(&st.core);
        Arc::new(move || core.upgrade()?.enrollment())
    };
    let (tls, fp) = web::lan::server_config(st.core.state_dir(), "IDN Node", membership)?;
    let port = st.core.settings().lan_port;
    let l = TcpListener::bind(("0.0.0.0", port))
        .map_err(|e| io::Error::new(e.kind(), format!("port {port}: {e}")))?;
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = st.clone();
    let served = web::serve_until(
        l,
        Some(tls),
        Arc::new(move |r| {
            // The push channel on the network: the login's cookie, or a site token (in
            // the header, `?token=`, or its first message; §N.31.5); then the shared front.
            if r.method == "GET" && r.path == "/api/v1/events" {
                if !r.same_origin() {
                    return Response::text("403 Forbidden", "not logged in");
                }
                let who = match s2.tokens.who(&r) {
                    Some(Ok(w)) => w,
                    Some(Err(e)) => return web::auth::refused(&e),
                    None if s2.lan.valid(r.cookie("idn_node_lan")) => {
                        // Open only while its login is (the LAN password changed, its
                        // sessions ended, and the channel went on; bug run 4).
                        let cookie = r.cookie("idn_node_lan").map(str::to_owned);
                        let w = Arc::downgrade(&s2);
                        let still: web::StillAllowed = Arc::new(move || {
                            w.upgrade().is_some_and(|s| s.lan.valid(cookie.as_deref()))
                        });
                        let who = web::Who::lan_password(&r);
                        return web::engine::handle_as(&s2, r, who).while_allowed(still);
                    }
                    None if s2.tokens.available() => web::Who::pending(&r, s2.tokens.clone()),
                    None => return Response::text("403 Forbidden", "not logged in"),
                };
                return web::engine::handle_as(&s2, r, who);
            }
            s2.lan(r)
        }),
        stop.clone(),
        &[],
        None,
    )?;
    st.core.log(&format!(
        "LAN panel on https://<this machine>:{port}/ (certificate SHA-256 {fp})"
    ));
    Ok((stop, served))
}

impl State {
    /// Requests on 127.0.0.1.
    fn loopback(self: &Arc<Self>, r: Request) -> Response {
        let host = r.header("host").unwrap_or("");
        if host != format!("127.0.0.1:{}", self.port) && host != format!("localhost:{}", self.port)
        {
            return Response::text("421 Misdirected Request", "wrong host");
        }
        if r.method == "GET"
            && r.path == "/"
            && let Some(t) = r.param("t")
        {
            if let Some(full) = self.core.take_token(t) {
                let sid = if full {
                    self.local.create()
                } else {
                    self.local.create_for(LIMITED)
                };
                // The session kept in this tab (sessionStorage: per address and port),
                // sent as a header: a cookie went to every port of 127.0.0.1, so a page
                // another local program served on another port was given the Node's
                // session when the operator opened it (bug round F, F6-9).
                return Response::ok(
                    "text/html; charset=utf-8",
                    format!(
                        "<!doctype html><meta charset=utf-8><title>IDN Node</title><script>try{{sessionStorage.setItem('{SESSION_KEY}','{sid}')}}catch(e){{}}location.replace('/')</script>"
                    ),
                )
                .with("Cache-Control", "no-store");
            }
            return Response::new("403 Forbidden", "text/html; charset=utf-8", LOCKED);
        }
        // The page itself (nothing in it but code): it sends the tab's session, or shows
        // how to open the panel.
        if r.method == "GET" && r.path == "/" {
            return Response::ok("text/html; charset=utf-8", loopback_index())
                .with("Cache-Control", "no-store");
        }
        if r.method == "GET" && r.path == "/locked" {
            return Response::new("403 Forbidden", "text/html; charset=utf-8", LOCKED);
        }
        // A session's request through the shared API front (panel/front.rs); one opened by
        // a program is written so in the audit (its user, `LIMITED`).
        match self.local.user(r.header("x-idn-session")) {
            Some(user) => {
                let who = local_who(&r, &user);
                web::engine::handle_as(self, r, who)
            }
            None => self.route(r, false, false),
        }
    }

    /// Requests on the LAN, over HTTPS: the password's session, or a site-signed token
    /// (§N.31.5; a read-only one's changes refused).
    fn lan(self: &Arc<Self>, r: Request) -> Response {
        let who = match self.tokens.who(&r) {
            Some(Ok(w)) => Some(w),
            Some(Err(e)) => {
                self.core
                    .log(&format!("LAN panel: token refused from {}: {e}", r.peer));
                return web::auth::refused(&e);
            }
            None => self
                .lan
                .valid(r.cookie("idn_node_lan"))
                .then(|| web::Who::lan_password(&r)),
        };
        let ok = who.is_some();
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/api/v1/login") => {
                if !r.may_change("X-IDN-Node") {
                    return Response::text("403 Forbidden", "refused");
                }
                // The check and the count in one step (overlapping tries all
                // slipped under the limit when counted after the slow password check).
                if !self.logins.attempt(r.peer) {
                    return Response::json_err(
                        "429 Too Many Requests",
                        "too many tries; wait a minute",
                    );
                }
                let pw = serde_json::from_slice::<Value>(&r.body)
                    .ok()
                    .and_then(|v| v.get("password").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                // (Argon2: a few at once, so logins sent together can't take the memory.)
                let hash = self.core.settings().lan_password;
                let good = web::PasswordChecks::shared()
                    .run_blocking(|| hash.is_some_and(|h| web::verify_password(&h, &pw)));
                if good {
                    self.logins.succeeded(r.peer);
                    let sid = self.lan.create();
                    self.core.log(&format!("LAN panel: login from {}", r.peer));
                    Response::json(r#"{"ok":true}"#)
                        .with("Set-Cookie", &self.lan.cookie("idn_node_lan", &sid, true))
                } else {
                    self.core
                        .log(&format!("LAN panel: wrong password from {}", r.peer));
                    Response::json_err("403 Forbidden", "wrong password")
                }
            }
            ("POST", "/api/v1/logout") if r.may_change("X-IDN-Node") => {
                self.lan.end(r.cookie("idn_node_lan"));
                Response::json(r#"{"ok":true}"#)
            }
            ("GET", "/") if !ok => {
                Response::ok("text/html; charset=utf-8", web::ui::login_page(&LOGIN))
            }
            // Logged in (or a token): the shared API front (panel/front.rs: a read-only
            // token's changes refused and audited there).
            _ => match who {
                Some(w) => web::engine::handle_as(self, r, w),
                None => self.route(r, ok, true),
            },
        }
    }

    /// Starts a search for announced AES67 streams, unless one runs.
    fn find_aes67(&self) {
        let found = self.aes67_found.clone();
        {
            let mut f = found.lock_ok();
            if f.running {
                return;
            }
            f.running = true;
        }
        let interface = self.core.settings().interface;
        std::thread::spawn(move || {
            let mut cmd =
                std::process::Command::new(crate::supervisor::Supervisor::idn_next_to_me());
            cmd.args([
                "audio",
                "aes67",
                "list",
                "--seconds",
                "31",
                "--tsv",
                "--no-node",
            ]);
            if let Some(i) = interface {
                cmd.args(["--interface", &i]);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000);
            }
            // (It listens 31 s for announcements: the limit is past that.)
            let out = cmd
                .stdin(std::process::Stdio::null())
                .output_within(std::time::Duration::from_secs(45));
            let mut f = found.lock_ok();
            f.running = false;
            f.at = Some(idn_common::epoch::secs());
            match out {
                Ok(o) if o.status.success() => {
                    f.error = None;
                    f.streams = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter_map(|l| {
                            let c: Vec<&str> = l.split('\t').collect();
                            (c.len() >= 6).then(|| {
                                json!({ "name": c[0], "address": c[1], "channels": c[2].parse::<u32>().ok(), "encoding": c[3], "rate": c[4].parse::<u32>().ok(), "from": c[5] })
                            })
                        })
                        .collect();
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    f.error = Some(err.lines().last().unwrap_or("the search failed").to_owned());
                }
                Err(e) => f.error = Some(format!("idn: {e}")),
            }
        });
    }

    /// The sources on the network, from browsers kept for the current settings.
    fn sources(&self) -> Value {
        let s = self.core.settings();
        let key = (s.server.clone(), s.interface.clone());
        let mut b = self.browser.lock_ok();
        if b.as_ref().is_none_or(|(k, _, _)| *k != key) {
            let interface = s.interface.as_deref().and_then(|i| i.parse().ok());
            let mut v = vec![
                idn_open_protocol::discovery::shared(BrowserConfig {
                    interface,
                    discovery_server: None,
                })
                .map_err(|e| format!("mDNS: {e}")),
            ];
            if let Some(sv) = s.server.clone() {
                v.push(
                    idn_open_protocol::discovery::shared(BrowserConfig {
                        interface,
                        discovery_server: Some(sv.clone()),
                    })
                    .map_err(|e| format!("Discovery Server {sv}: {e}")),
                );
            }
            *b = Some((key, v, Instant::now()));
        }
        let (_, browsers, since) = b.as_ref().expect("set above");
        let mut all = std::collections::BTreeMap::new();
        let mut errors = Vec::new();
        for br in browsers {
            match br {
                Ok(br) => {
                    for x in br.sources() {
                        all.entry(x.name.clone()).or_insert(x);
                    }
                }
                Err(e) => errors.push(e.clone()),
            }
        }
        if all.is_empty() && !errors.is_empty() {
            return json!({ "error": errors.join("; ") });
        }
        // mDNS answers and the server's list take a few seconds after a search starts.
        if all.is_empty() && since.elapsed() < Duration::from_secs(12) {
            return json!({ "searching": true });
        }
        Value::Array(
            all.into_values()
                .map(|x| {
                    json!({
                        "name": x.name,
                        "address": x.addr.map(|a| a.to_string()),
                        "port": x.port,
                        "idn": !x.idn_ext.is_empty(),
                        "encrypted": x.idn_ext.iter().any(|e| e.starts_with("secure/")),
                        "ext": x.idn_ext,
                    })
                })
                .collect(),
        )
    }
}

/// Whether Behringer's X-USB driver is installed (Windows: an ASIO driver named X-USB);
/// `None` on systems that need none (Linux and macOS take the X32 as it is).
fn x32_driver() -> Option<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
        };
        let path: Vec<u16> = "SOFTWARE\\ASIO\0".encode_utf16().collect();
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name; the key is closed; the buffer's length is passed.
        unsafe {
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return Some(false);
            }
            let mut found = false;
            for i in 0.. {
                let mut name = [0u16; 256];
                let mut len = name.len() as u32;
                let r = RegEnumKeyExW(
                    key,
                    i,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if r != 0 {
                    break;
                }
                let n = String::from_utf16_lossy(&name[..len as usize]);
                if idn_x32_name(&n) {
                    found = true;
                    break;
                }
            }
            RegCloseKey(key);
            Some(found)
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Virtual screens for the panel.
fn vscreens_state() -> Value {
    #[cfg(windows)]
    {
        let mut v = crate::virtual_screen::windows::state().json();
        v["os"] = json!("windows");
        v
    }
    #[cfg(target_os = "linux")]
    {
        crate::virtual_screen::state_json(&crate::service::system_state_dir().join("vscreen"))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    Value::Null
}

/// The panel's change to virtual screens; what to tell the person.
fn vscreens_change(
    action: &str,
    mode: Option<&str>,
    connector: Option<&str>,
) -> std::io::Result<String> {
    #[cfg(windows)]
    {
        let _ = connector;
        match action {
            "add" => {
                let m = mode
                    .and_then(idn_common::edid::parse_mode)
                    .ok_or_else(|| std::io::Error::other("a mode, WxH@HZ"))?;
                let (n, reboot) = crate::virtual_screen::windows::add(m)?;
                Ok(if reboot {
                    format!(
                        "{n} virtual screen(s): restart Windows to finish installing the driver"
                    )
                } else {
                    format!("{n} virtual screen(s): the new one is in Windows' Display settings")
                })
            }
            "remove" => {
                let n = crate::virtual_screen::windows::remove()?;
                Ok(format!("{n} virtual screen(s) left"))
            }
            _ => Err(std::io::Error::other("add or remove")),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let dir = crate::service::system_state_dir().join("vscreen");
        crate::virtual_screen::request(&dir, action, mode, connector)?;
        Ok(
            "asked: the change is made in a moment; restart the computer for it to take effect"
                .into(),
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (action, mode, connector);
        Err(std::io::Error::other("virtual screens: Linux and Windows"))
    }
}

/// Whether VB-Audio's VB-CABLE is installed (its driver's service, `VBAudioVACMME`).
#[cfg_attr(not(windows), allow(dead_code))]
fn vbcable_installed() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
        };
        let path: Vec<u16> = "SYSTEM\\CurrentControlSet\\Services\\VBAudioVACMME\0"
            .encode_utf16()
            .collect();
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name; the key is closed.
        unsafe {
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) == 0 {
                RegCloseKey(key);
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// An X-USB device or driver by its name (as idn-audio finds the card).
#[cfg_attr(not(windows), allow(dead_code))]
fn idn_x32_name(n: &str) -> bool {
    let n = n.to_lowercase();
    n.contains("x-usb") || n.contains("x_usb") || n.contains("x32")
}

/// The `ms` of a latency request (idn-routing.md §A.12): a number of ms in range, or
/// null / absent / empty for the program's own.
pub fn latency_of(v: &Value) -> Result<Option<u32>, &'static str> {
    let ms = match &v["ms"] {
        Value::Null => return Ok(None),
        Value::String(s) if s.trim().is_empty() => return Ok(None),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        x => x.as_u64().and_then(|n| u32::try_from(n).ok()),
    };
    ms.filter(|ms| idn_open_protocol::node::routing::LATENCY_MS.contains(ms))
        .map(Some)
        .ok_or("latency: 1 to 500 ms, or none for the program's own")
}
