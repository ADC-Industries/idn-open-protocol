//! Text-only sources (`text/1`, spec/extensions/idn-extensions.md §E.22): sources that
//! carry no picture and no sound, only text messages — a data feed's rows (IDN Data,
//! roadmap R.22), later a captioner's lines (G2). Receivers draw them however they
//! want. A source announces the role `text/1` in discovery; its newest message is sent
//! again every few seconds, so a receiver that joins has it at once.
//!
//! ```no_run
//! use idn_open_protocol::api::{Publisher, PublisherConfig};
//! use idn_open_protocol::text::{DataMessage, TextSource};
//! # fn main() -> std::io::Result<()> {
//! let p = Publisher::new(PublisherConfig::default())?;
//! let feed = TextSource::new(&p, "Data: Scores")?;
//! let mut row = std::collections::BTreeMap::new();
//! row.insert("Home".to_owned(), "3".to_owned());
//! feed.send_data(&DataMessage::rows(vec![row]));
//! # Ok(())
//! # }
//! ```

use idn_common::sync::LockOk as _;
use crate::api::{Output, OutputConfig, Publisher};
use crate::input::{Input, InputConfig, InputEvent};
use crate::xml::Element;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The role text-only sources announce (§E.22.1).
pub const ROLE: &str = "text/1";
/// How often a source sends its newest message again.
pub const REPEAT: Duration = Duration::from_secs(5);
/// The largest message (a receiver takes no more than 1 MB on the control connection);
/// rows past it are left out and counted in [`DataMessage::total`].
pub const MAX_MESSAGE: usize = 1_000_000;

/// A row: its values by column name.
pub type Row = BTreeMap<String, String>;

/// A data feed's message (§E.22.2): its rows now, or why it has none.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DataMessage {
    /// Counts up with each new content (repeats keep it).
    pub seq: u64,
    /// The rows sent, in order.
    pub rows: Vec<Row>,
    /// The columns: in the source's order where it has one (a CSV's header), else in name
    /// order (as the rows keep them).
    pub columns: Vec<String>,
    /// Rows the feed has (more than `rows` when the message would be too large).
    pub total: usize,
    /// When it was read, as the sender's local time (`YYYY-MM-DD HH:MM:SS`).
    pub read_at: String,
    /// Why the last read failed, if it did (the rows are the last good ones).
    pub error: Option<String>,
}

impl DataMessage {
    /// A message with these rows (their columns, in name order).
    pub fn rows(rows: Vec<Row>) -> DataMessage {
        let mut columns: Vec<String> = Vec::new();
        for r in &rows {
            for k in r.keys() {
                if !columns.contains(k) {
                    columns.push(k.clone());
                }
            }
        }
        DataMessage {
            total: rows.len(),
            rows,
            columns,
            ..Default::default()
        }
    }

    /// A message with these rows, its columns in `order` (a CSV's header: Team, Score
    /// came back as Score, Team), then any others in name order.
    pub fn rows_in_order(rows: Vec<Row>, order: &[String]) -> DataMessage {
        let mut m = DataMessage::rows(rows);
        let mut columns: Vec<String> = order
            .iter()
            .filter(|c| m.columns.contains(c))
            .cloned()
            .collect();
        columns.extend(m.columns.iter().filter(|c| !order.contains(c)).cloned());
        m.columns = columns;
        m
    }

    /// `<idn_data seq total read_at error>{"columns", "rows"}</idn_data>`, rows past
    /// [`MAX_MESSAGE`] left out.
    pub fn to_xml(&self) -> String {
        let mut keep = self.rows.len();
        loop {
            let body = json!({"columns": self.columns, "rows": &self.rows[..keep]}).to_string();
            let mut e = Element::new("idn_data")
                .with_attr("seq", &self.seq.to_string())
                .with_attr("total", &self.total.max(keep).to_string())
                .with_attr("read_at", &self.read_at);
            if let Some(err) = &self.error {
                e = e.with_attr("error", err);
            }
            let xml = e.with_text(&body).to_xml();
            if xml.len() <= MAX_MESSAGE || keep == 0 {
                return xml;
            }
            keep = keep * MAX_MESSAGE / xml.len() * 9 / 10;
        }
    }

