//! The machine's media folder and files between machines (idn-node.md §N.25, roadmap
//! R.22 stage 3): the Node runs the site's file channel (`channel/1`, idn-extensions.md
//! §E.12) with the machine's site membership; a program asks it to send a file of the
//! media folder to another machine of the site, whose Node keeps it in its own media
//! folder. Only files inside the media folder are sent, and only the site's machines can
//! send one here.
//!
//! With the library on (`data.library`, §N.26, R.23), the media folder is the site's
//! media library: the site's programs read its catalog, fetch its items, play them
//! through short-lived links ([`LINK_PORT`]) and ask for downloads
//! (`idn_open_protocol::library`).

use idn_common::sync::LockOk as _;
use idn_open_protocol::channel::{FileInfo, FileSink, Handler, PathSink, Peer, Reply, Served};
use idn_open_protocol::library::{self as lib, kinds};
use idn_open_protocol::web::{Request, Response};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// The port of the links that play library items directly (HTTP, ranges).
pub const LINK_PORT: u16 = idn_common::ports::MEDIA_LINK;
/// The most a link's answer carries at once (players ask for the rest as they go).
const LINK_CHUNK: u64 = 8 << 20;

/// Offers whose id starts so are media files (other uses of the channel are left alone).
pub const ID_PREFIX: &str = "media:";
/// A media file that replaces a finished file of its name, once all of it is there and
/// checked (`<media_send replace="true"/>`, 2026-11-26). Its own prefix: a Node
/// from before refuses it rather than keeping a "NAME (2).EXT" beside.
pub const REPLACE_PREFIX: &str = "media-replace:";
/// The largest file taken (a long 4K service recording).
pub const MAX_FILE: u64 = 64 << 30;
/// Transfers kept in the list.
const KEPT: usize = 50;

/// The media folder: on the system's service, a folder every user of the machine reads
/// (Windows: Public Videos\IDN Media; Linux: /var/lib/idn-media, group `idn`); for a
/// Node of one user, `media` in its state folder.
pub fn media_dir(state: &Path, service: bool) -> PathBuf {
    if !service {
        return state.join("media");
    }
    if cfg!(windows) {
        std::env::var_os("PUBLIC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"))
            .join("Videos")
            .join("IDN Media")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/Users/Shared/IDN Media")
    } else {
        PathBuf::from("/var/lib/idn-media")
    }
}

/// Makes the media folder, readable by the machine's users (Linux: group `idn`, which
/// may add files too, each its own: sticky).
pub fn make_dir(dir: &Path) -> io::Result<()> {
    // Not a link or a junction someone made in its place: the service would write
    // through it (on Windows the public folder is the users').
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: a link, not a folder", dir.display()),
        ));
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Sticky: the group may add files but not remove or replace the service's
        // (a partial file swapped for a link).
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o3775));
        #[cfg(target_os = "linux")]
        // SAFETY: no arguments.
        if unsafe { libc::geteuid() } == 0
            && let Some(gid) = idn_common::fsutil::group_id("idn")
        {
            use std::os::unix::ffi::OsStrExt;
            if let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) {
                // SAFETY: a NUL-terminated path; u32::MAX (-1) leaves the owner as it is.
                unsafe {
                    libc::chown(c.as_ptr(), u32::MAX, gid);
                }
            }
        }
    }
    Ok(())
}

/// Where a file named `name` goes in `dir`: its name, or "NAME (2).EXT" … when a
/// finished file has it (a partial one is the same transfer resuming).
pub fn place(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut k = 1;
    loop {
        let candidate = if k == 1 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({k}){ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
        k += 1;
    }
}

/// Where a file named `name` (made safe) goes in `dir`, `replace`: over a finished plain
/// file of the name (the new one renamed over it once complete and checked, the old one
/// kept until then); anything else of the name (a folder, a link) is left alone and the
/// file goes beside it as [`place`] does.
pub fn target(dir: &Path, name: &str, replace: bool) -> PathBuf {
    let at = dir.join(name);
    if replace && std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_file()) {
        return at;
    }
    place(dir, name)
}

