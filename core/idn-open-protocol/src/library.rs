//! The site's media library (roadmap R.23, idn-extensions.md §E.12.11): one machine's
//! media folder (its IDN Node's, idn-node.md §N.26) offered to the site's machines over
//! the file channel (`channel/1`), so only they reach it. A program finds the library,
//! reads its catalog, and either fetches items to a folder of its own ahead of time (a
//! presentation program's playlist for today; resumed and checked) or plays one straight
//! off the library through a short-lived link that any player opens and seeks in. It can
//! also ask the library's machine to download a video for it (IDN Data, R.22) and fetch
//! it when done.
//!
//! ```no_run
//! use idn_open_protocol::library::Library;
//! use idn_open_protocol::node::Node;
//! use std::time::Duration;
//! # fn main() -> std::io::Result<()> {
//! let node = Node::connect("My presentation app")?;
//! let lib = Library::find(node.enrollment().clone(), Duration::from_secs(10))?;
//! let today = lib.catalog()?;
//! for item in today.items.iter().filter(|i| i.folder == "Sunday") {
//!     lib.fetch(&item.id, &std::path::Path::new("cache").join(&item.name()), &mut |_, _| {})?;
//! }
//! # Ok(())
//! # }
//! ```

pub mod cache;
pub mod index;
pub mod server;

use crate::channel::{Channel, ChannelConfig, Handler, PathSink, Reply};
use crate::trust::Enrollment;
use idn_common::sync::RwLockOk as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// File ids of library items on the channel: `lib:` and the item's id.
pub const FILE_PREFIX: &str = "lib:";
/// The folder inside the library that holds what IDN Data keeps about it (metadata,
/// thumbnails, download requests); never an item.
pub const META_DIR: &str = ".library";
/// The metadata file IDN Data writes in [`META_DIR`].
pub const META_FILE: &str = "library.json";
/// How long a link to play an item directly lasts.
pub const LINK_LIFE: Duration = Duration::from_secs(12 * 3600);

/// The channel's request types (§E.12.11).
pub mod kinds {
    /// `{}` → `{"name", "items", "version"}`: is this the library.
    pub const HELLO: &str = "idn.library.hello";
    /// → the [`Catalog`](super::Catalog) as JSON.
    pub const CATALOG: &str = "idn.library.catalog";
    /// An item's id → its thumbnail (JPEG).
    pub const THUMB: &str = "idn.library.thumb";
    /// An item's id → `{"url", "expires"}`: a link to play it directly.
    pub const LINK: &str = "idn.library.link";
    /// `{"url", "choice", "title", "folder"}` → `{"job"}`: download a video into the
    /// library.
    pub const DOWNLOAD: &str = "idn.library.download";
    /// A job's id → `{"state", "done", "total", "item", "error"}`.
    pub const JOB: &str = "idn.library.job";
    // Media library v2 (2026-11-22): lasting ids, changes, uploads, deletes, marks.
    /// `{"id"}` → one item as the catalog lists it.
    pub const ITEM: &str = "idn.library.item";
    /// `{"since", "wait_s"}` → `{"events": [{"seq", "kind", "id"}], "next", "missed"}`.
    pub const CHANGES: &str = "idn.library.changes";
    /// `{"name", "folder", "size"}` → `{"token", "file_id"}`; then the file pushed as
    /// `file_id`.
    pub const UPLOAD: &str = "idn.library.upload";
    /// `{"token"}` → `{"state", "done", "total", "item", "existing", "error"}`.
    pub const UPLOAD_STATE: &str = "idn.library.upload_state";
    /// `{"id", "by", "confirm"?, "reason"?}` → `{"ok", "overridden"}` (into the trash).
    pub const DELETE: &str = "idn.library.delete";
    /// `{}` → `{"trash_days", "items": [{"id", "path", "size", "sha256", "at", "by",
    /// "holders"}]}`.
    pub const TRASH: &str = "idn.library.trash";
    /// `{"id"}` → `{"id", "path"}`: back from the trash.
    pub const RESTORE: &str = "idn.library.restore";
    /// `{"holder", "how": "set"|"add"|"remove", "ids"}` → `{"count"}`.
    pub const MARKS: &str = "idn.library.marks";
    /// `{}` → `{"library_bytes", "free_bytes", "total_bytes", "items", "trash",
    /// "trash_days", "keep_free"}`.
    pub const SPACE: &str = "idn.library.space";
    /// `{"trash_days"?}` → the space and settings.
    pub const SETTINGS: &str = "idn.library.settings";
    /// `{"add": NAME, "path"}` / `{"remove": NAME}` / `{}` → `{"roots": {NAME: path}}`:
    /// folders indexed in place (a NAS share; read-only; items `@NAME/…`).
    pub const ROOTS: &str = "idn.library.roots";
}

/// What an item is, by its file's ending.
pub fn kind_of(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        // (MPEG-1/2, Blu-ray and DVD files, 3GP, Ogg video and Flash video too:
        // formats a host's users added before.)
        "mp4" | "mov" | "mkv" | "m4v" | "webm" | "avi" | "mxf" | "ts" | "mts" | "wmv" | "mpg"
        | "mpeg" | "m2v" | "m2ts" | "vob" | "3gp" | "3g2" | "ogv" | "flv" | "f4v" | "dv"
        | "asf" => "video",
        "jpg" | "jpeg" | "jfif" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "heic"
        | "heif" | "avif" | "tga" => "picture",
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "aif" | "aiff" | "wma"
        | "ac3" | "mka" | "caf" => "sound",
        _ => {
            return EXTRA_KINDS.read_ok().get(&ext).copied();
        }
    })
}

