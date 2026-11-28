//! The site's media library, kept by IDN Data on its machine (idn-data.md §D.8, roadmap
//! R.23). The library is the Node's media folder with the library on (idn-node.md
//! §N.26); the Node serves it to the site's programs. Here: what is known about each
//! item (length, size in pixels, codec through `ffprobe`; a thumbnail through `ffmpeg`;
//! titles and tags people give) in `.library/library.json`; the downloads programs ask
//! for (`.library/requests/`), done into the library and their state written back; and
//! the panel's changes (titles, tags, folders, uploads, deletes).

use crate::downloads::{Downloads, JobState, folder_in};
use idn_common::backoff::Backoff;
use idn_common::cmd::RunWithin as _;
use idn_common::sync::LockOk as _;
use idn_open_protocol::library::{self as lib, META_DIR, META_FILE};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A program next to this one (the Windows installer puts FFmpeg's there), else the
/// system's.
fn program(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(&file)))
        .filter(|p| p.exists())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|d| d.join(&file))
                    .find(|p| p.exists())
            })
        })
}

thread_local! {
    /// The last `probe` on this thread ran out of time.
    static PROBE_TIMED_OUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What `ffprobe` says of a file: duration, width, height, codec (the video's, else the
/// sound's: an MP3 had none), and whether it has a picture (a sound file's
/// cover).
pub fn probe(path: &Path) -> Option<Value> {
    PROBE_TIMED_OUT.with(|t| t.set(false));
    let out = Command::new(program("ffprobe")?)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output_within(std::time::Duration::from_secs(30));
    // Out of time (a slow share, a disk spinning up): asked again at the next refresh,
    // not kept as an item without details.
    if out
        .as_ref()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
    {
        PROBE_TIMED_OUT.with(|t| t.set(true));
    }
    let out = out.ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let stream = |kind: &str| {
        v["streams"]
            .as_array()
            .and_then(|a| a.iter().find(|s| s["codec_type"] == kind).cloned())
    };
    let (video, audio) = (stream("video"), stream("audio"));
    // A sound file's cover is a picture, not video: its size is not the item's.
    let cover = video
        .as_ref()
        .is_some_and(|v| v["disposition"]["attached_pic"] == 1);
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0);
    let shown = video.as_ref().filter(|_| !cover);
    // An alpha channel (ProRes 4444, VP9 with alpha, PNG): a pixel format with one.
    let alpha = shown.map(|v| {
        let f = v["pix_fmt"].as_str().unwrap_or("");
        ["yuva", "rgba", "bgra", "argb", "abgr", "gbrap", "ya"]
            .iter()
            .any(|a| f.starts_with(a))
    });
    let audio_tracks = v["streams"].as_array().map_or(0, |a| {
        a.iter().filter(|s| s["codec_type"] == "audio").count()
    });
    Some(json!({
        "duration": duration,
        "width": shown.map(|v| v["width"].clone()), "height": shown.map(|v| v["height"].clone()),
        "codec": shown.or(audio.as_ref()).map(|s| s["codec_name"].clone()),
        "picture": video.is_some(), "alpha": alpha, "audio_tracks": audio_tracks,
    }))
}

/// A thumbnail of a video or picture (320 pixels wide, JPEG) at `to`.
pub fn thumbnail(path: &Path, kind: &str, duration: Option<f64>, to: &Path) -> bool {
    let Some(ff) = program("ffmpeg") else {
        return false;
    };
    if std::fs::create_dir_all(to.parent().unwrap_or(Path::new("."))).is_err() {
        return false;
    }
    let mut c = Command::new(ff);
    c.args(["-v", "error", "-y"]);
    if kind == "video" {
        // A tenth of the way in (not the black first picture), at most a minute.
        let at = duration.map_or(1.0, |d| (d / 10.0).min(60.0));
        c.arg("-ss").arg(format!("{at:.2}"));
    }
    c.arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf", "scale=320:-2", "-q:v", "5"])
        .arg(to)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // (30 s at most, as the probe: one file that hangs ffmpeg held every download
        // request behind it; bug round E, E4-5.)
        .status_within(std::time::Duration::from_secs(30))
        .is_ok_and(|s| s.success())
        && to.exists()
}

/// Whether a request's state is its last: done, failed or cancelled.
fn finished(state: &Value) -> bool {
    matches!(
        state["state"].as_str(),
        Some("done" | "failed" | "cancelled")
    )
}

/// The library at `dir`, kept by IDN Data.
pub struct Library {
    dir: PathBuf,
    downloads: Arc<Downloads>,
    /// One writer of `library.json` at a time.
    writing: Mutex<()>,
    /// Requests being downloaded: request id → download job.
    jobs: Mutex<BTreeMap<String, u64>>,
    /// Requests seen finished (their `N.state` done, failed or cancelled): not read
    /// again each second.
    finished: Mutex<HashSet<String>>,
    /// Items whose `ffprobe` ran out of time: not probed again until their wait is over
    /// (growing while it keeps running out), or their file changes.
    probe_waits: Mutex<HashMap<String, ProbeWait>>,
    stop: Arc<AtomicBool>,
}

/// An item whose probe ran out of time: the file it was (size, time), when to try again,
/// and the waits.
struct ProbeWait {
    file: (u64, u64),
    next: Instant,
    waits: Backoff,
}

/// The waits before an item whose probe ran out of time is probed again: the next
/// refresh (20 s), then longer each time, up to 10 minutes.
fn probe_backoff() -> Backoff {
    Backoff::new(Duration::from_secs(20), Duration::from_secs(600))
}

impl Library {
    pub fn new(dir: PathBuf, downloads: Arc<Downloads>) -> Arc<Library> {
        Arc::new(Library {
            dir,
            downloads,
            writing: Mutex::new(()),
            jobs: Mutex::new(BTreeMap::new()),
            finished: Mutex::new(HashSet::new()),
            probe_waits: Mutex::new(HashMap::new()),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Keeps it going: metadata for new and changed items every 20 s, programs'
    /// download requests every second.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || {
            let mut k = 0u32;
            while !me.stop.load(Ordering::Relaxed) {
                me.requests();
                if k.is_multiple_of(20) {
                    me.refresh();
                }
                k += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join(META_DIR).join(META_FILE)
    }

    /// Changes the metadata with `f` (on the items' map), written atomically.
    fn edit(&self, f: impl FnOnce(&mut Map<String, Value>)) -> Result<(), String> {
        let _one = self.writing.lock_ok();
        // A file that can't be read is set aside (and said), not read as empty and saved
        // over with every title and tag gone; written to the disk before it takes the
        // old one's place (a power cut left it empty; bug round E, E4-12).
        let mut items: Map<String, Value> =
            match idn_common::fsutil::load_saved(&self.meta_path(), |b| {
                serde_json::from_slice::<Value>(b).ok()?["items"]
                    .as_object()
                    .cloned()
            }) {
                Ok(m) => m.unwrap_or_default(),
                Err(e) => {
                    idn_open_protocol::log::log(&format!("library: {e}"));
                    Map::new()
                }
            };
        f(&mut items);
        let d = self.dir.join(META_DIR);
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        idn_common::fsutil::write_atomic(
            &self.meta_path(),
            json!({"items": items}).to_string().as_bytes(),
        )
        .map_err(|e| e.to_string())
    }

    /// What is known of each item made new where its file changed: `ffprobe`, a
    /// thumbnail; titles and tags kept. Items gone are forgotten.
    pub fn refresh(&self) {
        let cat = lib::scan(&self.dir, "");
        let known = lib::read_meta(&self.dir);
        let null = Value::Null;
        let mut fresh: Vec<(String, Value)> = Vec::new();
        for it in &cat.items {
            let k = known.get(&it.id).unwrap_or(&null);
            if k["size"].as_u64() == Some(it.size) && k["modified"].as_u64() == Some(it.modified) {
                continue;
            }
            let path = self.dir.join(&it.id);
            let p = if it.kind == "sound" || it.kind == "video" || it.kind == "picture" {
                let file = (it.size, it.modified);
                // Its probe ran out of time lately, the file as it was: not yet again.
                if self
                    .probe_waits
                    .lock_ok()
                    .get(&it.id)
                    .is_some_and(|w| w.file == file && Instant::now() < w.next)
                {
                    continue;
                }
                let p = probe(&path);
                if p.is_none() && PROBE_TIMED_OUT.with(|t| t.get()) {
                    let mut waits = self.probe_waits.lock_ok();
                    let w = waits.entry(it.id.clone()).or_insert_with(|| ProbeWait {
                        file,
                        next: Instant::now(),
                        waits: probe_backoff(),
                    });
                    if w.file != file {
                        w.file = file;
                        w.waits.reset();
                    }
                    w.next = Instant::now() + w.waits.next();
                    continue;
                }
                self.probe_waits.lock_ok().remove(&it.id);
                p.unwrap_or_default()
            } else {
                Value::Null
            };
            // A sound file's thumbnail is its cover, when it has one.
            let thumb = (it.kind != "sound" || p["picture"] == true)
                && thumbnail(
                    &path,
                    &it.kind,
                    p["duration"].as_f64(),
                    &lib::thumb_file(&self.dir, &it.id),
                );
            fresh.push((
                it.id.clone(),
                json!({
                    "size": it.size, "modified": it.modified,
                    "duration": p["duration"], "width": p["width"], "height": p["height"],
                    "codec": p["codec"], "thumb": thumb, "alpha": p["alpha"],
                    "audio_tracks": p["audio_tracks"],
                }),
            ));
        }
        let ids: HashSet<String> = cat.items.iter().map(|i| i.id.clone()).collect();
        self.probe_waits.lock_ok().retain(|id, _| ids.contains(id));
        let gone = known.keys().any(|k| !ids.contains(k));
        if fresh.is_empty() && !gone {
            return;
        }
        // What is gone judged now, not by the scan before the slow work: titles and tags
        // set meanwhile on a new or moved item were deleted, and a moved item's old id
        // came back (bug round E, E4-4).
        let dir = self.dir.clone();
        let _ = self.edit(|items| {
            items.retain(|k, _| ids.contains(k) || dir.join(k.as_str()).exists());
            for (id, v) in fresh {
                if !dir.join(id.as_str()).exists() {
                    continue;
                }
                let e = items.entry(id).or_insert_with(|| json!({}));
                // A title and tags given by people stay.
                if let (Some(o), Some(n)) = (e.as_object_mut(), v.as_object()) {
                    for (k, x) in n {
                        o.insert(k.clone(), x.clone());
                    }
                }
            }
        });
    }

    /// The downloads programs asked for (`requests/N.json` without `N.state`): started
    /// into the library; their state written to `N.state` as they go.
    fn requests(&self) {
        let rdir = self.dir.join(META_DIR).join("requests");
        let Ok(rd) = std::fs::read_dir(&rdir) else {
            return;
        };
        // The requests there now (a finished one whose files went is forgotten).
        let mut present = HashSet::new();
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            present.insert(id.to_owned());
            if self.finished.lock_ok().contains(id) {
                continue;
            }
            let state = rdir.join(format!("{id}.state"));
            let job = self.jobs.lock_ok().get(id).copied();
            let v = match job {
                // Not a job of this run: finished (left as it is), or cut short by a
                // restart — said so, not "downloading 40%" for ever (bug round E, E4-6).
                None if state.exists() => {
                    let st: Value = std::fs::read(&state)
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or_default();
                    if finished(&st) {
                        self.finished.lock_ok().insert(id.to_owned());
                        continue;
                    }
                    json!({"state": "failed", "error": "IDN Data restarted during the download: ask again"})
                }
                None => {
                    let req: Value = std::fs::read(e.path())
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or_default();
                    let s = |k: &str| req[k].as_str().unwrap_or("").to_owned();
                    match self.downloads.start_in(
                        &s("url"),
                        &s("choice"),
                        &s("title"),
                        &s("folder"),
                    ) {
                        Ok(j) => {
                            self.jobs.lock_ok().insert(id.to_owned(), j);
                            json!({"state": "queued"})
                        }
                        // As many running as may: it waits its turn (asked again next time).
                        // (No state written: one is an earlier run's, cut short.)
                        Err(x) if x.contains("downloads are running") => continue,
                        Err(x) => json!({"state": "failed", "error": x}),
                    }
                }
                Some(j) => match self.downloads.job_state(j) {
                    Some(JobState::Starting) | None => json!({"state": "queued"}),
                    Some(JobState::Downloading { done, total, .. }) => {
                        json!({"state": "downloading", "done": done, "total": total})
                    }
                    Some(JobState::Finishing) => json!({"state": "finishing"}),
                    Some(JobState::Done { file, size, .. }) => {
                        self.jobs.lock_ok().remove(id);
                        let item = file
                            .strip_prefix(&self.dir)
                            .ok()
                            .map(|p| p.to_string_lossy().replace('\\', "/"));
                        json!({"state": "done", "done": size, "total": size, "item": item})
                    }
                    Some(JobState::Failed(x)) => {
                        self.jobs.lock_ok().remove(id);
                        json!({"state": "failed", "error": x})
                    }
                    Some(JobState::Cancelled) => {
                        self.jobs.lock_ok().remove(id);
                        json!({"state": "cancelled"})
                    }
                },
            };
            // Every second while it downloads: whole, but not synced to the disk each time.
            if idn_common::fsutil::write_quick(&state, v.to_string()).is_ok() && finished(&v) {
                self.finished.lock_ok().insert(id.to_owned());
            }
        }
        self.finished.lock_ok().retain(|id| present.contains(id));
    }

    /// The catalog as the panel shows it.
    pub fn catalog(&self) -> Value {
        lib::scan(&self.dir, "").to_json()
    }

    /// Item `id`'s thumbnail.
    pub fn thumb(&self, id: &str) -> Option<Vec<u8>> {
        lib::item_path(&self.dir, id)?;
        std::fs::read(lib::thumb_file(&self.dir, id)).ok()
    }

    /// Gives item `id` a title (empty: its file's name) and tags.
    pub fn set_item(&self, id: &str, title: &str, tags: &[String]) -> Result<(), String> {
        lib::item_path(&self.dir, id).ok_or("no such item")?;
        let tags: Vec<String> = tags
            .iter()
            .map(|t| t.trim().chars().take(40).collect::<String>())
            .filter(|t| !t.is_empty())
            .take(20)
            .collect();
        let title: String = title.trim().chars().take(200).collect();
        self.edit(|items| {
            let e = items.entry(id.to_owned()).or_insert_with(|| json!({}));
            e["title"] = json!(title);
            e["tags"] = json!(tags);
        })
    }

    /// Deletes item `id` (its file and what is known of it).
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let p = lib::item_path(&self.dir, id).ok_or("no such item")?;
        std::fs::remove_file(&p).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(lib::thumb_file(&self.dir, id));
        self.edit(|items| {
            items.remove(id);
        })
    }

    /// Makes folder `path` ("Backgrounds/Easter").
    pub fn new_folder(&self, path: &str) -> Result<(), String> {
        if path.trim_matches('/').is_empty() {
            return Err("a folder's name".into());
        }
        std::fs::create_dir_all(folder_in(&self.dir, path)?).map_err(|e| e.to_string())
    }

    /// Removes folder `path` if it is empty.
    pub fn remove_folder(&self, path: &str) -> Result<(), String> {
        let p = folder_in(&self.dir, path)?;
        if p == self.dir {
            return Err("a folder's name".into());
        }
        std::fs::remove_dir(&p).map_err(|_| "only an empty folder can be removed".into())
    }

    /// Moves item `id` to `folder` (renaming it beside one of the same name).
    pub fn move_item(&self, id: &str, folder: &str) -> Result<String, String> {
        let from = lib::item_path(&self.dir, id).ok_or("no such item")?;
        let to_dir = folder_in(&self.dir, folder)?;
        std::fs::create_dir_all(&to_dir).map_err(|e| e.to_string())?;
        let name = from
            .file_name()
            .ok_or("no name")?
            .to_string_lossy()
            .into_owned();
        let mut to = to_dir.join(&name);
        let mut k = 2;
        while to.exists() {
            to = to_dir.join(numbered(&name, k));
            k += 1;
        }
        std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
        let new_id = to
            .strip_prefix(&self.dir)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let _ = std::fs::rename(
            lib::thumb_file(&self.dir, id),
            lib::thumb_file(&self.dir, &new_id),
        );
        self.edit(|items| {
            if let Some(v) = items.remove(id) {
                items.insert(new_id.clone(), v);
            }
        })?;
        Ok(new_id)
    }

    /// A piece of an upload (`offset` into the file `name` in `folder`): appended to
    /// its `.part`; the last piece (`last`) makes the file. The new item's id then.
    pub fn upload(
        &self,
        folder: &str,
        name: &str,
        offset: u64,
        data: &[u8],
        last: bool,
    ) -> Result<Option<String>, String> {
        let name = name.rsplit(['/', '\\']).next().unwrap_or("");
        if name.is_empty() || name.starts_with('.') || lib::kind_of(name).is_none() {
            return Err("a video, picture or sound file".into());
        }
        // (Not `C:evil.mp4` or `a.mp4:x` on Windows: outside the library, or a hidden
        // stream; bug round E, E4-9.)
        if !lib::safe_name(name) || name.contains(':') {
            return Err("a file name without : or other special characters".into());
        }
        let dir = folder_in(&self.dir, folder)?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let part = dir.join(format!("{name}.part"));
        let have = std::fs::metadata(&part).map_or(0, |m| m.len());
        if offset == 0 && have > 0 {
            let _ = std::fs::remove_file(&part);
        } else if offset != have {
            return Err(format!(
                "upload: expected the piece at {have}, not {offset}"
            ));
        }
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .and_then(|mut f| f.write_all(data))
            .map_err(|e| e.to_string())?;
        if !last {
            return Ok(None);
        }
        let mut to = dir.join(name);
        let mut k = 2;
        while to.exists() {
            to = dir.join(numbered(name, k));
            k += 1;
        }
        std::fs::rename(&part, &to).map_err(|e| e.to_string())?;
        Ok(Some(
            to.strip_prefix(&self.dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/"),
        ))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `name` numbered `k` for a free place: `clip (2).mp4`, `clip (2)` (no `.` left at the
/// end of a name without an extension: Windows drops it, and the item's id no longer
/// named its file; bug round F, F8-9).
fn numbered(name: &str, k: u32) -> String {
    match name.rsplit_once('.') {
        Some((s, x)) if !s.is_empty() => format!("{s} ({k}).{x}"),
        _ => format!("{name} ({k})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ytdlp::YtDlp;

    /// A file whose probe keeps running out of time is probed at the next refresh, then
    /// less and less often, up to every 10 minutes; a request's last states are known.
    #[test]
    fn probe_waits_grow_and_finished_requests() {
        let mut b = probe_backoff();
        let secs: Vec<u64> = (0..7).map(|_| b.next().as_secs()).collect();
        assert_eq!(secs, [20, 40, 80, 160, 320, 600, 600]);
        for st in ["done", "failed", "cancelled"] {
            assert!(finished(&json!({"state": st})));
        }
        for st in ["queued", "downloading", "finishing"] {
            assert!(!finished(&json!({"state": st})));
        }
        assert!(!finished(&Value::Null));
    }

    #[test]
    fn titles_tags_folders_moves_uploads_and_deletes() {
        let dir = idn_common::testing::TempDir::new("data-lib");
        let dl = Arc::new(Downloads::new(
            Arc::new(YtDlp::new(dir.join(".tools"), None)),
            dir.to_path_buf(),
        ));
        let l = Library::new(dir.to_path_buf(), dl);
        // An upload in two pieces.
        assert_eq!(
            l.upload("Sunday", "Welcome.mp4", 0, b"abc", false).unwrap(),
            None
        );
        assert!(l.upload("Sunday", "Welcome.mp4", 7, b"x", false).is_err());
        assert_eq!(
            l.upload("Sunday", "Welcome.mp4", 3, b"def", true)
                .unwrap()
                .as_deref(),
            Some("Sunday/Welcome.mp4")
        );
        assert_eq!(
            std::fs::read(dir.join("Sunday/Welcome.mp4")).unwrap(),
            b"abcdef"
        );
        assert!(l.upload("", "notes.txt", 0, b"x", true).is_err());
        assert!(l.upload("../x", "a.mp4", 0, b"x", true).is_err());
        // A title and tags; kept across a refresh.
        l.set_item(
            "Sunday/Welcome.mp4",
            "Welcome!",
            &["intro".into(), " ".into()],
        )
        .unwrap();
        l.refresh();
        let c = lib::scan(&dir, "");
        let w = c.item("Sunday/Welcome.mp4").unwrap();
        assert_eq!(
            (w.title.as_str(), w.tags.clone()),
            ("Welcome!", vec!["intro".to_string()])
        );
        // Moved: the title and tags go with it.
        l.new_folder("Archive").unwrap();
        let id = l.move_item("Sunday/Welcome.mp4", "Archive").unwrap();
        assert_eq!(id, "Archive/Welcome.mp4");
        let c = lib::scan(&dir, "");
        assert_eq!(c.item("Archive/Welcome.mp4").unwrap().title, "Welcome!");
        assert!(l.remove_folder("Archive").is_err(), "not empty");
        l.delete("Archive/Welcome.mp4").unwrap();
        l.remove_folder("Archive").unwrap();
        assert!(lib::scan(&dir, "").items.is_empty());
    }

    /// A program's download request (§D.8), with a stand-in for yt-dlp: picked up,
    /// downloaded into the folder asked for, its state written back with the item.
    #[cfg(unix)]
    #[test]
    fn a_programs_download_request_is_done_into_the_library() {
        use std::os::unix::fs::PermissionsExt;
        let dir = idn_common::testing::TempDir::new("data-req");
        std::fs::create_dir_all(dir.join(".library/requests")).unwrap();
        let fake = dir.join(".fake-yt-dlp");
        std::fs::write(
            &fake,
            "#!/bin/sh\nout=''\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=\"$2\"; shift; done\n\
             [ \"$out\" = '' ] && echo 2026.1.1 && exit 0\n\
             d=$(dirname \"$out\"); mkdir -p \"$d\"; printf video > \"$d/Song [x].mp4\"\n\
             echo 'IDNPROG 5 5 NA NA NA'; echo \"IDNFILE $d/Song [x].mp4\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dl = Arc::new(Downloads::new(
            Arc::new(YtDlp::new(dir.join(".tools"), Some(fake))),
            dir.to_path_buf(),
        ));
        let l = Library::new(dir.to_path_buf(), dl);
        std::fs::write(
            dir.join(".library/requests/7.json"),
            json!({"url": "https://example.com/v", "choice": "1080p", "title": "Song", "folder": "Sunday"})
                .to_string(),
        )
        .unwrap();
        let state = dir.join(".library/requests/7.state");
        let mut v = Value::Null;
        idn_common::testing::wait_until(Duration::from_secs(5), || {
            l.requests();
            v = std::fs::read(&state)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            v["state"] == "done" || v["state"] == "failed"
        });
        assert_eq!(v["state"], "done", "{v}");
        assert_eq!(v["item"], "Sunday/Song [x].mp4");
        assert_eq!(
            std::fs::read(dir.join("Sunday/Song [x].mp4")).unwrap(),
            b"video"
        );
        // Done once: not started again.
        l.requests();
        assert_eq!(lib::scan(&dir, "").items.len(), 1);
    }
}
