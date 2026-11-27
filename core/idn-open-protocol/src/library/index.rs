//! The library's index (media library v2, idn-extensions.md §E.12.11, 2026-11-22): every
//! item's lasting id, its place in the folder and its SHA-256; the changes as they happen
//! (so a program that was away catches up from where it was); the items programs hold in
//! use (they can't be deleted); and the trash. Kept in `.library/index.json`.
//!
//! An item keeps its id when it is renamed or moved: a file that appears where one went
//! away, with the same size and time (a move keeps both), or the same SHA-256 once known,
//! is that item. Ids are 16 hex digits, random.

use super::{META_DIR, kind_of, shown};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The index's file in the library's [`META_DIR`].
pub const INDEX_FILE: &str = "index.json";
/// The trash folder in [`META_DIR`]: a deleted item's file, under its id.
pub const TRASH_DIR: &str = "trash";
/// Changes kept for catching up (an older place answers `missed`).
pub const CHANGES_KEPT: usize = 1000;
/// How long a deleted item stays in the trash, unless the library says otherwise
/// ([`Index::set_trash_days`], 1–365).
pub const TRASH_DAYS: u64 = 30;

/// One item as the index keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its lasting id (16 hex digits).
    pub id: String,
    /// Its path in the library, `/` between folders.
    pub path: String,
    /// Bytes.
    pub size: u64,
    /// When the file last changed, Unix seconds.
    pub mtime: u64,
    /// Its SHA-256 (64 hex digits), once worked out (big files take a while; in the
    /// background).
    pub sha256: Option<String>,
    /// When it came into the library, Unix seconds.
    pub added: u64,
}

/// A deleted item waiting in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    /// The item as it was.
    pub entry: Entry,
    /// When it was deleted (Unix seconds).
    pub at: u64,
    /// By whom (a machine's name).
    pub by: String,
}

/// What happened to an item ([`Index::changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Its place (ask from here next).
    pub seq: u64,
    /// `added`, `removed`, `changed` (renamed, moved, rewritten, hashed), or
    /// `removed_in_use`: deleted although held in use, its holders overridden on purpose
    /// ([`Index::delete_in_use`]).
    pub kind: &'static str,
    /// The item's id.
    pub id: String,
}

/// At most this many items marked in use by one holder.
pub const MAX_MARKS: usize = 10_000;
/// At most this many holders of marks.
pub const MAX_HOLDERS: usize = 256;
/// A holder's name: at most this many bytes (as `LibraryServer::marks` has taken since
/// library v2).
pub const MAX_HOLDER_NAME: usize = 64;

/// The index.
#[derive(Debug, Default)]
pub struct Index {
    items: BTreeMap<String, Entry>,
    trash: BTreeMap<String, Trashed>,
    /// Holder → the ids it holds in use.
    marks: BTreeMap<String, BTreeSet<String>>,
    changes: VecDeque<Change>,
    seq: u64,
    dir: PathBuf,
    /// Days a deleted item stays in the trash (0 in a fresh index: [`TRASH_DAYS`]).
    trash_days: u64,
    /// Folders indexed in place (a NAS share), by name: read-only to the library, their
    /// items' paths `@NAME/…`.
    roots: BTreeMap<String, PathBuf>,
    /// The library's folders as last looked at (not kept in the file).
    folders: Vec<String>,
}

/// An indexed folder's name ([`Index::add_root`]): 1–32 letters, digits, `-`, `_`.
fn root_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn new_id() -> String {
    idn_common::hex::hex(&crate::random::secure_random::<8>())
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({"id": self.id, "path": self.path, "size": self.size, "mtime": self.mtime,
            "sha256": self.sha256, "added": self.added})
    }

    fn from_json(v: &Value) -> Option<Entry> {
        Some(Entry {
            id: v["id"].as_str()?.to_owned(),
            path: v["path"].as_str()?.to_owned(),
            size: v["size"].as_u64().unwrap_or(0),
            mtime: v["mtime"].as_u64().unwrap_or(0),
            sha256: v["sha256"].as_str().map(str::to_owned),
            added: v["added"].as_u64().unwrap_or(0),
        })
    }
}