/// Whether `path` is a file inside `dir` (after resolving links and `..`).
pub fn inside(dir: &Path, path: &Path) -> bool {
    match (dir.canonicalize(), path.canonicalize()) {
        (Ok(d), Ok(p)) => p.starts_with(&d) && p != d && p.is_file(),
        _ => false,
    }
}

/// A file coming or going.
#[derive(Debug, Clone)]
pub struct Transfer {
    /// "in" (to this machine) or "out".
    pub way: &'static str,
    /// The offer's id (the sender's).
    pub id: String,
    /// The other machine's name.
    pub peer: String,
    /// Its key fingerprint.
    pub peer_fp: String,
    pub file: String,
    pub size: u64,
    pub done: Arc<AtomicU64>,
    /// In: "receiving", "received" or "failed: …"; out: "offered", "sent" (the box
    /// fetched all of it), "refused" or "failed: …".
    pub state: String,
    /// Unix seconds.
    pub at: u64,
}

/// The channel's application: takes media files pushed by the site's machines.
pub struct Media {
    me: Weak<Media>,
    dir: Mutex<PathBuf>,
    transfers: Mutex<VecDeque<Transfer>>,
    /// The folder is the site's library (§N.26).
    library: AtomicBool,
    /// The machine's name, and the address links name ("" : the one it goes out by).
    name: Mutex<String>,
    host: Mutex<String>,
    /// Links to play items directly: token → (the file, until when, Unix seconds).
    links: Mutex<BTreeMap<String, (PathBuf, u64)>>,
    /// The links' port once their server runs.
    link_port: Mutex<Option<u16>>,
    /// Set by [`Media::stop`]: the links' server ends and its port is free (a hosted
    /// Node stopped; bug run O1-5).
    link_stop: Arc<AtomicBool>,
    /// The library's index and requests (media library v2) while the library is on.
    server: Mutex<Option<Arc<lib::server::LibraryServer>>>,
}

impl Media {
    pub fn new(dir: PathBuf) -> Arc<Media> {
        Arc::new_cyclic(|me| Media {
            me: me.clone(),
            dir: Mutex::new(dir),
            transfers: Mutex::new(VecDeque::new()),
            library: AtomicBool::new(false),
            name: Mutex::new(String::new()),
            host: Mutex::new(String::new()),
            links: Mutex::new(BTreeMap::new()),
            link_port: Mutex::new(None),
            link_stop: Arc::new(AtomicBool::new(false)),
            server: Mutex::new(None),
        })
    }

    /// The library's server, opened for the media folder when the library is on.
    fn server(&self) -> Option<Arc<lib::server::LibraryServer>> {
        if !self.library() {
            return None;
        }
        let mut s = self.server.lock_ok();
        let dir = self.dir();
        if s.as_ref().is_some_and(|x| x.dir() != dir) {
            *s = None;
        }
        if s.is_none() {
            match lib::server::LibraryServer::open(&dir, &self.name.lock_ok()) {
                Ok(x) => *s = Some(x),
                Err(e) => {
                    idn_open_protocol::log::log(&format!("library: opening {}: {e}", dir.display()))
                }
            }
        }
        s.clone()
    }

    /// The library on or off, the machine's name and the address links name (§N.26).
    pub fn set_library(&self, on: bool, name: &str, host: &str) {
        self.library.store(on, Ordering::Relaxed);
        *self.name.lock_ok() = name.to_owned();
        *self.host.lock_ok() = host.to_owned();
        if !on {
            self.links.lock_ok().clear();
            self.server.lock_ok().take();
        } else if let Some(s) = self.server() {
            s.set_name(name);
        }
    }

    /// Ends the links' server (its port free); links are not served after.
    pub fn stop(&self) {
        self.link_stop.store(true, Ordering::Relaxed);
    }

    /// Whether the folder is the site's library.
    pub fn library(&self) -> bool {
        self.library.load(Ordering::Relaxed)
    }

