//! The library's machine side (media library v2, idn-extensions.md §E.12.11, 2026-11-22):
//! the index ([`super::index`]) kept current in the background (the folder looked at again,
//! new files hashed, the trash emptied of what is past its days), and the site's requests
//! answered over `channel/1`: the catalog, changes since a place, uploads, deletes (refused
//! for an item held in use unless its name is typed back), the trash, the marks, the space.
//! Embeddable: the IDN Node runs one for its media folder (idn-node.md §N.26); a program
//! hosting the Site Manager (an embedding application) runs one with its own membership
//! (`host::Handle::join_own_site`) and channel.

use super::index::{self, Entry, Index};
use super::{Catalog, FILE_PREFIX, META_DIR, kinds};
use crate::channel::{FileInfo, FileSink, Handler, PathSink, Peer, Reply};
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// File ids of uploads on the channel: `upload:` and the upload's token.
pub const UPLOAD_PREFIX: &str = "upload:";
/// How fast new files are hashed in the background (bytes a second): the host's disk is
/// shared with live work (a full rebuild once stalled its other services).
pub const HASH_RATE: u64 = 40 * 1024 * 1024;
/// How often the folder is looked at again (files copied in by hand, renamed, removed).
pub const RESCAN_EVERY: Duration = Duration::from_secs(20);
/// Room kept free on the library's disk: an upload that would leave less is refused.
pub const KEEP_FREE: u64 = 2 * 1024 * 1024 * 1024;
/// The longest a `changes` request waits for one.
pub const CHANGES_WAIT: Duration = Duration::from_secs(25);
/// The incoming folder inside [`META_DIR`]: uploads as they arrive.
const INCOMING_DIR: &str = "incoming";
/// Background changes (hashes, facts) are written at most this often: a big library's
/// first look hashes thousands of small files, and its index written whole after each
/// would be gigabytes of writes on a disk others use.
const SAVE_EVERY: Duration = Duration::from_secs(5);
/// A file that couldn't be read for its hash (no permission, a share down) is tried
/// again after this long, others hashed meanwhile.
const HASH_RETRY: Duration = Duration::from_secs(600);

/// An upload, from its offer to its item.
#[derive(Debug, Clone)]
struct Upload {
    /// The uploading machine's fingerprint (only it may send the file).
    from: String,
    folder: String,
    name: String,
    size: u64,
    /// `waiting`, `receiving`, `done` or `failed`.
    state: &'static str,
    done: Arc<AtomicU64>,
    item: Option<String>,
    /// The item was in the library already (same SHA-256): that one is answered.
    existing: bool,
    error: String,
    at: Instant,
}

/// What a host knows of a media file ([`LibraryServer::set_prober`]): `{"duration",
/// "width", "height", "codec", "alpha", "audio_tracks"}` (any left out unknown); `None`
/// when it can't read it. `idn_ffmpeg::media::library_facts` is one.
pub type Prober = Arc<dyn Fn(&Path) -> Option<Value> + Send + Sync>;

/// Index entry `e` as the catalog lists it: what IDN Data keeps about it (`meta`, by
/// path) and the prober's facts where IDN Data hasn't them.
fn entry_item(
    e: &Entry,
    meta: &BTreeMap<String, Value>,
    facts: &BTreeMap<String, (u64, u64, Value)>,
) -> Option<super::Item> {
    let mut it = super::item_at(
        &e.path,
        e.size,
        e.mtime,
        meta.get(&e.path).unwrap_or(&Value::Null),
    )?;
    it.id = e.id.clone();
    it.sha256 = e.sha256.clone();
    if let Some((_, _, f)) = facts.get(&e.id) {
        it.duration = it.duration.or(f["duration"].as_f64());
        it.width = it.width.or(f["width"].as_u64().map(|x| x as u32));
        it.height = it.height.or(f["height"].as_u64().map(|x| x as u32));
        it.codec = it.codec.clone().or(f["codec"].as_str().map(str::to_owned));
        it.alpha = it.alpha.or(f["alpha"].as_bool());
        it.audio_tracks = it
            .audio_tracks
            .or(f["audio_tracks"].as_u64().map(|x| x as u32));
    }
    Some(it)
}

/// The library's machine side; dropping it stops its background work.
pub struct LibraryServer {
    dir: PathBuf,
    name: Mutex<String>,
    index: Mutex<Index>,
    /// Told when the index changes (`changes` waiters).
    changed: Condvar,
    uploads: Mutex<BTreeMap<String, Upload>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    prober: Mutex<Option<Prober>>,
    /// Facts the prober gave, by id: (size, mtime, facts), kept in `facts.json`.
    facts: Mutex<BTreeMap<String, (u64, u64, Value)>>,
    /// The index or the facts changed in the background and aren't written yet; and
    /// when they last were.
    index_dirty: AtomicBool,
    facts_dirty: AtomicBool,
    saved_at: Mutex<Instant>,
    /// Its folder as held in [`OPEN_DIRS`] (let go at the end of its `Drop`).
    held: PathBuf,
}

/// The folders a library is open on in this program: one at a time (an
/// instance dropped while its worker held it ended later, on the worker, and
/// saved its index and facts over those of the instance opened meanwhile). A second
/// `open` waits for the first's end; [`LibraryServer::close`] waits for it too.
static OPEN_DIRS: Mutex<std::collections::BTreeSet<PathBuf>> =
    Mutex::new(std::collections::BTreeSet::new());
static OPEN_DIRS_CHANGED: Condvar = Condvar::new();
/// How long `open` and `close` wait for an instance's end.
const OPEN_WAIT: Duration = Duration::from_secs(10);