/// Endings a host added ([`take_endings`]).
static EXTRA_KINDS: std::sync::RwLock<BTreeMap<String, &'static str>> =
    std::sync::RwLock::new(BTreeMap::new());

/// Has every library in this program take files ending in each of `endings` as `kind`
/// (`"video"`, `"picture"` or `"sound"`), beside the endings it takes anyway
/// ([`kind_of`]): a host whose users bring other formats. An
/// ending is letters and digits, at most 8 (no dot); others, or another kind, are an
/// error and nothing is added.
pub fn take_endings(endings: &[&str], kind: &str) -> Result<(), String> {
    let kind: &'static str = match kind {
        "video" => "video",
        "picture" => "picture",
        "sound" => "sound",
        _ => return Err("kind: video, picture or sound".into()),
    };
    let ok =
        |e: &&str| !e.is_empty() && e.len() <= 8 && e.bytes().all(|b| b.is_ascii_alphanumeric());
    if let Some(bad) = endings.iter().find(|e| !ok(e)) {
        return Err(format!(
            "{bad:?}: an ending is letters and digits, at most 8"
        ));
    }
    let mut extra = EXTRA_KINDS.write_ok();
    for e in endings {
        extra.insert(e.to_ascii_lowercase(), kind);
    }
    Ok(())
}

/// One item of the library.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Item {
    /// Its lasting id (16 hex digits; media library v2): it stays through renames and
    /// moves. (A library before v2 gave its path here.)
    pub id: String,
    /// Its path in the library, with `/` between folders ("Sunday/Welcome.mp4").
    pub path: String,
    /// Its SHA-256 (64 hex digits), once the library worked it out (new big files take a
    /// while).
    pub sha256: Option<String>,
    /// What it is called (its file's name without the ending, unless given).
    pub title: String,
    /// `video`, `picture` or `sound`.
    pub kind: String,
    /// The folder it is in ("" at the top).
    pub folder: String,
    /// Bytes.
    pub size: u64,
    /// When the file last changed, Unix seconds.
    pub modified: u64,
    /// Seconds (video and sound), when known.
    pub duration: Option<f64>,
    /// The picture's width in pixels, when known.
    pub width: Option<u32>,
    /// The picture's height in pixels, when known.
    pub height: Option<u32>,
    /// The video's codec ("h264"), when known.
    pub codec: Option<String>,
    /// Whether the picture has an alpha channel, when known.
    pub alpha: Option<bool>,
    /// How many sound tracks it has, when known.
    pub audio_tracks: Option<u32>,
    /// Words to find it by ("intro", "Christmas").
    pub tags: Vec<String>,
    /// Whether it has a thumbnail ([`Library::thumbnail`]).
    pub thumb: bool,
}

impl Item {
    /// Its file's name (from its path: a v2 id is no name).
    pub fn name(&self) -> &str {
        let p = if self.path.is_empty() {
            &self.id
        } else {
            &self.path
        };
        p.rsplit('/').next().unwrap_or(p)
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "path": self.path, "sha256": self.sha256,
            "title": self.title, "kind": self.kind, "folder": self.folder,
            "size": self.size, "modified": self.modified, "duration": self.duration,
            "width": self.width, "height": self.height, "codec": self.codec,
            "alpha": self.alpha, "audio_tracks": self.audio_tracks,
            "tags": self.tags, "thumb": self.thumb,
        })
    }

    fn from_json(v: &Value) -> Option<Item> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        let id = v["id"].as_str()?.to_owned();
        Some(Item {
            // (A library before v2: the id is the path.)
            path: v["path"].as_str().map_or_else(|| id.clone(), str::to_owned),
            sha256: v["sha256"].as_str().map(str::to_owned),
            id,
            title: s("title"),
            kind: s("kind"),
            folder: s("folder"),
            size: v["size"].as_u64().unwrap_or(0),
            modified: v["modified"].as_u64().unwrap_or(0),
            duration: v["duration"].as_f64(),
            width: v["width"].as_u64().map(|x| x as u32),
            height: v["height"].as_u64().map(|x| x as u32),
            codec: v["codec"].as_str().map(str::to_owned),
            alpha: v["alpha"].as_bool(),
            audio_tracks: v["audio_tracks"].as_u64().map(|x| x as u32),
            tags: v["tags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            thumb: v["thumb"].as_bool().unwrap_or(false),
        })
    }
}

/// What happened to an item ([`Library::changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Its place.
    pub seq: u64,
    /// `added`, `removed`, `changed`, or `removed_in_use` (deleted although held in use,
    /// confirmed by a person).
    pub kind: String,
    /// The item's id.
    pub id: String,
}

/// The library's contents.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    /// Goes up when anything in it changes (the newest change, Unix milliseconds).
    pub version: u64,
    /// The library machine's name.
    pub name: String,
    /// Every folder, by its path ("Sunday", "Backgrounds/Easter").
    pub folders: Vec<String>,
    /// Every item, by id.
    pub items: Vec<Item>,
}