    /// A link to play `path` directly, its server started the first time.
    fn link(&self, id: &str, path: PathBuf) -> Result<serde_json::Value, String> {
        let port = self.link_server()?;
        let token = idn_open_protocol::random::token_hex(16);
        let expires = idn_common::epoch::secs() + lib::LINK_LIFE.as_secs();
        {
            let mut l = self.links.lock_ok();
            let now = idn_common::epoch::secs();
            l.retain(|_, (_, until)| *until > now);
            l.insert(token.clone(), (path, expires));
        }
        let host = Some(self.host.lock_ok().clone())
            .filter(|h| !h.is_empty())
            .or_else(|| idn_common::net::outgoing_ip().map(|ip| ip.to_string()))
            .unwrap_or_else(|| "127.0.0.1".into());
        let name: String = id
            .rsplit('/')
            .next()
            .unwrap_or("item")
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        Ok(json!({"url": format!("http://{host}:{port}/lib/{token}/{name}"), "expires": expires}))
    }

    /// The links' server (plain HTTP on the network: a link's token is its key, 128 bits,
    /// for [`lib::LINK_LIFE`]).
    fn link_server(&self) -> Result<u16, String> {
        let mut p = self.link_port.lock_ok();
        if let Some(port) = *p {
            return Ok(port);
        }
        if self.link_stop.load(Ordering::Relaxed) {
            return Err("the Node is stopping".into());
        }
        let l = std::net::TcpListener::bind(("0.0.0.0", LINK_PORT))
            .or_else(|_| std::net::TcpListener::bind(("0.0.0.0", 0)))
            .map_err(|e| format!("the links' port: {e}"))?;
        let port = l.local_addr().map_err(|e| e.to_string())?.port();
        let me = self.me.clone();
        idn_open_protocol::web::serve(
            l,
            None,
            Arc::new(move |r: Request| match me.upgrade() {
                Some(m) => m.serve_link(&r),
                None => Response::new("404 Not Found", "text/plain", "gone"),
            }),
            self.link_stop.clone(),
        )
        .map_err(|e| e.to_string())?;
        *p = Some(port);
        Ok(port)
    }

