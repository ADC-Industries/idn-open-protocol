//! What the authority keeps for the Site Manager: alerts, crash reports, log snapshots.

use super::*;

/// A log snapshot: (device, snapshot, its files with their sizes).
pub type LogSnapshot = (String, String, Vec<(String, u64)>);

/// A name safe as one path component: letters, digits, `.`, `_`, `-`, not starting with
/// a dot.
pub(super) fn safe_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() < 160
        && !n.starts_with('.')
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Keeps a Node's crash report under `reports/<device>/` once (the newest 500 a device).
/// The alerts' file in the Site Manager's state (a line each, tab-separated).
pub const ALERTS: &str = "alerts.tsv";

/// The alerts kept in `dir`, newest first (the last 1000).
pub(super) fn read_alerts(dir: &std::path::Path) -> Vec<(u64, String, String, String, String)> {
    read_alert_entries(dir)
        .into_iter()
        .map(|a| (a.at, a.fingerprint, a.app, a.level, a.text))
        .collect()
}

/// One alert kept (idn-node.md §N.18), with the typed event it carries (§N.41).
#[derive(Debug, Clone, PartialEq)]
pub struct AlertEntry {
    /// When (Unix seconds).
    pub at: u64,
    /// The machine's fingerprint ("" for the site).
    pub fingerprint: String,
    /// The program that raised it.
    pub app: String,
    /// `error`, `warning`, `info` or `ok`.
    pub level: String,
    /// What happened, for people.
    pub text: String,
    /// The typed event's name (§N.41), "" for a plain alert.
    pub event: String,
    /// The event's fields, as JSON text ("" for none).
    pub detail: String,
}

/// The alerts kept in `dir`, newest first (the last 1000). A line has five fields, or
/// seven with an event (its name and fields, after the text).
pub(super) fn read_alert_entries(dir: &std::path::Path) -> Vec<AlertEntry> {
    let text = std::fs::read_to_string(dir.join(ALERTS)).unwrap_or_default();
    let mut v: Vec<_> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(7, '\t');
            Some(AlertEntry {
                at: f.next()?.parse().ok()?,
                fingerprint: f.next()?.to_owned(),
                app: f.next()?.to_owned(),
                level: f.next()?.to_owned(),
                text: f.next()?.to_owned(),
                event: f.next().unwrap_or("").to_owned(),
                detail: f.next().unwrap_or("").to_owned(),
            })
        })
        .collect();
    v.reverse();
    v.truncate(1000);
    v
}

/// Keeps an alert (§N.18): appended; past 2000 lines, the newest 1000 kept.
pub(super) fn keep_alert(dir: &std::path::Path, fingerprint: &str, m: &Element) -> io::Result<()> {
    use std::io::Write;
    let clean = |s: &str, n: usize| -> String {
        s.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(n)
            .collect()
    };
    let at = m
        .attr("at")
        .and_then(|a| a.parse::<u64>().ok())
        .unwrap_or(0);
    // A typed event (§N.41): its name and fields after the text (a reader of five fields
    // sees them as part of the text).
    let event = clean(m.attr("event").unwrap_or(""), 64);
    let extra = if event.is_empty() {
        String::new()
    } else {
        let detail = m
            .attr("detail")
            .filter(|d| d.len() <= 2048 && serde_json::from_str::<serde_json::Value>(d).is_ok())
            .unwrap_or("{}");
        format!("\t{event}\t{}", clean(detail, 2048))
    };
    let line = format!(
        "{at}\t{}\t{}\t{}\t{}{extra}\n",
        clean(fingerprint, 128),
        clean(m.attr("app").unwrap_or(""), 64),
        clean(m.attr("level").unwrap_or(""), 16),
        clean(&m.text, 1000)
    );
    let path = dir.join(ALERTS);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(line.as_bytes())?;
    let text = std::fs::read_to_string(&path)?;
    let n = text.lines().count();
    if n > 2000 {
        let keep: Vec<&str> = text.lines().skip(n - 1000).collect();
        std::fs::write(&path, keep.join("\n") + "\n")?;
    }
    Ok(())
}

pub(super) fn keep_report(dir: &std::path::Path, fingerprint: &str, m: &Element) -> io::Result<()> {
    let name = m.attr("name").unwrap_or("");
    if !safe_name(fingerprint) || !safe_name(name) || !name.starts_with("crash-") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a bad name"));
    }
    let d = dir.join("reports").join(fingerprint);
    std::fs::create_dir_all(&d)?;
    let path = d.join(name);
    if path.exists() {
        return Ok(());
    }
    std::fs::write(&path, m.text.as_bytes())?;
    let mut all: Vec<_> = std::fs::read_dir(&d)?.flatten().map(|e| e.path()).collect();
    all.sort();
    let extra = all.len().saturating_sub(500);
    for p in all.into_iter().take(extra) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}
