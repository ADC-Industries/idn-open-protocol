//! IDN Data (spec/extensions/idn-roadmap.md R.22, idn-data.md): a program of its own,
//! which the IDN Node runs as a service, that fetches things for the site. Data feeds
//! ([`feeds`]: a Google Sheet, CSV, JSON, XML or RSS, each read every so often) go out as
//! text-only sources ([`idn_open_protocol::text`]) that the switcher's titles and anything
//! else take by name. Video downloads ([`downloads`]) go through yt-dlp ([`ytdlp`]) into
//! the media folder.

use idn_common::sync::LockOk as _;
pub mod downloads;
/// The feeds' reading and formats, shared with the switcher's titles ([`idn_feeds`]).
pub use idn_feeds as feeds;
pub mod library;
pub mod program;
pub mod ytdlp;

use feeds::Format;
use idn_open_protocol::api::Publisher;
use idn_open_protocol::text::{DataMessage, TextSource};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The file the feeds are kept in, in the state directory.
pub const FEEDS_FILE: &str = "feeds.json";
/// Each feed's last good rows, kept beside [`FEEDS_FILE`]: after a restart a feed whose
/// address is down still has them (it had none).
pub const ROWS_FILE: &str = "feed-rows.json";
/// The most feeds.
pub const MAX_FEEDS: usize = 64;

/// A feed as set: its name, where it is read from, how, and how often.
#[derive(Debug, Clone, PartialEq)]
pub struct Feed {
    /// What it is called; its source is "HOST (Data: NAME)".
    pub name: String,
    /// A file on this machine or an `http(s)://` address (a Google Sheets link works).
    pub from: String,
    pub format: Format,
    /// Seconds between reads (5–86400, default 30).
    pub every: f32,
}

impl Feed {
    /// From the API's JSON: `{"name", "from", "format", "every"}`.
    pub fn from_json(v: &Value) -> Result<Feed, String> {
        let name = v["name"].as_str().unwrap_or("").trim();
        if name.is_empty()
            || name.chars().count() > 60
            || name.chars().any(|c| c.is_control() || "()\"".contains(c))
        {
            return Err("feed name: 1 to 60 characters, no brackets or quotes".into());
        }
        let from = v["from"].as_str().unwrap_or("").trim();
        if from.is_empty() {
            return Err("feed from: a file or an address".into());
        }
        let format = match &v["format"] {
            Value::Null => Format::Auto,
            f => Format::from_name(f.as_str().unwrap_or(""))
                .ok_or("feed format: auto, csv, tsv, json or xml")?,
        };
        let every = match &v["every"] {
            Value::Null => 30.0,
            x => x
                .as_f64()
                .filter(|x| x.is_finite())
                .ok_or("feed every: seconds")?
                .clamp(5.0, 86_400.0) as f32,
        };
        Ok(Feed {
            name: name.to_owned(),
            from: from.to_owned(),
            format,
            every,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "from": self.from, "format": self.format.name(),
               "every": self.every})
    }

    /// Its source's own name (the network shows "HOST (Data: NAME)").
    pub fn source_name(&self) -> String {
        format!("Data: {}", self.name)
    }
}

/// Reads `from` once, as a feed would: its rows as a message (for trying an address
/// before adding it).
pub fn preview(from: &str, format: Format) -> Result<DataMessage, String> {
    let (rows, columns) = feeds::read_table(from, format).map_err(|e| e.to_string())?;
    let mut m = DataMessage::rows_in_order(rows, &columns);
    m.read_at = now_text();
    Ok(m)
}