impl Catalog {
    /// As the library sends it.
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version, "name": self.name, "folders": self.folders,
            "items": self.items.iter().map(Item::to_json).collect::<Vec<_>>(),
        })
    }

    /// As the library sends it, as text: [`Catalog::to_json`] written straight out (a
    /// large library's catalog is megabytes; no tree of values made first).
    pub fn to_json_text(&self) -> Vec<u8> {
        fn put<T: serde::Serialize + ?Sized>(out: &mut Vec<u8>, key: &str, v: &T, first: bool) {
            if !first {
                out.push(b',');
            }
            out.push(b'"');
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(b"\":");
            // (Writing to a Vec doesn't fail; a float that isn't finite is written null.)
            let _ = serde_json::to_writer(&mut *out, v);
        }
        let mut out = Vec::with_capacity(64 + self.items.len() * 300);
        out.push(b'{');
        put(&mut out, "version", &self.version, true);
        put(&mut out, "name", &self.name, false);
        put(&mut out, "folders", &self.folders, false);
        out.extend_from_slice(b",\"items\":[");
        for (i, it) in self.items.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.push(b'{');
            put(&mut out, "id", &it.id, true);
            put(&mut out, "path", &it.path, false);
            put(&mut out, "sha256", &it.sha256, false);
            put(&mut out, "title", &it.title, false);
            put(&mut out, "kind", &it.kind, false);
            put(&mut out, "folder", &it.folder, false);
            put(&mut out, "size", &it.size, false);
            put(&mut out, "modified", &it.modified, false);
            put(&mut out, "duration", &it.duration, false);
            put(&mut out, "width", &it.width, false);
            put(&mut out, "height", &it.height, false);
            put(&mut out, "codec", &it.codec, false);
            put(&mut out, "alpha", &it.alpha, false);
            put(&mut out, "audio_tracks", &it.audio_tracks, false);
            put(&mut out, "tags", &it.tags, false);
            put(&mut out, "thumb", &it.thumb, false);
            out.push(b'}');
        }
        out.extend_from_slice(b"]}");
        out
    }

    /// As a program reads it.
    pub fn from_json(v: &Value) -> Option<Catalog> {
        Some(Catalog {
            version: v["version"].as_u64().unwrap_or(0),
            name: v["name"].as_str().unwrap_or("").to_owned(),
            folders: v["folders"]
                .as_array()?
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect(),
            items: v["items"]
                .as_array()?
                .iter()
                .filter_map(Item::from_json)
                .collect(),
        })
    }

    /// The item `id`.
    pub fn item(&self, id: &str) -> Option<&Item> {
        // (Sorted by id as the library sends it: found by halves; a list in another order
        // looked through.)
        match self.items.binary_search_by(|i| i.id.as_str().cmp(id)) {
            Ok(i) => Some(&self.items[i]),
            Err(_) => self.items.iter().find(|i| i.id == id),
        }
    }
}

/// A file or folder name the library shows (not hidden, not a partial download).
fn shown(name: &str) -> bool {
    !name.starts_with('.') && !name.ends_with(".part") && !name.ends_with(".ytdl")
}

/// The item at library path `path` (a media file; its size and time), with what IDN
/// Data keeps about it (`meta`: its entry in [`META_FILE`], or null). Its id is its path
/// (a library before v2).
pub(crate) fn item_at(path: &str, size: u64, modified: u64, meta: &Value) -> Option<Item> {
    let name = path.rsplit('/').next()?;
    let mut it = Item {
        id: path.to_owned(),
        path: path.to_owned(),
        kind: kind_of(name)?.to_owned(),
        title: name.rsplit_once('.').map_or(name, |(s, _)| s).to_owned(),
        folder: path.rsplit_once('/').map_or("", |(f, _)| f).to_owned(),
        size,
        modified,
        ..Default::default()
    };
    apply_meta(&mut it, meta);
    Some(it)
}

/// What IDN Data keeps about the item (`m`: its entry in [`META_FILE`]) taken in: only if
/// kept for this very file (its size and time); tags stay even if the file changed.
fn apply_meta(it: &mut Item, m: &Value) {
    if m.is_object()
        && m["size"].as_u64() == Some(it.size)
        && m["modified"].as_u64() == Some(it.modified)
    {
        if let Some(t) = m["title"].as_str().filter(|t| !t.is_empty()) {
            it.title = t.to_owned();
        }
        it.duration = m["duration"].as_f64();
        it.width = m["width"].as_u64().map(|x| x as u32);
        it.height = m["height"].as_u64().map(|x| x as u32);
        it.codec = m["codec"].as_str().map(str::to_owned);
        it.alpha = m["alpha"].as_bool();
        it.audio_tracks = m["audio_tracks"].as_u64().map(|x| x as u32);
        it.thumb = m["thumb"].as_bool().unwrap_or(false);
    }
    if let Some(t) = m["tags"].as_array() {
        it.tags = t
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect();
    }
}

