//! The Node's log and the programs' log files and crash reports it keeps track of.

use super::*;
use idn_open_protocol::diag::Owner;

impl NodeCore {
    /// A line for the log (the panel shows the last 200; also on standard error, for the
    /// service manager's journal).
    pub fn log(&self, m: &str) {
        // One line: names read from the network are in some (bug run N1-6: a device named
        // with a line break wrote lines of its own into the log).
        let m: String = m
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let line = format!("{} {m}", utc_now());
        eprintln!("{line}");
        let mut l = self.log.lock_ok();
        if l.len() >= 200 {
            l.pop_front();
        }
        l.push_back(line);
    }

    /// The last log lines.
    pub fn log_lines(&self) -> Vec<String> {
        self.log.lock_ok().iter().cloned().collect()
    }

    /// The programs with a log this Node knows (§N.42, the Launcher's per-service logs):
    /// each program's newest log file, by its name.
    pub fn program_logs(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for (app, p) in self.log_files().into_iter().rev() {
            if !out.iter().any(|(a, _)| *a == app) && self.still_a_program_log(&p) {
                out.push((app, p));
            }
        }
        out.sort();
        out
    }

    /// The last `lines` lines (at most 2000, of the file's last 1 MB) of a program's log
    /// (`program_logs`); `None`: no log for it.
    pub fn program_log_tail(&self, app: &str, lines: usize) -> Option<Vec<String>> {
        let (_, p) = self.program_logs().into_iter().find(|(a, _)| a == app)?;
        // (Opened as `diag::tail` does: not through a link, a regular file, the program's
        // own; it followed links, and a link swapped in after the checks was read with the
        // Node's rights; bug run 4.)
        // (A cut first line is left out there.)
        let owner = self.log_owner(&p);
        // (On Windows only with its account known.)
        if cfg!(windows) && owner.is_none() {
            return None;
        }
        let (text, _) = idn_open_protocol::diag::tail_owned(&p, 1 << 20, owner.as_ref()).ok()?;
        let mut l: Vec<String> = text.lines().map(str::to_owned).collect();
        let n = lines.clamp(1, 2000);
        Some(l.split_off(l.len().saturating_sub(n)))
    }

    /// The folder of crash reports kept for the Site Manager (§N.16).
    pub(super) fn crashes_dir(&self) -> PathBuf {
        self.state_dir().join("crashes")
    }

    /// The log files programs said they write, `app\tpath` a line (the newest 50; then
    /// `\tuid`, the program's account, where known).
    pub(super) fn log_files(&self) -> Vec<(String, PathBuf)> {
        self.log_files_owned()
            .into_iter()
            .map(|(a, p, _)| (a, p))
            .collect()
    }

    fn log_files_owned(&self) -> Vec<(String, PathBuf, Option<Owner>)> {
        std::fs::read_to_string(self.state_dir().join("log-files.txt"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let (a, p) = (f.next()?, f.next()?);
                Some((
                    a.to_owned(),
                    PathBuf::from(p),
                    f.next().and_then(Owner::parse),
                ))
            })
            .collect()
    }

    /// The account a registered log belongs to, where known (a number on Unix, SIDs on
    /// Windows).
    pub(super) fn log_owner(&self, p: &Path) -> Option<Owner> {
        self.log_files_owned()
            .into_iter()
            .rev()
            .find(|(_, q, _)| q == p)
            .and_then(|(_, _, u)| u)
    }

