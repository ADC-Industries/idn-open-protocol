//! yt-dlp (public domain, github.com/yt-dlp/yt-dlp), run as a program of its own for
//! IDN Data's downloads (idn-data.md §D.6): fetched from its GitHub releases, checked
//! against the release's SHA2-256SUMS, whose signature by yt-dlp's pinned key is checked
//! first ([`sig`]), and checked again weekly (a new file only when the published
//! checksum changed), since sites change and yt-dlp follows within days. A release that
//! fails a check leaves the yt-dlp already here in use (and the Node alerted).
//! A yt-dlp of the system's can be named instead (then it is left as it is).

pub mod sig;

use idn_common::cmd::RunWithin as _;
use idn_common::sync::LockOk as _;
use idn_open_protocol::hash::sha256_hex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// The latest release: it redirects to `…/releases/tag/TAG`, and that release's files are
/// all taken from `…/releases/download/TAG/` (not three `latest` requests, which could
/// straddle a new release; F2-2).
pub const LATEST: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest";
const DOWNLOAD: &str = "https://github.com/yt-dlp/yt-dlp/releases/download/";
/// After a failed check, the next try (not every hour; F2-3).
const RETRY_AFTER: Duration = Duration::from_secs(24 * 3600);
/// How often the release is checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(7 * 24 * 3600);
/// The largest file taken (the builds are about 35 MB).
const MAX_SIZE: usize = 200 << 20;

/// The release file for this system: `yt-dlp.exe`, `yt-dlp_linux`,
/// `yt-dlp_linux_aarch64` or `yt-dlp_macos`.
pub fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        ("macos", _) => Some("yt-dlp_macos"),
        _ => None,
    }
}

/// The checksum `SHA2-256SUMS` gives `file` (lines `HEX  NAME`).
pub fn sum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, name) = l.trim().split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == file && hash.len() == 64)
            .then(|| hash.to_ascii_lowercase())
    })
}

fn now() -> u64 {
    idn_common::epoch::secs()
}

/// How yt-dlp is: its program, version, when it was last checked, and the last error.
#[derive(Debug, Clone, Default)]
pub struct ToolState {
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    /// Unix seconds of the last check against the release.
    pub checked: Option<u64>,
    pub error: Option<String>,
    /// What kind of failure (`signature`, `checksum`, `older`, `network`, `in use`,
    /// `file`): the site is alerted once per kind, not per message (F2-3).
    pub error_kind: Option<&'static str>,
    /// Unix seconds of the last failed check.
    pub failed_at: Option<u64>,
    /// Kept by IDN Data (fetched and updated), or the system's (left alone).
    pub managed: bool,
}

/// yt-dlp for this machine.
pub struct YtDlp {
    /// Where IDN Data keeps its own (`tools/`), or the system's program.
    dir: PathBuf,
    system: Option<PathBuf>,
    state: Mutex<ToolState>,
    /// One fetch at a time.
    fetching: Mutex<()>,
}