/// Waits up to `wait` until `cond` holds of the open folders; the folders then, held.
fn open_dirs_wait(
    wait: Duration,
    cond: impl Fn(&std::collections::BTreeSet<PathBuf>) -> bool,
) -> std::sync::MutexGuard<'static, std::collections::BTreeSet<PathBuf>> {
    let end = Instant::now() + wait;
    let mut g = OPEN_DIRS.lock_ok();
    while !cond(&g) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        g = OPEN_DIRS_CHANGED
            .wait_timeout(g, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    g
}

/// The facts' file in the library's [`META_DIR`].
const FACTS_FILE: &str = "facts.json";

impl Drop for LibraryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // (Dropped on the worker itself, when it held the last reference: not waited for,
        // which would never end; it returns at its next look at `stop`.)
        if let Some(w) = self.worker.lock_ok().take()
            && w.thread().id() != std::thread::current().id()
        {
            let _ = w.join();
        }
        // What the background changed and hadn't written yet.
        if self.index_dirty.load(Ordering::Relaxed) {
            self.saved(&self.index.lock_ok());
        }
        self.save_facts();
        // Its folder free for the next instance (all written first).
        OPEN_DIRS.lock_ok().remove(&self.held);
        OPEN_DIRS_CHANGED.notify_all();
    }
}

/// A file name made safe for the library: [`super::safe_file_name`], and a media file.
fn safe_file_name(name: &str) -> Option<String> {
    super::safe_file_name(name).filter(|s| super::kind_of(s).is_some())
}

/// A folder path made safe ("" for the top): parts as [`safe_file_name`] without the
/// media check.
fn safe_folder(folder: &str) -> Option<String> {
    let parts: Vec<&str> = folder
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    // (Nor `:` on any system: on Windows "C:/…" or "D:x" joined to the library's folder
    // replaced it, and an upload landed anywhere; bug run 4. Each part a plain name.)
    if parts.iter().any(|p| {
        *p == ".."
            || !super::shown(p)
            || p.contains(['\\', '\0', ':'])
            || !matches!(
                std::path::Path::new(p).components().next(),
                Some(std::path::Component::Normal(_))
            )
    }) {
        return None;
    }
    // (`@NAME` is an indexed folder: read-only, nothing put there.)
    if parts.first().is_some_and(|p| p.starts_with('@')) {
        return None;
    }
    Some(parts.join("/"))
}