    /// `uid`: the asking program's account, where known: the file must be its own (any
    /// user's log was read with the Node's rights and sent to the Site Manager; bug round
    /// F, F6-11).
    pub(super) fn remember_log_file(&self, app: &str, path: &str, owner: Option<Owner>) {
        // A program's own log only: `…/logs/<name>.log` (or `.log.1`), as programs keep
        // theirs (diag::log_dir), never in this Node's state folder. The Node reads it
        // with its own rights and sends it to the Site Manager: any path was a way to read
        // the machine's key, or another user's files.
        let p = Path::new(path);
        let named_as_log = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            let stem = n.strip_suffix(".log").or_else(|| n.strip_suffix(".log.1"));
            stem.is_some_and(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
        });
        let in_logs = p
            .parent()
            .and_then(|d| d.file_name())
            .is_some_and(|d| d == "logs");
        // On Windows a drive's path only (not `\\.\pipe\…`, a device or another machine's
        // share: a pipe named like a log hung the reads, a share made the machine
        // authenticate to it; bug run 2026-11-25).
        #[cfg(windows)]
        let on_disk = matches!(
            p.components().next(),
            Some(std::path::Component::Prefix(x))
                if matches!(x.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))
        );
        #[cfg(not(windows))]
        let on_disk = true;
        // (One line a log in `log-files.txt`, `APP<tab>PATH`: a tab or line break in
        // either made another entry, past every check here; bug review 3.)
        let plain = |t: &str| !t.chars().any(char::is_control);
        let one_line = plain(app) && app.chars().count() <= 64 && plain(path);
        let refuse = |why: &str| {
            idn_open_protocol::log::log(&format!("a program's log file refused ({why}): {path}"));
        };
        if !one_line || !p.is_absolute() || !on_disk || !named_as_log || !in_logs {
            refuse("not a program's log");
            return;
        }
        // (Its own `logs` folder is logs: programs it runs as itself keep theirs there.)
        // Compared as named and as it really is (on Windows the real path is `\\?\C:\…`).
        let state = std::fs::canonicalize(self.state_dir())
            .unwrap_or_else(|_| self.state_dir().to_path_buf());
        let in_state = |d: &Path| {
            (d.starts_with(self.state_dir()) && d != self.state_dir().join("logs"))
                || (d.starts_with(&state) && d != state.join("logs"))
        };
        let ours = p.parent().is_some_and(in_state)
            || std::fs::canonicalize(p.parent().unwrap_or(p)).is_ok_and(|d| in_state(&d));
        if ours {
            refuse("in the IDN Node's own folder");
            return;
        }
        // There, and its own: on Unix its account (as before: a path not there yet passed,
        // for another user's file made there later); on Windows its SIDs on the file as
        // opened, and refused when the program's account isn't known (any path was read
        // with the Node's rights; bug run 2026-11-25).
        #[cfg(unix)]
        let its_own = owner.as_ref().is_none_or(|o| {
            use std::os::unix::fs::MetadataExt;
            matches!(o, Owner::Uid(u) if std::fs::symlink_metadata(p).is_ok_and(|m| m.uid() == *u))
        });
        #[cfg(windows)]
        let its_own = owner.as_ref().is_some_and(|o| {
            idn_open_protocol::diag::open_log(p).is_ok_and(|f| {
                f.metadata()
                    .is_ok_and(|m| idn_open_protocol::diag::owned_by(&f, &m, o))
            })
        });
        #[cfg(not(any(unix, windows)))]
        let its_own = true;
        if !its_own {
            refuse(if owner.is_some() {
                "not the program's own file"
            } else {
                "the program's account isn't known"
            });
            return;
        }
        let mut list = self.log_files_owned();
        list.retain(|(_, p, _)| p != Path::new(path));
        list.push((app.to_owned(), PathBuf::from(path), owner));
        let extra = list.len().saturating_sub(50);
        list.drain(..extra);
        let text: String = list
            .iter()
            .map(|(a, p, u)| match u {
                Some(u) => format!("{a}\t{}\t{}\n", p.display(), u.to_text()),
                None => format!("{a}\t{}\n", p.display()),
            })
            .collect();
        let _ = idn_common::fsutil::write_atomic(
            &self.state_dir().join("log-files.txt"),
            text.as_bytes(),
        );
    }

    /// A registered log as it is now: a plain file, not a link, in a `logs` folder that
    /// isn't (and doesn't lead into) this Node's state, its own logs apart.
    pub(super) fn still_a_program_log(&self, p: &Path) -> bool {
        let plain = std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file());
        let Ok(real) = std::fs::canonicalize(p) else {
            return false;
        };
        let state = std::fs::canonicalize(self.state_dir())
            .unwrap_or_else(|_| self.state_dir().to_path_buf());
        let own_logs = state.join("logs");
        let dir_ok = real.parent().is_some_and(|d| {
            d.file_name().is_some_and(|n| n == "logs") && (!d.starts_with(&state) || d == own_logs)
        });
        plain && dir_ok
    }

    /// The name a connected program at `path` gave, for the rule's label.
    pub(super) fn app_of(&self, path: &str) -> String {
        self.clients
            .lock_ok()
            .values()
            .find(|c| c.exe == path)
            .map(|c| c.app.clone())
            .unwrap_or_default()
    }
}
