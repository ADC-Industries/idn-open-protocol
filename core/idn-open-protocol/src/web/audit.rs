//! The audit (idn-node.md §N.31.3, §N.31.6): every change, who made it, from where, and
//! whether it was done, one JSON object a line in `audit.jsonl`, oldest first:
//! `{"at", "user", "peer", "client", "action", "ok", "error", "detail"}`. The Site
//! Manager keeps one; so does each engine (the desk, the switcher, IDN Data, the Node's
//! panel) in its state folder.
//!
//! Kept for two years, and past 50,000 lines the newest 40,000. An engine's continuous
//! moves (a fader dragged: one control, one caller, changes under a second apart) are one
//! line with the last value and `"repeats"`, so that a service's fader rides don't push
//! the year's changes out.

use super::auth::Who;
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The audit's file in its folder.
pub const FILE: &str = "audit.jsonl";
/// Trimmed past this many lines, to the newest [`KEEP`].
pub const MAX: usize = 50_000;
pub const KEEP: usize = 40_000;
/// Lines older than this are dropped when trimming (two years).
pub const DAYS: u64 = 2 * 365;
/// The newest lines also kept in memory (the push channel's, and an audit without a
/// folder).
const RECENT: usize = 1000;
/// A continuous move's line is written once its control is still this long…
const QUIET: Duration = Duration::from_secs(1);
/// …or after this long at most.
const LONGEST: Duration = Duration::from_secs(10);

/// Body fields never written to an audit.
pub const SECRET: &[&str] = &[
    "password",
    "passphrase",
    "pin",
    "lan_password",
    "sitemanager_password",
    // (A join code typed in the Node's panel, live for 10 minutes; bug run 4.)
    "code",
    "data",
    "token",
];

/// Whether body field `k` is a secret: one of [`SECRET`], or named as one (`panel_pin`,
/// `admin_code`, `site_password`, `api_token`: the Node panel's simple-mode PIN and admin
/// code were written in clear; bug review 3).
fn secret(k: &str) -> bool {
    SECRET.contains(&k)
        || ["_pin", "password", "passphrase", "_code", "token", "secret"]
            .iter()
            .any(|end| k.ends_with(end))
}

/// `v` with secret fields ([`secret`]) replaced by `…`, strings cut at 200 characters
/// and arrays at 64 items, at any depth.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let x = if secret(k) {
                        Value::String("…".into())
                    } else {
                        redact(x)
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().take(64).map(redact).collect()),
        // An address keeps only its scheme and host: a stream key is in its path
        // (`rtmp://…/live2/KEY`), a camera's password in `user:pass@` or the query (the
        // switcher's audit kept them, readable with a read token; bug run 2026-11-25).
        Value::String(s) if s.contains("://") => Value::String(url_host_only(s)),
        Value::String(s) if s.chars().count() > 200 => {
            Value::String(s.chars().take(200).chain(['…']).collect())
        }
        _ => v.clone(),
    }
}

/// `scheme://host[:port]/…` of an address: no user, password, path or query.
fn url_host_only(s: &str) -> String {
    let Some((scheme, rest)) = s.split_once("://") else {
        return s.to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let more = rest.len() > authority.len() || authority.contains('@');
    let scheme: String = scheme.chars().take(16).collect();
    let host: String = host.chars().take(200).collect();
    format!("{scheme}://{host}{}", if more { "/…" } else { "" })
}

/// Fields that say which control a change is for (compared exactly when lines are
/// joined); other numbers are the values that move.
const IDS: &[&str] = &[
    "n", "id", "index", "channel", "ch", "bus", "group", "matrix", "fx", "dca", "aux", "input",
    "output", "mix", "layer", "overlay", "slot", "band", "item", "to", "from", "scene",
];

/// What makes two changes the same control's: the caller, the action, the body's keys,
/// and every value but the moving numbers.
fn control_key(user: &str, peer: &str, client: &str, action: &str, detail: &Value) -> String {
    let shape = match detail {
        Value::Object(m) => {
            let mut parts: Vec<String> = m
                .iter()
                .map(|(k, v)| match v {
                    Value::Number(_) if !IDS.contains(&k.as_str()) => format!("{k}=#"),
                    v => format!("{k}={v}"),
                })
                .collect();
            parts.sort();
            parts.join("\u{1f}")
        }
        v => v.to_string(),
    };
    format!("{user}\u{1e}{peer}\u{1e}{client}\u{1e}{action}\u{1e}{shape}")
}

struct Pending {
    key: String,
    line: Value,
    first: Instant,
    last: Instant,
    repeats: u64,
}

#[derive(Default)]
struct Inner {
    /// Lines written, counted (the push channel's place).
    seq: u64,
    recent: VecDeque<(u64, Value)>,
    pending: Option<Pending>,
}

/// An audit, in a folder (`audit.jsonl`) or, without one, the newest lines in memory.
pub struct Audit {
    path: Option<PathBuf>,
    /// Whether continuous moves are joined (the engines'; not the Site Manager's).
    join: bool,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Audit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Audit({:?})", self.path)
    }
}