/// The catalog of the library at `dir`: its files and folders (hidden ones and partial
/// downloads left out), with what IDN Data keeps about each ([`META_FILE`]: title,
/// duration, size in pixels, codec, tags, thumbnail) where it has it.
pub fn scan(dir: &Path, name: &str) -> Catalog {
    let meta = load_meta(dir);
    let mut c = Catalog {
        name: name.to_owned(),
        ..Default::default()
    };
    let mut newest = 0u128;
    let root = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    walk(&root, dir, "", 0, &mut c, &mut newest);
    for it in &mut c.items {
        apply_meta(it, &meta["items"][&it.id]);
    }
    let meta_time = std::fs::metadata(dir.join(META_DIR).join(META_FILE))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis());
    c.version = newest.max(meta_time) as u64;
    c.items.sort_by(|a, b| a.id.cmp(&b.id));
    c.folders.sort();
    c
}

/// Lists `dir` (`rel` within the library at `root`, canonical) into `c`. A link is listed as
/// what it points to, and only when that is inside the library: the files listed are
/// those [`item_path`] serves (a link was listed by its own size, and
/// one leading out listed but never served).
fn walk(root: &Path, dir: &Path, rel: &str, depth: usize, c: &mut Catalog, newest: &mut u128) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        if !shown(&name) {
            continue;
        }
        let Ok(m) = std::fs::metadata(e.path()) else {
            continue;
        };
        if e.file_type().is_ok_and(|t| t.is_symlink())
            && !e.path().canonicalize().is_ok_and(|p| p.starts_with(root))
        {
            continue;
        }
        let t = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        *newest = (*newest).max(t.map_or(0, |d| d.as_millis()));
        let id = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if m.is_dir() {
            if depth < 8 {
                c.folders.push(id.clone());
                walk(root, &e.path(), &id, depth + 1, c, newest);
            }
        } else if let Some(kind) = kind_of(&name) {
            let title = name
                .rsplit_once('.')
                .map_or(name.as_str(), |(s, _)| s)
                .to_owned();
            c.items.push(Item {
                path: id.clone(),
                id,
                title,
                kind: kind.into(),
                folder: rel.to_owned(),
                size: m.len(),
                modified: t.map_or(0, |d| d.as_secs()),
                ..Default::default()
            });
        }
    }
}

/// The file of item `id` in the library at `dir`, if it is one: inside the library,
/// shown, a media file.
pub fn item_path(dir: &Path, id: &str) -> Option<PathBuf> {
    if !safe_name(id) {
        return None;
    }
    kind_of(id)?;
    let p = dir.join(id);
    let (d, f) = (dir.canonicalize().ok()?, p.canonicalize().ok()?);
    (f.starts_with(&d) && f.is_file()).then_some(f)
}

/// Whether `id` is a name that stays inside the library's folder on every system: parts
/// split by `/`, none empty, `..`, hidden or with `\\` or NUL, and on Windows no `:` (a
/// drive; Linux names may have one).
pub fn safe_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 1000
        && !id.contains(['\\', '\0'])
        && !(cfg!(windows) && id.contains(':'))
        && id
            .split('/')
            .all(|p| !p.is_empty() && p != ".." && shown(p))
}

/// A file name made safe to keep in a folder: no folders, nothing hidden or partial
/// (`.part`), nothing Windows refuses (`<>:"|?*`, controls: each made `_`; no trailing
/// dot or space), at most 200 characters. `None` when nothing is left.
pub fn safe_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(200)
        .collect();
    let s = s.trim_end_matches(['.', ' ']).trim().to_owned();
    (!s.is_empty() && shown(&s)).then_some(s)
}

/// Where item `id`'s thumbnail is kept (IDN Data makes it): `.library/thumbs/HASH.jpg`.
pub fn thumb_file(dir: &Path, id: &str) -> PathBuf {
    let h = idn_common::hex::hex(&crate::hash::sha256(id.as_bytes())[..16]);
    dir.join(META_DIR).join("thumbs").join(format!("{h}.jpg"))
}

/// A download a program asked the library for, as the library's machine tells it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Job {
    /// `queued`, `downloading`, `finishing`, `done`, `failed` or `cancelled`.
    pub state: String,
    /// Bytes fetched so far.
    pub done: u64,
    /// The download's size, when known.
    pub total: Option<u64>,
    /// When done: the item's id.
    pub item: Option<String>,
    /// Why it failed.
    pub error: Option<String>,
}

/// Answers nothing: a program's channel only reaches out.
struct Quiet;
impl Handler for Quiet {}

/// A library in the site ([`Library::list`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LibraryInfo {
    /// The library's own name: the one its host gave (`LibraryServer::open(dir, "MyLibrary")`),
    /// else its machine's.
    pub name: String,
    /// The machine serving it: its name in the site.
    pub machine: String,
    /// The machine serving it: its fingerprint ([`Library::find_on`]).
    pub fingerprint: String,
    /// How many items it has.
    pub items: u64,
    /// Media library v2 (lasting ids, changes, uploads; §E.12.11.1).
    pub v2: bool,
}

/// The site's library, as a program reaches it.
pub struct Library {
    channel: Arc<Channel>,
    /// The library machine's fingerprint and name.
    host: String,
    name: String,
}

impl Library {
    /// Finds the site's library (the first machine to answer, within `timeout`), with
    /// this machine's site membership (`Node::enrollment`, or a device's own). A site with
    /// more than one: [`find_named`](Self::find_named), [`find_on`](Self::find_on),
    /// [`list`](Self::list).
    pub fn find(enrollment: Arc<Enrollment>, timeout: Duration) -> io::Result<Library> {
        Self::find_where(enrollment, timeout, None, |_| true)
    }