    /// The message of an `<idn_data>` element, if it is one.
    pub fn from_xml(xml: &str) -> Option<DataMessage> {
        let e = crate::xml::parse(xml)?;
        if e.name != "idn_data" {
            return None;
        }
        let v: Value = serde_json::from_str(&e.text).ok()?;
        let rows: Vec<Row> = v["rows"]
            .as_array()?
            .iter()
            .filter_map(|r| {
                Some(
                    r.as_object()?
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                v.as_str().map_or_else(|| v.to_string(), str::to_owned),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        let columns = v["columns"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Some(DataMessage {
            seq: e.attr("seq")?.parse().ok()?,
            total: e
                .attr("total")
                .and_then(|t| t.parse().ok())
                .unwrap_or(rows.len()),
            rows,
            columns,
            read_at: e.attr("read_at").unwrap_or("").to_owned(),
            error: e.attr("error").map(str::to_owned),
        })
    }
}

/// A line of captions (§E.22.3): what a captioner heard, a draft corrected in place
/// (the same `line`) until it is final.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaptionLine {
    /// Counts up with each message (repeats keep it).
    pub seq: u64,
    /// The line: drafts of one line share it; the next line has the next number.
    pub line: u64,
    /// What was said.
    pub text: String,
    /// No more corrections to this line.
    pub is_final: bool,
    /// Its language (`en`, `es`).
    pub lang: String,
    /// When it was said, Unix milliseconds of the sender's clock (the site clock where it
    /// runs one).
    pub at_ms: i64,
}

impl CaptionLine {
    /// `<idn_caption seq line final lang at>TEXT</idn_caption>`.
    pub fn to_xml(&self) -> String {
        Element::new("idn_caption")
            .with_attr("seq", &self.seq.to_string())
            .with_attr("line", &self.line.to_string())
            .with_attr("final", if self.is_final { "true" } else { "false" })
            .with_attr("lang", &self.lang)
            .with_attr("at", &self.at_ms.to_string())
            .with_text(&self.text)
            .to_xml()
    }

    /// The line of an `<idn_caption>` element, if it is one.
    pub fn from_xml(xml: &str) -> Option<CaptionLine> {
        let e = crate::xml::parse(xml)?;
        (e.name == "idn_caption").then(|| CaptionLine {
            seq: e.attr("seq").and_then(|v| v.parse().ok()).unwrap_or(0),
            line: e.attr("line").and_then(|v| v.parse().ok()).unwrap_or(0),
            text: e.text.clone(),
            is_final: e.attr("final") == Some("true"),
            lang: e.attr("lang").unwrap_or("").to_owned(),
            at_ms: e.attr("at").and_then(|v| v.parse().ok()).unwrap_or(0),
        })
    }
}

struct Shared {
    out: Output,
    last: Mutex<Option<String>>,
    seq: AtomicU64,
    stop: AtomicBool,
}

/// A text-only source: an output with no picture or sound, announcing [`ROLE`], that
/// sends its newest message again every [`REPEAT`]. Gone when dropped.
pub struct TextSource {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TextSource {
    /// A source named `name` ("HOST (NAME)" on the network).
    pub fn new(publisher: &Publisher, name: &str) -> io::Result<TextSource> {
        let out = Output::new(
            publisher,
            OutputConfig {
                roles: vec![ROLE.into()],
                ..OutputConfig::new(name)
            },
        )?;
        let shared = Arc::new(Shared {
            out,
            last: Mutex::new(None),
            seq: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("idn-text".into())
                .spawn(move || {
                    let step = Duration::from_millis(100);
                    let mut waited = Duration::ZERO;
                    while !sh.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(step);
                        waited += step;
                        if waited >= REPEAT {
                            waited = Duration::ZERO;
                            if let Some(x) = sh.last.lock_ok().clone() {
                                let _ = sh.out.send_metadata(&x);
                            }
                        }
                    }
                })?
        };
        Ok(TextSource {
            shared,
            thread: Some(thread),
        })
    }

    /// Sends a data feed's rows (its `seq` is set here: one more than the last).
    pub fn send_data(&self, m: &DataMessage) {
        let mut m = m.clone();
        m.seq = self.shared.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let xml = m.to_xml();
        let _ = self.shared.out.send_metadata(&xml);
        *self.shared.last.lock_ok() = Some(xml);
    }

    /// Sends a line of captions (its `seq` set here). The newest line is what is repeated.
    pub fn send_caption(&self, c: &CaptionLine) {
        let mut c = c.clone();
        c.seq = self.shared.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let xml = c.to_xml();
        let _ = self.shared.out.send_metadata(&xml);
        *self.shared.last.lock_ok() = Some(xml);
    }

    /// The output (its full name, port, receivers).
    pub fn output(&self) -> &Output {
        &self.shared.out
    }
}

impl Drop for TextSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The caption lines a receiver keeps.
const CAPTION_LINES_KEPT: usize = 8;

/// A connection to a text-only source that keeps its newest data message and its newest
/// caption lines.
pub struct TextReceiver {
    input: Input,
    last: Mutex<Option<DataMessage>>,
    captions: Mutex<std::collections::VecDeque<CaptionLine>>,
    /// Final lines not yet taken ([`TextReceiver::new_finals`]).
    finals: Mutex<Vec<CaptionLine>>,
}

impl TextReceiver {
    /// Connects to the source `name` ("HOST (Data: Scores)" or an address); `cfg` gives
    /// the encryption, discovery server and receiver name (its picture and sound are
    /// turned off here).
    pub fn open(name: &str, cfg: InputConfig) -> io::Result<TextReceiver> {
        let input = Input::open(InputConfig {
            source: name.to_owned(),
            receive_video: false,
            audio_channels: Some(Vec::new()),
            ..cfg
        })?;
        Ok(TextReceiver {
            input,
            last: Mutex::new(None),
            captions: Mutex::new(Default::default()),
            finals: Mutex::new(Vec::new()),
        })
    }

    /// Takes what has arrived, waiting up to `wait` for something.
    fn poll(&self, wait: Duration) {
        let mut wait = wait;
        while let Some(ev) = self.input.recv(wait) {
            wait = Duration::ZERO;
            let InputEvent::Metadata { xml, .. } = ev else {
                continue;
            };
            if let Some(m) = DataMessage::from_xml(&xml) {
                *self.last.lock_ok() = Some(m);
            } else if let Some(c) = CaptionLine::from_xml(&xml) {
                let mut l = self.captions.lock_ok();
                // A repeat (the same message again) changes nothing.
                if l.iter().any(|x| x.seq == c.seq && x.line == c.line) {
                    continue;
                }
                let was_final = l.iter().any(|x| x.line == c.line && x.is_final);
                l.retain(|x| x.line != c.line);
                if c.is_final && !was_final {
                    self.finals.lock_ok().push(c.clone());
                }
                l.push_back(c);
                while l.len() > CAPTION_LINES_KEPT {
                    l.pop_front();
                }
            }
        }
    }

    /// Takes what has arrived (waiting up to `wait` for something) and gives the newest
    /// data message, if any came so far.
    pub fn data(&self, wait: Duration) -> Option<DataMessage> {
        self.poll(wait);
        self.last.lock_ok().clone()
    }

    /// Takes what has arrived (waiting up to `wait`) and gives the newest caption lines,
    /// oldest first (the last may be a draft).
    pub fn captions(&self, wait: Duration) -> Vec<CaptionLine> {
        self.poll(wait);
        self.captions.lock_ok().iter().cloned().collect()
    }

    /// The lines that became final since the last call, in order (for closed captions,
    /// which take each line once).
    pub fn new_finals(&self) -> Vec<CaptionLine> {
        std::mem::take(&mut *self.finals.lock_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kv: &[(&str, &str)]) -> Row {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn data_messages_round_trip_and_stay_under_the_limit() {
        let mut m = DataMessage::rows(vec![
            row(&[("Team", "Lions & <Tigers>"), ("Score", "3")]),
            row(&[("Team", "Bears \"B\""), ("Score", "1")]),
        ]);
        m.seq = 7;
        m.read_at = "2026-10-30 14:00:00".into();
        m.error = Some("timed out".into());
        let back = DataMessage::from_xml(&m.to_xml()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.columns, vec!["Score", "Team"]);
        // Too many rows for one message: the first ones, and the count of all.
        let big: Vec<Row> = (0..40_000)
            .map(|i| row(&[("Line", &format!("{i:05} {}", "x".repeat(40)))]))
            .collect();
        let m = DataMessage::rows(big);
        let xml = m.to_xml();
        assert!(xml.len() <= MAX_MESSAGE, "{}", xml.len());
        let back = DataMessage::from_xml(&xml).unwrap();
        assert_eq!(back.total, 40_000);
        assert!(back.rows.len() > 1000 && back.rows.len() < 40_000);
        assert_eq!(back.rows[0]["Line"], m.rows[0]["Line"]);
        assert!(DataMessage::from_xml("<idn_tally/>").is_none());
    }

    #[test]
    fn caption_lines_round_trip() {
        let c = CaptionLine {
            seq: 4,
            line: 2,
            text: "Grace & peace <to you>".into(),
            is_final: true,
            lang: "en".into(),
            at_ms: 1_793_000_000_000,
        };
        assert_eq!(CaptionLine::from_xml(&c.to_xml()).unwrap(), c);
        assert!(CaptionLine::from_xml("<idn_data seq=\"1\"/>").is_none());
    }

    /// Over the network: a receiver has the rows; one that joins later has them by the
    /// next repeat.
    #[test]
    fn receivers_get_the_newest_rows_and_late_ones_the_repeat() {
        use crate::api::PublisherConfig;
        use crate::link::LinkConfig;
        let p = Publisher::new(PublisherConfig {
            mdns: Some(false),
            link: Some(LinkConfig::ephemeral()),
            ..Default::default()
        })
        .unwrap();
        let src = TextSource::new(&p, "Data: Test").unwrap();
        let at = format!("127.0.0.1:{}", src.output().port());
        let open = || {
            TextReceiver::open(
                &at,
                InputConfig {
                    link: Some(LinkConfig::ephemeral()),
                    ..InputConfig::new("")
                },
            )
            .unwrap()
        };
        let wait = |r: &TextReceiver, seq: u64| {
            let end = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                match r.data(Duration::from_millis(100)) {
                    Some(m) if m.seq >= seq => return Some(m),
                    _ if std::time::Instant::now() > end => return None,
                    _ => {}
                }
            }
        };
        let first = open();
        std::thread::sleep(Duration::from_millis(500));
        src.send_data(&DataMessage::rows(vec![row(&[("Home", "1")])]));
        src.send_data(&DataMessage::rows(vec![row(&[("Home", "2")])]));
        let m = wait(&first, 2).expect("rows");
        assert_eq!(m.rows[0]["Home"], "2");
        let late = open();
        let m = wait(&late, 2).expect("the repeat");
        assert_eq!(m.rows[0]["Home"], "2");
    }
}
