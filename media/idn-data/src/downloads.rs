//! Video downloads (idn-data.md §D.6, roadmap R.22): a link's formats read first and
//! offered in plain words, then the chosen one fetched by yt-dlp ([`crate::ytdlp`]) into
//! the media folder as an MP4 (video and sound joined by FFmpeg), with its progress.

use idn_common::sync::LockOk as _;
use idn_common::cmd::RunWithin as _;
use crate::ytdlp::{YtDlp, ffmpeg_program};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The ready-made choices: id, what the panel calls it.
pub const PRESETS: [(&str, &str); 4] = [
    (
        "1080p",
        "1080p (H.264 where there is one: plays on every machine)",
    ),
    ("4k", "Best up to 4K"),
    ("best", "Best available"),
    ("audio", "Sound only (M4A)"),
];

/// What a copy-protected video's lookup or download says.
const DRM: &str = "this video is copy-protected (DRM): it can't be downloaded";

/// The file's name: the video's title and id, then the picture size, frame rate, HDR and
/// codec of what was chosen ("Easter [aqz-KE-bpKQ] 2160p60 VP9.mp4"; sound alone, or a
/// file's own address, just "TITLE [ID]"), so two choices never share a file and the same
/// choice again finds the one it made. The codec and HDR are named plainly by
/// [`NAMES`] first.
const FILE_NAME: &str = "%(title).150B [%(id)s]%(height& {}p|)s%(fps&{:.0f}|)s%(dynamic_range& {}|)s%(vcodec& {}|)s.%(ext)s";

/// yt-dlp's `--replace-in-metadata` for [`FILE_NAME`], after the format is chosen
/// ("video"): codec ids as plain names, nothing for SDR or no picture.
const NAMES: [(&str, &str, &str); 7] = [
    ("video:vcodec", "^(avc|h264).*", "H.264"),
    ("video:vcodec", "^(hev|hvc|h265).*", "HEVC"),
    ("video:vcodec", "^vp0?9.*", "VP9"),
    ("video:vcodec", "^av0?1.*", "AV1"),
    ("video:vcodec", "^none$", ""),
    ("video:dynamic_range", "^SDR$", ""),
    ("video:dynamic_range", "^HDR.*", "HDR"),
];

/// The finished downloads and jobs kept in the list, at most.
const MAX_JOBS: usize = 100;

/// A codec's plain name and whether machines decode it in hardware (`hw`: "every",
/// "most" or "newer").
pub fn codec(vcodec: &str) -> (&'static str, &'static str) {
    let v = vcodec.to_ascii_lowercase();
    if v.starts_with("avc") || v.starts_with("h264") {
        ("H.264", "every")
    } else if v.starts_with("hev") || v.starts_with("hvc") || v.starts_with("h265") {
        ("HEVC", "most")
    } else if v.starts_with("vp09") || v.starts_with("vp9") {
        ("VP9", "most")
    } else if v.starts_with("av01") || v.starts_with("av1") {
        ("AV1", "newer")
    } else {
        ("other", "most")
    }
}

fn size_text(b: f64) -> String {
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else {
        format!("{:.0} MB", (b / 1e6).max(1.0))
    }
}