    /// `GET /lib/TOKEN/NAME` with a byte range: up to [`LINK_CHUNK`] of the file.
    fn serve_link(&self, r: &Request) -> Response {
        let not_found = || Response::new("404 Not Found", "text/plain", "no such link");
        if r.method != "GET" && r.method != "HEAD" {
            return not_found();
        }
        // (Only a link asked for, with its own short-lived token: the links opened with a
        // host program's long-lived site key over plain HTTP are gone.)
        let Some(token) = r
            .path
            .strip_prefix("/lib/")
            .and_then(|p| p.split('/').next())
        else {
            return not_found();
        };
        let path = {
            let l = self.links.lock_ok();
            match l.get(token) {
                Some((p, until)) if *until > idn_common::epoch::secs() && self.library() => {
                    p.clone()
                }
                _ => return not_found(),
            }
        };
        let Ok(mut f) = std::fs::File::open(&path) else {
            return not_found();
        };
        let size = f.metadata().map_or(0, |m| m.len());
        let (start, end) = match r.header("range").and_then(|v| range(v, size)) {
            Some(x) => x,
            None if r.header("range").is_some() => {
                return Response::new("416 Range Not Satisfiable", "text/plain", "")
                    .with("Content-Range", &format!("bytes */{size}"));
            }
            None => (0, size.saturating_sub(1)),
        };
        let end = end.min(start + LINK_CHUNK - 1);
        let mut body = vec![
            0u8;
            if size == 0 {
                0
            } else {
                (end - start + 1) as usize
            }
        ];
        if r.method == "GET"
            && (f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut body).is_err())
        {
            return Response::new("500 Internal Server Error", "text/plain", "read");
        }
        if r.method == "HEAD" {
            body.clear();
        }
        let ctype = match lib::kind_of(&path.to_string_lossy()) {
            Some("picture") => "image/jpeg",
            Some("sound") => "audio/mpeg",
            _ => "video/mp4",
        };
        Response::new("206 Partial Content", ctype, body)
            .with("Accept-Ranges", "bytes")
            .with("Content-Range", &format!("bytes {start}-{end}/{size}"))
    }

    /// A download a program asked for (§N.26): written for IDN Data to pick up; its id.
    fn request_download(&self, from: &Peer, payload: &[u8]) -> Result<serde_json::Value, String> {
        let v: serde_json::Value =
            serde_json::from_slice(payload).map_err(|e| format!("the request: {e}"))?;
        let url = v["url"].as_str().unwrap_or("").trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) || url.len() > 2000 {
            return Err("url: an http:// or https:// address".into());
        }
        // Not this machine's own (or a link-local) address: asked from another machine, a
        // download of `http://127.0.0.1:<an engine's port>/…` reached this machine's
        // engines as if from itself (bug run 4).
        if !public_host(url) {
            return Err("url: not this machine's own or a link-local address".into());
        }
        let dir = self.dir().join(lib::META_DIR).join("requests");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let job = idn_common::epoch::now().as_micros() as u64;
        let req = json!({
            "url": url, "choice": v["choice"].as_str().unwrap_or("1080p"),
            "title": v["title"].as_str().unwrap_or(""),
            "folder": v["folder"].as_str().unwrap_or(""),
            "from": from.name, "at": idn_common::epoch::secs(),
        });
        std::fs::write(dir.join(format!("{job}.json")), req.to_string())
            .map_err(|e| e.to_string())?;
        Ok(json!({"job": job}))
    }

    /// How download `job` is doing: IDN Data's `.state` file, else queued.
    fn job_state(&self, job: &str) -> Result<serde_json::Value, String> {
        if job.is_empty() || !job.bytes().all(|b| b.is_ascii_digit()) {
            return Err("job: its number".into());
        }
        let dir = self.dir().join(lib::META_DIR).join("requests");
        if let Ok(b) = std::fs::read(dir.join(format!("{job}.state"))) {
            return serde_json::from_slice(&b).map_err(|e| e.to_string());
        }
        if dir.join(format!("{job}.json")).exists() {
            return Ok(json!({"state": "queued"}));
        }
        Err("no such download".into())
    }

    /// The media folder.
    pub fn dir(&self) -> PathBuf {
        self.dir.lock_ok().clone()
    }

    /// Moves the media folder (the system's service has its own, §N.25).
    pub fn set_dir(&self, dir: PathBuf) {
        *self.dir.lock_ok() = dir;
    }

    /// `<media dir><transfer way offer peer file size done state at/>…</media>`, the
    /// newest first.
    pub fn element(&self) -> idn_open_protocol::xml::Element {
        let mut e = idn_open_protocol::xml::Element::new("media")
            .with_attr("dir", &self.dir().to_string_lossy())
            .with_attr("library", if self.library() { "true" } else { "false" });
        for t in self.transfers() {
            e = e.with_child(
                idn_open_protocol::xml::Element::new("transfer")
                    .with_attr("way", t.way)
                    .with_attr("offer", &t.id)
                    .with_attr("peer", &t.peer)
                    .with_attr("file", &t.file)
                    .with_attr("size", &t.size.to_string())
                    .with_attr("done", &t.done.load(Ordering::Relaxed).to_string())
                    .with_attr("state", &t.state)
                    .with_attr("at", &t.at.to_string()),
            );
        }
        e
    }

    /// The transfers, newest first.
    pub fn transfers(&self) -> Vec<Transfer> {
        self.transfers.lock_ok().iter().rev().cloned().collect()
    }

    /// Notes a transfer.
    pub fn note(&self, t: Transfer) {
        let mut l = self.transfers.lock_ok();
        l.push_back(t);
        while l.len() > KEPT {
            l.pop_front();
        }
    }

    fn set_state(&self, peer: &str, id: &str, state: String) {
        if let Some(t) = self
            .transfers
            .lock_ok()
            .iter_mut()
            .rev()
            .find(|t| t.way == "in" && t.peer == peer && t.id == id)
        {
            t.state = state;
        }
    }

    /// As [`Self::set_state`], only while it still says "receiving".
    fn set_state_if_receiving(&self, peer: &str, id: &str, state: String) {
        if let Some(t) = self
            .transfers
            .lock_ok()
            .iter_mut()
            .rev()
            .find(|t| t.way == "in" && t.peer == peer && t.id == id)
            && t.state == "receiving"
        {
            t.state = state;
        }
    }

    /// Sets the state of file `id` this Node offered to the box `fp` (not taken: "refused",
    /// "failed: …").
    pub fn set_out_state(&self, id: &str, fp: &str, state: String) {
        if let Some(t) = self
            .transfers
            .lock_ok()
            .iter_mut()
            .rev()
            .find(|t| t.way == "out" && t.id == id && t.peer_fp == fp)
        {
            t.state = state;
        }
    }

    /// A file this Node offered, as the box fetching it goes: its bytes, then "sent" or
    /// "failed: …" (by the offer's id and the box it was offered to: another that asks
    /// for the id changes nothing).
    fn served(&self, to: &Peer, id: &str, served: Served) {
        let mut l = self.transfers.lock_ok();
        let Some(t) = l
            .iter_mut()
            .rev()
            .find(|t| t.way == "out" && t.id == id && t.peer_fp == to.fingerprint)
        else {
            return;
        };
        match served {
            Served::Progress { sent, .. } => {
                t.done.store(sent, Ordering::Relaxed);
            }
            Served::Done { size } => {
                t.done.store(size, Ordering::Relaxed);
                t.state = "sent".into();
            }
            // (A later fetch that resumes and finishes makes it "sent" after all.)
            Served::Failed(e) => t.state = format!("failed: {e}"),
        }
    }
}