impl Audit {
    /// The audit in `dir` (none: in memory only), each change a line of its own.
    pub fn new(dir: Option<&Path>) -> Arc<Audit> {
        Arc::new(Audit {
            path: dir.map(|d| d.join(FILE)),
            join: false,
            inner: Mutex::default(),
        })
    }

    /// An engine's audit in `dir`: continuous moves of one control joined into a line,
    /// written once the control is still for a second (by a thread of its own).
    pub fn for_engine(dir: Option<&Path>) -> Arc<Audit> {
        if let Some(d) = dir {
            let _ = std::fs::create_dir_all(d);
        }
        let a = Arc::new(Audit {
            path: dir.map(|d| d.join(FILE)),
            join: true,
            inner: Mutex::default(),
        });
        let weak = Arc::downgrade(&a);
        let _ = std::thread::Builder::new()
            .name("idn-audit".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let Some(a) = weak.upgrade() else { return };
                    a.flush_if_quiet();
                }
            });
        a
    }

    /// A change by `who`: `action` (a command's name or path), its `detail` (redacted
    /// here), `error` if it wasn't done.
    pub fn record(&self, who: &Who, action: &str, detail: &Value, error: Option<&str>) {
        self.write(&who.user, &who.peer, &who.client, action, detail, error);
    }

    /// A change, its caller given as text.
    pub fn write(
        &self,
        user: &str,
        peer: &str,
        client: &str,
        action: &str,
        detail: &Value,
        error: Option<&str>,
    ) {
        let detail = redact(detail);
        let line = json!({
            "at": idn_common::epoch::secs(),
            "user": user,
            "peer": peer,
            "client": client,
            "action": action,
            "ok": error.is_none(),
            "error": error,
            "detail": detail,
        });
        let mut g = self.inner.lock_ok();
        if !self.join || error.is_some() {
            let p = g.pending.take();
            if let Some(p) = p {
                self.put(&mut g, p.line, p.repeats);
            }
            self.put(&mut g, line, 1);
            return;
        }
        let key = control_key(user, peer, client, action, &detail);
        let t = Instant::now();
        match &mut g.pending {
            Some(p) if p.key == key && t.duration_since(p.first) < LONGEST => {
                // The same control moving: the last value kept, the first time.
                let at = p.line["at"].clone();
                p.line = line;
                p.line["at"] = at;
                p.last = t;
                p.repeats += 1;
            }
            _ => {
                let before = g.pending.replace(Pending {
                    key,
                    line,
                    first: t,
                    last: t,
                    repeats: 1,
                });
                if let Some(p) = before {
                    self.put(&mut g, p.line, p.repeats);
                }
            }
        }
    }

    /// A joined line written once its control has been still for a second.
    fn flush_if_quiet(&self) {
        let mut g = self.inner.lock_ok();
        if g.pending
            .as_ref()
            .is_some_and(|p| p.last.elapsed() >= QUIET || p.first.elapsed() >= LONGEST)
            && let Some(p) = g.pending.take()
        {
            self.put(&mut g, p.line, p.repeats);
        }
    }

    /// Writes any line still being joined.
    pub fn flush(&self) {
        let mut g = self.inner.lock_ok();
        if let Some(p) = g.pending.take() {
            self.put(&mut g, p.line, p.repeats);
        }
    }

    /// One line out: to the file (trimmed now and then) and the newest in memory.
    fn put(&self, g: &mut Inner, mut line: Value, repeats: u64) {
        if repeats > 1 {
            line["repeats"] = json!(repeats);
        }
        if let Some(path) = &self.path {
            let r = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| writeln!(f, "{line}"));
            if let Err(e) = r {
                crate::log::log(&format!("audit {}: {e}", path.display()));
            } else if std::fs::metadata(path).is_ok_and(|m| m.len() > (MAX as u64) * 200) {
                let _ = trim(path);
            }
        }
        g.seq += 1;
        let seq = g.seq;
        g.recent.push_back((seq, line));
        while g.recent.len() > RECENT {
            g.recent.pop_front();
        }
    }

    /// The newest `n` lines, newest first.
    pub fn newest(&self, n: usize) -> Vec<Value> {
        self.flush();
        if let Some(path) = &self.path {
            // (A file not there yet: nothing changed since it was made.)
            let text = std::fs::read_to_string(path).unwrap_or_default();
            return text
                .lines()
                .rev()
                .filter_map(|l| serde_json::from_str(l).ok())
                .take(n)
                .collect();
        }
        let g = self.inner.lock_ok();
        g.recent
            .iter()
            .rev()
            .take(n)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// The place of the newest line written (for [`since`](Self::since)).
    pub fn seq(&self) -> u64 {
        self.inner.lock_ok().seq
    }

    /// The lines written after place `seq`, oldest first, and the place now (a push
    /// channel's `audit` topic).
    pub fn since(&self, seq: u64) -> (Vec<Value>, u64) {
        let g = self.inner.lock_ok();
        let out = g
            .recent
            .iter()
            .filter(|(s, _)| *s > seq)
            .map(|(_, v)| v.clone())
            .collect();
        (out, g.seq)
    }
}