impl YtDlp {
    /// IDN Data's own, kept in `dir`; or, with `system`, that program (not updated).
    pub fn new(dir: PathBuf, system: Option<PathBuf>) -> YtDlp {
        let managed = system.is_none();
        let path = match &system {
            Some(p) => Some(p.clone()),
            None => asset()
                .map(|_| dir.join(local_name()))
                .filter(|p| p.exists()),
        };
        let checked = std::fs::read_to_string(dir.join("yt-dlp.checked"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let y = YtDlp {
            dir,
            system,
            state: Mutex::new(ToolState {
                path,
                checked,
                managed,
                ..Default::default()
            }),
            fetching: Mutex::new(()),
        };
        y.read_version();
        y
    }

    /// How it is now.
    pub fn state(&self) -> ToolState {
        self.state.lock_ok().clone()
    }

    /// The program, fetched first if IDN Data has none yet.
    pub fn program(&self) -> Result<PathBuf, String> {
        if let Some(p) = self.state.lock_ok().path.clone() {
            return Ok(p);
        }
        self.update()?;
        self.state.lock_ok().path.clone().ok_or("no yt-dlp".into())
    }

    /// Checks the release now if a week has passed (or it was never checked).
    pub fn update_if_due(&self) {
        let due = {
            let s = self.state.lock_ok();
            let t = now();
            // (A time ahead of now, from a clock that was wrong then: due; F2-4.)
            let since = |c: u64| if c > t { u64::MAX } else { t - c };
            s.managed
                && s.path.is_some()
                && s.checked.is_none_or(|c| since(c) >= CHECK_EVERY.as_secs())
                && s.failed_at
                    .is_none_or(|f| since(f) >= RETRY_AFTER.as_secs())
        };
        if due && let Err(e) = self.update() {
            idn_open_protocol::log::log(&format!("idn-data: yt-dlp update: {e}"));
        }
    }

    /// Checks the release now: fetches its file when the published checksum isn't the
    /// one on this machine, and checks what it fetched against it.
    pub fn update(&self) -> Result<(), String> {
        if self.system.is_some() {
            return Err("yt-dlp is the system's (--yt-dlp): update it there".into());
        }
        let _one = self.fetching.lock_ok();
        let r = self.fetch();
        let mut s = self.state.lock_ok();
        s.error = r.as_ref().err().map(|(_, e)| e.clone());
        s.error_kind = r.as_ref().err().map(|(k, _)| *k);
        s.failed_at = r.is_err().then(now);
        drop(s);
        if r.is_ok() {
            self.read_version();
        }
        r.map_err(|(_, e)| e)
    }

    /// The release on GitHub now: its tag (`…/releases/tag/TAG`).
    fn latest_tag(t: Duration) -> Result<String, String> {
        let to = idn_open_protocol::web::httpc::redirect_of(LATEST, t)
            .map_err(|e| e.to_string())?
            .ok_or("no redirect to a release")?;
        let tag = to
            .split_once("/releases/tag/")
            .map(|(_, t)| t.trim_end_matches('/'))
            .ok_or_else(|| format!("not a release: {to}"))?;
        let ok = !tag.is_empty()
            && tag.len() <= 64
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        ok.then(|| tag.to_owned())
            .ok_or_else(|| format!("not a release tag: {tag}"))
    }

    fn fetch(&self) -> Result<(), (&'static str, String)> {
        let file = asset().ok_or(("file", "no yt-dlp build for this system".to_owned()))?;
        let t = Duration::from_secs(30);
        let net = |what: &str, e: String| ("network", format!("{what}: {e}"));
        let tag = Self::latest_tag(t).map_err(|e| net("the latest release", e))?;
        let base = format!("{DOWNLOAD}{tag}/");
        let sums = idn_open_protocol::web::httpc::get(&format!("{base}SHA2-256SUMS"), t)
            .map_err(|e| net("the release's checksums", e.to_string()))?;
        // Signed by yt-dlp's own key (pinned), or nothing of this release is taken.
        let signature = idn_open_protocol::web::httpc::get(&format!("{base}SHA2-256SUMS.sig"), t)
            .map_err(|e| net("the release's signature", e.to_string()))?;
        let signed = sig::verify(&sums, &signature).map_err(|e| {
            (
                "signature",
                format!("the release's checksums aren't signed by yt-dlp's key: {e}"),
            )
        })?;
        // Never an older release than one already taken (or than the one this was written
        // against): a genuine old release, its known faults and all, put up again as the
        // latest is refused (F2-1).
        let floor_file = self.dir.join("yt-dlp.signed");
        let floor = std::fs::read_to_string(&floor_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .max(sig::SIGNED_2026_10_26);
        if signed < floor {
            return Err((
                "older",
                format!(
                    "release {tag} was signed before the one already taken ({signed} < {floor}): an older release is not used"
                ),
            ));
        }
        let want = sum_for(&String::from_utf8_lossy(&sums), file).ok_or((
            "checksum",
            format!("the release's checksums don't list {file}"),
        ))?;
        let path = self.dir.join(local_name());
        let have = std::fs::read(&path).ok().map(|b| sha256_hex(&b));
        if have.as_deref() != Some(want.as_str()) {
            let body =
                idn_open_protocol::web::httpc::get_up_to(&format!("{base}{file}"), t, MAX_SIZE)
                    .map_err(|e| net(file, e.to_string()))?;
            let got = sha256_hex(&body);
            if got != want {
                return Err((
                    "checksum",
                    format!("{file}: its checksum is {got}, the release says {want}: not used"),
                ));
            }
            let io = |e: std::io::Error| ("file", e.to_string());
            std::fs::create_dir_all(&self.dir).map_err(io)?;
            let tmp = self.dir.join(format!("{}.new", local_name()));
            std::fs::write(&tmp, &body).map_err(io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
            }
            // Windows: a running program can't be replaced, but it can be renamed (a
            // download in progress keeps running from the old one; F2-3).
            #[cfg(windows)]
            if path.exists() {
                let old = self.dir.join("yt-dlp.old.exe");
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&path, &old).map_err(|e| ("in use", e.to_string()))?;
            }
            std::fs::rename(&tmp, &path).map_err(io)?;
            idn_open_protocol::log::log(&format!("idn-data: yt-dlp {tag} {file} fetched ({want})"));
        }
        let _ = std::fs::write(&floor_file, signed.to_string());
        let at = now();
        let _ = std::fs::write(self.dir.join("yt-dlp.checked"), at.to_string());
        let mut s = self.state.lock_ok();
        s.path = Some(path);
        s.checked = Some(at);
        Ok(())
    }

    fn read_version(&self) {
        let Some(p) = self.state.lock_ok().path.clone() else {
            return;
        };
        let v = Command::new(&p)
            .args(["--ignore-config", "--version"])
            .output_within(std::time::Duration::from_secs(30))
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
        self.state.lock_ok().version = v;
    }
}

/// IDN Data's own copy's name.
fn local_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

/// The `ffmpeg` program yt-dlp joins video and sound with: next to this program (the
/// Windows installer puts it there), else the system's.
pub fn ffmpeg_program() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let here = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .filter(|p| p.exists());
    here.or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| p.exists())
        })
    })
}