    /// The library named `name` (its own name: "MyLibrary" for a host's
    /// `LibraryServer::open(dir, "MyLibrary")`, else its machine's), within `timeout`.
    pub fn find_named(
        enrollment: Arc<Enrollment>,
        name: &str,
        timeout: Duration,
    ) -> io::Result<Library> {
        Self::find_where(enrollment, timeout, None, |i| i.name == name)
    }

    /// The library of the machine with fingerprint `fingerprint`, within `timeout`.
    pub fn find_on(
        enrollment: Arc<Enrollment>,
        fingerprint: &str,
        timeout: Duration,
    ) -> io::Result<Library> {
        Self::find_where(enrollment, timeout, Some(fingerprint), |_| true)
    }

    /// [`find_named`](Self::find_named) over the program's own channel (one endpoint
    /// toward the library's machine instead of two; the library's requests need no
    /// handler on it). On a machine whose IDN Node holds the machine's endpoint, the
    /// program's channel is `client_only`.
    pub fn find_named_via(
        channel: Arc<Channel>,
        name: &str,
        timeout: Duration,
    ) -> io::Result<Library> {
        Self::find_with(channel, Instant::now() + timeout, None, |i| i.name == name)
    }

    /// [`find_on`](Self::find_on) over the program's own channel
    /// ([`find_named_via`](Self::find_named_via)).
    pub fn find_on_via(
        channel: Arc<Channel>,
        fingerprint: &str,
        timeout: Duration,
    ) -> io::Result<Library> {
        Self::find_with(channel, Instant::now() + timeout, Some(fingerprint), |_| {
            true
        })
    }

    /// Every library in the site that answers within `timeout` (each machine asked once
    /// it is listed; at most 4 s for their answers once one has answered).
    pub fn list(enrollment: Arc<Enrollment>, timeout: Duration) -> io::Result<Vec<LibraryInfo>> {
        let end = Instant::now() + timeout;
        let channel = Self::channel(&enrollment, end)?;
        Self::list_via(channel, end.saturating_duration_since(Instant::now()))
    }