impl Drop for Audit {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Drops lines older than two years, then all but the newest [`KEEP`] when over [`MAX`].
pub fn trim(path: &Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let cutoff = idn_common::epoch::secs().saturating_sub(DAYS * 86_400);
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| v["at"].as_u64())
                .is_some_and(|at| at >= cutoff)
        })
        .collect();
    if lines.len() > MAX {
        lines.drain(..lines.len() - KEEP);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    // Whole or not at all (the lines are appended to it in place between trims).
    idn_common::fsutil::write(path, out)
}

/// `GET /api/v1/audit?n=` (newest first; 200 by default, 5000 at most).
pub fn answer(audit: &Audit, r: &super::Request) -> super::Response {
    let n = r
        .param("n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200usize)
        .min(5000);
    super::Response::json(Value::Array(audit.newest(n)).to_string())
        .with("Cache-Control", "no-store")
}

/// A push channel's `audit` topic: the lines written since `seen` as
/// `{"type": "audit", "entries": […]}` (none: nothing), `seen` moved on.
pub fn pushed(audit: &Audit, seen: &mut u64) -> Option<String> {
    let (lines, now) = audit.since(*seen);
    *seen = now;
    (!lines.is_empty()).then(|| json!({"type": "audit", "entries": lines}).to_string())
}

#[cfg(test)]
mod url_tests {
    #[test]
    fn addresses_keep_only_their_host() {
        let v = serde_json::json!({
            "url": "rtmp://a.rtmp.youtube.com/live2/abcd-efgh-ijkl",
            "backup": "rtsp://admin:secret@10.0.0.5:554/stream?passphrase=x",
            "name": "Main",
        });
        let r = super::redact(&v);
        assert_eq!(r["url"], "rtmp://a.rtmp.youtube.com/…");
        assert_eq!(r["backup"], "rtsp://10.0.0.5:554/…");
        assert_eq!(r["name"], "Main");
        assert_eq!(super::url_host_only("http://host"), "http://host");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_say_who_did_what_without_secrets() {
        let d = idn_common::testing::TempDir::new("audit-lines");
        let a = Audit::new(Some(d.path()));
        a.write(
            "sam",
            "10.0.0.5",
            "ExampleApp",
            "/api/v1/node",
            &json!({"lan_password": "door", "x": 1,
                "machine": {"panel_pin": "4321"}, "admin_code": "777777", "code": "482 913"}),
            None,
        );
        a.write("", "10.0.0.6", "", "login", &json!({}), Some("wrong"));
        let l = a.newest(10);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0]["ok"], false);
        assert_eq!(l[1]["detail"]["lan_password"], "…");
        // Named as secrets: the panel's PIN, an admin code (bug review 3).
        assert_eq!(l[1]["detail"]["machine"]["panel_pin"], "…");
        assert_eq!(l[1]["detail"]["admin_code"], "…");
        assert_eq!(l[1]["detail"]["code"], "…");
        assert_eq!(l[1]["detail"]["x"], 1);
        assert_eq!(l[1]["client"], "ExampleApp");
        let (since, seq) = a.since(0);
        assert_eq!((since.len(), seq), (2, 2));
        assert!(a.since(seq).0.is_empty());
    }

    /// A fader dragged is one line (its last value), another control or caller its own.
    #[test]
    fn an_engines_continuous_moves_are_one_line() {
        let a = Audit::for_engine(None);
        for v in 0..50 {
            a.write(
                "alice",
                "10.0.0.7",
                "",
                "channel",
                &json!({"n": 1, "fader": -(v as f64)}),
                None,
            );
        }
        a.write(
            "alice",
            "10.0.0.7",
            "",
            "channel",
            &json!({"n": 2, "fader": -3.0}),
            None,
        );
        a.write(
            "alice",
            "10.0.0.7",
            "",
            "channel",
            &json!({"n": 2, "mute": true}),
            None,
        );
        a.write(
            "alice",
            "10.0.0.7",
            "",
            "channel",
            &json!({"n": 2, "mute": false}),
            None,
        );
        let l = a.newest(10);
        assert_eq!(l.len(), 4, "{l:?}");
        assert_eq!(l[3]["detail"]["fader"], -49.0);
        assert_eq!(l[3]["repeats"], 50);
        assert!(l[2].get("repeats").is_none());
        // Written by itself once still.
        a.write("bob", "", "", "cut", &json!({}), None);
        idn_common::testing::wait_for(
            "the last line written once still",
            Duration::from_secs(5),
            || a.since(0).0.len() == 5,
        );
    }
}