/// yt-dlp's `-J` description of a link, as the panel offers it: title, uploader, length,
/// thumbnail, the presets, and each picture size worth choosing (one a resolution, frame
/// rate and codec: the largest), highest first, with its size including the best sound.
pub fn formats_of(info: &Value) -> Value {
    let all = info["formats"].as_array().cloned().unwrap_or_default();
    // Copy-protected formats can't be downloaded: not offered. yt-dlp may say so only for
    // the whole video (`_has_drm`, every format's own `has_drm` false: a Vimeo player
    // link).
    let whole = info["_has_drm"] == true;
    let fmts: Vec<Value> = all
        .iter()
        .filter(|f| !whole && f["has_drm"] != true)
        .cloned()
        .collect();
    let duration = info["duration"].as_f64();
    // The site's size (known), else the bitrate (kbit/s) times the length: an estimate,
    // high for a stream's (HLS) whose bitrate is its peak (2.2 GB said, a
    // 1.36 GB file).
    let known = |f: &Value| {
        f["filesize"]
            .as_f64()
            .or_else(|| f["filesize_approx"].as_f64())
    };
    let size = |f: &Value| known(f).or_else(|| Some(f["tbr"].as_f64()? * 125.0 * duration?));
    let hls = |f: &Value| {
        f["protocol"]
            .as_str()
            .is_some_and(|p| p.starts_with("m3u8"))
    };
    let audio = fmts
        .iter()
        .filter(|f| f["vcodec"] == "none" && f["acodec"] != "none")
        .filter_map(size)
        .fold(0.0, f64::max);
    // The rank within a choice: frame rate, a size the site gives, not a stream, bitrate.
    type Rank = (i64, bool, bool, f64);
    let mut best: BTreeMap<(i64, i64, &'static str, bool), (Rank, Value)> = BTreeMap::new();
    for f in &fmts {
        let v = f["vcodec"].as_str().unwrap_or("none");
        let h = f["height"].as_i64().unwrap_or(0);
        if v == "none" || h == 0 {
            continue;
        }
        let fps = f["fps"].as_f64().unwrap_or(30.0).round() as i64;
        let (name, hw) = codec(v);
        let with_sound = f["acodec"].as_str().is_some_and(|a| a != "none");
        let bytes = size(f).map(|s| s + if with_sound { 0.0 } else { audio });
        let estimate = known(f).is_none();
        let tbr = f["tbr"].as_f64().unwrap_or(0.0);
        let hdr = f["dynamic_range"].as_str().is_some_and(|d| d != "SDR");
        let label = format!(
            "{h}p{} · {name}{}{}",
            if fps > 30 {
                fps.to_string()
            } else {
                String::new()
            },
            if hdr { " HDR" } else { "" },
            bytes
                .map(|b| format!(" · {}{}", if estimate { "~" } else { "" }, size_text(b)))
                .unwrap_or_default(),
        );
        let entry = json!({
            "id": f["format_id"], "label": label, "height": h, "fps": fps, "codec": name,
            "hw": hw, "hdr": hdr, "size": bytes, "sound": with_sound,
        });
        // Frame rates up to 30 are one choice (the label doesn't tell them apart): the
        // highest of them wins, as the higher bitrate does.
        let key = (-h, -fps.max(30), name, hdr);
        let rank = (fps, !estimate, !hls(f), tbr);
        if best
            .get(&key)
            .is_none_or(|(t, _)| rank.partial_cmp(t) == Some(std::cmp::Ordering::Greater))
        {
            best.insert(key, (rank, entry));
        }
    }
    json!({
        "title": info["title"], "uploader": info["uploader"], "duration": info["duration"],
        "thumbnail": info["thumbnail"], "url": info["webpage_url"],
        "presets": PRESETS.iter().map(|(id, l)| json!({"id": id, "label": l})).collect::<Vec<_>>(),
        "formats": best.into_values().map(|(_, v)| v).collect::<Vec<_>>(),
        "drm": all.len() > fmts.len(),
        "drm_only": !all.is_empty() && fmts.is_empty(),
    })
}

/// yt-dlp's format and sort for a choice: a preset's id, or a format id from
/// [`formats_of`] (with the best sound added when it has none).
pub fn selection(choice: &str) -> Result<Vec<String>, String> {
    let a = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Ok(match choice {
        "1080p" | "" => a(&[
            "-f",
            "bv*[height<=1080]+ba/b[height<=1080]/bv*+ba/b",
            "-S",
            "vcodec:h264,res,acodec:m4a",
        ]),
        "4k" => a(&[
            "-f",
            "bv*[height<=2160]+ba/b[height<=2160]/bv*+ba/b",
            "-S",
            "res,fps,vcodec:h264,acodec:m4a",
        ]),
        "best" => a(&["-f", "bv*+ba/b", "-S", "acodec:m4a"]),
        "audio" => a(&["-f", "ba[acodec^=mp4a]/ba/b", "-x", "--audio-format", "m4a"]),
        id if id.starts_with(|c: char| c.is_ascii_alphanumeric())
            && id.len() <= 40
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) =>
        {
            vec!["-f".into(), format!("{id}+ba[acodec^=mp4a]/{id}+ba/{id}")]
        }
        _ => return Err(format!("choice {choice:?}: a preset or a format id")),
    })
}

