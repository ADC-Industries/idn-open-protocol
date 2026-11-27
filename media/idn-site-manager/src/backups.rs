//! Scheduled site backups (idn-node.md §N.42): the site's backup (§N.17) made by itself
//! every few hours into a folder (by default `backups` in the Site Manager's folder; a
//! network share's mount is the point), the newest kept. The schedule and its passphrase
//! are kept in `backup-schedule.json`, readable by the Site Manager's account only.

use super::*;

/// The schedule's file.
pub(super) const SCHEDULE_FILE: &str = "backup-schedule.json";
/// The default folder (in the Site Manager's folder; left out of the backups themselves).
pub(super) const DEFAULT_FOLDER: &str = "backups";
/// A backup file's name starts and ends so (only those are pruned).
const PREFIX: &str = "site-backup-";
const SUFFIX: &str = ".idnsite";

/// The schedule as kept.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Schedule {
    #[serde(default)]
    pub on: bool,
    #[serde(default = "day")]
    pub every_hours: u32,
    #[serde(default = "week")]
    pub keep: u32,
    /// Empty: `backups` in the Site Manager's folder.
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub passphrase: String,
    /// The last run: when (seconds since 1970), its file, why it failed.
    #[serde(default)]
    pub last_at: u64,
    #[serde(default)]
    pub last_file: String,
    #[serde(default)]
    pub last_error: String,
}

fn day() -> u32 {
    24
}

fn week() -> u32 {
    7
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            on: false,
            every_hours: day(),
            keep: week(),
            folder: String::new(),
            passphrase: String::new(),
            last_at: 0,
            last_file: String::new(),
            last_error: String::new(),
        }
    }
}

/// The schedule saved, or the default (a damaged file set aside and said, not read as
/// "no backups" and saved over).
fn load(dir: &Path) -> Schedule {
    idn_common::fsutil::load_json(&dir.join(SCHEDULE_FILE))
        .unwrap_or_else(|e| {
            idn_open_protocol::log::log(&format!("backups: {e}"));
            None
        })
        .unwrap_or_default()
}

fn store(dir: &Path, s: &Schedule) -> io::Result<()> {
    let b = serde_json::to_vec_pretty(s).map_err(io::Error::other)?;
    idn_common::fsutil::write_private(&dir.join(SCHEDULE_FILE), &b)
}

/// The folder the backups go to.
fn folder_of(dir: &Path, s: &Schedule) -> PathBuf {
    if s.folder.trim().is_empty() {
        dir.join(DEFAULT_FOLDER)
    } else {
        PathBuf::from(s.folder.trim())
    }
}

/// The backup files in `folder`, oldest first: (name, size, modified seconds).
fn files_in(folder: &Path) -> Vec<(String, u64, u64)> {
    let mut l: Vec<(String, u64, u64)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.starts_with(PREFIX) && name.ends_with(SUFFIX)) {
                return None;
            }
            let m = e.metadata().ok()?;
            let at = m
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some((name, m.len(), at))
        })
        .collect();
    l.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
    l
}

impl State {
    /// `backup_schedule {on?, every_hours?, keep?, folder?, passphrase?}` (admins): the
    /// fields given; a passphrase is needed (at least 8 characters) to turn it on.
    pub(super) fn backup_schedule(&self, v: &serde_json::Value) -> Result<(), String> {
        let mut s = load(&self.dir);
        if let Some(h) = v["every_hours"].as_u64() {
            if !(1..=168).contains(&h) {
                return Err("every_hours: 1 to 168".into());
            }
            s.every_hours = h as u32;
        }
        if let Some(k) = v["keep"].as_u64() {
            if !(1..=100).contains(&k) {
                return Err("keep: 1 to 100".into());
            }
            s.keep = k as u32;
        }
        if let Some(f) = v["folder"].as_str() {
            let f = f.trim();
            if !f.is_empty() && !Path::new(f).is_absolute() {
                return Err("folder: a full path (empty: the Site Manager's own)".into());
            }
            // Not inside the Site Manager's folder (but its own `backups`, the default): each
            // backup carried all the earlier ones, until too large to make (bug run
            // 2026-11-25).
            let inside = |p: &Path| {
                let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                canon(p).starts_with(canon(&self.dir))
            };
            if !f.is_empty() && inside(Path::new(f)) {
                return Err(
                    "folder: outside the Site Manager's folder (empty: its own backups folder)"
                        .into(),
                );
            }
            s.folder = f.to_owned();
        }
        if let Some(p) = v["passphrase"].as_str() {
            if p.chars().count() < idn_open_protocol::backup::MIN_PASSPHRASE {
                return Err(format!(
                    "passphrase: at least {} characters",
                    idn_open_protocol::backup::MIN_PASSPHRASE
                ));
            }
            s.passphrase = p.to_owned();
        }
        if let Some(on) = field_bool(v.to_string().as_bytes(), "on") {
            if on && s.passphrase.is_empty() {
                return Err("a passphrase first: the backups are encrypted with it".into());
            }
            s.on = on;
        }
        let folder = folder_of(&self.dir, &s);
        if s.on {
            std::fs::create_dir_all(&folder)
                .map_err(|e| format!("folder {}: {e}", folder.display()))?;
        }
        store(&self.dir, &s).map_err(|e| e.to_string())?;
        self.log(&format!(
            "backups: {} every {} h, keeping {}, in {}",
            if s.on { "on" } else { "off" },
            s.every_hours,
            s.keep,
            folder.display()
        ));
        Ok(())
    }