    /// [`list`](Self::list) over the program's own channel.
    pub fn list_via(channel: Arc<Channel>, timeout: Duration) -> io::Result<Vec<LibraryInfo>> {
        let end = Instant::now() + timeout;
        loop {
            let rx = Self::hellos(&channel, None);
            let mut out = Vec::new();
            let round = Instant::now() + Duration::from_secs(4);
            while let Ok(i) =
                rx.recv_timeout(round.min(end).saturating_duration_since(Instant::now()))
            {
                out.push(i);
            }
            if !out.is_empty() || Instant::now() >= end {
                out.sort_by(|a, b| a.name.cmp(&b.name).then(a.fingerprint.cmp(&b.fingerprint)));
                return Ok(out);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// The membership's channel, once it has its site certificate.
    fn channel(enrollment: &Arc<Enrollment>, end: Instant) -> io::Result<Arc<Channel>> {
        // The membership's site certificate comes a moment after a program connects to
        // its Node.
        while enrollment.creds().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
        }
        Channel::start(
            ChannelConfig {
                client_only: true,
                ..ChannelConfig::new(enrollment.clone())
            },
            Arc::new(Quiet),
        )
    }

    /// The site's machines (or only `only`) asked whether they serve a library: their
    /// answers as they come.
    fn hellos(
        channel: &Arc<Channel>,
        only: Option<&str>,
    ) -> std::sync::mpsc::Receiver<LibraryInfo> {
        let (tx, rx) = std::sync::mpsc::channel();
        for d in channel
            .devices()
            .into_iter()
            .filter(|d| only.is_none_or(|f| d.fingerprint == f))
        {
            let (ch, tx) = (channel.clone(), tx.clone());
            std::thread::spawn(move || {
                let r = ch.request(&d.fingerprint, kinds::HELLO, b"{}", Duration::from_secs(3));
                if let Ok(Reply::Ok(b)) = r {
                    let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                    let _ = tx.send(LibraryInfo {
                        name: v["name"]
                            .as_str()
                            .filter(|n| !n.is_empty())
                            .unwrap_or(&d.name)
                            .to_owned(),
                        machine: d.name,
                        fingerprint: d.fingerprint,
                        items: v["items"].as_u64().unwrap_or(0),
                        v2: v["v"].as_u64().unwrap_or(1) >= 2,
                    });
                }
            });
        }
        rx
    }

    fn find_where(
        enrollment: Arc<Enrollment>,
        timeout: Duration,
        only: Option<&str>,
        pick: impl Fn(&LibraryInfo) -> bool,
    ) -> io::Result<Library> {
        let end = Instant::now() + timeout;
        let channel = Self::channel(&enrollment, end)?;
        Self::find_with(channel, end, only, pick)
    }

    fn find_with(
        channel: Arc<Channel>,
        end: Instant,
        only: Option<&str>,
        pick: impl Fn(&LibraryInfo) -> bool,
    ) -> io::Result<Library> {
        loop {
            let rx = Self::hellos(&channel, only);
            let round = Instant::now() + Duration::from_secs(4);
            while let Ok(i) =
                rx.recv_timeout(round.min(end).saturating_duration_since(Instant::now()))
            {
                if pick(&i) {
                    return Ok(Library {
                        channel,
                        host: i.fingerprint,
                        name: i.machine,
                    });
                }
            }
            if Instant::now() >= end {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no such media library in the site",
                ));
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// The library machine's fingerprint.
    pub fn host_fingerprint(&self) -> &str {
        &self.host
    }

    /// The library machine's name.
    pub fn host_name(&self) -> &str {
        &self.name
    }

    fn ask(&self, kind: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
        match self
            .channel
            .request(&self.host, kind, payload, Duration::from_secs(10))?
        {
            Reply::Ok(b) => Ok(b),
            Reply::Error(e) => Err(io::Error::other(e)),
            Reply::Refused => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the library refused",
            )),
            Reply::Unknown => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "that machine has no library now",
            )),
        }
    }

    fn ask_json(&self, kind: &str, payload: &[u8]) -> io::Result<Value> {
        serde_json::from_slice(&self.ask(kind, payload)?).map_err(io::Error::other)
    }

    /// The catalog now.
    pub fn catalog(&self) -> io::Result<Catalog> {
        Catalog::from_json(&self.ask_json(kinds::CATALOG, b"")?)
            .ok_or_else(|| io::Error::other("the library's catalog isn't readable"))
    }

    /// One item as the catalog lists it (media library v2).
    pub fn item(&self, id: &str) -> io::Result<Item> {
        Item::from_json(&self.ask_json(kinds::ITEM, json!({"id": id}).to_string().as_bytes())?)
            .ok_or_else(|| io::Error::other("the library's item isn't readable"))
    }

    /// The library's changes after place `since` (0: all still kept), waiting up to
    /// `wait` (at most 25 s) for one: the changes, the place to ask from next, and whether
    /// some after `since` were no longer kept (read the catalog again then).
    pub fn changes(&self, since: u64, wait: Duration) -> io::Result<(Vec<Change>, u64, bool)> {
        let v = self.ask_long(
            kinds::CHANGES,
            json!({"since": since, "wait_s": wait.as_secs().min(25)}),
            wait + Duration::from_secs(10),
        )?;
        let events = v["events"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| Change {
                seq: e["seq"].as_u64().unwrap_or(0),
                kind: e["kind"].as_str().unwrap_or("").to_owned(),
                id: e["id"].as_str().unwrap_or("").to_owned(),
            })
            .collect();
        Ok((
            events,
            v["next"].as_u64().unwrap_or(since),
            v["missed"] == true,
        ))
    }

    /// Puts the file at `path` into the library's `folder` ("" at the top), resumable and
    /// checked; `progress` gets (bytes, total) as the library receives it. The item's id,
    /// and whether it was there already (the same SHA-256: that item, nothing added).
    /// Refused when it won't fit on the library's disk.
    pub fn upload(
        &self,
        path: &Path,
        folder: &str,
        progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<(String, bool)> {
        let size = std::fs::metadata(path)?.len();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let v = self.ask_json(
            kinds::UPLOAD,
            json!({"name": name, "folder": folder, "size": size})
                .to_string()
                .as_bytes(),
        )?;
        let (token, file_id) = (
            v["token"].as_str().unwrap_or("").to_owned(),
            v["file_id"].as_str().unwrap_or("").to_owned(),
        );
        match self
            .channel
            .push(&self.host, &file_id, path, Duration::from_secs(10))?
        {
            Reply::Ok(_) => {}
            Reply::Error(e) => return Err(io::Error::other(e)),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the library refused the file",
                ));
            }
        }
        let mut quiet = Instant::now();
        let mut last = 0;
        loop {
            let s = self.ask_json(
                kinds::UPLOAD_STATE,
                json!({"token": token}).to_string().as_bytes(),
            )?;
            let done = s["done"].as_u64().unwrap_or(0);
            progress(done, size);
            match s["state"].as_str().unwrap_or("") {
                "done" => {
                    return Ok((
                        s["item"].as_str().unwrap_or("").to_owned(),
                        s["existing"] == true,
                    ));
                }
                "failed" => {
                    return Err(io::Error::other(
                        s["error"]
                            .as_str()
                            .unwrap_or("the upload failed")
                            .to_owned(),
                    ));
                }
                _ => {}
            }
            if done != last {
                (last, quiet) = (done, Instant::now());
            } else if quiet.elapsed() > Duration::from_secs(60) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the upload made no progress for a minute",
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Deletes item `id` into the trash (`by`: who, for the trash and the log). Refused
    /// while held in use: [`delete_in_use`](Self::delete_in_use) overrides that.
    pub fn delete(&self, id: &str, by: &str) -> io::Result<()> {
        self.ask_json(
            kinds::DELETE,
            json!({"id": id, "by": by}).to_string().as_bytes(),
        )
        .map(|_| ())
    }

    /// Deletes item `id` although held in use, as a person confirmed: `confirm` is the
    /// item's file name (or id) typed back, `reason` for the log. The holders overridden.
    pub fn delete_in_use(
        &self,
        id: &str,
        by: &str,
        confirm: &str,
        reason: &str,
    ) -> io::Result<Vec<String>> {
        let v = self.ask_json(
            kinds::DELETE,
            json!({"id": id, "by": by, "confirm": confirm, "reason": reason})
                .to_string()
                .as_bytes(),
        )?;
        Ok(v["overridden"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect())
    }

    /// The trash as the library sends it: `{"trash_days", "items": [{"id", "path",
    /// "size", "sha256", "at", "by", "holders"}]}`.
    pub fn trash(&self) -> io::Result<Value> {
        self.ask_json(kinds::TRASH, b"{}")
    }

    /// Puts item `id` back from the trash (its id kept): its path now.
    pub fn restore(&self, id: &str) -> io::Result<String> {
        let v = self.ask_json(kinds::RESTORE, json!({"id": id}).to_string().as_bytes())?;
        Ok(v["path"].as_str().unwrap_or("").to_owned())
    }

    /// `holder`'s "in use" marks: `how` is `set` (replace all of them: after a restart),
    /// `add` or `remove`. A marked item can't be deleted without its name typed back. The
    /// holder's marks now.
    pub fn marks(&self, holder: &str, how: &str, ids: &[String]) -> io::Result<usize> {
        let v = self.ask_json(
            kinds::MARKS,
            json!({"holder": holder, "how": how, "ids": ids})
                .to_string()
                .as_bytes(),
        )?;
        Ok(v["count"].as_u64().unwrap_or(0) as usize)
    }

    /// The items `holder` holds in use, by id, without changing them
    /// (their count is the list's length).
    pub fn marked(&self, holder: &str) -> io::Result<Vec<String>> {
        let v = self.ask_json(
            kinds::MARKS,
            json!({"holder": holder, "how": "list"})
                .to_string()
                .as_bytes(),
        )?;
        Ok(v["ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect())
    }

    /// The library's size, its disk's free and total bytes, the trash's days:
    /// `{"library_bytes", "free_bytes", "total_bytes", "items", "trash", "trash_days",
    /// "keep_free"}`.
    pub fn space(&self) -> io::Result<Value> {
        self.ask_json(kinds::SPACE, b"{}")
    }

    /// Indexes the folder `path` on the library's machine (a NAS share mounted there; a
    /// full path) in place as `name`: its files listed as `@NAME/…`, with ids and SHA-256s,
    /// read-only to the library.
    pub fn add_root(&self, name: &str, path: &str) -> io::Result<()> {
        self.ask_json(
            kinds::ROOTS,
            json!({"add": name, "path": path}).to_string().as_bytes(),
        )
        .map(|_| ())
    }

    /// Stops indexing folder `name` (its files untouched).
    pub fn remove_root(&self, name: &str) -> io::Result<()> {
        self.ask_json(kinds::ROOTS, json!({"remove": name}).to_string().as_bytes())
            .map(|_| ())
    }

    /// How long deleted items stay in the trash (1–365 days).
    pub fn set_trash_days(&self, days: u64) -> io::Result<()> {
        self.ask_json(
            kinds::SETTINGS,
            json!({"trash_days": days}).to_string().as_bytes(),
        )
        .map(|_| ())
    }

    fn ask_long(&self, kind: &str, payload: Value, timeout: Duration) -> io::Result<Value> {
        match self
            .channel
            .request(&self.host, kind, payload.to_string().as_bytes(), timeout)?
        {
            Reply::Ok(b) => serde_json::from_slice(&b).map_err(io::Error::other),
            Reply::Error(e) => Err(io::Error::other(e)),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "that machine has no library now",
            )),
        }
    }

    /// Item `id`'s thumbnail (JPEG).
    pub fn thumbnail(&self, id: &str) -> io::Result<Vec<u8>> {
        self.ask(kinds::THUMB, id.as_bytes())
    }

    /// Fetches item `id` to the file `to` (its folder made), resuming what an earlier
    /// fetch left (`to.part`) and checking it whole. `progress` gets (bytes, total).
    pub fn fetch(&self, id: &str, to: &Path, progress: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        let mut sink = PathSink::new(to);
        self.channel
            .fetch(
                &self.host,
                &format!("{FILE_PREFIX}{id}"),
                &mut sink,
                progress,
            )
            .map(|_| ())
    }

    /// Fetches items `ids` into `dir` ahead of time (a playlist for today): each as its
    /// file's name; one already there with the item's size is left as it is. Each
    /// item's file, or why it couldn't be fetched. `progress` gets (item, bytes, total).
    pub fn prefetch(
        &self,
        ids: &[String],
        dir: &Path,
        progress: &mut dyn FnMut(&str, u64, u64),
    ) -> Vec<(String, io::Result<PathBuf>)> {
        let cat = self.catalog();
        ids.iter()
            .map(|id| {
                let r = (|| {
                    let cat = cat
                        .as_ref()
                        .map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
                    let item = cat
                        .item(id)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such item"))?;
                    // A name from another machine's catalog: only one that stays in `dir`.
                    if !safe_name(item.name()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the catalog names a file outside the library",
                        ));
                    }
                    let to = dir.join(item.name());
                    if std::fs::metadata(&to).is_ok_and(|m| m.len() == item.size) {
                        return Ok(to);
                    }
                    self.fetch(id, &to, &mut |n, t| progress(id, n, t))?;
                    Ok(to)
                })();
                (id.clone(), r)
            })
            .collect()
    }

    /// A link to play item `id` straight off the library (any player opens it and seeks
    /// in it), good for [`LINK_LIFE`].
    pub fn link(&self, id: &str) -> io::Result<String> {
        let v = self.ask_json(kinds::LINK, id.as_bytes())?;
        v["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("no link"))
    }

    /// Asks the library's machine to download `url` (a preset, `1080p`, or a format id:
    /// IDN Data's choices, idn-data.md §D.6) into `folder` of the library: its job.
    pub fn download(&self, url: &str, choice: &str, title: &str, folder: &str) -> io::Result<u64> {
        let v = self.ask_json(
            kinds::DOWNLOAD,
            json!({"url": url, "choice": choice, "title": title, "folder": folder})
                .to_string()
                .as_bytes(),
        )?;
        v["job"].as_u64().ok_or_else(|| io::Error::other("no job"))
    }

    /// How download `job` is doing.
    pub fn job(&self, job: u64) -> io::Result<Job> {
        let v = self.ask_json(kinds::JOB, job.to_string().as_bytes())?;
        Ok(job_from_json(&v))
    }
}