/// A [`PathSink`] that counts what arrived.
struct Counting {
    inner: PathSink,
    done: Arc<AtomicU64>,
    /// To mark the transfer failed if it ends without a result (below).
    media: Weak<Media>,
    peer: String,
    id: String,
    /// The file was finished (checked and in place).
    finished: bool,
}

impl Drop for Counting {
    /// A transfer whose task ended with neither the file finished nor an error told (it
    /// was stopped, or failed where nothing reported it) shows "failed: interrupted",
    /// not "receiving" for ever. A state already told isn't touched.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(m) = self.media.upgrade() {
            m.set_state_if_receiving(&self.peer, &self.id, "failed: interrupted".into());
        }
    }
}

impl FileSink for Counting {
    fn held(&mut self) -> io::Result<Option<(u64, String)>> {
        let h = self.inner.held()?;
        self.done
            .store(h.as_ref().map_or(0, |(n, _)| *n), Ordering::Relaxed);
        Ok(h)
    }
    fn restart(&mut self) -> io::Result<()> {
        self.done.store(0, Ordering::Relaxed);
        self.inner.restart()
    }
    fn begin(&mut self, info: &FileInfo) -> io::Result<()> {
        self.inner.begin(info)
    }
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.done.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.inner.write(data)
    }
    fn finish(&mut self, info: &FileInfo) -> io::Result<()> {
        self.inner.finish(info)?;
        self.finished = true;
        Ok(())
    }
}

/// A `Range: bytes=A-B` / `bytes=A-` / `bytes=-N` of a file of `size` bytes, as (first,
/// last); `None` when it isn't one that fits.
fn range(v: &str, size: u64) -> Option<(u64, u64)> {
    let r = v.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = r.split_once('-')?;
    if size == 0 {
        return None;
    }
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (size.saturating_sub(n), size - 1)
        }
        (a, "") => (a.parse().ok()?, size - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(size - 1)),
    };
    (start <= end && start < size).then_some((start, end))
}

impl Handler for Media {
    /// The library's requests (§N.26): only with the library on.
    fn on_request(&self, from: &Peer, kind: &str, payload: Vec<u8>) -> Reply {
        if !self.library() || !kind.starts_with("idn.library.") {
            return Reply::Unknown;
        }
        let Some(server) = self.server() else {
            return Reply::Unknown;
        };
        // The index's requests (v2: ids, changes, uploads, deletes, marks, space).
        if let Some(r) = server.request(from, kind, &payload) {
            return r;
        }
        let dir = self.dir();
        let id = || String::from_utf8_lossy(&payload).trim().to_owned();
        // An item by its lasting id, through the index (an indexed folder's `@NAME/…`
        // item too).
        let item = || -> Option<(String, PathBuf)> {
            let id = id();
            Some((server.rel_path(&id)?, server.path_of(&id)?))
        };
        let r: Result<Vec<u8>, String> = match kind {
            kinds::THUMB => match item() {
                Some((rel, _)) => std::fs::read(lib::thumb_file(&dir, &rel))
                    .map_err(|_| "no thumbnail".to_owned()),
                None => Err("no such item".into()),
            },
            kinds::LINK => match item() {
                Some((rel, p)) => self.link(&rel, p).map(|v| v.to_string().into_bytes()),
                None => Err("no such item".into()),
            },
            kinds::DOWNLOAD => self
                .request_download(from, &payload)
                .map(|v| v.to_string().into_bytes()),
            // A finished download's item as its lasting id (IDN Data names its path).
            kinds::JOB => self.job_state(&id()).map(|mut v| {
                if let Some(path) = v["item"].as_str().map(str::to_owned) {
                    server.rescan();
                    if let Some(i) = server.id_of(&path) {
                        v["item"] = json!(i);
                    }
                }
                v.to_string().into_bytes()
            }),
            _ => return Reply::Unknown,
        };
        match r {
            Ok(b) => Reply::Ok(b),
            Err(e) => Reply::Error(e),
        }
    }

