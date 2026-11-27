//! A show box's local copies of library items (media library v2: IDN
//! moves the data around and provides the cache helper). Live output plays only from
//! local disk, so the program says what the box must keep (`pin`: a set of item ids,
//! replacing the last; scheduled media, what is live or about to be) and asks for others
//! when needed (`want`). The cache fetches them from the library (resumed, checked), says
//! each item's state (cached, fetching with progress, waiting, missing, failed), fetches a
//! copy again when the library's item changed (its SHA-256 or size), and makes room when
//! over its cap by removing unpinned copies, least recently used first. Kept in
//! `cache.json` in its folder.

use super::{Catalog, Library};
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How often the library is asked what changed ([`Library::changes`]; the items that did
/// are read again).
pub const CHANGES_EVERY: Duration = Duration::from_secs(5);
/// How often the whole catalog is read again from a library that doesn't tell its changes
/// (before v2).
pub const CATALOG_EVERY: Duration = Duration::from_secs(30);
/// At most this many changed items are asked for one by one; more: the catalog read whole.
const CHANGES_ONE_BY_ONE: usize = 64;
/// The cache's own file in its folder.
const CACHE_FILE: &str = "cache.json";

/// Where an item stands in the cache ([`Cache::state`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ItemState {
    /// Here, ready to play: its file.
    Cached(PathBuf),
    /// Being copied: bytes so far and in all.
    Fetching {
        /// Bytes copied.
        done: u64,
        /// Bytes in all.
        total: u64,
    },
    /// Wanted or pinned, waiting its turn (or asked for and not yet looked for in the
    /// library: [`Cache::want`] and [`Cache::pin`] make the cache look at once).
    Waiting,
    /// Not in the library (deleted, or not there yet).
    Missing,
    /// The last copy failed (it is tried again).
    Failed(String),
    /// Neither pinned, wanted nor here.
    NotKept,
}

/// A copy kept.
#[derive(Debug, Clone)]
struct Copy {
    file: String,
    size: u64,
    sha256: Option<String>,
    /// When last used (Unix seconds): the oldest unpinned go first.
    used: u64,
}

#[derive(Default)]
struct State {
    copies: BTreeMap<String, Copy>,
    pinned: BTreeSet<String>,
    wanted: VecDeque<String>,
    fetching: Option<(String, u64, u64)>,
    failed: BTreeMap<String, (String, Instant)>,
    catalog: Option<Catalog>,
    /// When the library was last asked (its changes), and when the catalog was last
    /// read whole.
    catalog_at: Option<Instant>,
    whole_at: Option<Instant>,
    /// Items asked for (wanted or pinned) the catalog didn't have, by when they were
    /// asked (`asks`): `Waiting`, not `Missing`, until the library has been asked since.
    unseen: BTreeMap<String, u64>,
    asks: u64,
    /// The library to be asked at once (an item asked for the catalog doesn't have).
    look_now: bool,
}

impl State {
    /// Items just asked for: those the catalog doesn't have are looked for at once.
    fn asked<'a>(&mut self, ids: impl IntoIterator<Item = &'a String>) {
        self.asks += 1;
        for id in ids {
            if self.catalog.as_ref().is_some_and(|c| c.item(id).is_none()) {
                self.unseen.insert(id.clone(), self.asks);
                self.look_now = true;
            }
        }
    }
}

/// A show box's cache of library items; dropping it stops its work.
pub struct Cache {
    dir: PathBuf,
    cap: std::sync::atomic::AtomicU64,
    lib: Arc<Library>,
    state: Mutex<State>,
    wake: Condvar,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.notify_all();
        // Not waited for when dropped on the worker itself (its last reference: waiting
        // would never end), nor while it copies a file (a big one takes minutes: it
        // returns once the copy ends, finding the cache gone).
        let fetching = self.state.lock_ok().fetching.is_some();
        if let Some(w) = self.worker.lock_ok().take()
            && w.thread().id() != std::thread::current().id()
            && !fetching
        {
            let _ = w.join();
        }
    }
}