/// A download's state.
/// Downloads running at once at most.
const MAX_RUNNING: usize = 4;

impl JobState {
    /// Done, failed or cancelled.
    pub fn finished(&self) -> bool {
        matches!(
            self,
            JobState::Done { .. } | JobState::Failed(_) | JobState::Cancelled
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Starting,
    /// Fetching part `part` (1: the video, 2: the sound), `done` of `total` bytes.
    Downloading {
        part: u32,
        done: u64,
        total: Option<u64>,
        speed: Option<f64>,
        eta: Option<u64>,
    },
    /// Joining video and sound, making the MP4.
    Finishing,
    Done {
        file: PathBuf,
        size: u64,
        /// The same choice had made it before: nothing fetched.
        already: bool,
    },
    Failed(String),
    Cancelled,
}

struct Job {
    id: u64,
    url: String,
    choice: String,
    folder: PathBuf,
    title: String,
    state: JobState,
    started: Instant,
    child: Option<Arc<Mutex<Child>>>,
}

struct Inner {
    tool: Arc<YtDlp>,
    media: PathBuf,
    jobs: Mutex<Vec<Job>>,
    next: AtomicU64,
    version: Mutex<u64>,
    changed: Condvar,
}

/// The downloads, into `media`.
pub struct Downloads {
    inner: Arc<Inner>,
}

impl Downloads {
    pub fn new(tool: Arc<YtDlp>, media: PathBuf) -> Downloads {
        Downloads {
            inner: Arc::new(Inner {
                tool,
                media,
                jobs: Mutex::new(Vec::new()),
                next: AtomicU64::new(1),
                version: Mutex::new(1),
                changed: Condvar::new(),
            }),
        }
    }

    /// The yt-dlp it uses.
    pub fn tool(&self) -> &Arc<YtDlp> {
        &self.inner.tool
    }

    /// The media folder.
    pub fn media(&self) -> &PathBuf {
        &self.inner.media
    }

    /// What a link offers ([`formats_of`]): asks yt-dlp (fetching it the first time).
    pub fn formats(&self, url: &str) -> Result<Value, String> {
        check_url(url)?;
        let prog = self.inner.tool.program()?;
        let out = Command::new(prog)
            // (No config file read: one planted in the folder or the account's config
            // could add `--exec`; bug round E, E4-3.)
            .args(["--ignore-config", "-J", "--no-playlist", "--no-warnings", "--no-colors"])
            .arg(url)
            .stdin(Stdio::null())
            // (A minute at most: a hanging site held the panel's thread for good; E4-10.)
            .output_within(std::time::Duration::from_secs(60))
            .map_err(|e| format!("yt-dlp: {e}"))?;
        if !out.status.success() {
            return Err(last_error(&String::from_utf8_lossy(&out.stderr)));
        }
        let info: Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("yt-dlp's answer: {e}"))?;
        let f = formats_of(&info);
        if f["drm_only"] == true {
            return Err(DRM.into());
        }
        Ok(f)
    }

    /// Starts downloading `url` as `choice` ([`selection`]); its job's id.
    pub fn start(&self, url: &str, choice: &str, title: &str) -> Result<u64, String> {
        self.start_in(url, choice, title, "")
    }

    /// As [`Downloads::start`], into `folder` of the media folder ("Sunday",
    /// "Backgrounds/Easter"; made if new): the library's folders (R.23).
    pub fn start_in(
        &self,
        url: &str,
        choice: &str,
        title: &str,
        folder: &str,
    ) -> Result<u64, String> {
        check_url(url)?;
        let sel = selection(choice)?;
        let out = folder_in(&self.inner.media, folder)?;
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        {
            let mut jobs = self.inner.jobs.lock_ok();
            // Two of the same would write the same file (not yet started counts: it went
            // through before yt-dlp was running; bug round E, E4-7).
            if jobs
                .iter()
                .any(|j| !j.state.finished() && j.url == url && j.choice == choice && j.folder == out)
            {
                return Err("that download is already running".into());
            }
            // A few at once (a program asking for hundreds started hundreds of yt-dlp;
            // E4-8).
            if jobs.iter().filter(|j| !j.state.finished()).count() >= MAX_RUNNING {
                return Err(format!("{MAX_RUNNING} downloads are running: wait for one to end"));
            }
            jobs.push(Job {
                id,
                url: url.to_owned(),
                choice: choice.to_owned(),
                folder: out.clone(),
                title: if title.trim().is_empty() {
                    url.to_owned()
                } else {
                    title.trim().to_owned()
                },
                state: JobState::Starting,
                started: Instant::now(),
                child: None,
            });
            // The oldest finished ones go (not one still starting: it ran on unseen).
            while jobs.len() > MAX_JOBS {
                match jobs.iter().position(|j| j.state.finished()) {
                    Some(k) => {
                        jobs.remove(k);
                    }
                    None => break,
                }
            }
        }
        changed(&self.inner);
        let inner = self.inner.clone();
        let url = url.to_owned();
        std::thread::spawn(move || run(&inner, id, &url, sel, &out));
        Ok(id)
    }

    /// Stops job `id` (its part-file is left for yt-dlp to go on with next time): yt-dlp
    /// and everything it started (the one-file yt-dlp is a starter and the program it
    /// unpacks; FFmpeg), so nothing goes on writing the part-file.
    pub fn cancel(&self, id: u64) -> Result<(), String> {
        let mut jobs = self.inner.jobs.lock_ok();
        let j = jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or("no such download")?;
        if let Some(c) = j.child.take() {
            stop(&mut c.lock_ok());
            j.state = JobState::Cancelled;
        } else if !j.state.finished() {
            // Not started yet (yt-dlp still being fetched): it won't be (it went ahead).
            j.state = JobState::Cancelled;
        }
        drop(jobs);
        changed(&self.inner);
        Ok(())
    }

    /// Job `id`'s state now.
    pub fn job_state(&self, id: u64) -> Option<JobState> {
        self.inner
            .jobs
            .lock_ok()
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.state.clone())
    }