impl Index {
    /// The index of the library at `dir` (empty when there is none yet). One that can't
    /// be read is set aside (said in the log) and the index starts empty: [`Index::load`]
    /// refuses instead.
    pub fn open(dir: &Path) -> Index {
        Index::load(dir).unwrap_or_else(|e| {
            let path = dir.join(META_DIR).join(INDEX_FILE);
            let why = idn_common::fsutil::load_saved(&path, |_| None::<()>)
                .err()
                .unwrap_or_else(|| e.to_string());
            crate::log::log(&format!("library: {why}"));
            Index {
                dir: dir.to_owned(),
                ..Default::default()
            }
        })
    }

    /// The index of the library at `dir` (empty when there is none yet); an error when
    /// its file is there but can't be read (bug run 4: read as empty, every item got a new
    /// id, and marks, the trash and the indexed folders were lost at the next save).
    pub fn load(dir: &Path) -> io::Result<Index> {
        let mut ix = Index {
            dir: dir.to_owned(),
            ..Default::default()
        };
        let b = match std::fs::read(ix.file()) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ix),
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", ix.file().display()),
                ));
            }
        };
        let v = serde_json::from_slice::<Value>(&b).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} can't be read ({e}): put it right, or move it away to start the index afresh",
                    ix.file().display()
                ),
            )
        })?;
        ix.seq = v["seq"].as_u64().unwrap_or(0);
        ix.trash_days = v["trash_days"].as_u64().unwrap_or(0);
        if let Some(r) = v["roots"].as_object() {
            for (name, p) in r {
                if let Some(p) = p.as_str() {
                    ix.roots.insert(name.clone(), PathBuf::from(p));
                }
            }
        }
        for e in v["items"].as_array().into_iter().flatten() {
            if let Some(e) = Entry::from_json(e) {
                ix.items.insert(e.id.clone(), e);
            }
        }
        for t in v["trash"].as_array().into_iter().flatten() {
            if let Some(entry) = Entry::from_json(&t["entry"]) {
                ix.trash.insert(
                    entry.id.clone(),
                    Trashed {
                        entry,
                        at: t["at"].as_u64().unwrap_or(0),
                        by: t["by"].as_str().unwrap_or("").to_owned(),
                    },
                );
            }
        }
        if let Some(m) = v["marks"].as_object() {
            for (holder, ids) in m {
                let set: BTreeSet<String> = ids
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect();
                ix.marks.insert(holder.clone(), set);
            }
        }
        Ok(ix)
    }

    fn file(&self) -> PathBuf {
        self.dir.join(META_DIR).join(INDEX_FILE)
    }

    /// Writes the index (whole, through a temporary file).
    pub fn save(&self) -> io::Result<()> {
        let v = json!({
            "seq": self.seq,
            "trash_days": self.trash_days(),
            "roots": self.roots.iter().map(|(n, p)| (n.clone(), json!(p.to_string_lossy()))).collect::<serde_json::Map<_, _>>(),
            "items": self.items.values().map(Entry::to_json).collect::<Vec<_>>(),
            "trash": self.trash.values().map(|t| json!({"entry": t.entry.to_json(),
                "at": t.at, "by": t.by})).collect::<Vec<_>>(),
            "marks": self.marks.iter().map(|(h, s)| (h.clone(), json!(s))).collect::<serde_json::Map<_, _>>(),
        });
        std::fs::create_dir_all(self.dir.join(META_DIR))?;
        idn_common::fsutil::write_private(&self.file(), v.to_string().as_bytes())
    }

    fn note(&mut self, kind: &'static str, id: &str) {
        self.seq += 1;
        self.changes.push_back(Change {
            seq: self.seq,
            kind,
            id: id.to_owned(),
        });
        while self.changes.len() > CHANGES_KEPT {
            self.changes.pop_front();
        }
    }

    /// Item `id` changed in a way the folder doesn't show (its facts read): told as a
    /// change.
    pub fn note_changed(&mut self, id: &str) {
        if self.items.contains_key(id) {
            self.note("changed", id);
        }
    }

    /// The place of the newest change.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The changes after place `since`, the place now, and whether some after `since`
    /// are no longer kept (or were before this start: read the catalog again).
    pub fn changes(&self, since: u64) -> (Vec<Change>, u64, bool) {
        let oldest = self.changes.front().map_or(self.seq + 1, |c| c.seq);
        // (A place past the newest: another index, or this one started again: missed.)
        let missed = since > self.seq || (since < self.seq && since + 1 < oldest);
        (
            self.changes
                .iter()
                .filter(|c| c.seq > since)
                .cloned()
                .collect(),
            self.seq,
            missed,
        )
    }

    /// Every item.
    pub fn items(&self) -> impl Iterator<Item = &Entry> {
        self.items.values()
    }

    /// The item `id`.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.items.get(id)
    }

    /// The item at `path`.
    pub fn by_path(&self, path: &str) -> Option<&Entry> {
        self.items.values().find(|e| e.path == path)
    }

    /// The item with this SHA-256, if any.
    pub fn by_sha(&self, sha: &str) -> Option<&Entry> {
        self.items
            .values()
            .find(|e| e.sha256.as_deref() == Some(sha))
    }

    /// Looks at the folder again: new files get ids (or the id of the item that went away
    /// with the same size and time: moved or renamed), files gone are removed, rewritten
    /// ones lose their SHA-256 (worked out again). Says whether anything changed.
    pub fn rescan(&mut self) -> bool {
        match look(&self.dir, &self.roots) {
            Some(l) => self.apply(l, false),
            None => false,
        }
    }

    /// What [`look`] found taken in (as [`Index::rescan`]). `recheck`: the look was taken
    /// without the index held, so the index may have changed meanwhile (an upload placed,
    /// an item deleted, a folder indexed or dropped): an item not found is removed only if
    /// its file is gone now, a file found is added only if it is still there.
    pub fn apply(&mut self, look: Look, recheck: bool) -> bool {
        let Look {
            mut found,
            folders,
            unreachable,
        } = look;
        self.folders = folders;
        // (A folder indexed in place no longer: its files not taken in.)
        found.retain(|p, _| {
            !p.starts_with('@')
                || p[1..]
                    .split_once('/')
                    .is_some_and(|(n, _)| self.roots.contains_key(n))
        });
        // An indexed folder out of reach now: its items kept as they were, not removed
        // for a share that is down a moment.
        for e in self.items.values() {
            if unreachable.iter().any(|u| e.path.starts_with(u)) {
                found.insert(e.path.clone(), (e.size, e.mtime));
            }
        }
        let here = |ix: &Index, path: &str| ix.full(path).is_some_and(|f| f.is_file());
        let mut changed = false;
        // Gone: their paths no longer there (kept aside to match a move, by size and time).
        let gone_ids: Vec<String> = self
            .items
            .values()
            .filter(|e| !found.contains_key(&e.path) && !(recheck && here(self, &e.path)))
            .map(|e| e.id.clone())
            .collect();
        let mut gone: HashMap<(u64, u64), Vec<Entry>> = HashMap::new();
        let mut gone_order = Vec::with_capacity(gone_ids.len());
        for id in gone_ids {
            if let Some(e) = self.items.remove(&id) {
                gone_order.push(id);
                gone.entry((e.size, e.mtime)).or_default().push(e);
            }
        }
        let by_path: HashMap<String, String> = self
            .items
            .values()
            .map(|e| (e.path.clone(), e.id.clone()))
            .collect();
        for (path, (size, mtime)) in found {
            if let Some(e) = by_path.get(&path).and_then(|id| self.items.get_mut(id)) {
                if (e.size, e.mtime) != (size, mtime) {
                    (e.size, e.mtime, e.sha256) = (size, mtime, None);
                    let id = e.id.clone();
                    self.note("changed", &id);
                    changed = true;
                }
                continue;
            }
            if recheck && !here(self, &path) {
                continue;
            }
            // A moved or renamed item: one gone with the same size and time.
            let moved = gone
                .get_mut(&(size, mtime))
                .filter(|v| !v.is_empty())
                .map(|v| v.remove(0));
            if let Some(mut e) = moved {
                e.path = path;
                let id = e.id.clone();
                self.items.insert(id.clone(), e);
                self.note("changed", &id);
            } else {
                let id = new_id();
                self.items.insert(
                    id.clone(),
                    Entry {
                        id: id.clone(),
                        path,
                        size,
                        mtime,
                        sha256: None,
                        added: idn_common::epoch::secs(),
                    },
                );
                self.note("added", &id);
            }
            changed = true;
        }
        // The rest gone for good (told in the order they were found gone).
        let left: BTreeSet<String> = gone.into_values().flatten().map(|e| e.id).collect();
        for id in gone_order.into_iter().filter(|id| left.contains(id)) {
            self.note("removed", &id);
            changed = true;
        }
        changed
    }

    /// The library's folders as last looked at ("Sunday", "Backgrounds/Easter"; not the
    /// indexed folders').
    pub fn folders(&self) -> &[String] {
        &self.folders
    }

    /// The library's folder and its indexed folders, for [`look`] taken without the index
    /// held.
    pub fn look_at(&self) -> (PathBuf, BTreeMap<String, PathBuf>) {
        (self.dir.clone(), self.roots.clone())
    }

    /// Items still without a SHA-256: (id, path).
    pub fn unhashed(&self) -> Vec<(String, String)> {
        self.items
            .values()
            .filter(|e| e.sha256.is_none())
            .map(|e| (e.id.clone(), e.path.clone()))
            .collect()
    }

    /// Item `id`'s SHA-256, worked out for the file at size and time `(size, mtime)` (not
    /// taken if the file changed since). Another item with it already: this one is the
    /// same file again (a copy), kept as its own item.
    pub fn set_sha(&mut self, id: &str, at: (u64, u64), sha: String) {
        if let Some(e) = self.items.get_mut(id)
            && (e.size, e.mtime) == at
            && e.sha256.is_none()
        {
            e.sha256 = Some(sha);
            self.note("changed", id);
        }
    }

    /// A file just put in the library (an upload, an import): its item, given its
    /// SHA-256 at once.
    pub fn add(&mut self, path: &str, sha: String) -> io::Result<Entry> {
        let m = std::fs::metadata(self.dir.join(path))?;
        let e = Entry {
            id: new_id(),
            path: path.to_owned(),
            size: m.len(),
            mtime: mtime_of(&m),
            sha256: Some(sha),
            added: idn_common::epoch::secs(),
        };
        self.items.insert(e.id.clone(), e.clone());
        self.note("added", &e.id);
        Ok(e)
    }

    /// Who holds item `id` in use.
    pub fn holders(&self, id: &str) -> Vec<String> {
        self.marks
            .iter()
            .filter(|(_, s)| s.contains(id))
            .map(|(h, _)| h.clone())
            .collect()
    }

    /// `holder`'s marks: `set` replaces them all (re-synced after a restart), `add` and
    /// `remove` change some. The holder's marks now.
    ///
    /// Bounded (bug run 2026-11-24: any machine could grow them without end): only ids of
    /// items here or in the trash are kept, at most [`MAX_MARKS`] a holder and
    /// [`MAX_HOLDERS`] holders.
    pub fn mark(&mut self, holder: &str, how: &str, ids: &[String]) -> Result<usize, String> {
        if holder.is_empty() || holder.len() > MAX_HOLDER_NAME {
            return Err(format!(
                "holder: a name of up to {MAX_HOLDER_NAME} characters"
            ));
        }
        if !self.marks.contains_key(holder) && self.marks.len() >= MAX_HOLDERS && how != "remove" {
            return Err(format!("at most {MAX_HOLDERS} holders"));
        }
        let known: Vec<String> = ids
            .iter()
            .filter(|id| self.items.contains_key(*id) || self.trash.contains_key(*id))
            .cloned()
            .collect();
        // Checked before the holder is made (bug run 2026-11-25: a refused request left an
        // empty holder, and 256 of them shut real holders out).
        let room = {
            let empty = BTreeSet::new();
            let set = self.marks.get(holder).unwrap_or(&empty);
            set.len() + known.iter().filter(|i| !set.contains(*i)).count() <= MAX_MARKS
        };
        match how {
            "set" if known.len() > MAX_MARKS => {
                return Err(format!("at most {MAX_MARKS} marks a holder"));
            }
            "add" if !room => return Err(format!("at most {MAX_MARKS} marks a holder")),
            "set" | "add" | "remove" => {}
            _ => return Err("how: set, add or remove".into()),
        }
        let set = self.marks.entry(holder.to_owned()).or_default();
        match how {
            "set" => *set = known.into_iter().collect(),
            "add" => set.extend(known),
            _ => {
                for id in ids {
                    set.remove(id);
                }
            }
        }
        let n = set.len();
        if n == 0 {
            self.marks.remove(holder);
        }
        Ok(n)
    }

    /// The items `holder` holds in use (by id, in order); none: not a holder.
    pub fn marked_by(&self, holder: &str) -> Vec<String> {
        self.marks
            .get(holder)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The file of library path `path`: in the library's folder, or (`@NAME/…`) in an
    /// indexed folder.
    pub fn full(&self, path: &str) -> Option<PathBuf> {
        match path.strip_prefix('@') {
            Some(rest) => {
                let (name, rel) = rest.split_once('/')?;
                Some(self.roots.get(name)?.join(rel))
            }
            None => Some(self.dir.join(path)),
        }
    }

    /// The folders indexed in place, by name.
    pub fn roots(&self) -> &BTreeMap<String, PathBuf> {
        &self.roots
    }

    /// Indexes the folder `path` (a NAS share, a full path) in place as `name`: its files
    /// get ids and SHA-256s like any item, listed as `@NAME/…`, never moved, uploaded into
    /// or deleted by the library (read-only). Looked at at the next rescan.
    pub fn add_root(&mut self, name: &str, path: &Path) -> Result<(), String> {
        if !root_name_ok(name) {
            return Err("name: 1–32 letters, digits, - or _".into());
        }
        if !path.is_absolute() || !path.is_dir() {
            return Err(format!("{}: not a folder (a full path)", path.display()));
        }
        if path.starts_with(&self.dir) || self.dir.starts_with(path) {
            return Err("an indexed folder can't hold the library or be inside it".into());
        }
        self.roots.insert(name.to_owned(), path.to_owned());
        Ok(())
    }

    /// Stops indexing folder `name`: its items leave the catalog (their files untouched).
    pub fn remove_root(&mut self, name: &str) -> Result<(), String> {
        self.roots.remove(name).ok_or("no such indexed folder")?;
        let prefix = format!("@{name}/");
        let gone: Vec<String> = self
            .items
            .values()
            .filter(|e| e.path.starts_with(&prefix))
            .map(|e| e.id.clone())
            .collect();
        for id in gone {
            self.items.remove(&id);
            self.note("removed", &id);
        }
        Ok(())
    }

    /// Days a deleted item stays in the trash.
    pub fn trash_days(&self) -> u64 {
        if self.trash_days == 0 {
            TRASH_DAYS
        } else {
            self.trash_days
        }
    }

    /// Sets how long deleted items stay in the trash (1–365 days).
    pub fn set_trash_days(&mut self, days: u64) -> Result<(), String> {
        if !(1..=365).contains(&days) {
            return Err("trash days: 1 to 365".into());
        }
        self.trash_days = days;
        Ok(())
    }

    /// Deletes item `id` (by `by`) into the trash: refused while anyone holds it in use
    /// (named in the error; [`delete_in_use`](Self::delete_in_use) overrides that).
    pub fn delete(&mut self, id: &str, by: &str) -> Result<(), String> {
        let holders = self.holders(id);
        if !holders.is_empty() {
            return Err(format!(
                "in use by {} (deleting it anyway needs its name typed back)",
                holders.join(", ")
            ));
        }
        self.move_to_trash(id, by, "removed")
    }

    /// Deletes item `id` although it is held in use (overriding
    /// needs a verification): `confirm` must be the item's file name or its id, typed
    /// back. Into the trash like any delete (restorable, still marked); its holders see
    /// `removed_in_use`. The holders overridden, for the audit.
    pub fn delete_in_use(
        &mut self,
        id: &str,
        by: &str,
        confirm: &str,
    ) -> Result<Vec<String>, String> {
        let e = self.items.get(id).ok_or("no such item")?;
        let name = e.path.rsplit('/').next().unwrap_or(&e.path);
        let typed = confirm.trim();
        if typed.is_empty() || (typed != name && typed != id) {
            return Err(format!(
                "to delete an item in use, type its name ({name}) or id back"
            ));
        }
        let holders = self.holders(id);
        let kind = if holders.is_empty() {
            "removed"
        } else {
            "removed_in_use"
        };
        self.move_to_trash(id, by, kind)?;
        Ok(holders)
    }

    fn move_to_trash(&mut self, id: &str, by: &str, kind: &'static str) -> Result<(), String> {
        let e = self.items.get(id).cloned().ok_or("no such item")?;
        if e.path.starts_with('@') {
            return Err("in an indexed folder (read-only to the library): delete it there".into());
        }
        let into = self.dir.join(META_DIR).join(TRASH_DIR).join(id);
        std::fs::create_dir_all(&into).map_err(|x| x.to_string())?;
        let name = e.path.rsplit('/').next().unwrap_or(&e.path).to_owned();
        std::fs::rename(self.dir.join(&e.path), into.join(name)).map_err(|x| x.to_string())?;
        self.items.remove(id);
        self.trash.insert(
            id.to_owned(),
            Trashed {
                entry: e,
                at: idn_common::epoch::secs(),
                by: by.to_owned(),
            },
        );
        self.note(kind, id);
        Ok(())
    }

    /// The trash, newest first.
    pub fn trash(&self) -> Vec<Trashed> {
        let mut t: Vec<Trashed> = self.trash.values().cloned().collect();
        t.sort_by_key(|x| std::cmp::Reverse(x.at));
        t
    }

    /// Puts item `id` back where it was (beside it, "NAME (2)", if that place is taken),
    /// with its id.
    pub fn restore(&mut self, id: &str) -> Result<Entry, String> {
        let t = self.trash.get(id).cloned().ok_or("not in the trash")?;
        let name = t
            .entry
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&t.entry.path)
            .to_owned();
        let folder = t
            .entry
            .path
            .rsplit_once('/')
            .map_or("", |(f, _)| f)
            .to_owned();
        let from = self.dir.join(META_DIR).join(TRASH_DIR).join(id).join(&name);
        std::fs::create_dir_all(self.dir.join(&folder)).map_err(|x| x.to_string())?;
        let to = free_place(&self.dir.join(&folder), &name);
        std::fs::rename(&from, &to).map_err(|x| x.to_string())?;
        let _ = std::fs::remove_dir(from.parent().unwrap_or(&from));
        let mut e = t.entry;
        e.path = rel(&self.dir, &to).ok_or("restored outside the library")?;
        self.trash.remove(id);
        self.items.insert(id.to_owned(), e.clone());
        self.note("added", id);
        Ok(e)
    }

    /// Trash older than [`trash_days`](Self::trash_days) removed for good.
    pub fn purge_trash(&mut self) {
        let old = idn_common::epoch::secs().saturating_sub(self.trash_days() * 86_400);
        let ids: Vec<String> = self
            .trash
            .iter()
            .filter(|(_, t)| t.at < old)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = std::fs::remove_dir_all(self.dir.join(META_DIR).join(TRASH_DIR).join(&id));
            self.trash.remove(&id);
        }
    }

    /// The bytes the library's items and trash take.
    pub fn bytes(&self) -> u64 {
        self.items.values().map(|e| e.size).sum::<u64>()
            + self.trash.values().map(|t| t.entry.size).sum::<u64>()
    }
}