impl Cache {
    /// The cache in `dir` (made if missing) of at most `cap` bytes, fetching from `lib`;
    /// the copies already there kept.
    pub fn open(dir: &Path, cap: u64, lib: Arc<Library>) -> io::Result<Arc<Cache>> {
        std::fs::create_dir_all(dir)?;
        let mut st = State::default();
        // (One that can't be read is set aside and said, not written over at once.)
        let saved = idn_common::fsutil::load_saved(&dir.join(CACHE_FILE), |b| {
            serde_json::from_slice::<Value>(b).ok()
        })
        .unwrap_or_else(|why| {
            crate::log::log(&format!("library cache: {why}"));
            None
        });
        if let Some(v) = saved {
            for c in v["copies"].as_array().into_iter().flatten() {
                let (Some(id), Some(file)) = (c["id"].as_str(), c["file"].as_str()) else {
                    continue;
                };
                if dir.join(file).is_file() {
                    st.copies.insert(
                        id.to_owned(),
                        Copy {
                            file: file.to_owned(),
                            size: c["size"].as_u64().unwrap_or(0),
                            sha256: c["sha256"].as_str().map(str::to_owned),
                            used: c["used"].as_u64().unwrap_or(0),
                        },
                    );
                }
            }
            st.pinned = v["pinned"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect();
        }
        // Copies it doesn't know of (its list lost, or a copy cut short) go: kept, they
        // were outside the cap for good (bug run 4). Only its own: an item id (16 hex
        // digits) and an ending, or a partial copy.
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.split('.').next().unwrap_or("");
                let ours = id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit());
                let known = st.copies.values().any(|c| c.file == name);
                if ours && !known && e.file_type().is_ok_and(|t| t.is_file()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let c = Arc::new(Cache {
            dir: dir.to_owned(),
            cap: std::sync::atomic::AtomicU64::new(cap),
            lib,
            state: Mutex::new(st),
            wake: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        });
        let weak = Arc::downgrade(&c);
        let stop = c.stop.clone();
        let w = std::thread::Builder::new()
            .name("idn-library-cache".into())
            .spawn(move || work(weak, stop))?;
        *c.worker.lock_ok() = Some(w);
        Ok(c)
    }

    fn save(&self, st: &State) {
        let v = json!({
            "pinned": st.pinned,
            "copies": st.copies.iter().map(|(id, c)| json!({"id": id, "file": c.file,
                "size": c.size, "sha256": c.sha256, "used": c.used})).collect::<Vec<_>>(),
        });
        if let Err(e) =
            idn_common::fsutil::write_private(&self.dir.join(CACHE_FILE), v.to_string().as_bytes())
        {
            crate::log::log(&format!("library cache: saving: {e}"));
        }
    }

    /// The items this box must keep (replacing the set before): fetched if missing, never
    /// removed to make room.
    pub fn pin(&self, ids: &[String]) {
        let mut st = self.state.lock_ok();
        let new: Vec<String> = ids
            .iter()
            .filter(|id| !st.pinned.contains(*id))
            .cloned()
            .collect();
        st.pinned = ids.iter().cloned().collect();
        st.asked(&new);
        self.save(&st);
        self.wake.notify_all();
    }

    /// Item `id` wanted now (not pinned): fetched before the pinned ones still missing,
    /// then kept while there is room. One the cache hasn't seen in the library yet (just
    /// added) is looked for at once, `Waiting` meanwhile.
    pub fn want(&self, id: &str) {
        let mut st = self.state.lock_ok();
        if !st.copies.contains_key(id) && !st.wanted.iter().any(|w| w == id) {
            st.wanted.push_back(id.to_owned());
            st.asked([&id.to_owned()]);
        }
        self.wake.notify_all();
    }

    /// The cache's size cap in bytes.
    pub fn set_cap(&self, bytes: u64) {
        self.cap.store(bytes, Ordering::Relaxed);
        self.wake.notify_all();
    }

    /// The bytes kept, and the cap.
    pub fn usage(&self) -> (u64, u64) {
        let st = self.state.lock_ok();
        (
            st.copies.values().map(|c| c.size).sum(),
            self.cap.load(Ordering::Relaxed),
        )
    }

    /// The file to play item `id` from, if here (noted as used: kept longer).
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        let mut st = self.state.lock_ok();
        let c = st.copies.get_mut(id)?;
        c.used = idn_common::epoch::secs();
        let p = self.dir.join(&c.file);
        p.is_file().then_some(p)
    }

    /// Where item `id` stands.
    pub fn state(&self, id: &str) -> ItemState {
        let st = self.state.lock_ok();
        state_of(&self.dir, &st, id)
    }

    /// Every pinned, wanted or kept item and where it stands.
    pub fn states(&self) -> BTreeMap<String, ItemState> {
        let st = self.state.lock_ok();
        let ids: BTreeSet<&String> = st
            .pinned
            .iter()
            .chain(st.wanted.iter())
            .chain(st.copies.keys())
            .collect();
        ids.into_iter()
            .map(|id| (id.clone(), state_of(&self.dir, &st, id)))
            .collect()
    }
}