    /// Takes finished job `id` off the list (its file stays).
    pub fn forget(&self, id: u64) -> Result<(), String> {
        let mut jobs = self.inner.jobs.lock_ok();
        let k = jobs
            .iter()
            .position(|j| j.id == id && j.state.finished())
            .ok_or("no such finished download")?;
        jobs.remove(k);
        drop(jobs);
        changed(&self.inner);
        Ok(())
    }

    /// The jobs (newest first), yt-dlp's state and the media folder's files:
    /// `{"downloads": [...], "yt_dlp": {...}, "media": {"folder", "files": [...]}}`.
    pub fn state(&self) -> Value {
        let jobs = self.inner.jobs.lock_ok();
        let list: Vec<Value> = jobs
            .iter()
            .rev()
            .map(|j| {
                let mut v = json!({"id": j.id, "url": j.url, "choice": j.choice, "title": j.title,
                                   "for_s": j.started.elapsed().as_secs()});
                match &j.state {
                    JobState::Starting => v["state"] = json!("starting"),
                    JobState::Downloading {
                        part,
                        done,
                        total,
                        speed,
                        eta,
                    } => {
                        v["state"] = json!("downloading");
                        v["part"] = json!(part);
                        v["done"] = json!(done);
                        v["total"] = json!(total);
                        v["speed"] = json!(speed);
                        v["eta"] = json!(eta);
                    }
                    JobState::Finishing => v["state"] = json!("finishing"),
                    JobState::Done {
                        file,
                        size,
                        already,
                    } => {
                        v["state"] = json!("done");
                        v["already"] = json!(already);
                        v["file"] = json!(file.file_name().map(|f| f.to_string_lossy()));
                        v["size"] = json!(size);
                    }
                    JobState::Failed(e) => {
                        v["state"] = json!("failed");
                        v["error"] = json!(e);
                    }
                    JobState::Cancelled => v["state"] = json!("cancelled"),
                }
                v
            })
            .collect();
        drop(jobs);
        let t = self.inner.tool.state();
        let mut files: Vec<(String, u64)> = std::fs::read_dir(&self.inner.media)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let m = e.metadata().ok()?;
                        (m.is_file() && !name.ends_with(".part") && !name.ends_with(".ytdl"))
                            .then_some((name, m.len()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        json!({
            "downloads": list,
            "yt_dlp": {"path": t.path, "version": t.version, "checked": t.checked,
                       "error": t.error, "managed": t.managed},
            "ffmpeg": ffmpeg_program(),
            "media": {"folder": self.inner.media, "files": files.iter()
                .map(|(n, s)| json!({"name": n, "size": s})).collect::<Vec<_>>()},
        })
    }

    /// The state's version (it goes up with every change).
    pub fn version(&self) -> u64 {
        *self.inner.version.lock_ok()
    }

    /// Waits up to `timeout` for the version to pass `after`.
    pub fn wait_change(&self, after: u64, timeout: Duration) {
        let v = self.inner.version.lock_ok();
        let _ = self
            .inner
            .changed
            .wait_timeout_while(v, timeout, |v| *v <= after);
    }
}

/// `folder` inside `media`: each part a plain name (no `..`, nothing hidden, nothing
/// Windows refuses).
pub fn folder_in(media: &std::path::Path, folder: &str) -> Result<PathBuf, String> {
    let mut p = media.to_path_buf();
    for part in folder.split('/').filter(|x| !x.is_empty()) {
        if part == ".."
            || part.starts_with('.')
            || part.len() > 100
            || part
                .chars()
                .any(|c| c.is_control() || "<>:\"|?*\\".contains(c))
        {
            return Err(format!("folder {folder:?}: plain names, / between them"));
        }
        p.push(part);
    }
    Ok(p)
}

fn check_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://"))
        || u.len() > 2000
        || u.chars().any(char::is_whitespace)
    {
        return Err("a link: an http:// or https:// address".into());
    }
    Ok(())
}

/// yt-dlp's last `ERROR:` line (or its last line), with what it means where that's
/// known.
fn last_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let e = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or(lines.last())
        .map(|l| l.trim_start_matches("ERROR:").trim().to_owned())
        .unwrap_or_else(|| "yt-dlp failed".into());
    if e.contains("DRM protected") {
        format!("{DRM} ({e})")
    } else if e.contains("logged-in") || e.contains("--cookies") || e.contains("Sign in") {
        format!("the site wants a signed-in account, which IDN Data doesn't do yet ({e})")
    } else {
        e
    }
}