/// What a look at the library's folders found ([`look`], [`Index::apply`]).
#[derive(Debug, Default)]
pub struct Look {
    /// Path → (size, time) of every media file.
    found: BTreeMap<String, (u64, u64)>,
    /// The library's folders.
    folders: Vec<String>,
    /// Indexed folders out of reach: `@NAME/`.
    unreachable: Vec<String>,
}

/// Looks at the library's folder `dir` and its indexed folders `roots` (no index held:
/// a big folder or a slow share takes a while). None when the library's own folder is out
/// of reach (a drive not mounted yet): nothing changed then, not every item gone (their
/// ids and marks would be lost).
pub fn look(dir: &Path, roots: &BTreeMap<String, PathBuf>) -> Option<Look> {
    if !dir.is_dir() {
        return None;
    }
    let mut l = Look::default();
    walk(dir, dir, &mut l.found, Some(&mut l.folders));
    for (name, root) in roots {
        if !root.is_dir() {
            l.unreachable.push(format!("@{name}/"));
            continue;
        }
        let mut here = BTreeMap::new();
        walk(root, root, &mut here, None);
        l.found
            .extend(here.into_iter().map(|(p, v)| (format!("@{name}/{p}"), v)));
    }
    l.folders.sort();
    Some(l)
}

/// `path` as a library path (`/` between folders), if inside `dir`.
pub fn rel(dir: &Path, path: &Path) -> Option<String> {
    let r = path.strip_prefix(dir).ok()?;
    let parts: Vec<String> = r
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A free name for `name` in `folder`: itself, else "NAME (2).EXT", "NAME (3).EXT"…
pub fn free_place(folder: &Path, name: &str) -> PathBuf {
    let p = folder.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_owned(), format!(".{e}")),
        None => (name.to_owned(), String::new()),
    };
    (2..)
        .map(|n| folder.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("a free name")
}