fn state_of(dir: &Path, st: &State, id: &str) -> ItemState {
    if let Some((f, done, total)) = &st.fetching
        && f == id
    {
        return ItemState::Fetching {
            done: *done,
            total: *total,
        };
    }
    if let Some(c) = st.copies.get(id) {
        return ItemState::Cached(dir.join(&c.file));
    }
    if let Some((e, _)) = st.failed.get(id) {
        return ItemState::Failed(e.clone());
    }
    let asked = st.pinned.contains(id) || st.wanted.iter().any(|w| w == id);
    if !asked {
        return ItemState::NotKept;
    }
    match &st.catalog {
        Some(c) if c.item(id).is_none() && !st.unseen.contains_key(id) => ItemState::Missing,
        _ => ItemState::Waiting,
    }
}

/// The cache's work: the catalog read now and then, stale copies dropped, the next item
/// fetched (wanted first, then pinned), room made.
fn work(cache: std::sync::Weak<Cache>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let Some(c) = cache.upgrade() else { return };
        // The library's changes followed (the catalog read whole only at first, when
        // many changed or some were missed, or now and then from a library before v2).
        refresh(&c);
        // The next to fetch: wanted, then pinned, missing here and in the library.
        let next = {
            let mut st = c.state.lock_ok();
            let retry_ok = |st: &State, id: &str| {
                st.failed
                    .get(id)
                    .is_none_or(|(_, at)| at.elapsed() >= Duration::from_secs(30))
            };
            while let Some(w) = st.wanted.front() {
                if st.copies.contains_key(w) {
                    st.wanted.pop_front();
                } else {
                    break;
                }
            }
            let pick = st
                .wanted
                .iter()
                .chain(st.pinned.iter())
                .find(|id| {
                    !st.copies.contains_key(*id)
                        && retry_ok(&st, id)
                        && st.catalog.as_ref().is_none_or(|cat| cat.item(id).is_some())
                })
                .cloned();
            pick.and_then(|id| {
                let it = st.catalog.as_ref().and_then(|cat| cat.item(&id).cloned())?;
                st.fetching = Some((id.clone(), 0, it.size));
                Some((id, it))
            })
        };
        if let Some((id, it)) = next {
            // (The ending from the library's catalog: only a plain one, `.mp4`.)
            let ext = it
                .path
                .rsplit_once('.')
                .map(|(_, e)| e)
                .filter(|e| e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()))
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            // Named by the id when it is a v2 one (16 hex digits), else by a hash of it:
            // a v1 library's ids are paths, and the catalog is another machine's word
            // (bug run 2026-11-24: `../..` wrote outside the cache).
            let stem = if id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                id.clone()
            } else {
                idn_common::hex::hex(&crate::hash::sha256(id.as_bytes())[..8])
            };
            let file = format!("{stem}{ext}");
            let to = c.dir.join(&file);
            let (lib, cache2) = (c.lib.clone(), cache.clone());
            drop(c);
            let r = lib.fetch(&id, &to, &mut |done, total| {
                if let Some(c) = cache2.upgrade() {
                    c.state.lock_ok().fetching = Some((id.clone(), done, total));
                }
            });
            let Some(c) = cache.upgrade() else { return };
            let mut st = c.state.lock_ok();
            st.fetching = None;
            match r {
                Ok(()) => {
                    st.failed.remove(&id);
                    st.wanted.retain(|w| w != &id);
                    st.copies.insert(
                        id.clone(),
                        Copy {
                            file,
                            size: it.size,
                            sha256: it.sha256.clone(),
                            used: idn_common::epoch::secs(),
                        },
                    );
                }
                Err(e) => {
                    st.failed
                        .insert(id.clone(), (e.to_string(), Instant::now()));
                }
            }
            make_room(&c, &mut st);
            c.save(&st);
            continue;
        }
        {
            let mut st = c.state.lock_ok();
            make_room(&c, &mut st);
            let _ = c
                .wake
                .wait_timeout(st, Duration::from_secs(5))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Items changed: each id, and the item now (`None`: gone).
type Changed = Vec<(String, Option<super::Item>)>;

/// The catalog brought up to date, every [`CHANGES_EVERY`]: the library asked what
/// changed since the catalog's place (a few bytes when nothing did), the items that changed
/// asked for one by one; the whole catalog read when there is none yet, when many changed
/// or some were missed, or every [`CATALOG_EVERY`] from a library that doesn't tell its
/// changes (before v2). A copy whose item changed (another SHA-256 or size) goes.
fn refresh(c: &Cache) {
    let (have, at, whole_at, asks, now) = {
        let mut st = c.state.lock_ok();
        let now = std::mem::take(&mut st.look_now);
        (
            st.catalog.as_ref().map(|cat| cat.version),
            st.catalog_at.filter(|_| !now),
            st.whole_at,
            st.asks,
            now,
        )
    };
    if at.is_some_and(|t| t.elapsed() < CHANGES_EVERY) {
        return;
    }
    // (Asked without the state held: the library may be slow to answer.)
    let mut update: Option<(u64, Changed)> = None;
    let mut whole = have.is_none();
    if let Some(since) = have {
        match c.lib.changes(since, Duration::ZERO) {
            Ok((ev, next, false)) if ev.len() <= CHANGES_ONE_BY_ONE => {
                // The last word on each item: gone, or as it is now.
                let mut last: BTreeMap<String, bool> = BTreeMap::new();
                for e in &ev {
                    last.insert(e.id.clone(), e.kind.starts_with("removed"));
                }
                let mut items = Vec::new();
                for (id, removed) in last {
                    if removed {
                        items.push((id, None));
                        continue;
                    }
                    match c.lib.item(&id) {
                        Ok(it) => items.push((id, Some(it))),
                        // (Gone again meanwhile, or the library away: read whole.)
                        Err(_) => {
                            whole = true;
                            break;
                        }
                    }
                }
                if !whole {
                    update = Some((next, items));
                }
            }
            Ok(_) => whole = true,
            // A library before v2 (no changes told), or away: read whole now and then, and
            // at once when asked to look (`want`, `pin`; bug run 2026-11-24).
            Err(_) => whole = now || whole_at.is_none_or(|t| t.elapsed() >= CATALOG_EVERY),
        }
    }
    let fresh = if whole { c.lib.catalog().ok() } else { None };
    let looked = fresh.is_some() || update.is_some();
    let mut st = c.state.lock_ok();
    st.catalog_at = Some(Instant::now());
    // Asked for before this look and still not in the library: missing now.
    if looked {
        st.unseen.retain(|_, at| *at > asks);
    }
    if whole {
        st.whole_at = st.catalog_at;
    }
    match (fresh, update, st.catalog.as_mut()) {
        (Some(cat), _, _) => st.catalog = Some(cat),
        (None, Some((next, items)), Some(cat)) if !items.is_empty() || next != cat.version => {
            for (id, it) in items {
                let at = cat.items.binary_search_by(|i| i.id.as_str().cmp(&id));
                match (at, it) {
                    (Ok(i), Some(it)) => cat.items[i] = it,
                    (Err(i), Some(it)) => cat.items.insert(i, it),
                    (Ok(i), None) => {
                        cat.items.remove(i);
                    }
                    (Err(_), None) => {}
                }
            }
            cat.version = next;
        }
        _ => return,
    }
    let Some(cat) = st.catalog.as_ref() else {
        return;
    };
    let stale: Vec<String> = st
        .copies
        .iter()
        .filter(|(id, cp)| {
            cat.item(id).is_some_and(|it| {
                it.size != cp.size
                    || (it.sha256.is_some() && cp.sha256.is_some() && it.sha256 != cp.sha256)
            })
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        if let Some(cp) = st.copies.remove(&id) {
            let _ = std::fs::remove_file(c.dir.join(&cp.file));
        }
    }
    c.save(&st);
}

/// Over the cap: unpinned, unwanted copies removed, least recently used first.
fn make_room(c: &Cache, st: &mut State) {
    let cap = c.cap.load(Ordering::Relaxed);
    let mut total: u64 = st.copies.values().map(|cp| cp.size).sum();
    if total <= cap {
        return;
    }
    let mut free: Vec<(u64, String)> = st
        .copies
        .iter()
        .filter(|(id, _)| !st.pinned.contains(*id) && !st.wanted.iter().any(|w| w == *id))
        .map(|(id, cp)| (cp.used, id.clone()))
        .collect();
    free.sort();
    for (_, id) in free {
        if total <= cap {
            break;
        }
        if let Some(cp) = st.copies.remove(&id) {
            let _ = std::fs::remove_file(c.dir.join(&cp.file));
            total = total.saturating_sub(cp.size);
        }
    }
}
