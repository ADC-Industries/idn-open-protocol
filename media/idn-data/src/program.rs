//! `idn-data`: IDN Data as a program (spec/extensions/idn-data.md, roadmap R.22): data
//! feeds as text-only sources and video downloads, with an HTTP/JSON API on this machine
//! (no UI of its own: the UI is the client's). The IDN Node runs it as a service
//! (`data.on`); `idn data` is the same.
//!
//!   idn-data [--port N] [--lan-port N] [--no-lan] [--state-dir DIR] [--media DIR]
//!            [--yt-dlp PROGRAM] [--no-node]

use crate::downloads::Downloads;
use crate::ytdlp::YtDlp;
use crate::{Feed, Feeds, feeds::Format};
use idn_open_protocol::api::{Publisher, PublisherConfig};
use idn_open_protocol::web::{self, HandOff, Handler, Request, Response, Sessions};
use idn_open_protocol::xml::Element;
use serde_json::{Value, json};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: idn-data [--port N] [--lan-port N] [--no-lan] [--state-dir DIR] [--media DIR]
                [--yt-dlp PROGRAM] [--no-node]

  IDN Data: each data feed (a Google Sheet, a CSV, TSV, JSON or XML file or address, an
  RSS feed) is read every so often and sent as the text-only source \"HOST (Data: NAME)\",
  which the switcher's titles take by name. Videos are downloaded by yt-dlp (fetched from
  its GitHub releases, checked against their checksums, and checked weekly; --yt-dlp:
  the system's instead) into --media (default: media in the state directory) as MP4.
  The feeds are kept in feeds.json in the state directory. With the machine's IDN Node
  (unless --no-node): its site identity. Its API is on http://127.0.0.1:PORT/api/v1/
  (default port 29212; no UI of its own: the UI is the client's); a client logs in with the IDN Node's one-time token
  (GET /?t=TOKEN, the Node's data_panel request), which gives a session cookie, or with
  a token from the Site Manager (Authorization: Bearer idn1…). On a machine in a site,
  the API is also on the network over HTTPS (--lan-port, default 29221) for the Site
  Manager's tokens only; --no-lan keeps it closed. Every change is written in
  audit.jsonl in the state directory (GET /api/v1/audit).";

/// The network's front (tokens only: no password, so no login page).
const LOGIN: web::ui::Login = web::ui::Login {
    title: "IDN Data",
    hint: None,
    label: "Password",
    header: "X-IDN-Data",
    who: "data",
    console: false,
    user: false,
};

/// How long an API session lasts.
const SESSION: Duration = Duration::from_secs(12 * 3600);

/// Where the feeds are kept.
fn default_state_dir() -> PathBuf {
    idn_open_protocol::link::LinkConfig::default_dir()
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("idn-open-protocol"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("data")
}

pub fn main() {
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-data");
    let (mut port, mut use_node, mut dir) = (idn_common::ports::DATA, true, None::<PathBuf>);
    let (mut lan_port, mut no_lan) = (idn_common::ports::DATA_LAN, false);
    let (mut media, mut ytdlp) = (None::<PathBuf>, None::<PathBuf>);
    // (Bad or missing values print the usage and stop; `--version` answers.)
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.value(&a),
            "--lan-port" => lan_port = it.value(&a),
            "--no-lan" => no_lan = true,
            "--state-dir" => dir = Some(it.value::<String>(&a).into()),
            "--media" => media = Some(it.value::<String>(&a).into()),
            "--yt-dlp" => ytdlp = Some(it.value::<String>(&a).into()),
            "--no-node" => use_node = false,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--version" => {
                println!(
                    "idn-data {} - {}",
                    env!("CARGO_PKG_VERSION"),
                    idn_open_protocol::build_line()
                );
                return;
            }
            other => {
                eprintln!("idn-data: unknown argument {other:?}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    let node = use_node
        .then(
            || match idn_open_protocol::node::Node::connect("idn-data") {
                Ok(n) => Some(n),
                Err(e) => {
                    eprintln!("idn-data: no IDN Node ({e}): feeds without the site's identity");
                    None
                }
            },
        )
        .flatten();
    let publisher = Publisher::new(PublisherConfig {
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| {
        eprintln!("idn-data: {e}");
        std::process::exit(1)
    });
    let dir = dir.unwrap_or_else(default_state_dir);
    let feeds = Arc::new(
        Feeds::start(publisher, Some(dir.clone())).unwrap_or_else(|e| {
            eprintln!("idn-data: {e}");
            std::process::exit(1)
        }),
    );
    if let Some(y) = &ytdlp
        && !crate::ytdlp::answers(y)
    {
        eprintln!(
            "idn-data: --yt-dlp {}: it doesn't answer --version",
            y.display()
        );
        std::process::exit(1)
    }
    let tool = Arc::new(YtDlp::new(dir.join("tools"), ytdlp));
    let node = node.map(Arc::new);
    // The Node's media folder (§D.7) when there is a Node and it can be written here,
    // else this program's own.
    let media = media
        .or_else(|| node_media_dir(node.as_deref()))
        .unwrap_or_else(|| dir.join("media"));
    let downloads = Arc::new(Downloads::new(tool.clone(), media.clone()));
    // The media folder kept as a library (§D.8): what is known of each item, programs'
    // download requests (the Node serves it to the site when its library is on).
    let library = crate::library::Library::new(media, downloads.clone());
    library.start();
    // yt-dlp's release checked weekly (sites change, and it follows within days). A
    // failed update (a release not signed by yt-dlp's key, a bad file) keeps the one here
    // and alerts the site once; "ok" when an update works again.
    {
        let node = node.clone();
        std::thread::spawn(move || {
            // (Once per kind of failure, not per message: a changing byte count or
            // checksum alerted again every hour; F2-3.)
            let mut alerted: Option<&'static str> = None;
            loop {
                tool.update_if_due();
                let st = tool.state();
                let (error, kind) = (st.error, st.error_kind);
                if let Some(n) = &node {
                    match (&error, &alerted) {
                        (Some(e), a) if *a != kind => {
                            let _ = n.alert(
                                "warning",
                                &format!("yt-dlp wasn't updated (the one here is kept): {e}"),
                            );
                        }
                        (None, Some(_)) => {
                            let _ = n.alert("ok", "yt-dlp updated again");
                        }
                        _ => {}
                    }
                }
                alerted = kind;
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    }
    // A client's API session starts with a one-time token: the IDN Node's (its key in our
    // environment, or, started by hand with a Node, asked for), so a token from the Node's
    // data_panel request logs a client in; else one of this program's own.
    let handoff = idn_common::switches::value("IDN_DATA_PANEL_KEY")
        .and_then(|k| HandOff::from_hex(&k))
        .or_else(|| {
            let n = node.as_ref()?;
            let r = n
                .request(Element::new("data_panel").with_attr("key", "1"))
                .ok()?;
            HandOff::from_hex(r.attr("key")?)
        })
        .unwrap_or_else(HandOff::random);
    // Site-signed tokens for IDN Data on this machine (idn-node.md §N.31.5), with a Node.
    let tokens = node.as_ref().map(|n| {
        idn_open_protocol::web::TokenCheck::from_enrollment("data", n.enrollment().clone())
    });
    let app = Arc::new(App {
        feeds,
        downloads,
        node,
        library,
        dir: dir.clone(),
        handoff,
        sessions: Sessions::new(SESSION),
        audit: idn_open_protocol::web::audit::Audit::for_engine(Some(&dir)),
        lan: tokens
            .clone()
            .map(|t| web::PasswordGate::tokens_only(t, "idn_data", "X-IDN-Data")),
        tokens: tokens.clone(),
    });
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("idn-data: port {port}: {e}");
        std::process::exit(1)
    });
    let handler: Handler = {
        let app = app.clone();
        Arc::new(move |r: Request| handle(&app, r))
    };
    // Uploads to the library come in pieces of up to 4 MB.
    let stop = Arc::new(AtomicBool::new(false));
    web::serve_with(
        listener,
        None,
        handler,
        stop.clone(),
        &[("/api/v1/upload", 5 << 20)],
    )
    .unwrap_or_else(|e| {
        eprintln!("idn-data: {e}");
        std::process::exit(1)
    });
    // On the network once the machine is in a site: tokens only.
    if let (Some(t), false) = (tokens, no_lan) {
        let handler: Handler = {
            let app = app.clone();
            Arc::new(move |r: Request| {
                web::lan::front(
                    app.lan.as_ref(),
                    &LOGIN,
                    r,
                    &|m| eprintln!("idn-data: {m}"),
                    |r, who| serve(&app, r, Some(who)),
                )
            })
        };
        web::lan::start_when(
            &dir,
            "IDN Data",
            lan_port,
            stop,
            handler,
            move || t.available(),
            |m| println!("idn-data: {m}"),
        );
    }
    println!(
        "idn-data: API http://127.0.0.1:{port}/api/v1/ (log in with the IDN Node's one-time token)"
    );
    idn_open_protocol::shutdown::install();
    loop {
        std::thread::park();
    }
}

struct App {
    feeds: Arc<Feeds>,
    downloads: Arc<Downloads>,
    /// The machine's Node: the site's machines, and files sent to them (§D.7).
    node: Option<Arc<idn_open_protocol::node::Node>>,
    /// The media folder as a library (§D.8).
    library: Arc<crate::library::Library>,
    /// The state directory (feeds' local files may be here, or in the media folder).
    dir: PathBuf,
    /// The panel's one-time tokens and its sessions.
    handoff: HandOff,
    sessions: Sessions,
    /// Every change, who made it and from where (§D.9).
    audit: Arc<idn_open_protocol::web::audit::Audit>,
    /// Site-signed tokens (idn-node.md §N.31.5), with a Node: on this machine's API and
    /// on the network's (`lan`).
    tokens: Option<Arc<idn_open_protocol::web::TokenCheck>>,
    lan: Option<web::PasswordGate>,
}

/// A feed's source: an http(s) address, or a file in the media folder or the state
/// directory. Any other path was read as the service's account and its rows shown or
/// published.
fn source_allowed(app: &App, from: &str) -> Result<(), String> {
    let from = from.trim();
    if from.starts_with("http://") || from.starts_with("https://") {
        return Ok(());
    }
    let inside = |root: &std::path::Path| {
        let (Ok(root), Ok(f)) = (
            root.canonicalize(),
            std::path::Path::new(from).canonicalize(),
        ) else {
            return false;
        };
        f.starts_with(&root)
    };
    if inside(app.library.dir()) || inside(&app.dir) {
        return Ok(());
    }
    Err(format!(
        "from: an http(s) address, or a file in the media folder ({}) or {}",
        app.library.dir().display(),
        app.dir.display()
    ))
}

/// The Node's media folder, if this program can write in it.
fn node_media_dir(node: Option<&idn_open_protocol::node::Node>) -> Option<PathBuf> {
    let reply = node?.request(Element::new("media")).ok()?;
    let dir = PathBuf::from(reply.attr("dir").filter(|d| !d.is_empty())?);
    std::fs::create_dir_all(&dir).ok()?;
    let probe = dir.join(".idn-data-probe");
    std::fs::write(&probe, b"").ok()?;
    let _ = std::fs::remove_file(probe);
    Some(dir)
}

/// The Node's media state: the site's machines and the files to and from them.
fn node_media(node: Option<&idn_open_protocol::node::Node>) -> Value {
    let Some(r) = node.and_then(|n| n.request(Element::new("media")).ok()) else {
        return Value::Null;
    };
    if r.name != "media" {
        return Value::Null;
    }
    json!({
        "dir": r.attr("dir"),
        "library": r.attr("library") == Some("true"),
        "machines": r.children_named("machine")
            .map(|m| json!({"fp": m.attr("fp"), "name": m.attr("name")}))
            .collect::<Vec<_>>(),
        "transfers": r.children_named("transfer")
            .map(|x| json!({"way": x.attr("way"), "peer": x.attr("peer"), "file": x.attr("file"),
                            "size": x.attr("size").and_then(|v| v.parse::<u64>().ok()),
                            "done": x.attr("done").and_then(|v| v.parse::<u64>().ok()),
                            "state": x.attr("state")}))
            .collect::<Vec<_>>(),
    })
}

impl App {
    fn version(&self) -> u64 {
        self.feeds.version() + self.downloads.version()
    }

    /// Waits up to `timeout` for anything to change after `after`.
    fn wait_change(&self, after: u64, timeout: Duration) {
        let end = Instant::now() + timeout;
        while self.version() <= after && Instant::now() < end {
            self.downloads
                .wait_change(self.downloads.version(), Duration::from_millis(250));
        }
    }
}

/// Query parameter `k`, percent-decoded (folder and file names, item ids).
fn param(r: &Request, k: &str) -> String {
    let raw = r.param(k).unwrap_or("").as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' if i + 2 < raw.len() => {
                match u8::from_str_radix(std::str::from_utf8(&raw[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle(app: &App, r: Request) -> Response {
    // A one-time token from the IDN Node (its data_panel request) gives a session: the
    // API's login (no UI of its own; the UI is the client's).
    if r.method == "GET"
        && r.path == "/"
        && let Some(t) = r.param("t")
    {
        if app.handoff.take(t) {
            let sid = app.sessions.create();
            return Response::json(r#"{"ok":true}"#)
                .with("Set-Cookie", &app.sessions.cookie("idn_data", &sid, false));
        }
        return Response::json_err("403 Forbidden", "that token isn't valid (any more)");
    }
    // The Node's session (this machine's client), else a site token.
    let who = if app.sessions.valid(r.cookie("idn_data")) {
        Some(web::Who::local(&r))
    } else {
        match app.tokens.as_ref().and_then(|t| t.who(&r)) {
            Some(Ok(w)) => Some(w),
            Some(Err(e)) => return web::auth::refused(&e),
            None => None,
        }
    };
    serve(app, r, who)
}

/// A request from `who` (none: not logged in): a read-only caller's changes refused
/// (403), every change written in the audit as theirs (§D.9).
fn serve(app: &App, r: Request, who: Option<web::Who>) -> Response {
    let ok = who.as_ref().is_some_and(web::Who::ready);
    if r.method == "POST"
        && r.path.starts_with("/api/v1/")
        && let Some(w) = &who
        && let Some(no) = w.refusal()
    {
        let detail = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let action = r.path.trim_start_matches("/api/v1/");
        app.audit.record(w, action, &detail, w.may_change().err());
        return no;
    }
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/") => Response::json(
            serde_json::json!({"service": "IDN Data", "api": "/api/v1/", "logged_in": ok})
                .to_string(),
        ),
        (_, p) if p.starts_with("/api/") && !ok => {
            Response::json_err("403 Forbidden", "not logged in")
        }
        ("GET", "/api/v1/state") => {
            if let Some(after) = r.param("after").and_then(|v| v.parse::<u64>().ok()) {
                // Files moving between machines change without telling: 5 s at most.
                let wait = if app.node.is_some() { 5 } else { 20 };
                app.wait_change(after, Duration::from_secs(wait));
            }
            let mut s = app.feeds.state();
            if let (Some(o), Value::Object(d)) = (s.as_object_mut(), app.downloads.state()) {
                o.extend(d);
            }
            s["site"] = node_media(app.node.as_deref());
            // The library's catalog is its own request (it can be large): its version.
            s["library_version"] =
                json!(idn_open_protocol::library::scan(app.library.dir(), "").version);
            s["version"] = json!(app.version());
            s["build"] = json!(idn_open_protocol::build_line());
            Response::json(s.to_string()).with("Cache-Control", "no-store")
        }
        // The library (§D.8): its catalog and thumbnails.
        ("GET", "/api/v1/library") => {
            Response::json(app.library.catalog().to_string()).with("Cache-Control", "no-store")
        }
        // The audit (§D.9): newest first.
        ("GET", "/api/v1/audit") => web::audit::answer(&app.audit, &r),
        ("GET", "/api/v1/thumb") => match app.library.thumb(&param(&r, "id")) {
            Some(b) => Response::ok("image/jpeg", b),
            None => Response::json_err("404 Not Found", "no thumbnail"),
        },
        // A piece of an upload: the body is the file's bytes.
        ("POST", "/api/v1/upload") => {
            if !r.may_change("X-IDN-Data") {
                return Response::json_err("403 Forbidden", "missing X-IDN-Data: 1");
            }
            let offset = r.param("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
            let done = app.library.upload(
                &param(&r, "folder"),
                &param(&r, "name"),
                offset,
                &r.body,
                r.param("last") == Some("1"),
            );
            // (An upload's pieces are one line: the same file, its offset moving.)
            if let Some(w) = &who {
                app.audit.record(
                    w,
                    "upload",
                    &json!({"folder": param(&r, "folder"), "name": param(&r, "name"),
                            "offset": offset, "last": r.param("last") == Some("1")}),
                    done.as_ref().err().map(String::as_str),
                );
            }
            match done {
                Ok(id) => Response::json(json!({"ok": true, "id": id}).to_string()),
                Err(e) => Response::json_err("400 Bad Request", &e),
            }
        }
        ("POST", p) => {
            if !r.may_change("X-IDN-Data") {
                return Response::json_err("403 Forbidden", "missing X-IDN-Data: 1");
            }
            let Some(cmd) = p.strip_prefix("/api/v1/") else {
                return Response::json_err("404 Not Found", "no such page");
            };
            let v: Value = if r.body.is_empty() {
                json!({})
            } else {
                match serde_json::from_slice(&r.body) {
                    Ok(v) => v,
                    Err(e) => return Response::json_err("400 Bad Request", &format!("JSON: {e}")),
                }
            };
            let done = command(app, cmd, &v);
            if let Some(w) = &who {
                app.audit
                    .record(w, cmd, &v, done.as_ref().err().map(String::as_str));
            }
            match done {
                Ok(out) => Response::json(out.to_string()),
                Err(e) => Response::json_err("400 Bad Request", &e),
            }
        }
        _ => Response::json_err("404 Not Found", "no such page"),
    }
}

/// An API command (`POST /api/v1/<cmd>`, idn-data.md §D.3, §D.6).
fn command(app: &App, cmd: &str, v: &Value) -> Result<Value, String> {
    let (feeds, dl) = (&app.feeds, &app.downloads);
    let name = || {
        v["name"]
            .as_str()
            .map(str::to_owned)
            .ok_or("name: the feed's".to_owned())
    };
    let id = || v["id"].as_u64().ok_or("id: the download's".to_owned());
    let url = || v["url"].as_str().unwrap_or("").trim().to_owned();
    let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
    match cmd {
        "feed_add" => {
            source_allowed(app, v["from"].as_str().unwrap_or(""))?;
            feeds.add(Feed::from_json(v)?).map(|()| json!({"ok": true}))
        }
        // {"name": its name now, "feed": {name, from, format, every}}.
        "feed_set" => {
            source_allowed(app, v["feed"]["from"].as_str().unwrap_or(""))?;
            feeds
                .set(&name()?, Feed::from_json(&v["feed"])?)
                .map(|()| json!({"ok": true}))
        }
        "feed_remove" => feeds.remove(&name()?).map(|()| json!({"ok": true})),
        "feed_read" => feeds.read_now(&name()?).map(|()| json!({"ok": true})),
        // Reads an address once without adding it: {"from", "format"}.
        "preview" => {
            let from = v["from"].as_str().unwrap_or("").trim();
            if from.is_empty() {
                return Err("from: a file or an address".into());
            }
            source_allowed(app, from)?;
            let format = Format::from_name(v["format"].as_str().unwrap_or("auto"))
                .ok_or("format: auto, csv, tsv, json or xml")?;
            let m = crate::preview(from, format)?;
            Ok(json!({"ok": true, "columns": m.columns, "total": m.total,
                      "sample": m.rows.iter().take(10).collect::<Vec<_>>()}))
        }
        // Downloads (§D.6): what a link offers, then the chosen one.
        "formats" => dl.formats(&url()),
        "download" => dl
            .start_in(
                &url(),
                v["choice"].as_str().unwrap_or("1080p"),
                v["title"].as_str().unwrap_or(""),
                v["folder"].as_str().unwrap_or(""),
            )
            .map(|id| json!({"ok": true, "id": id})),
        // The library (§D.8).
        "lib_item" => {
            let tags: Vec<String> = v["tags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            app.library
                .set_item(&s("id"), &s("title"), &tags)
                .map(|()| json!({"ok": true}))
        }
        "lib_delete" => app.library.delete(&s("id")).map(|()| json!({"ok": true})),
        "lib_move" => app
            .library
            .move_item(&s("id"), &s("folder"))
            .map(|id| json!({"ok": true, "id": id})),
        "lib_folder" => app
            .library
            .new_folder(&s("path"))
            .map(|()| json!({"ok": true})),
        "lib_folder_remove" => app
            .library
            .remove_folder(&s("path"))
            .map(|()| json!({"ok": true})),
        "download_cancel" => dl.cancel(id()?).map(|()| json!({"ok": true})),
        "download_forget" => dl.forget(id()?).map(|()| json!({"ok": true})),
        "yt_dlp_update" => dl.tool().update().map(|()| json!({"ok": true})),
        // A file of the media folder to a machine of the site (§D.7): {"file", "to"}.
        "send" => {
            let node = app.node.as_ref().ok_or("no IDN Node on this machine")?;
            let file = v["file"].as_str().unwrap_or("");
            let to = v["to"].as_str().unwrap_or("");
            let path = dl.media().join(file);
            let r = node
                .request(
                    Element::new("media_send")
                        .with_attr("path", &path.to_string_lossy())
                        .with_attr("to", to),
                )
                .map_err(|e| e.to_string())?;
            if r.name == "error" {
                return Err(r.attr("text").unwrap_or("refused").to_owned());
            }
            Ok(json!({"ok": true, "to": r.attr("to")}))
        }
        _ => Err(format!("no command {cmd:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_open_protocol::trust::pki::KeyFile;
    use idn_open_protocol::trust::token::{self, Claims, Scope};
    use idn_open_protocol::web::{SiteKeys, TokenCheck};

    fn request(method: &str, path: &str, auth: Option<&str>, body: Value) -> Request {
        let mut headers: std::collections::HashMap<String, String> =
            [("x-idn-data".to_owned(), "1".to_owned())].into();
        if let Some(t) = auth {
            headers.insert("authorization".into(), format!("Bearer {t}"));
        }
        Request {
            method: method.into(),
            path: path.into(),
            query: String::new(),
            headers,
            body: body.to_string().into_bytes(),
            peer: "10.0.0.8".parse().unwrap(),
            tls: true,
        }
    }

    /// Site tokens (idn-node.md §N.31.5): a read-only one reads and is refused changes, a
    /// control one changes, another engine's is refused; the audit says who (§D.9).
    #[test]
    fn idn_data_takes_site_tokens_and_keeps_an_audit() {
        let dir = idn_common::testing::TempDir::new("data-tokens");
        let publisher = Publisher::new(PublisherConfig {
            mdns: Some(false),
            link: Some(idn_open_protocol::link::LinkConfig::ephemeral()),
            ..Default::default()
        })
        .unwrap();
        let feeds = Arc::new(Feeds::start(publisher, Some(dir.join("state"))).unwrap());
        let tool = Arc::new(YtDlp::new(dir.join("tools"), None));
        let downloads = Arc::new(Downloads::new(tool, dir.join("media")));
        std::fs::create_dir_all(dir.join("media")).unwrap();
        let library = crate::library::Library::new(dir.join("media"), downloads.clone());
        let key = KeyFile::generate().unwrap();
        let site = key.fingerprint()[..16].to_owned();
        let (public, s2) = (key.public, site.clone());
        let check = TokenCheck::new("data", move || {
            Some(SiteKeys {
                ca_public: public,
                site: s2.clone(),
                machine: "m1".into(),
                options: Default::default(),
            })
        });
        let app = App {
            feeds,
            downloads,
            node: None,
            library,
            dir: dir.join("state"),
            handoff: HandOff::random(),
            sessions: Sessions::new(SESSION),
            audit: idn_open_protocol::web::audit::Audit::for_engine(Some(&dir.join("state"))),
            lan: Some(web::PasswordGate::tokens_only(
                check.clone(),
                "idn_data",
                "X-IDN-Data",
            )),
            tokens: Some(check),
        };
        let make = |sub: &str, engine: &str, scope: Scope| {
            let c = Claims::new(
                &site,
                sub,
                sub,
                engine,
                "*",
                scope,
                idn_common::epoch::secs(),
                600,
            )
            .unwrap();
            token::sign(&c, &key).unwrap()
        };
        let read = make("alice", "data", Scope::Read);
        let control = make("bob", "data", Scope::Control);
        let folder = json!({"path": "Sermons"});
        let r = handle(
            &app,
            request("GET", "/api/v1/library", Some(&read), Value::Null),
        );
        assert_eq!(r.status, "200 OK");
        let r = handle(
            &app,
            request("POST", "/api/v1/lib_folder", Some(&read), folder.clone()),
        );
        assert!(r.status.starts_with("403"), "{}", r.status);
        assert!(!dir.join("media/Sermons").exists());
        let r = handle(
            &app,
            request("POST", "/api/v1/lib_folder", Some(&control), folder),
        );
        assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
        assert!(dir.join("media/Sermons").is_dir());
        let other = make("bob", "desk", Scope::Control);
        let r = handle(
            &app,
            request("GET", "/api/v1/state", Some(&other), Value::Null),
        );
        assert!(r.status.starts_with("401"), "{}", r.status);
        // Without a token or the Node's session: not logged in, as before.
        let r = handle(&app, request("GET", "/api/v1/state", None, Value::Null));
        assert!(r.status.starts_with("403"), "{}", r.status);
        let log = app.audit.newest(10);
        assert!(
            log.iter()
                .any(|l| l["user"] == "alice" && l["ok"] == false && l["peer"] == "10.0.0.8")
        );
        assert!(
            log.iter()
                .any(|l| l["user"] == "bob" && l["action"] == "lib_folder" && l["ok"] == true)
        );
    }
}