impl LibraryServer {
    /// The library at `dir` (made if missing) for the machine named `name`: its index read
    /// and brought up to date, its background work started.
    /// One open at a time on a folder in a program: while another instance on it is still
    /// open (or ending), this waits up to 10 s for its end, then refuses.
    pub fn open(dir: &Path, name: &str) -> io::Result<Arc<LibraryServer>> {
        std::fs::create_dir_all(dir.join(META_DIR))?;
        let held = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        {
            let mut open = open_dirs_wait(OPEN_WAIT, |o| !o.contains(&held));
            if !open.insert(held.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "the library at {} is still open in this program (close the other first)",
                        dir.display()
                    ),
                ));
            }
        }
        // (Let go again if the open fails below.)
        struct Holding(Option<PathBuf>);
        impl Drop for Holding {
            fn drop(&mut self) {
                if let Some(h) = self.0.take() {
                    OPEN_DIRS.lock_ok().remove(&h);
                    OPEN_DIRS_CHANGED.notify_all();
                }
            }
        }
        let mut holding = Holding(Some(held.clone()));
        // Uploads left by an earlier run: their tokens ended with it.
        let _ = std::fs::remove_dir_all(dir.join(META_DIR).join(INCOMING_DIR));
        // (An index that can't be read stops the library, rather than every item getting
        // a new id; bug run 4.)
        let mut ix = Index::load(dir)?;
        if ix.rescan() {
            ix.save()?;
        }
        let s = Arc::new(LibraryServer {
            dir: dir.to_owned(),
            name: Mutex::new(name.to_owned()),
            index: Mutex::new(ix),
            changed: Condvar::new(),
            uploads: Mutex::new(BTreeMap::new()),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            prober: Mutex::new(None),
            // (One that can't be read is set aside and said, not saved over.)
            facts: Mutex::new(
                idn_common::fsutil::load_saved(&dir.join(META_DIR).join(FACTS_FILE), |b| {
                    serde_json::from_slice::<Value>(b).ok()
                })
                .unwrap_or_else(|why| {
                    crate::log::log(&format!("library: {why}"));
                    None
                })
                .and_then(|v| v.as_object().cloned())
                .map(|m| {
                    m.into_iter()
                        .filter_map(|(id, f)| {
                            Some((
                                id,
                                (
                                    f["size"].as_u64()?,
                                    f["mtime"].as_u64()?,
                                    f["facts"].clone(),
                                ),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            ),
            index_dirty: AtomicBool::new(false),
            facts_dirty: AtomicBool::new(false),
            saved_at: Mutex::new(Instant::now()),
            held,
        });
        // (Held by the instance from now on: its `Drop` lets go.)
        holding.0 = None;
        let weak = Arc::downgrade(&s);
        let stop = s.stop.clone();
        let w = std::thread::Builder::new()
            .name("idn-library".into())
            .spawn(move || background(weak, stop))?;
        *s.worker.lock_ok() = Some(w);
        Ok(s)
    }

    /// Closes this library and returns once its work has stopped and everything is saved,
    /// so the folder can be opened again at once. Other handles
    /// to it still held: waits up to 10 s for them to go, then says so (it ends with the
    /// last of them). Its background work (rescans, hashing new files, emptying the trash)
    /// stops at once either way: a handle still serving it (a channel's) answers from what
    /// it has until dropped, so drop those first.
    pub fn close(self: Arc<Self>) -> io::Result<()> {
        let held = self.held.clone();
        self.stop.store(true, Ordering::Relaxed);
        drop(self);
        let open = open_dirs_wait(OPEN_WAIT, |o| !o.contains(&held));
        if open.contains(&held) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the library is still held elsewhere in this program: it ends with the last handle",
            ));
        }
        Ok(())
    }

    /// Serves this library to the site over a channel of its own with `member` (a program
    /// hosting the Site Manager: `host::Handle::join_own_site`'s membership): the site's
    /// programs find it (`Library::find`) and use it as the IDN Node's. Keep the channel:
    /// dropping it stops serving.
    pub fn serve(
        self: &Arc<Self>,
        member: Arc<crate::trust::Enrollment>,
    ) -> io::Result<Arc<crate::channel::Channel>> {
        crate::channel::Channel::start(crate::channel::ChannelConfig::new(member), self.clone())
    }

    /// The library's folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How this host reads a media file's facts (duration, size in pixels, codec, alpha,
    /// sound tracks): each item probed once in the background (again when it changes),
    /// listed in the catalog where IDN Data hasn't said them.
    pub fn set_prober(&self, prober: Prober) {
        *self.prober.lock_ok() = Some(prober);
    }

    /// The machine's name, as the catalog says it.
    pub fn set_name(&self, name: &str) {
        *self.name.lock_ok() = name.to_owned();
    }

    fn saved(&self, ix: &Index) {
        self.index_dirty.store(false, Ordering::Relaxed);
        *self.saved_at.lock_ok() = Instant::now();
        if let Err(e) = ix.save() {
            crate::log::log(&format!("library: saving its index: {e}"));
        }
        self.changed.notify_all();
    }

    /// A background change (a hash): told to waiters at once, written at most every
    /// [`SAVE_EVERY`] (and when the work is done, and at the end).
    fn touched(&self, ix: &Index) {
        self.index_dirty.store(true, Ordering::Relaxed);
        if self.saved_at.lock_ok().elapsed() >= SAVE_EVERY {
            self.saved(ix);
        } else {
            self.changed.notify_all();
        }
    }

    /// The prober's facts written, if changed since.
    fn save_facts(&self) {
        if !self.facts_dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let v: serde_json::Map<String, Value> = self
            .facts
            .lock_ok()
            .iter()
            .map(|(k, (sz, mt, f))| (k.clone(), json!({"size": sz, "mtime": mt, "facts": f})))
            .collect();
        if let Err(e) = idn_common::fsutil::write_private(
            &self.dir.join(META_DIR).join(FACTS_FILE),
            Value::Object(v).to_string().as_bytes(),
        ) {
            crate::log::log(&format!("library: saving its facts: {e}"));
        }
    }

    /// The folder looked at again now (a file copied in by hand): whether anything changed.
    pub fn rescan(&self) -> bool {
        // (The folders walked without the index held: a big library or a slow share
        // takes a while, and every request waits on the index.)
        let (dir, roots) = self.index.lock_ok().look_at();
        let Some(look) = index::look(&dir, &roots) else {
            return false;
        };
        let mut ix = self.index.lock_ok();
        let changed = ix.apply(look, true);
        if changed {
            self.saved(&ix);
        }
        changed
    }

    /// The catalog: every item with its lasting id, path and SHA-256 (none until worked
    /// out), and what IDN Data keeps about it (title, duration, size in pixels, codec,
    /// tags, thumbnail).
    pub fn catalog(&self) -> Catalog {
        // (Files copied in by hand since the last look: listed at once, as before v2.)
        self.rescan();
        let name = self.name.lock_ok().clone();
        let meta = super::read_meta(&self.dir);
        let facts = self.facts.lock_ok();
        let ix = self.index.lock_ok();
        // (From the index and the look it just took: the folder isn't walked again.)
        Catalog {
            version: ix.seq(),
            name,
            folders: ix.folders().to_vec(),
            items: ix
                .items()
                .filter_map(|e| entry_item(e, &meta, &facts))
                .collect(),
        }
    }

    /// Item `id` as the catalog lists it (without a look at the folder or a whole
    /// catalog made).
    pub fn item(&self, id: &str) -> Option<super::Item> {
        let meta = super::read_meta(&self.dir);
        let facts = self.facts.lock_ok();
        let ix = self.index.lock_ok();
        entry_item(ix.get(id)?, &meta, &facts)
    }

    /// Item `id`'s file, if it is in the library now.
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        let ix = self.index.lock_ok();
        let p = ix.full(&ix.get(id)?.path)?;
        p.is_file().then_some(p)
    }

    /// Item `id`'s path in the library ("Sunday/Welcome.mp4").
    pub fn rel_path(&self, id: &str) -> Option<String> {
        self.index.lock_ok().get(id).map(|e| e.path.clone())
    }

    /// The id of the item at library path `path` ("Sunday/Welcome.mp4").
    pub fn id_of(&self, path: &str) -> Option<String> {
        self.index.lock_ok().by_path(path).map(|e| e.id.clone())
    }

    /// Changes after place `since`, waiting up to `wait` for one: (changes, next place,
    /// whether some were no longer kept).
    pub fn changes(&self, since: u64, wait: Duration) -> (Vec<index::Change>, u64, bool) {
        let end = Instant::now() + wait.min(CHANGES_WAIT);
        let mut ix = self.index.lock_ok();
        // (A place past the newest is answered at once: `missed`.)
        while ix.seq() == since {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || self.stop.load(Ordering::Relaxed) {
                break;
            }
            ix = self
                .changed
                .wait_timeout(ix, left.min(Duration::from_secs(1)))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        ix.changes(since)
    }

    /// The library's size, the disk's free and total bytes, and its settings.
    pub fn space(&self) -> Value {
        let (free, total) = crate::diag::free_space(&self.dir).unwrap_or((0, 0));
        let ix = self.index.lock_ok();
        json!({"library_bytes": ix.bytes(), "free_bytes": free, "total_bytes": total,
            "items": ix.items().count(), "trash": ix.trash().len(),
            "trash_days": ix.trash_days(), "keep_free": KEEP_FREE})
    }

    /// Whether `size` more bytes fit (leaving [`KEEP_FREE`]).
    fn fits(&self, size: u64) -> Result<(), String> {
        let (free, _) = crate::diag::free_space(&self.dir).unwrap_or((u64::MAX, 0));
        // (Uploads already taken and still to come count: each alone would fit.)
        let coming: u64 = self
            .uploads
            .lock_ok()
            .values()
            .filter(|u| matches!(u.state, "waiting" | "receiving"))
            .map(|u| u.size.saturating_sub(u.done.load(Ordering::Relaxed)))
            .sum();
        let size = size.saturating_add(coming);
        if free < size.saturating_add(KEEP_FREE) {
            return Err(format!(
                "no room: {} MB free on the library's disk, {} MB needed (and {} GB kept free)",
                free / 1_000_000,
                size / 1_000_000,
                KEEP_FREE / 1_000_000_000
            ));
        }
        Ok(())
    }

    /// A file on the library's machine taken into the library (a host's media moved over,
    /// imported shows): copied (or moved) into `folder` under its name ("NAME (2)" when
    /// taken), its SHA-256 kept. One already there with the same SHA-256: that item, and
    /// nothing copied or moved.
    pub fn import(&self, path: &Path, folder: &str, copy: bool) -> io::Result<Entry> {
        use idn_common::err::invalid_input as bad;
        let name = safe_file_name(&path.file_name().unwrap_or_default().to_string_lossy())
            .ok_or_else(|| bad("not a media file name the library takes"))?;
        let folder = safe_folder(folder).ok_or_else(|| bad("folder: no .. or hidden parts"))?;
        let sha = index::sha256_of(path, 0)?;
        if let Some(e) = self.index.lock_ok().by_sha(&sha).cloned() {
            return Ok(e);
        }
        let size = std::fs::metadata(path)?.len();
        self.fits(size).map_err(io::Error::other)?;
        let into = self.dir.join(&folder);
        std::fs::create_dir_all(&into)?;
        // Moved (or copied: kept, or another disk) into the incoming folder first,
        // outside the lock; put in its place under it (two files of one name at once:
        // "NAME (2)", not one over the other).
        let tmp = self.dir.join(META_DIR).join(INCOMING_DIR).join(format!(
            "import-{}",
            idn_common::hex::hex(&crate::random::secure_random::<8>())
        ));
        std::fs::create_dir_all(tmp.parent().unwrap_or(&self.dir))?;
        let moved = !copy && std::fs::rename(path, &tmp).is_ok();
        if !moved && let Err(e) = std::fs::copy(path, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let mut ix = self.index.lock_ok();
        let to = index::free_place(&into, &name);
        if let Err(e) = std::fs::rename(&tmp, &to) {
            // (Moved: put back where it was; copied: the copy goes.)
            if moved {
                let _ = std::fs::rename(&tmp, path);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
            return Err(e);
        }
        // A move across disks: the original goes once the copy is in its place.
        if !copy && !moved {
            let _ = std::fs::remove_file(path);
        }
        let rel = index::rel(&self.dir, &to).ok_or_else(|| bad("outside the library"))?;
        let e = ix.add(&rel, sha)?;
        self.saved(&ix);
        Ok(e)
    }

    /// Deletes item `id` (`by`: who, for the trash): refused while held in use, unless
    /// `confirm` types back its name or id. The holders overridden.
    pub fn delete(&self, id: &str, by: &str, confirm: Option<&str>) -> Result<Vec<String>, String> {
        let mut ix = self.index.lock_ok();
        let r = match confirm {
            Some(c) => ix.delete_in_use(id, by, c),
            None => ix.delete(id, by).map(|()| Vec::new()),
        };
        if r.is_ok() {
            self.saved(&ix);
        }
        r
    }

    /// The items `holder` holds in use, by id (read only).
    pub fn marked(&self, holder: &str) -> Vec<String> {
        self.index.lock_ok().marked_by(holder)
    }

    /// The marks of `holder` changed (`set` replaces all of them).
    pub fn marks(&self, holder: &str, how: &str, ids: &[String]) -> Result<usize, String> {
        if holder.trim().is_empty() || holder.len() > super::index::MAX_HOLDER_NAME {
            return Err(format!(
                "holder: a name of up to {} characters",
                super::index::MAX_HOLDER_NAME
            ));
        }
        let mut ix = self.index.lock_ok();
        let n = ix.mark(holder.trim(), how, ids)?;
        self.saved(&ix);
        Ok(n)
    }

    /// Indexes the folder `path` (a NAS share) in place as `name` (read-only; its items
    /// `@NAME/…`), looked at at once.
    pub fn add_root(&self, name: &str, path: &Path) -> Result<(), String> {
        let mut ix = self.index.lock_ok();
        ix.add_root(name, path)?;
        ix.rescan();
        self.saved(&ix);
        Ok(())
    }

    /// Stops indexing folder `name` (its files untouched).
    pub fn remove_root(&self, name: &str) -> Result<(), String> {
        let mut ix = self.index.lock_ok();
        ix.remove_root(name)?;
        self.saved(&ix);
        Ok(())
    }

    /// The folders indexed in place: `{"roots": {name: path}}`.
    pub fn roots(&self) -> Value {
        let ix = self.index.lock_ok();
        let roots: serde_json::Map<String, Value> = ix
            .roots()
            .iter()
            .map(|(n, p)| (n.clone(), json!(p.to_string_lossy())))
            .collect();
        json!({ "roots": roots })
    }

    /// The trash, newest first.
    pub fn trash(&self) -> Value {
        let ix = self.index.lock_ok();
        json!({"trash_days": ix.trash_days(), "items": ix.trash().iter().map(|t| json!({
            "id": t.entry.id, "path": t.entry.path, "size": t.entry.size,
            "sha256": t.entry.sha256, "at": t.at, "by": t.by,
            "holders": ix.holders(&t.entry.id),
        })).collect::<Vec<_>>()})
    }

    /// Puts item `id` back from the trash.
    pub fn restore(&self, id: &str) -> Result<Entry, String> {
        let mut ix = self.index.lock_ok();
        let e = ix.restore(id)?;
        self.saved(&ix);
        Ok(e)
    }

    /// How long deleted items stay in the trash (1–365 days).
    pub fn set_trash_days(&self, days: u64) -> Result<(), String> {
        let mut ix = self.index.lock_ok();
        ix.set_trash_days(days)?;
        self.saved(&ix);
        Ok(())
    }

    /// An upload offered by machine `from`: its token, once the name, folder and size are
    /// good and it fits.
    fn upload_offer(&self, from: &Peer, v: &Value) -> Result<Value, String> {
        let name = safe_file_name(v["name"].as_str().unwrap_or(""))
            .ok_or("name: a media file's name (video, picture or sound)")?;
        let folder = safe_folder(v["folder"].as_str().unwrap_or(""))
            .ok_or("folder: no .. or hidden parts")?;
        let size = v["size"].as_u64().ok_or("size: the file's bytes")?;
        self.fits(size)?;
        let token = idn_common::hex::hex(&crate::random::secure_random::<12>());
        let mut ups = self.uploads.lock_ok();
        // Uploads a day old end, their partial files with them.
        ups.retain(|token, u| {
            let keep = u.at.elapsed() < Duration::from_secs(24 * 3600);
            if !keep {
                self.remove_incoming(token);
            }
            keep
        });
        if ups.len() >= 256 {
            return Err("too many uploads at once".into());
        }
        ups.insert(
            token.clone(),
            Upload {
                from: from.fingerprint.clone(),
                folder,
                name,
                size,
                state: "waiting",
                done: Arc::default(),
                item: None,
                existing: false,
                error: String::new(),
                at: Instant::now(),
            },
        );
        Ok(json!({"token": token, "file_id": format!("{UPLOAD_PREFIX}{token}")}))
    }

    fn upload_state(&self, token: &str) -> Result<Value, String> {
        let ups = self.uploads.lock_ok();
        let u = ups.get(token).ok_or("no such upload")?;
        Ok(
            json!({"state": u.state, "done": u.done.load(Ordering::Relaxed), "total": u.size,
            "item": u.item, "existing": u.existing, "error": u.error}),
        )
    }

    fn incoming(&self, token: &str) -> PathBuf {
        self.dir.join(META_DIR).join(INCOMING_DIR).join(token)
    }

    /// An upload's files in the incoming folder (whole, partial, its version) removed.
    fn remove_incoming(&self, token: &str) {
        let f = self.incoming(token);
        for ext in ["", ".part", ".part.version"] {
            let mut p = f.clone().into_os_string();
            p.push(ext);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }

    /// An upload arrived whole, its SHA-256 `sha` (checked by the channel as it arrived:
    /// not read again): taken in (or the same file found already there).
    fn upload_arrived(&self, token: &str, sha: &str) -> Result<(String, bool), String> {
        let u = self
            .uploads
            .lock_ok()
            .get(token)
            .cloned()
            .ok_or("no such upload")?;
        let file = self.incoming(token);
        let sha = sha.to_ascii_lowercase();
        let into = self.dir.join(&u.folder);
        std::fs::create_dir_all(&into).map_err(|e| e.to_string())?;
        // Placed under the lock: two uploads of one name at once get "NAME (2)", not one
        // file over the other.
        let mut ix = self.index.lock_ok();
        if let Some(e) = ix.by_sha(&sha).cloned() {
            let _ = std::fs::remove_file(&file);
            return Ok((e.id, true));
        }
        let to = index::free_place(&into, &u.name);
        std::fs::rename(&file, &to).map_err(|e| e.to_string())?;
        let rel = index::rel(&self.dir, &to).ok_or("outside the library")?;
        let e = ix.add(&rel, sha).map_err(|e| e.to_string())?;
        self.saved(&ix);
        Ok((e.id, false))
    }

    /// One of the library's requests (§E.12.11): `None` when it isn't one this answers
    /// (the IDN Node's own: thumbnails, links, downloads).
    pub fn request(&self, from: &Peer, kind: &str, payload: &[u8]) -> Option<Reply> {
        let v: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let text = || String::from_utf8_lossy(payload).trim().to_owned();
        let ids = |v: &Value| -> Vec<String> {
            v["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        };
        let r: Result<Value, String> = match kind {
            kinds::HELLO => {
                let ix = self.index.lock_ok();
                Ok(
                    json!({"name": *self.name.lock_ok(), "items": ix.items().count(),
                    "version": ix.seq(), "v": 2}),
                )
            }
            kinds::CATALOG => return Some(Reply::Ok(self.catalog().to_json_text())),
            kinds::ITEM => {
                let id = v["id"].as_str().map_or_else(text, str::to_owned);
                self.item(&id)
                    .map(|i| i.to_json())
                    .ok_or_else(|| "no such item".to_owned())
            }
            kinds::CHANGES => {
                let (ch, next, missed) = self.changes(
                    v["since"].as_u64().unwrap_or(0),
                    Duration::from_secs(v["wait_s"].as_u64().unwrap_or(0)),
                );
                Ok(
                    json!({"next": next, "missed": missed, "events": ch.iter().map(|c| json!({
                    "seq": c.seq, "kind": c.kind, "id": c.id})).collect::<Vec<_>>()}),
                )
            }
            kinds::UPLOAD => self.upload_offer(from, &v),
            kinds::UPLOAD_STATE => {
                self.upload_state(&v["token"].as_str().map_or_else(text, str::to_owned))
            }
            kinds::DELETE => {
                let id = v["id"].as_str().unwrap_or("");
                let who = format!("{} ({})", from.name, v["by"].as_str().unwrap_or("?"));
                self.delete(id, &who, v["confirm"].as_str()).map(|held| {
                    if !held.is_empty() {
                        crate::log::log(&format!(
                            "library: {who} deleted {id}, held in use by {}: {}",
                            held.join(", "),
                            v["reason"].as_str().unwrap_or("(no reason given)")
                        ));
                    }
                    json!({"ok": true, "overridden": held})
                })
            }
            kinds::TRASH => Ok(self.trash()),
            kinds::RESTORE => self
                .restore(v["id"].as_str().unwrap_or(""))
                .map(|e| json!({"id": e.id, "path": e.path})),
            // (`how: "list"`: read only, the ids too.)
            kinds::MARKS if v["how"] == "list" => {
                let ids = self.marked(v["holder"].as_str().unwrap_or("").trim());
                Ok(json!({"count": ids.len(), "ids": ids}))
            }
            kinds::MARKS => self
                .marks(
                    v["holder"].as_str().unwrap_or(""),
                    v["how"].as_str().unwrap_or(""),
                    &ids(&v),
                )
                .map(|n| json!({"count": n})),
            kinds::SPACE => Ok(self.space()),
            // Adding or removing an indexed folder, or the trash's days: the site's
            // administrators only (a folder indexed is served to the whole site).
            kinds::ROOTS | kinds::SETTINGS
                if !from.admin
                    && (v["add"].is_string()
                        || v["remove"].is_string()
                        || !v["trash_days"].is_null()) =>
            {
                Err(
                    "only the site's administrators change the library's folders and settings"
                        .into(),
                )
            }
            kinds::ROOTS => {
                let r = match (v["add"].as_str(), v["remove"].as_str()) {
                    (Some(name), _) => {
                        self.add_root(name, Path::new(v["path"].as_str().unwrap_or("")))
                    }
                    (None, Some(name)) => self.remove_root(name),
                    _ => Ok(()),
                };
                r.map(|()| self.roots())
            }
            kinds::SETTINGS => match v["trash_days"].as_u64() {
                Some(d) => self.set_trash_days(d).map(|()| self.space()),
                None => Ok(self.space()),
            },
            _ => return None,
        };
        Some(match r {
            Ok(v) => Reply::Ok(v.to_string().into_bytes()),
            Err(e) => Reply::Error(e),
        })
    }

    /// The file behind a channel file id (`lib:<id>`), if it is an item now.
    pub fn file(&self, id: &str) -> Option<PathBuf> {
        self.path_of(id.strip_prefix(FILE_PREFIX)?)
    }

    /// An upload's file offered by `from` (`upload:<token>`): where it is received.
    pub fn on_push(&self, from: &Peer, offer: &FileInfo) -> Option<Box<dyn FileSink>> {
        let token = offer.id.strip_prefix(UPLOAD_PREFIX)?;
        let mut ups = self.uploads.lock_ok();
        let u = ups.get_mut(token)?;
        if u.from != from.fingerprint || u.size != offer.size || u.state == "done" {
            return None;
        }
        u.state = "receiving";
        let done = u.done.clone();
        let path = self.incoming(token);
        let _ = std::fs::create_dir_all(path.parent()?);
        Some(Box::new(Counting {
            inner: PathSink::new(path),
            done,
        }))
    }

    /// An upload's file arrived, or failed.
    pub fn on_received(&self, offer: &FileInfo, result: io::Result<()>) {
        let Some(token) = offer.id.strip_prefix(UPLOAD_PREFIX) else {
            return;
        };
        let outcome = result
            .map_err(|e| e.to_string())
            .and_then(|()| self.upload_arrived(token, &offer.version));
        if let Some(u) = self.uploads.lock_ok().get_mut(token) {
            match outcome {
                Ok((id, existing)) => (u.state, u.item, u.existing) = ("done", Some(id), existing),
                Err(e) => (u.state, u.error) = ("failed", e),
            }
        }
    }
}

/// A library alone on a machine's channel (a program hosting the Site Manager): its
/// requests, files and uploads.
impl Handler for LibraryServer {
    fn on_request(&self, from: &Peer, kind: &str, payload: Vec<u8>) -> Reply {
        self.request(from, kind, &payload).unwrap_or(Reply::Unknown)
    }

    fn file(&self, _from: &Peer, id: &str) -> Option<PathBuf> {
        LibraryServer::file(self, id)
    }

    fn on_push(&self, from: &Peer, offer: &FileInfo) -> Option<Box<dyn FileSink>> {
        LibraryServer::on_push(self, from, offer)
    }

    fn on_received(&self, _from: &Peer, offer: &FileInfo, result: io::Result<()>) {
        LibraryServer::on_received(self, offer, result);
    }
}

/// A sink that counts what it writes (an upload's progress).
struct Counting {
    inner: PathSink,
    done: Arc<AtomicU64>,
}

impl FileSink for Counting {
    fn held(&mut self) -> io::Result<Option<(u64, String)>> {
        self.inner.held()
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
        self.inner.finish(info)
    }
}

/// The background work: the folder looked at again every [`RESCAN_EVERY`], new files
/// hashed (at [`HASH_RATE`]), the trash emptied of what is past its days (hourly).
fn background(lib: std::sync::Weak<LibraryServer>, stop: Arc<AtomicBool>) {
    let mut last_scan = Instant::now();
    let mut last_purge: Option<Instant> = None;
    let mut failed: BTreeMap<String, Instant> = BTreeMap::new();
    while !stop.load(Ordering::Relaxed) {
        let Some(s) = lib.upgrade() else { return };
        if last_scan.elapsed() >= RESCAN_EVERY {
            s.rescan();
            last_scan = Instant::now();
        }
        if last_purge.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
            let mut ix = s.index.lock_ok();
            let before = ix.trash().len();
            ix.purge_trash();
            if ix.trash().len() != before {
                s.saved(&ix);
            }
            last_purge = Some(Instant::now());
        }
        // One file hashed at a time, outside the lock (one that couldn't be read is left
        // a while, the others hashed meanwhile: no spinning on it).
        failed.retain(|_, at: &mut Instant| at.elapsed() < HASH_RETRY);
        let next = s
            .index
            .lock_ok()
            .unhashed()
            .into_iter()
            .find(|(id, _)| !failed.contains_key(id));
        if let Some((id, path)) = next {
            let file = s.index.lock_ok().full(&path).unwrap_or_default();
            let at = std::fs::metadata(&file)
                .ok()
                .map(|m| (m.len(), index::mtime_of(&m)));
            drop(s);
            let sha = at.and_then(|_| index::sha256_of_until(&file, HASH_RATE, &stop).ok());
            let Some(s) = lib.upgrade() else { return };
            match (at, sha) {
                (Some(at), Some(sha)) => {
                    let mut ix = s.index.lock_ok();
                    ix.set_sha(&id, at, sha);
                    s.touched(&ix);
                }
                // Gone (the next look at the folder removes it), out of reach or
                // unreadable: tried again later.
                _ => {
                    failed.insert(id, Instant::now());
                }
            }
            continue;
        }
        // One file probed at a time (a host that reads facts), outside the locks.
        let prober = s.prober.lock_ok().clone();
        if let Some(p) = prober {
            let todo = {
                let facts = s.facts.lock_ok();
                s.index
                    .lock_ok()
                    .items()
                    .find(|e| {
                        facts
                            .get(&e.id)
                            .is_none_or(|(sz, mt, _)| (*sz, *mt) != (e.size, e.mtime))
                    })
                    .map(|e| (e.id.clone(), e.path.clone(), e.size, e.mtime))
            };
            if let Some((id, path, size, mtime)) = todo {
                let file = s.index.lock_ok().full(&path).unwrap_or_default();
                drop(s);
                let f = p(&file).unwrap_or(Value::Null);
                let Some(s) = lib.upgrade() else { return };
                s.facts.lock_ok().insert(id.clone(), (size, mtime, f));
                s.facts_dirty.store(true, Ordering::Relaxed);
                // (Written with the index, at most every SAVE_EVERY.)
                if s.saved_at.lock_ok().elapsed() >= SAVE_EVERY {
                    s.save_facts();
                }
                let mut ix = s.index.lock_ok();
                ix.note_changed(&id);
                s.touched(&ix);
                continue;
            }
        }
        // Nothing to do: what the background changed written now.
        if s.index_dirty.load(Ordering::Relaxed) {
            s.saved(&s.index.lock_ok());
        }
        s.save_facts();
        drop(s);
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's own media taken in (a migration): copied in with its SHA-256 kept; the
    /// same file again is the item already there, nothing copied.
    #[test]
    fn existing_files_are_imported_with_their_sha() {
        let base = idn_common::testing::TempDir::new("lib-import");
        let (dir, old) = (base.join("library"), base.join("host-media"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("Walk in.mp4"), b"some video bytes").unwrap();
        let s = LibraryServer::open(&dir, "MyLibrary").unwrap();
        let sha = index::sha256_of(&old.join("Walk in.mp4"), 0).unwrap();
        let e = s
            .import(&old.join("Walk in.mp4"), "Imported", true)
            .unwrap();
        assert_eq!(
            (e.path.as_str(), e.sha256.as_deref()),
            ("Imported/Walk in.mp4", Some(sha.as_str()))
        );
        assert!(old.join("Walk in.mp4").exists(), "copied, not moved");
        let again = s
            .import(&old.join("Walk in.mp4"), "Elsewhere", false)
            .unwrap();
        assert_eq!(again.id, e.id);
        assert!(!dir.join("Elsewhere").exists());
        assert_eq!(
            s.catalog().item(&e.id).unwrap().path,
            "Imported/Walk in.mp4"
        );
        assert!(s.import(&old.join("x.txt"), "", true).is_err());
        // A host's prober: each item's facts read once, listed in the catalog.
        s.set_prober(Arc::new(|_p: &Path| {
            Some(
                json!({"duration": 12.5, "width": 1920, "height": 1080, "codec": "h264",
                "alpha": false, "audio_tracks": 2}),
            )
        }));
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let it = s.catalog().item(&e.id).cloned().unwrap();
            if it.duration.is_some() {
                assert_eq!(
                    (it.width, it.alpha, it.audio_tracks),
                    (Some(1920), Some(false), Some(2))
                );
                break;
            }
            assert!(Instant::now() < end, "never probed");
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(s);
        // (What the background found is written by the end at the latest.)
        assert!(dir.join(META_DIR).join(FACTS_FILE).is_file());
    }

    /// Bug review 2026-11-22: the folders and settings only for the site's administrators;
    /// a place past the newest answered at once as missed; a move taken in.
    #[test]
    fn admins_only_and_a_place_past_the_newest() {
        let base = idn_common::testing::TempDir::new("lib-admin");
        let (dir, nas) = (base.join("library"), base.join("nas"));
        std::fs::create_dir_all(&nas).unwrap();
        let s = LibraryServer::open(&dir, "MyLibrary").unwrap();
        let peer = |admin| Peer {
            fingerprint: "f".into(),
            name: "Presenter".into(),
            kind: "app".into(),
            admin,
        };
        let ask = |admin, v: Value| s.request(&peer(admin), kinds::ROOTS, v.to_string().as_bytes());
        let add = json!({"add": "NAS", "path": nas.to_string_lossy()});
        assert!(matches!(ask(false, add.clone()), Some(Reply::Error(_))));
        assert!(
            matches!(ask(false, json!({})), Some(Reply::Ok(_))),
            "listing is open"
        );
        assert!(matches!(ask(true, add), Some(Reply::Ok(_))));
        assert!(matches!(
            s.request(&peer(false), kinds::SETTINGS, br#"{"trash_days": 3}"#),
            Some(Reply::Error(_))
        ));
        // A place past the newest (another index, or this one started again): at once.
        let (t, newest) = (Instant::now(), s.index.lock_ok().seq());
        let (_, next, missed) = s.changes(newest + 50, Duration::from_secs(5));
        assert!(missed && t.elapsed() < Duration::from_secs(2));
        assert_eq!(next, newest);
        // Moved in (not kept): the original gone, the item there.
        std::fs::write(base.join("Clip.mp4"), b"moved bytes").unwrap();
        let e = s.import(&base.join("Clip.mp4"), "", false).unwrap();
        assert!(!base.join("Clip.mp4").exists() && dir.join("Clip.mp4").is_file());
        assert_eq!(e.path, "Clip.mp4");
        drop(s);
    }

    /// One instance on a folder at a time: a second open waits for the first's end;
    /// `close` returns once it has ended and saved. The formats
    /// a host's users bring are taken, and a host may add endings.
    #[test]
    fn one_open_at_a_time_and_more_formats() {
        let base = idn_common::testing::TempDir::new("lib-once");
        let dir = base.join("library");
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "a.mpg", "b.m2ts", "c.3gp", "d.ogv", "e.flv", "f.tga", "g.heif", "h.jfif",
        ] {
            std::fs::write(dir.join(f), f.as_bytes()).unwrap();
        }
        let s = LibraryServer::open(&dir, "MyLibrary").unwrap();
        assert_eq!(s.catalog().items.len(), 8);
        // A second while the first is held elsewhere: refused after the wait (shortened
        // here by holding it in another thread that lets go after 300 ms).
        let held = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        s.close().unwrap();
        t.join().unwrap();
        let s = LibraryServer::open(&dir, "MyLibrary").unwrap();
        // A host's own endings.
        assert!(super::super::kind_of("x.r3d").is_none());
        super::super::take_endings(&["R3D"], "video").unwrap();
        assert_eq!(super::super::kind_of("x.r3d"), Some("video"));
        assert!(super::super::take_endings(&["a.b"], "video").is_err());
        assert!(super::super::take_endings(&["x"], "text").is_err());
        drop(s);
    }

    /// Folders an upload or import may name: inside the library only, on every system.
    #[test]
    fn folders_stay_inside() {
        assert_eq!(
            safe_folder("Sunday/Easter").as_deref(),
            Some("Sunday/Easter")
        );
        assert_eq!(safe_folder("").as_deref(), Some(""));
        for bad in [
            "C:/Users/Public",
            "D:x",
            "a/../b",
            ".hidden",
            "a\\b",
            "@NAS/x",
            "/etc",
        ] {
            assert!(
                safe_folder(bad).is_none_or(|f| !f.contains(':') && !f.starts_with('/')),
                "{bad}"
            );
        }
        assert!(safe_folder("C:/x").is_none() && safe_folder("D:x").is_none());
    }

    /// Profile: a large library (20 000 files in 200 folders) looked at, listed, one item
    /// asked for. `cargo test -p idn-open-protocol --release -- --ignored large_library
    /// --nocapture`
    #[test]
    #[ignore]
    fn large_library() {
        let base = idn_common::testing::TempDir::new("lib-large");
        let dir = base.join("library");
        for f in 0..200 {
            let d = dir.join(format!("Folder {f}"));
            std::fs::create_dir_all(&d).unwrap();
            for i in 0..100 {
                std::fs::write(d.join(format!("Clip {i}.mp3")), format!("{f}-{i}")).unwrap();
            }
        }
        let t = Instant::now();
        let mut ix = Index::open(&dir);
        ix.rescan();
        eprintln!("first look: {:?}", t.elapsed());
        let t = Instant::now();
        assert!(!ix.rescan());
        eprintln!("look again, nothing new: {:?}", t.elapsed());
        let t = Instant::now();
        for f in 0..10 {
            let p = format!("Folder {f}/Clip 5.mp3");
            assert!(ix.by_path(&p).is_some());
        }
        eprintln!("by_path x10: {:?}", t.elapsed());
        // The whole folder moved away and back under another name: every item a move.
        std::fs::rename(dir.join("Folder 0"), dir.join("Folder zero")).unwrap();
        let t = Instant::now();
        assert!(ix.rescan());
        eprintln!("a folder of 100 renamed: {:?}", t.elapsed());
        ix.save().unwrap();
        drop(ix);
        let s = LibraryServer::open(&dir, "MyLibrary").unwrap();
        s.stop.store(true, Ordering::Relaxed);
        let t = Instant::now();
        let c = s.catalog();
        eprintln!("catalog ({} items): {:?}", c.items.len(), t.elapsed());
        let t = Instant::now();
        let json = c.to_json().to_string();
        eprintln!("catalog as text ({} bytes): {:?}", json.len(), t.elapsed());
        let t = Instant::now();
        let text = c.to_json_text();
        eprintln!(
            "catalog written straight ({} bytes): {:?}",
            text.len(),
            t.elapsed()
        );
        let id = c.items[c.items.len() / 2].id.clone();
        let peer = Peer {
            fingerprint: "f".into(),
            name: "Presenter".into(),
            kind: "app".into(),
            admin: false,
        };
        let t = Instant::now();
        let r = s.request(&peer, kinds::ITEM, json!({"id": id}).to_string().as_bytes());
        assert!(matches!(r, Some(Reply::Ok(_))));
        eprintln!("one item: {:?}", t.elapsed());
        drop(s);
    }
}