/// Whether `p` is a program that answers `--version` (for checking a system yt-dlp).
pub fn answers(p: &Path) -> bool {
    Command::new(p)
        .args(["--ignore-config", "--version"])
        .output_within(std::time::Duration::from_secs(30))
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// yt-dlp's latest release fetched for real: its signature, checksum and program
    /// (network; `cargo nextest run -p idn-data --run-ignored only`).
    #[test]
    #[ignore = "network: fetches yt-dlp's latest release from GitHub"]
    fn the_latest_release_is_fetched_and_checked() {
        let dir = idn_common::testing::TempDir::new("ytdlp-live");
        let y = YtDlp::new(dir.to_path_buf(), None);
        y.update().unwrap();
        let st = y.state();
        assert!(st.path.as_ref().is_some_and(|p| p.exists()), "{st:?}");
        assert!(st.version.is_some(), "{st:?}");
    }

    #[test]
    fn checksums_are_read_by_file_name() {
        let sums = "\
0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0  yt-dlp
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA  yt-dlp.exe
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *yt-dlp_linux
";
        assert_eq!(sum_for(sums, "yt-dlp.exe").unwrap(), "a".repeat(64));
        assert_eq!(sum_for(sums, "yt-dlp_linux").unwrap(), "b".repeat(64));
        assert!(sum_for(sums, "yt-dlp_macos").is_none());
        assert!(sum_for("short  yt-dlp_linux", "yt-dlp_linux").is_none());
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