fn now_text() -> String {
    let t = idn_open_protocol::localtime::now();
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

struct Running {
    feed: Feed,
    source: TextSource,
    /// The last message sent (rows, error).
    last: Option<DataMessage>,
    read_at: Option<Instant>,
    reading: bool,
}

struct Inner {
    publisher: Publisher,
    dir: Option<PathBuf>,
    feeds: Mutex<Vec<Running>>,
    version: Mutex<u64>,
    changed: Condvar,
    stop: AtomicBool,
    reads: AtomicU64,
}

/// The feeds, each read when due and sent as its source; kept in [`FEEDS_FILE`].
/// Stopped when dropped.
pub struct Feeds {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feeds {
    /// Starts the feeds kept in `dir` (none: none kept), as sources of `publisher`.
    pub fn start(publisher: Publisher, dir: Option<PathBuf>) -> std::io::Result<Feeds> {
        let inner = Arc::new(Inner {
            publisher,
            dir,
            feeds: Mutex::new(Vec::new()),
            version: Mutex::new(1),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            reads: AtomicU64::new(0),
        });
        // (One that can't be read is set aside and said: read as none, the next feed
        // added or removed wrote the list over with only itself; bug run 4.)
        let kept: Vec<Value> = match inner
            .dir
            .as_ref()
            .map(|d| idn_common::fsutil::load_json(&d.join(FEEDS_FILE)))
        {
            Some(Ok(v)) => v.unwrap_or_default(),
            Some(Err(why)) => {
                idn_open_protocol::log::log(&format!("data: {why}"));
                Vec::new()
            }
            None => Vec::new(),
        };
        let rows: Value = inner
            .dir
            .as_ref()
            .and_then(|d| {
                idn_common::fsutil::load_json(&d.join(ROWS_FILE)).unwrap_or_else(|e| {
                    idn_open_protocol::log::log(&format!("data: {e}"));
                    None
                })
            })
            .unwrap_or_default();
        for v in kept {
            match Feed::from_json(&v) {
                Ok(f) => {
                    let name = f.name.clone();
                    if let Err(e) = add(&inner, f, false) {
                        idn_open_protocol::log::log(&format!("idn-data: kept feed: {e}"));
                    } else if let Some(m) = kept_rows(&rows[&name]) {
                        // Sent at once; the first read replaces them, or adds its error.
                        let mut list = inner.feeds.lock_ok();
                        if let Some(r) = list.iter_mut().find(|r| r.feed.name == name) {
                            r.source.send_data(&m);
                            r.last = Some(m);
                        }
                    }
                }
                Err(e) => idn_open_protocol::log::log(&format!("idn-data: kept feed: {e}")),
            }
        }
        let thread = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("idn-data-feeds".into())
                .spawn(move || tick(&inner))?
        };
        Ok(Feeds {
            inner,
            thread: Some(thread),
        })
    }

    /// Adds a feed (a name not in use).
    pub fn add(&self, f: Feed) -> Result<(), String> {
        add(&self.inner, f, true)
    }

    /// Changes feed `name` to `f` (renamed: its source too).
    pub fn set(&self, name: &str, f: Feed) -> Result<(), String> {
        {
            let mut list = self.inner.feeds.lock_ok();
            let k = list
                .iter()
                .position(|r| r.feed.name == name)
                .ok_or(format!("no feed {name:?}"))?;
            if f.name != name && list.iter().any(|r| r.feed.name == f.name) {
                return Err(format!("there is a feed {:?} already", f.name));
            }
            if f.name == name {
                let r = &mut list[k];
                r.feed = f;
                r.read_at = None;
            } else {
                let source = TextSource::new(&self.inner.publisher, &f.source_name())
                    .map_err(|e| e.to_string())?;
                list[k] = Running {
                    feed: f,
                    source,
                    last: None,
                    read_at: None,
                    reading: false,
                };
            }
        }
        changed(&self.inner, true);
        Ok(())
    }

    /// Removes feed `name` (its source goes).
    pub fn remove(&self, name: &str) -> Result<(), String> {
        {
            let mut list = self.inner.feeds.lock_ok();
            let n = list.len();
            list.retain(|r| r.feed.name != name);
            if list.len() == n {
                return Err(format!("no feed {name:?}"));
            }
        }
        changed(&self.inner, true);
        Ok(())
    }

    /// Reads feed `name` again soon, whatever its time.
    pub fn read_now(&self, name: &str) -> Result<(), String> {
        let mut list = self.inner.feeds.lock_ok();
        let r = list
            .iter_mut()
            .find(|r| r.feed.name == name)
            .ok_or(format!("no feed {name:?}"))?;
        r.read_at = None;
        Ok(())
    }

    /// The feeds and how each is doing: `{"feeds": [{name, from, format, every, source,
    /// port, receivers, reading, rows, columns, total, read_at, error, sample}]}`
    /// (sample: the first rows).
    pub fn state(&self) -> Value {
        let list = self.inner.feeds.lock_ok();
        json!({
            "feeds": list.iter().map(|r| {
                let mut v = r.feed.to_json();
                v["source"] = json!(r.source.output().name());
                v["port"] = json!(r.source.output().port());
                v["receivers"] = json!(r.source.output().receivers());
                v["reading"] = json!(r.reading);
                if let Some(m) = &r.last {
                    v["rows"] = json!(m.rows.len());
                    v["total"] = json!(m.total);
                    v["columns"] = json!(m.columns);
                    v["read_at"] = json!(m.read_at);
                    v["error"] = json!(m.error);
                    v["sample"] = json!(m.rows.iter().take(5).collect::<Vec<_>>());
                }
                v
            }).collect::<Vec<_>>(),
        })
    }

    /// The state's version: it goes up with every change (for long polling).
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

impl Drop for Feeds {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn add(inner: &Arc<Inner>, f: Feed, save: bool) -> Result<(), String> {
    {
        let mut list = inner.feeds.lock_ok();
        if list.iter().any(|r| r.feed.name == f.name) {
            return Err(format!("there is a feed {:?} already", f.name));
        }
        if list.len() >= MAX_FEEDS {
            return Err(format!("at most {MAX_FEEDS} feeds"));
        }
        let source =
            TextSource::new(&inner.publisher, &f.source_name()).map_err(|e| e.to_string())?;
        list.push(Running {
            feed: f,
            source,
            last: None,
            read_at: None,
            reading: false,
        });
    }
    changed(inner, save);
    Ok(())
}

/// A feed's rows as kept in [`ROWS_FILE`].
fn kept_rows(v: &Value) -> Option<DataMessage> {
    let rows: Vec<feeds::Row> = serde_json::from_value(v.get("rows")?.clone()).ok()?;
    let columns: Vec<String> = serde_json::from_value(v["columns"].clone()).unwrap_or_default();
    let mut m = DataMessage::rows_in_order(rows, &columns);
    m.read_at = v["read_at"].as_str().unwrap_or("").to_owned();
    Some(m)
}

/// Writes every feed's last good rows to [`ROWS_FILE`].
fn save_rows(inner: &Inner, list: &[Running]) {
    let Some(d) = &inner.dir else { return };
    let all: serde_json::Map<String, Value> = list
        .iter()
        .filter_map(|r| {
            let m = r.last.as_ref().filter(|m| !m.rows.is_empty())?;
            Some((
                r.feed.name.clone(),
                json!({"rows": m.rows, "columns": m.columns, "read_at": m.read_at}),
            ))
        })
        .collect();
    let _ = std::fs::create_dir_all(d);
    let _ = idn_common::fsutil::write_atomic(
        &d.join(ROWS_FILE),
        &serde_json::to_vec(&all).unwrap_or_default(),
    );
}

/// The state moved on; with `save`, the feeds are written to their file.
fn changed(inner: &Inner, save: bool) {
    if save && let Some(d) = &inner.dir {
        let list: Vec<Value> = inner
            .feeds
            .lock_ok()
            .iter()
            .map(|r| r.feed.to_json())
            .collect();
        // (Synced: a power cut could leave the renamed file empty.)
        let _ = std::fs::create_dir_all(d);
        let _ = idn_common::fsutil::write_atomic(
            &d.join(FEEDS_FILE),
            &serde_json::to_vec_pretty(&list).unwrap_or_default(),
        );
    }
    *inner.version.lock_ok() += 1;
    inner.changed.notify_all();
}

/// Starts a read of each feed that is due, each on a thread of its own (a slow address
/// holds up only itself); a read whose rows or error changed is sent.
fn tick(inner: &Arc<Inner>) {
    while !inner.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        let due: Vec<Feed> = {
            let mut list = inner.feeds.lock_ok();
            list.iter_mut()
                .filter(|r| {
                    !r.reading
                        && r.read_at
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs_f32(r.feed.every))
                })
                .map(|r| {
                    r.reading = true;
                    r.feed.clone()
                })
                .collect()
        };
        for f in due {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let read = feeds::read_table(&f.from, f.format).map_err(|e| e.to_string());
                inner.reads.fetch_add(1, Ordering::Relaxed);
                let mut list = inner.feeds.lock_ok();
                // Unless it was changed or removed while it was read: then the feed of that
                // name is free to be read again (it stayed `reading` for good, its titles
                // frozen; bug round E, E4-2).
                let Some(r) = list.iter_mut().find(|r| r.feed == f) else {
                    if let Some(r) = list.iter_mut().find(|r| r.feed.name == f.name) {
                        r.reading = false;
                        r.read_at = None;
                    }
                    drop(list);
                    return;
                };
                r.reading = false;
                r.read_at = Some(Instant::now());
                let mut m = match read {
                    Ok((rows, columns)) => DataMessage::rows_in_order(rows, &columns),
                    Err(e) => {
                        // The last good rows stay, with why this read failed.
                        let mut m = r.last.clone().unwrap_or_default();
                        m.error = Some(e);
                        m
                    }
                };
                let same = r.last.as_ref().is_some_and(|l| {
                    l.rows == m.rows && l.columns == m.columns && l.error == m.error
                });
                m.read_at = now_text();
                let good = m.error.is_none();
                if !same {
                    r.source.send_data(&m);
                }
                r.last = Some(m);
                if !same && good {
                    save_rows(&inner, &list);
                }
                drop(list);
                changed(&inner, false);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds_are_checked() {
        let f = Feed::from_json(&json!({"name": "Scores", "from": "https://x/y.csv"})).unwrap();
        assert_eq!((f.format, f.every), (Format::Auto, 30.0));
        assert_eq!(f.source_name(), "Data: Scores");
        assert_eq!(Feed::from_json(&f.to_json()).unwrap(), f);
        let every = Feed::from_json(&json!({"name": "A", "from": "f.csv", "every": 1})).unwrap();
        assert_eq!(every.every, 5.0);
        for bad in [
            json!({"name": "", "from": "f.csv"}),
            json!({"name": "A (b)", "from": "f.csv"}),
            json!({"name": "A", "from": ""}),
            json!({"name": "A", "from": "f.csv", "format": "pdf"}),
        ] {
            assert!(Feed::from_json(&bad).is_err(), "{bad}");
        }
    }
}