    /// A library item, fetched by a machine of the site (§N.26).
    fn file(&self, _from: &Peer, id: &str) -> Option<PathBuf> {
        // By its lasting id (`lib:<id>`).
        self.server()?.file(id)
    }

    fn on_push(&self, from: &Peer, offer: &FileInfo) -> Option<Box<dyn FileSink>> {
        // An upload into the library (v2).
        if offer.id.starts_with(lib::server::UPLOAD_PREFIX) {
            return self.server()?.on_push(from, offer);
        }
        let replace = offer.id.starts_with(REPLACE_PREFIX);
        if !(replace || offer.id.starts_with(ID_PREFIX)) || offer.size > MAX_FILE {
            return None;
        }
        let name = lib::safe_file_name(&offer.name)?;
        let dir = self.dir();
        if let Err(e) = make_dir(&dir) {
            idn_open_protocol::log::log(&format!(
                "media from {}: the media folder {}: {e}",
                from.name,
                dir.display()
            ));
            return None;
        }
        // (Replacing: the new file comes in as NAME.part and is renamed over the old one
        // only once complete and its SHA-256 checked; a failed transfer leaves it.)
        let path = target(&dir, &name, replace);
        let file = path.file_name()?.to_string_lossy().into_owned();
        let done = Arc::new(AtomicU64::new(0));
        self.note(Transfer {
            way: "in",
            id: offer.id.clone(),
            peer: from.name.clone(),
            peer_fp: from.fingerprint.clone(),
            file,
            size: offer.size,
            done: done.clone(),
            state: "receiving".into(),
            at: idn_common::epoch::secs(),
        });
        Some(Box::new(Counting {
            inner: PathSink::new(path),
            done,
            media: self.me.clone(),
            peer: from.name.clone(),
            id: offer.id.clone(),
            finished: false,
        }))
    }

    /// A file this Node offered, fetched by the box it went to (§N.25: "sent").
    fn on_served(&self, to: &Peer, id: &str, served: Served) {
        if id.starts_with(ID_PREFIX) || id.starts_with(REPLACE_PREFIX) {
            self.served(to, id, served);
        }
    }

    fn on_received(&self, from: &Peer, offer: &FileInfo, result: io::Result<()>) {
        if offer.id.starts_with(lib::server::UPLOAD_PREFIX) {
            if let Some(s) = self.server() {
                s.on_received(offer, result);
            }
            return;
        }
        self.set_state(
            &from.name,
            &offer.id,
            match result {
                Ok(()) => "received".into(),
                Err(e) => format!("failed: {e}"),
            },
        );
    }
}