/// A job's state as JSON (the library's machine) and back (a program).
pub fn job_from_json(v: &Value) -> Job {
    Job {
        state: v["state"].as_str().unwrap_or("queued").to_owned(),
        done: v["done"].as_u64().unwrap_or(0),
        total: v["total"].as_u64(),
        item: v["item"].as_str().map(str::to_owned),
        error: v["error"].as_str().map(str::to_owned),
    }
}

/// The library's metadata ([`META_FILE`]) as IDN Data writes it: by item id, the file's
/// size and time it was made for, and title, duration, size in pixels, codec, tags,
/// thumbnail.
pub fn read_meta(dir: &Path) -> BTreeMap<String, Value> {
    load_meta(dir)["items"]
        .as_object()
        .map(|o| o.clone().into_iter().collect())
        .unwrap_or_default()
}

/// [`META_FILE`] as it is, `Null` when there is none. One that can't be read is set aside
/// and said (IDN Data writes it again), not read as empty and then saved over.
fn load_meta(dir: &Path) -> Value {
    idn_common::fsutil::load_json(&dir.join(META_DIR).join(META_FILE))
        .unwrap_or_else(|e| {
            crate::log::log(&format!("library: {e}"));
            None
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_becomes_a_catalog() {
        let dir = idn_common::testing::TempDir::new("library");
        std::fs::create_dir_all(dir.join("Sunday")).unwrap();
        std::fs::create_dir_all(dir.join(".library")).unwrap();
        std::fs::write(dir.join("Sunday/Welcome.mp4"), b"video").unwrap();
        std::fs::write(dir.join("Sunday/Welcome.mp4.part"), b"half").unwrap();
        std::fs::write(dir.join("Background.png"), b"png").unwrap();
        std::fs::write(dir.join("notes.txt"), b"not media").unwrap();
        std::fs::write(dir.join(".hidden.mp4"), b"x").unwrap();
        let c = scan(&dir, "SERVER");
        assert_eq!(c.folders, ["Sunday"]);
        let ids: Vec<&str> = c.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["Background.png", "Sunday/Welcome.mp4"]);
        let w = c.item("Sunday/Welcome.mp4").unwrap();
        assert_eq!(
            (w.kind.as_str(), w.folder.as_str(), w.title.as_str(), w.size),
            ("video", "Sunday", "Welcome", 5)
        );
        // IDN Data's metadata for this very file is taken; tags always.
        let meta = json!({"items": {
            "Sunday/Welcome.mp4": {"size": 5, "modified": w.modified, "title": "Welcome!", "duration": 12.5,
                                   "width": 1920, "height": 1080, "codec": "h264", "thumb": true, "tags": ["intro"]},
            "Background.png": {"size": 999, "modified": 1, "title": "stale", "tags": ["loop"]}
        }});
        std::fs::write(dir.join(".library/library.json"), meta.to_string()).unwrap();
        let c = scan(&dir, "SERVER");
        let w = c.item("Sunday/Welcome.mp4").unwrap();
        assert_eq!(
            (w.title.as_str(), w.duration, w.width, w.thumb),
            ("Welcome!", Some(12.5), Some(1920), true)
        );
        assert_eq!(w.tags, ["intro"]);
        let b = c.item("Background.png").unwrap();
        assert_eq!(
            (b.title.as_str(), b.tags.clone()),
            ("Background", vec!["loop".to_string()])
        );
        assert_eq!(Catalog::from_json(&c.to_json()).unwrap(), c);
        let text: Value = serde_json::from_slice(&c.to_json_text()).unwrap();
        assert_eq!(text, c.to_json());
        // Item paths: only the library's media files.
        assert!(item_path(&dir, "Sunday/Welcome.mp4").is_some());
        for bad in [
            "../x.mp4",
            "notes.txt",
            ".hidden.mp4",
            "Sunday/Welcome.mp4.part",
            "/etc/x.mp4",
            "Sunday//Welcome.mp4",
            ".library/library.json",
        ] {
            assert!(item_path(&dir, bad).is_none(), "{bad}");
        }
    }
}