pub(crate) fn mtime_of(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// The library's media files under `at` (shown names only; not the metadata folder):
/// path → (size, time); its folders into `folders` when given. (Links are not followed.)
fn walk(
    root: &Path,
    at: &Path,
    out: &mut BTreeMap<String, (u64, u64)>,
    mut folders: Option<&mut Vec<String>>,
) {
    let Ok(rd) = std::fs::read_dir(at) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !shown(&name) {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() {
            if let Some(f) = folders.as_deref_mut()
                && let Some(p) = rel(root, &e.path())
            {
                f.push(p);
            }
            walk(root, &e.path(), out, folders.as_deref_mut());
        } else if m.is_file()
            && kind_of(&name).is_some()
            && let Some(p) = rel(root, &e.path())
        {
            out.insert(p, (m.len(), mtime_of(&m)));
        }
    }
}

/// The SHA-256 of the file at `path` (64 hex digits), read at most `bytes_per_s` (0: as
/// fast as it goes): background hashing must not stall a disk others use.
pub fn sha256_of(path: &Path, bytes_per_s: u64) -> io::Result<String> {
    sha256_of_until(
        path,
        bytes_per_s,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

/// [`sha256_of`], given up (`Interrupted`) once `stop` is set: the library stopping
/// doesn't wait for a big file's hash.
pub fn sha256_of_until(
    path: &Path,
    bytes_per_s: u64,
    stop: &std::sync::atomic::AtomicBool,
) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let start = std::time::Instant::now();
    let mut done = 0u64;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as u64;
        if bytes_per_s > 0 {
            let due = std::time::Duration::from_secs_f64(done as f64 / bytes_per_s as f64);
            if let Some(wait) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(wait.min(std::time::Duration::from_millis(200)));
            }
        }
    }
    Ok(idn_common::hex::hex(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(t: &str) -> idn_common::testing::TempDir {
        idn_common::testing::TempDir::new(&format!("lib-index-{t}"))
    }

    #[test]
    fn ids_last_through_renames_and_moves() {
        let d = dir("ids");
        std::fs::write(d.join("a.mp4"), b"aaaa").unwrap();
        std::fs::create_dir_all(d.join("Sunday")).unwrap();
        let mut ix = Index::open(&d);
        assert!(ix.rescan());
        let id = ix.by_path("a.mp4").unwrap().id.clone();
        assert_eq!(id.len(), 16);
        // Moved and renamed: the same item.
        std::fs::rename(d.join("a.mp4"), d.join("Sunday/Welcome.mp4")).unwrap();
        assert!(ix.rescan());
        assert_eq!(ix.by_path("Sunday/Welcome.mp4").unwrap().id, id);
        assert!(ix.by_path("a.mp4").is_none());
        let (ch, next, missed) = ix.changes(0);
        assert!(!missed && next == ix.seq());
        assert_eq!(
            ch.iter().map(|c| c.kind).collect::<Vec<_>>(),
            ["added", "changed"]
        );
        // Saved and opened again: the same.
        ix.save().unwrap();
        let ix2 = Index::open(&d);
        assert_eq!(ix2.get(&id).unwrap().path, "Sunday/Welcome.mp4");
    }

    /// A NAS share indexed in place: its files get ids (`@NAME/…`), read-only; a share
    /// down a moment keeps its items; removed, they leave.
    #[test]
    fn a_folder_is_indexed_in_place() {
        let d = dir("roots");
        let nas = dir("roots-nas");
        std::fs::create_dir_all(nas.join("Videos")).unwrap();
        std::fs::write(nas.join("Videos/Christmas.mp4"), b"xmas").unwrap();
        let mut ix = Index::open(&d);
        assert!(ix.add_root("bad name", &nas).is_err());
        assert!(ix.add_root("NAS", Path::new("relative")).is_err());
        ix.add_root("NAS", &nas).unwrap();
        ix.rescan();
        let e = ix.by_path("@NAS/Videos/Christmas.mp4").unwrap().clone();
        assert_eq!(ix.full(&e.path).unwrap(), nas.join("Videos/Christmas.mp4"));
        let err = ix.delete(&e.id, "x").unwrap_err();
        assert!(err.contains("indexed folder"), "{err}");
        assert!(nas.join("Videos/Christmas.mp4").exists());
        ix.save().unwrap();
        assert_eq!(Index::open(&d).roots().len(), 1);
        // Down a moment: kept.
        std::fs::rename(&nas, nas.with_extension("away")).unwrap();
        ix.rescan();
        assert!(ix.get(&e.id).is_some());
        std::fs::rename(nas.with_extension("away"), &nas).unwrap();
        ix.remove_root("NAS").unwrap();
        assert!(ix.get(&e.id).is_none());
        let _ = std::fs::remove_dir_all(&nas);
    }

    #[test]
    fn marks_protect_and_the_trash_restores() {
        let d = dir("trash");
        std::fs::write(d.join("b.mp3"), b"bbbb").unwrap();
        let mut ix = Index::open(&d);
        ix.rescan();
        let id = ix.by_path("b.mp3").unwrap().id.clone();
        ix.mark("mylibrary", "set", std::slice::from_ref(&id))
            .unwrap();
        // Only ids of items here are kept (bug run 2026-11-24: any id, without end).
        assert_eq!(
            ix.mark("mylibrary", "add", &["made-up".into(), id.clone()])
                .unwrap(),
            1
        );
        assert!(ix.mark("", "add", std::slice::from_ref(&id)).is_err());
        let e = ix.delete(&id, "Presenter").unwrap_err();
        assert!(e.contains("mylibrary"), "{e}");
        // Overriding needs the name (or id) typed back; the holders see it.
        assert!(ix.delete_in_use(&id, "Presenter", "wrong").is_err());
        let held = ix.delete_in_use(&id, "Presenter", "b.mp3").unwrap();
        assert_eq!(held, ["mylibrary"]);
        assert_eq!(ix.changes(0).0.last().unwrap().kind, "removed_in_use");
        ix.restore(&id).unwrap();
        ix.mark("mylibrary", "set", &[]).unwrap();
        ix.delete(&id, "Presenter").unwrap();
        assert!(!d.join("b.mp3").exists() && ix.get(&id).is_none());
        assert_eq!(ix.trash()[0].by, "Presenter");
        let back = ix.restore(&id).unwrap();
        assert_eq!(
            (back.id.as_str(), back.path.as_str()),
            (id.as_str(), "b.mp3")
        );
        assert!(d.join("b.mp3").exists());
        // The trash's length: adjustable, 1–365 days, kept.
        assert_eq!(ix.trash_days(), TRASH_DAYS);
        assert!(ix.set_trash_days(0).is_err() && ix.set_trash_days(400).is_err());
        ix.set_trash_days(7).unwrap();
        ix.save().unwrap();
        assert_eq!(Index::open(&d).trash_days(), 7);
        // The library's folder out of reach (a drive not mounted yet): its items kept.
        let away = d.with_extension("away");
        std::fs::rename(&d, &away).unwrap();
        assert!(!ix.rescan());
        assert!(ix.get(&id).is_some());
        std::fs::rename(&away, &d).unwrap();
        let sha = sha256_of(&d.join("b.mp3"), 0).unwrap();
        assert_eq!(sha.len(), 64);
    }
}