/// Whether `url`'s host is neither this machine (`localhost`, a loopback or unspecified
/// address) nor link-local.
fn public_host(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = auth.rsplit_once('@').map_or(auth, |x| x.1);
    let host = idn_open_protocol::web::split_host(host)
        .0
        .trim_end_matches('.');
    if host.is_empty() || idn_open_protocol::web::is_loopback_name(host) {
        return false;
    }
    // A port that isn't one (`host:x`): not read as public (fail closed; an empty port,
    // `127.0.0.1:`, is the default one and already split off).
    if host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err() {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => {
            !(a.is_loopback() || a.is_unspecified() || a.is_link_local())
        }
        Ok(std::net::IpAddr::V6(a)) => {
            !(a.is_loopback()
                || a.is_unspecified()
                || (a.segments()[0] & 0xffc0) == 0xfe80
                || a.to_ipv4_mapped()
                    .is_some_and(|v| v.is_loopback() || v.is_unspecified()))
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_name_no_local_host() {
        for u in [
            "http://127.0.0.1:29214/api/v1/state",
            "http://localhost/x",
            "https://[::1]/x",
            "http://0.0.0.0/",
            "http://169.254.1.1/",
            "http://user@127.0.0.2:80/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(!public_host(u), "{u}");
        }
        for u in [
            "https://www.youtube.com/watch?v=x",
            "http://10.0.0.5/a.mp4",
            "http://[2001:db8::1]/a",
        ] {
            assert!(public_host(u), "{u}");
        }
    }

    #[test]
    fn names_are_kept_safe_and_never_overwrite() {
        let safe = lib::safe_file_name;
        assert_eq!(safe("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            safe(r"C:\x\Song: live?.mp4").as_deref(),
            Some("Song_ live_.mp4")
        );
        assert_eq!(safe(".hidden"), None);
        assert_eq!(safe(".."), None);
        assert_eq!(safe("v.mp4.part"), None);
        assert_eq!(safe("a. . "), Some("a".into()));
        let tmp = idn_common::testing::TempDir::new("idn-media-test");
        let dir = tmp.path();
        assert_eq!(place(dir, "v.mp4"), dir.join("v.mp4"));
        std::fs::write(dir.join("v.mp4"), b"x").unwrap();
        assert_eq!(place(dir, "v.mp4"), dir.join("v (2).mp4"));
        // Replacing: over the finished file of the name; without, beside it.
        assert_eq!(target(dir, "v.mp4", true), dir.join("v.mp4"));
        assert_eq!(target(dir, "v.mp4", false), dir.join("v (2).mp4"));
        assert_eq!(target(dir, "new.mp4", true), dir.join("new.mp4"));
        // Never over a folder (or a link) of the name: beside it.
        std::fs::create_dir_all(dir.join("f.mp4")).unwrap();
        assert_eq!(target(dir, "f.mp4", true), dir.join("f (2).mp4"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("v.mp4"), dir.join("l.mp4")).unwrap();
            assert_eq!(target(dir, "l.mp4", true), dir.join("l (2).mp4"));
        }
        // The new file replaces the old only once complete and checked: a failed one
        // leaves it; a resumed one (its .part kept) finishes over it.
        let new: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        let src = dir.join("source.bin");
        std::fs::write(&src, &new).unwrap();
        let info = FileInfo {
            id: format!("{REPLACE_PREFIX}1-v.mp4"),
            name: "v.mp4".into(),
            size: new.len() as u64,
            version: lib::index::sha256_of(&src, 0).unwrap(),
        };
        let mut s = PathSink::new(target(dir, "v.mp4", true));
        s.begin(&info).unwrap();
        s.write(&new[..40_000]).unwrap();
        assert_eq!(
            std::fs::read(dir.join("v.mp4")).unwrap(),
            b"x",
            "the old one stays"
        );
        drop(s);
        let mut s = PathSink::new(target(dir, "v.mp4", true));
        assert_eq!(s.held().unwrap(), Some((40_000, info.version.clone())));
        s.write(&new[40_000..]).unwrap();
        let bad = FileInfo {
            version: "0".repeat(64),
            ..info.clone()
        };
        assert!(s.finish(&bad).is_err());
        assert_eq!(
            std::fs::read(dir.join("v.mp4")).unwrap(),
            b"x",
            "a bad one: kept"
        );
        let mut s = PathSink::new(target(dir, "v.mp4", true));
        s.begin(&info).unwrap();
        s.write(&new).unwrap();
        s.finish(&info).unwrap();
        assert_eq!(std::fs::read(dir.join("v.mp4")).unwrap(), new);
        assert!(!dir.join("v (2).mp4").exists());
        assert!(inside(dir, &dir.join("v.mp4")));
        assert!(!inside(dir, &dir.join("../v.mp4")));
        assert!(!inside(dir, dir));
        assert!(!inside(dir, Path::new("/etc/hostname")));
    }
}