    /// Makes a backup now into the schedule's folder (with its passphrase), the oldest
    /// beyond `keep` removed: the file's name.
    pub(super) fn backup_now(&self) -> Result<String, String> {
        // One at a time (the tick and the API at once wrote over each other).
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = ONE.lock_ok();
        let s = load(&self.dir);
        if s.passphrase.is_empty() {
            return Err("no passphrase set for the backups (backup_schedule)".into());
        }
        let folder = folder_of(&self.dir, &s);
        let made = (|| -> io::Result<String> {
            std::fs::create_dir_all(&folder)?;
            // A folder inside the Site Manager's own (saved before that was refused) left
            // out too, else each backup carries the earlier ones (bug run 2026-11-25).
            let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            let inner = canon(&folder)
                .strip_prefix(canon(&self.dir))
                .ok()
                .and_then(|r| r.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned());
            let mut skip: Vec<&str> = BACKUP_SKIP.to_vec();
            if let Some(n) = inner.as_deref() {
                skip.push(n);
            }
            let b =
                idn_open_protocol::backup::make(&self.dir, "site-manager", &skip, &s.passphrase)?;
            let t = idn_open_protocol::localtime::now();
            let name = format!(
                "{PREFIX}{}-{:04}{:02}{:02}-{:02}{:02}{:02}{SUFFIX}",
                idn_open_protocol::discovery::hostname().to_lowercase(),
                t.year,
                t.month,
                t.day,
                t.hour,
                t.minute,
                t.second
            );
            let part = folder.join(format!("{name}.part"));
            idn_common::fsutil::write_private(&part, &b)?;
            std::fs::rename(&part, folder.join(&name))?;
            // The oldest beyond `keep` (ours only, by name).
            let l = files_in(&folder);
            for (old, _, _) in l.iter().take(l.len().saturating_sub(s.keep as usize)) {
                let _ = std::fs::remove_file(folder.join(old));
            }
            Ok(name)
        })();
        // The schedule as it is now (changed meanwhile, perhaps: turned off, another
        // folder): only what this backup says is written back (bug run 2026-11-24).
        let mut s = load(&self.dir);
        s.last_at = idn_common::epoch::secs();
        match &made {
            Ok(name) => {
                s.last_file = name.clone();
                s.last_error.clear();
                self.log(&format!("backups: {name} made"));
            }
            Err(e) => {
                s.last_error = e.to_string();
                self.log(&format!("backups: failed: {e}"));
            }
        }
        let _ = store(&self.dir, &s);
        made.map_err(|e| e.to_string())
    }

    /// Once a minute: a backup when one is due.
    pub(super) fn backup_tick(&self) {
        let s = load(&self.dir);
        let now = idn_common::epoch::secs();
        if s.on && now.saturating_sub(s.last_at) >= u64::from(s.every_hours) * 3600 {
            let _ = self.backup_now();
        }
    }

    /// The schedule for the status (never its passphrase) with the files there.
    pub(super) fn backups_state(&self) -> serde_json::Value {
        let s = load(&self.dir);
        let folder = folder_of(&self.dir, &s);
        serde_json::json!({
            "on": s.on,
            "every_hours": s.every_hours,
            "keep": s.keep,
            "folder": folder.display().to_string(),
            "passphrase_set": !s.passphrase.is_empty(),
            "last_at": (s.last_at > 0).then_some(s.last_at),
            "last_file": s.last_file,
            "last_error": s.last_error,
            "files": files_in(&folder)
                .into_iter()
                .rev()
                .map(|(name, size, at)| serde_json::json!({"name": name, "size": size, "at": at}))
                .collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_backup_files_are_listed_oldest_first() {
        let d = idn_common::testing::TempDir::new("idn-sm-backups");
        std::fs::write(d.join("site-backup-a-20261101-000000.idnsite"), b"1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(d.join("site-backup-a-20261102-000000.idnsite"), b"22").unwrap();
        std::fs::write(d.join("notes.txt"), b"x").unwrap();
        let l = files_in(&d);
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].1, 2);
    }

    #[test]
    fn a_damaged_schedule_is_set_aside_not_saved_over() {
        let d = idn_common::testing::TempDir::new("idn-sm-schedule");
        std::fs::write(d.join(SCHEDULE_FILE), b"{\"on\": tru").unwrap();
        assert_eq!(load(&d), Schedule::default());
        assert!(!d.join(SCHEDULE_FILE).exists());
        let aside = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("backup-schedule.json.bad-")
            })
            .count();
        assert_eq!(aside, 1, "set aside as NAME.bad-SECS");
    }
}
