//! Crash reports and log snapshots, kept and sent to the Site Manager.

use super::*;

impl NodeCore {
    /// Keeps a program's crash report (`name` as the program made it), once.
    pub(super) fn keep_crash(&self, name: &str, app: &str, text: &str) -> io::Result<()> {
        let ok = name.starts_with("crash-")
            && name.ends_with(".txt")
            && name.len() < 120
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        if !ok {
            return Err(idn_common::err::invalid_input("crash: a bad name"));
        }
        let dir = self.crashes_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(&path, text.as_bytes())?;
            self.log(&format!(
                "{app} crashed: report {name} kept for the Site Manager"
            ));
            self.crash_kept.store(true, Ordering::Relaxed);
        }
        // At most the newest 200, by when they were written (by name, the alphabetically
        // first programs' went first, a new report deleted at once; bug run 2026-11-25).
        let mut all: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)?
            .flatten()
            .map(|e| {
                let at = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (at, e.path())
            })
            .collect();
        all.sort();
        let all: Vec<PathBuf> = all.into_iter().map(|(_, p)| p).collect();
        let extra = all
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .count()
            .saturating_sub(200);
        for p in all
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .take(extra)
        {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(p.with_extension("sent"));
        }
        Ok(())
    }

    /// Whether a crash report was kept since the last call.
    pub(super) fn crash_kept(&self) -> bool {
        self.crash_kept.swap(false, Ordering::Relaxed)
    }

    /// Crash reports not yet sent to the Site Manager (`all`: also those sent in the last
    /// day, sent again on a new connection in case the last one dropped them). `sent`:
    /// those sent since the Node started, by name, with when (a report whose `.sent`
    /// couldn't be written is not sent again at each look).
    pub(super) fn crashes_to_send(
        &self,
        all: bool,
        sent: &BTreeMap<String, Instant>,
    ) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.crashes_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                to_send(all, sent.get(name).copied(), &p.with_extension("sent"))
            })
            .collect();
        v.sort();
        v.truncate(super::background::CRASHES_AT_ONCE);
        v
    }

    /// This machine's logs for the Site Manager (§N.16): the Node's own and each program's
    /// that told it where its log is, the newest part of each (256 KB; 600 KB in all,
    /// under the trust messages' 1 MiB).
    pub(super) fn log_snapshot(&self) -> Vec<Element> {
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        let own = idn_open_protocol::diag::current();
        if let Some((app, path)) = own.clone() {
            files.push((app, path));
        }
        // Checked again now, as they are (the file or its folder made a link to the
        // Node's key since it was named: read with the Node's rights, deep bug run
        // 2026-11-09).
        files.extend(
            self.log_files()
                .into_iter()
                .filter(|(_, p)| self.still_a_program_log(p)),
        );
        let mut left: u64 = 600 << 10;
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (app, path) in files.into_iter().rev() {
            if !seen.insert(path.clone()) || left == 0 {
                continue;
            }
            // On Windows a program's log only with its account known (the Node's own
            // apart).
            let mine = own.as_ref().is_some_and(|(_, p)| *p == path);
            if cfg!(windows) && !mine && self.log_owner(&path).is_none() {
                continue;
            }
            let e = Element::new("log")
                .with_attr("app", &app)
                .with_attr("path", &path.display().to_string());
            out.push(
                match idn_open_protocol::diag::tail_owned(
                    &path,
                    left.min(256 << 10),
                    self.log_owner(&path).as_ref(),
                ) {
                    Ok((text, cut)) => {
                        left = left.saturating_sub(text.len() as u64);
                        e.with_attr("cut", if cut { "true" } else { "false" })
                            .with_text(&text)
                    }
                    Err(err) => e.with_attr("error", &err.to_string()),
                },
            );
        }
        out
    }
}

/// A day: reports sent within it are sent again on a new connection.
const RESEND_WITHIN: Duration = Duration::from_secs(86_400);

/// Whether a report is to be sent: never sent (no `.sent` file, not sent since the Node
/// started); with `all`, also one sent within the last day.
fn to_send(all: bool, sent_here: Option<Instant>, sent_file: &Path) -> bool {
    let file = || std::fs::metadata(sent_file).and_then(|m| m.modified());
    if !all {
        return sent_here.is_none() && !sent_file.exists();
    }
    match sent_here {
        Some(t) => t.elapsed() < RESEND_WITHIN,
        None => match file() {
            Ok(t) => t.elapsed().is_ok_and(|d| d < RESEND_WITHIN),
            Err(_) => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report sent since the start is not sent again at the next look, even with its
    /// `.sent` file missing (it couldn't be written); on a new connection it is, within
    /// the day.
    #[test]
    fn a_report_sent_is_not_sent_at_each_look() {
        let dir = std::env::temp_dir().join(format!("idn-crash-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sent_file = dir.join("crash-x-20261127-101010-1.sent");
        let _ = std::fs::remove_file(&sent_file);
        assert!(to_send(false, None, &sent_file));
        assert!(!to_send(false, Some(Instant::now()), &sent_file));
        assert!(to_send(true, Some(Instant::now()), &sent_file));
        std::fs::write(&sent_file, b"").unwrap();
        assert!(!to_send(false, None, &sent_file));
        assert!(to_send(true, None, &sent_file));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