/// Stops a yt-dlp and what it started: its process group (Unix), its process tree
/// (Windows).
fn stop(child: &mut Child) {
    let pid = child.id().to_string();
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stderr(Stdio::null())
        .status_within(std::time::Duration::from_secs(10));
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status_within(std::time::Duration::from_secs(10));
    let _ = child.kill();
}

fn changed(inner: &Inner) {
    *inner.version.lock_ok() += 1;
    inner.changed.notify_all();
}

fn set(inner: &Inner, id: u64, st: JobState) {
    if let Some(j) = inner.jobs.lock_ok().iter_mut().find(|j| j.id == id) {
        // A cancelled job stays so.
        if j.state != JobState::Cancelled {
            j.state = st;
        }
    }
    changed(inner);
}

/// Bytes done, the total, its estimate, the speed and the time left.
type Progress = (u64, Option<u64>, Option<f64>, Option<u64>);

/// One progress line (`--progress-template`): `IDNPROG done total estimate speed eta`
/// (`NA` where yt-dlp doesn't know).
fn progress(line: &str) -> Option<Progress> {
    let mut w = line.strip_prefix("IDNPROG ")?.split_whitespace();
    let num = |s: Option<&str>| s.and_then(|x| x.parse::<f64>().ok());
    let done = num(w.next())? as u64;
    let total = num(w.next()).or(num(w.next()));
    let speed = num(w.next());
    let eta = num(w.next()).map(|e| e as u64);
    Some((done, total.map(|t| t as u64), speed, eta))
}

fn run(inner: &Arc<Inner>, id: u64, url: &str, sel: Vec<String>, out: &std::path::Path) {
    let prog = match inner.tool.program() {
        Ok(p) => p,
        Err(e) => return set(inner, id, JobState::Failed(e)),
    };
    if let Err(e) = std::fs::create_dir_all(out) {
        return set(
            inner,
            id,
            JobState::Failed(format!("{}: {e}", out.display())),
        );
    }
    let mut cmd = Command::new(prog);
    cmd.args([
        "--ignore-config",
        "--no-playlist",
        "--newline",
        "--no-colors",
        "--progress",
        "--windows-filenames",
        "--merge-output-format",
        "mp4",
        "--progress-template",
        "download:IDNPROG %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s",
        "--print",
        "after_move:IDNFILE %(filepath)s",
        "-o",
    ])
    .arg(out.join(FILE_NAME))
    .args(&sel);
    for (field, from, to) in NAMES {
        cmd.args(["--replace-in-metadata", field, from, to]);
    }
    // Its own process group, all stopped together ([`stop`]).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    if let Some(ff) = ffmpeg_program() {
        cmd.arg("--ffmpeg-location").arg(ff);
    }
    let child = cmd
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return set(inner, id, JobState::Failed(format!("yt-dlp: {e}"))),
    };
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let child = Arc::new(Mutex::new(child));
    {
        let mut jobs = inner.jobs.lock_ok();
        match jobs.iter_mut().find(|j| j.id == id) {
            // Cancelled (or forgotten) while yt-dlp was starting: stopped at once.
            Some(j) if j.state != JobState::Cancelled => j.child = Some(child.clone()),
            _ => {
                drop(jobs);
                stop(&mut child.lock_ok());
                return;
            }
        }
    }
    let errors = Arc::new(Mutex::new(String::new()));
    if let Some(err) = err {
        let errors = errors.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                let mut e = errors.lock_ok();
                e.push_str(&l);
                e.push('\n');
            }
        });
    }
    let (mut part, mut last_done, mut file) = (0u32, u64::MAX, None::<PathBuf>);
    let mut said = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
    if let Some(out) = out {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some((done, total, speed, eta)) = progress(&l) {
                // A new part starts again from nothing.
                if done < last_done || part == 0 {
                    part += u32::from(done < last_done || part == 0);
                }
                last_done = done;
                let finished = total.is_some_and(|t| done >= t);
                if finished || said.elapsed() >= Duration::from_millis(500) {
                    said = Instant::now();
                    set(
                        inner,
                        id,
                        if finished {
                            JobState::Finishing
                        } else {
                            JobState::Downloading {
                                part,
                                done,
                                total,
                                speed,
                                eta,
                            }
                        },
                    );
                }
            } else if let Some(f) = l.strip_prefix("IDNFILE ") {
                file = Some(PathBuf::from(f.trim()));
            }
        }
    }
    let status = child.lock_ok().wait();
    if let Some(j) = inner.jobs.lock_ok().iter_mut().find(|j| j.id == id) {
        j.child = None;
    }
    let st = match (status, file) {
        (Ok(s), Some(f)) if s.success() => {
            let size = std::fs::metadata(&f).map_or(0, |m| m.len());
            JobState::Done {
                file: f,
                size,
                // No progress at all: the file was there (`--print` keeps yt-dlp from
                // saying so).
                already: part == 0,
            }
        }
        (Ok(s), _) if s.success() => JobState::Failed("yt-dlp named no file".into()),
        _ => JobState::Failed(last_error(&errors.lock_ok())),
    };
    set(inner, id, st);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_offered_plainly() {
        let info = json!({
            "title": "Worship", "uploader": "Church", "duration": 300.0,
            "webpage_url": "https://example.com/v",
            "formats": [
                {"format_id": "140", "vcodec": "none", "acodec": "mp4a.40.2", "filesize": 5_000_000},
                {"format_id": "137", "vcodec": "avc1.640028", "acodec": "none", "height": 1080, "fps": 30, "tbr": 4000.0, "filesize": 100_000_000},
                {"format_id": "248", "vcodec": "vp9", "acodec": "none", "height": 1080, "fps": 30, "tbr": 3000.0, "filesize_approx": 80_000_000},
                {"format_id": "401", "vcodec": "av01.0.12M.08", "acodec": "none", "height": 2160, "fps": 60, "tbr": 20000.0, "filesize": 900_000_000, "dynamic_range": "HDR10"},
                {"format_id": "18", "vcodec": "avc1.42001E", "acodec": "mp4a.40.2", "height": 360, "fps": 30, "tbr": 500.0, "filesize": 20_000_000},
                {"format_id": "136", "vcodec": "avc1.4d401f", "acodec": "none", "height": 720, "fps": 30, "tbr": 1500.0},
                {"format_id": "136b", "vcodec": "avc1.4d401f", "acodec": "none", "height": 720, "fps": 30, "tbr": 2500.0},
                {"format_id": "400", "vcodec": "av01.0.12M.08", "acodec": "none", "height": 2160, "fps": 60, "tbr": 15000.0, "filesize": 600_000_000},
                {"format_id": "602", "vcodec": "vp09.00.10.08", "acodec": "none", "height": 144, "fps": 15, "tbr": 90.0},
                {"format_id": "603", "vcodec": "vp09.00.11.08", "acodec": "none", "height": 144, "fps": 30, "tbr": 80.0},
                {"format_id": "hls-1080", "vcodec": "avc1.640028", "acodec": "mp4a.40.2", "height": 1080, "fps": 30, "tbr": 9000.0, "has_drm": true},
                {"format_id": "hls-1080b", "vcodec": "avc1.640028", "acodec": "mp4a.40.2", "height": 1080, "fps": 30, "tbr": 9000.0, "protocol": "m3u8_native"}
            ]
        });
        let f = formats_of(&info);
        assert_eq!(f["title"], "Worship");
        assert_eq!(f["presets"][0]["id"], "1080p");
        let list = f["formats"].as_array().unwrap();
        let labels: Vec<&str> = list.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(
            labels,
            [
                "2160p60 · AV1 · 605 MB",
                "2160p60 · AV1 HDR · 905 MB",
                "1080p · H.264 · 105 MB",
                "1080p · VP9 · 85 MB",
                "720p · H.264 · ~99 MB",
                "360p · H.264 · 20 MB",
                "144p · VP9 · ~8 MB",
            ]
        );
        // A size the site gives wins over a stream's estimate from its peak bitrate.
        assert_eq!(list[2]["id"], "137");
        assert_eq!(list[0]["hw"], "newer");
        assert_eq!(list[2]["hw"], "every");
        // Of two alike, the higher bitrate; of two up to 30 frames a second, the higher
        // frame rate.
        assert_eq!(list[4]["id"], "136b");
        assert_eq!(list[5]["sound"], true);
        assert_eq!(list[6]["id"], "603");
        // The copy-protected one isn't offered.
        assert_eq!(f["drm"], true);
        assert_eq!(f["drm_only"], false);
        let drm = formats_of(&json!({"formats": [{"format_id": "a", "has_drm": true}]}));
        assert_eq!(drm["drm_only"], true);
        let drm = formats_of(&json!({"_has_drm": true, "formats": [
            {"format_id": "hls-794", "vcodec": "avc1.64001F", "acodec": "none", "height": 360, "has_drm": false}
        ]}));
        assert_eq!(drm["drm_only"], true);
        assert_eq!(drm["formats"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn choices_become_selections() {
        assert_eq!(
            selection("1080p").unwrap()[1],
            "bv*[height<=1080]+ba/b[height<=1080]/bv*+ba/b"
        );
        assert_eq!(
            selection("137").unwrap(),
            ["-f", "137+ba[acodec^=mp4a]/137+ba/137"]
        );
        assert!(selection("137; rm -rf").is_err());
        assert!(selection("--exec").is_err());
        assert_eq!(
            progress("IDNPROG 1024 NA 4096 512.5 3"),
            Some((1024, Some(4096), Some(512.5), Some(3)))
        );
        assert_eq!(
            progress("IDNPROG 10 20 NA NA NA"),
            Some((10, Some(20), None, None))
        );
        assert_eq!(progress("[download] 5%"), None);
        assert_eq!(
            last_error("WARNING: x\nERROR: [youtube] abc: Video unavailable\n"),
            "[youtube] abc: Video unavailable"
        );
        assert!(
            last_error("ERROR: [vimeo] 1: This format is DRM protected").contains("copy-protected")
        );
        assert!(check_url("file:///etc/passwd").is_err() && check_url("https://a b").is_err());
    }
}
