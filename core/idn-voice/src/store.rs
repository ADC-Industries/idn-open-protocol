//! What Voice keeps (§V.1): channels, the site's settings, messages, the recordings' index
//! and users' settings, in one database in the Site Manager's `voice/` folder; the
//! recordings themselves are files beside it (`record.rs`).

use crate::model::{Channel, Handset, Settings, UserSettings};
use idn_common::sync::LockOk as _;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

const CHANNELS: TableDefinition<u32, &[u8]> = TableDefinition::new("channels");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// Messages by conversation (`c:ID` a channel, `d:A\nB` two users, names sorted) and
/// sequence: the key is the conversation, a NUL, the sequence (u64 BE).
const MESSAGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("messages");
const TRANSMISSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("transmissions");
const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("users");
/// Handsets by id (§V.13).
const HANDSETS: TableDefinition<&str, &[u8]> = TableDefinition::new("handsets");

fn db_err(e: impl std::fmt::Display) -> io::Error {
    idn_common::err::other(e)
}

/// A text message (§V.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub seq: u64,
    pub conv: String,
    pub from: String,
    pub text: String,
    pub time_ms: i64,
    /// The handset it came from, when a handset sent it (`from` is its driver then, §V.13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handset: Option<String>,
    /// Who sent it when that isn't `from` (a handset's key, `driver:…`, §V.13.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
}

/// A recorded transmission (§V.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transmission {
    pub id: u64,
    /// The channel it went into, or none for a user's own (instant talk, a call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
    /// The users it went to, for one not in a channel.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    pub from: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Opus frames recorded.
    pub frames: u32,
    /// The handset it came from, when a handset talked (`from` is its driver then, §V.13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handset: Option<String>,
    /// Who talked when that isn't `from` (a handset's key, `driver:…`, §V.13.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
}

impl Transmission {
    /// Who talked ([`crate::hub::Who::key`]).
    pub fn from_key(&self) -> &str {
        self.from_id.as_deref().unwrap_or(&self.from)
    }
}

/// The conversation key of two users' messages.
pub fn direct(a: &str, b: &str) -> String {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    format!("d:{x}\n{y}")
}

/// The conversation key of a channel's messages.
pub fn in_channel(id: u32) -> String {
    format!("c:{id}")
}

pub struct Store {
    db: Arc<Database>,
    /// The next message and transmission numbers, given here at once; written down with
    /// what has them.
    next_message: AtomicU64,
    next_transmission: AtomicU64,
    /// The writer: messages and transmissions are written down on a thread of their own,
    /// so that a talk's start or end or a message never waits for the disk in the hub's
    /// lock (review 2026-11-07); what reads them waits for what was handed in before.
    writer: Mutex<Option<mpsc::Sender<Job>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What the writer does.
enum Job {
    /// A message: its key, its value, the next number.
    Message(Vec<u8>, Vec<u8>, u64),
    /// A transmission: its id, its value.
    Transmission(u64, Vec<u8>),
    /// Answered once everything handed in before is written.
    Flush(mpsc::SyncSender<()>),
}

/// The writer's loop: each batch of jobs waiting in one transaction.
fn write_jobs(db: &Database, rx: &mpsc::Receiver<Job>) {
    while let Ok(first) = rx.recv() {
        let mut jobs = vec![first];
        while jobs.len() < 256 {
            match rx.try_recv() {
                Ok(j) => jobs.push(j),
                Err(_) => break,
            }
        }
        let mut answers = Vec::new();
        let r = (|| -> Result<(), redb::Error> {
            let w = db.begin_write()?;
            {
                let mut msgs = w.open_table(MESSAGES)?;
                let mut tx = w.open_table(TRANSMISSIONS)?;
                let mut meta = w.open_table(META)?;
                let stored = |meta: &redb::Table<&str, &[u8]>, k: &str| -> u64 {
                    meta.get(k)
                        .ok()
                        .flatten()
                        .and_then(|v| v.value().try_into().ok().map(u64::from_le_bytes))
                        .unwrap_or(0)
                };
                for j in jobs.drain(..) {
                    match j {
                        Job::Message(k, v, next) => {
                            msgs.insert(k.as_slice(), v.as_slice())?;
                            if stored(&meta, "next_message") < next {
                                meta.insert("next_message", next.to_le_bytes().as_slice())?;
                            }
                        }
                        Job::Transmission(id, v) => {
                            tx.insert(id, v.as_slice())?;
                            if stored(&meta, "next_transmission") <= id {
                                meta.insert(
                                    "next_transmission",
                                    (id + 1).to_le_bytes().as_slice(),
                                )?;
                            }
                        }
                        Job::Flush(a) => answers.push(a),
                    }
                }
            }
            w.commit()?;
            Ok(())
        })();
        if let Err(e) = r {
            idn_open_protocol::log::log(&format!("voice: writing messages and recordings: {e}"));
        }
        for a in answers {
            let _ = a.send(());
        }
    }
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let db = Database::create(dir.join("voice.redb")).map_err(db_err)?;
        let w = db.begin_write().map_err(db_err)?;
        w.open_table(CHANNELS).map_err(db_err)?;
        w.open_table(META).map_err(db_err)?;
        w.open_table(MESSAGES).map_err(db_err)?;
        w.open_table(TRANSMISSIONS).map_err(db_err)?;
        w.open_table(USERS).map_err(db_err)?;
        w.open_table(HANDSETS).map_err(db_err)?;
        w.commit().map_err(db_err)?;
        let (next_message, next_transmission) = {
            let r = db.begin_read().map_err(db_err)?;
            let t = r.open_table(META).map_err(db_err)?;
            let get = |k: &str| -> io::Result<u64> {
                Ok(t.get(k)
                    .map_err(db_err)?
                    .and_then(|v| v.value().try_into().ok().map(u64::from_le_bytes))
                    .unwrap_or(0)
                    .max(1))
            };
            (get("next_message")?, get("next_transmission")?)
        };
        let db = Arc::new(db);
        let (tx, rx) = mpsc::channel();
        let d = db.clone();
        let thread = std::thread::Builder::new()
            .name("idn-voice-store".into())
            .spawn(move || write_jobs(&d, &rx))?;
        Ok(Store {
            db,
            next_message: AtomicU64::new(next_message),
            next_transmission: AtomicU64::new(next_transmission),
            writer: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        })
    }

    fn hand(&self, j: Job) {
        if let Some(w) = self.writer.lock_ok().as_ref() {
            let _ = w.send(j);
        }
    }

    /// Waits until what was handed to the writer is written.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::sync_channel(1);
        self.hand(Job::Flush(tx));
        let _ = rx.recv();
    }

    fn meta_u64(&self, w: &redb::WriteTransaction, key: &str) -> io::Result<u64> {
        let t = w.open_table(META).map_err(db_err)?;
        let v = t.get(key).map_err(db_err)?;
        Ok(
            v.and_then(|v| v.value().try_into().ok().map(u64::from_le_bytes))
                .unwrap_or(0),
        )
    }

    fn set_meta_u64(&self, w: &redb::WriteTransaction, key: &str, v: u64) -> io::Result<()> {
        w.open_table(META)
            .map_err(db_err)?
            .insert(key, v.to_le_bytes().as_slice())
            .map_err(db_err)?;
        Ok(())
    }

    // ---- channels

    pub fn channels(&self) -> io::Result<Vec<Channel>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(CHANNELS).map_err(db_err)?;
        let mut out = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (_, v) = e.map_err(db_err)?;
            if let Ok(c) = serde_json::from_slice::<Channel>(v.value()) {
                out.push(c);
            }
        }
        out.sort_by_key(|c| c.name.to_lowercase());
        Ok(out)
    }

    /// Stores a channel; `id` 0 gives it a new number. Returns it as stored.
    pub fn put_channel(&self, mut c: Channel) -> io::Result<Channel> {
        let w = self.db.begin_write().map_err(db_err)?;
        if c.id == 0 {
            let n = self.meta_u64(&w, "next_channel")?.max(1);
            c.id = n as u32;
            self.set_meta_u64(&w, "next_channel", n + 1)?;
        }
        w.open_table(CHANNELS)
            .map_err(db_err)?
            .insert(c.id, serde_json::to_vec(&c).map_err(db_err)?.as_slice())
            .map_err(db_err)?;
        w.commit().map_err(db_err)?;
        Ok(c)
    }

    pub fn remove_channel(&self, id: u32) -> io::Result<bool> {
        let w = self.db.begin_write().map_err(db_err)?;
        let had = w
            .open_table(CHANNELS)
            .map_err(db_err)?
            .remove(id)
            .map_err(db_err)?
            .is_some();
        w.commit().map_err(db_err)?;
        Ok(had)
    }

    // ---- settings

    pub fn settings(&self) -> Settings {
        let r = self.db.begin_read().ok();
        let t = r.as_ref().and_then(|r| r.open_table(META).ok());
        t.and_then(|t| t.get("settings").ok().flatten().map(|v| v.value().to_vec()))
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default()
    }

    pub fn put_settings(&self, s: &Settings) -> io::Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(META)
            .map_err(db_err)?
            .insert(
                "settings",
                serde_json::to_vec(s).map_err(db_err)?.as_slice(),
            )
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    pub fn user_settings(&self, name: &str) -> UserSettings {
        let r = self.db.begin_read().ok();
        let t = r.as_ref().and_then(|r| r.open_table(USERS).ok());
        t.and_then(|t| t.get(name).ok().flatten().map(|v| v.value().to_vec()))
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default()
    }

    pub fn put_user_settings(&self, name: &str, s: &UserSettings) -> io::Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(USERS)
            .map_err(db_err)?
            .insert(name, serde_json::to_vec(s).map_err(db_err)?.as_slice())
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    // ---- handsets (§V.13)

    pub fn handsets(&self) -> io::Result<Vec<Handset>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(HANDSETS).map_err(db_err)?;
        let mut out = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (_, v) = e.map_err(db_err)?;
            if let Ok(h) = serde_json::from_slice::<Handset>(v.value()) {
                out.push(h);
            }
        }
        Ok(out)
    }

    pub fn put_handset(&self, h: &Handset) -> io::Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(HANDSETS)
            .map_err(db_err)?
            .insert(
                h.id.as_str(),
                serde_json::to_vec(h).map_err(db_err)?.as_slice(),
            )
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    pub fn remove_handset(&self, id: &str) -> io::Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(HANDSETS)
            .map_err(db_err)?
            .remove(id)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    // ---- messages

    /// A message, numbered at once and written down by the writer.
    pub fn add_message(
        &self,
        conv: &str,
        from: &str,
        text: &str,
        time_ms: i64,
    ) -> io::Result<Message> {
        self.add_message_by(conv, from, None, None, text, time_ms)
    }

    /// [`add_message`](Self::add_message) from a handset (`from` its driver's name shown,
    /// `from_id` its key, §V.13).
    pub fn add_message_by(
        &self,
        conv: &str,
        from: &str,
        from_id: Option<&str>,
        handset: Option<&str>,
        text: &str,
        time_ms: i64,
    ) -> io::Result<Message> {
        let seq = self.next_message.fetch_add(1, Ordering::Relaxed);
        let m = Message {
            seq,
            conv: conv.to_owned(),
            from: from.to_owned(),
            text: text.to_owned(),
            time_ms,
            handset: handset.map(str::to_owned),
            from_id: from_id.map(str::to_owned),
        };
        let mut k = conv.as_bytes().to_vec();
        k.push(0);
        k.extend_from_slice(&seq.to_be_bytes());
        self.hand(Job::Message(
            k,
            serde_json::to_vec(&m).map_err(db_err)?,
            seq + 1,
        ));
        Ok(m)
    }

    /// A conversation's last `n` messages, oldest first.
    pub fn history(&self, conv: &str, n: usize) -> io::Result<Vec<Message>> {
        self.flush();
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(MESSAGES).map_err(db_err)?;
        let mut lo = conv.as_bytes().to_vec();
        lo.push(0);
        let mut hi = conv.as_bytes().to_vec();
        hi.push(1);
        let mut out = Vec::new();
        for e in t.range(lo.as_slice()..hi.as_slice()).map_err(db_err)?.rev() {
            let (_, v) = e.map_err(db_err)?;
            if let Ok(m) = serde_json::from_slice::<Message>(v.value()) {
                out.push(m);
            }
            if out.len() >= n {
                break;
            }
        }
        out.reverse();
        Ok(out)
    }

    // ---- transmissions

    /// A transmission's number, given at once (written down with it).
    pub fn next_transmission(&self) -> io::Result<u64> {
        Ok(self.next_transmission.fetch_add(1, Ordering::Relaxed))
    }

    /// A transmission, written down by the writer.
    pub fn put_transmission(&self, t: &Transmission) -> io::Result<()> {
        self.hand(Job::Transmission(
            t.id,
            serde_json::to_vec(t).map_err(db_err)?,
        ));
        Ok(())
    }

    pub fn transmission(&self, id: u64) -> io::Result<Option<Transmission>> {
        self.flush();
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(TRANSMISSIONS).map_err(db_err)?;
        Ok(t.get(id)
            .map_err(db_err)?
            .and_then(|v| serde_json::from_slice(v.value()).ok()))
    }

    /// Transmissions that started at or after `since_ms` and match `keep`, oldest first, at
    /// most `n`.
    pub fn transmissions_since(
        &self,
        since_ms: i64,
        n: usize,
        keep: impl Fn(&Transmission) -> bool,
    ) -> io::Result<Vec<Transmission>> {
        self.flush();
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(TRANSMISSIONS).map_err(db_err)?;
        let mut out = Vec::new();
        // Newest first (ids grow with time), until older than `since_ms`.
        for e in t.iter().map_err(db_err)?.rev() {
            let (_, v) = e.map_err(db_err)?;
            let Ok(x) = serde_json::from_slice::<Transmission>(v.value()) else {
                continue;
            };
            if x.start_ms < since_ms {
                break;
            }
            if keep(&x) {
                out.push(x);
                if out.len() >= n {
                    break;
                }
            }
        }
        out.reverse();
        Ok(out)
    }

    pub fn remove_transmission(&self, id: u64) -> io::Result<()> {
        self.flush();
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(TRANSMISSIONS)
            .map_err(db_err)?
            .remove(id)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    /// Forgets messages and transmissions older than `before_ms`; the transmissions' ids
    /// (their files are the caller's to delete).
    pub fn prune(&self, before_ms: i64) -> io::Result<Vec<u64>> {
        self.flush();
        let w = self.db.begin_write().map_err(db_err)?;
        let gone;
        {
            let mut t = w.open_table(TRANSMISSIONS).map_err(db_err)?;
            let old: Vec<u64> = t
                .iter()
                .map_err(db_err)?
                .filter_map(|e| e.ok())
                .filter_map(|(k, v)| {
                    let x: Transmission = serde_json::from_slice(v.value()).ok()?;
                    (x.end_ms < before_ms).then_some(k.value())
                })
                .collect();
            for id in &old {
                t.remove(*id).map_err(db_err)?;
            }
            gone = old;
            let mut m = w.open_table(MESSAGES).map_err(db_err)?;
            m.retain(|_, v| {
                serde_json::from_slice::<Message>(v)
                    .map(|x| x.time_ms >= before_ms)
                    .unwrap_or(false)
            })
            .map_err(db_err)?;
        }
        w.commit().map_err(db_err)?;
        Ok(gone)
    }
}

impl Drop for Store {
    /// What was handed in is written before the database closes.
    fn drop(&mut self) {
        self.writer.lock_ok().take();
        if let Some(t) = self.thread.lock_ok().take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Access, Kind};

    fn dir(name: &str) -> idn_common::testing::TempDir {
        idn_common::testing::TempDir::new(&format!("voice-store-{name}"))
    }

    /// Messages and transmissions written by the writer thread are there after a restart,
    /// and their numbers go on from where they were.
    #[test]
    fn numbers_go_on_after_a_restart() {
        let d = dir("restart");
        let t = |id| Transmission {
            id,
            channel: Some(1),
            to: Vec::new(),
            from: "a".into(),
            start_ms: 1,
            end_ms: 2,
            frames: 3,
            handset: None,
            from_id: None,
        };
        {
            let s = Store::open(&d).unwrap();
            for i in 0..5 {
                s.add_message("c:1", "a", &format!("m{i}"), i).unwrap();
            }
            let id = s.next_transmission().unwrap();
            s.put_transmission(&t(id)).unwrap();
            assert_eq!(s.history("c:1", 10).unwrap().len(), 5);
        }
        let s = Store::open(&d).unwrap();
        let m = s.add_message("c:1", "a", "after", 9).unwrap();
        assert_eq!(m.seq, 6);
        assert_eq!(s.next_transmission().unwrap(), 2);
        assert!(s.transmission(1).unwrap().is_some());
        let h = s.history("c:1", 10).unwrap();
        assert_eq!(h.len(), 6);
        assert_eq!(h.last().unwrap().text, "after");
        drop(s);
    }

    #[test]
    fn channels_messages_and_transmissions_are_kept() {
        let d = dir("keep");
        let s = Store::open(&d).unwrap();
        let c = s
            .put_channel(Channel {
                id: 0,
                name: "Crew".into(),
                description: String::new(),
                kind: Kind::Walkie {
                    half_duplex: false,
                    limit_s: 60,
                },
                access: Access::Open,
                record: true,
                talk_source: None,
                as_source: false,
                store_forward_s: None,
                jitter_start_ms: None,
                jitter_max_ms: None,
            })
            .unwrap();
        assert_eq!(c.id, 1);
        let c2 = s
            .put_channel(Channel {
                id: 0,
                name: "B".into(),
                ..c.clone()
            })
            .unwrap();
        assert_eq!(c2.id, 2);
        assert_eq!(s.channels().unwrap().len(), 2);
        for i in 0..5 {
            s.add_message(&in_channel(1), "ann", &format!("m{i}"), 1000 + i)
                .unwrap();
        }
        s.add_message(&direct("bob", "ann"), "bob", "hi", 2000)
            .unwrap();
        let h = s.history(&in_channel(1), 3).unwrap();
        assert_eq!(
            h.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["m2", "m3", "m4"]
        );
        assert_eq!(s.history(&direct("ann", "bob"), 10).unwrap().len(), 1);
        let id = s.next_transmission().unwrap();
        s.put_transmission(&Transmission {
            id,
            channel: Some(1),
            to: vec![],
            from: "ann".into(),
            start_ms: 5000,
            end_ms: 6000,
            frames: 50,
            handset: Some("Truck 12".into()),
            from_id: Some("driver:D1".into()),
        })
        .unwrap();
        assert_eq!(s.transmissions_since(4000, 10, |_| true).unwrap().len(), 1);
        assert!(
            s.transmissions_since(5001, 10, |_| true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.prune(7000).unwrap(), vec![id]);
        assert!(s.history(&in_channel(1), 10).unwrap().is_empty());
        drop(s);
        let s = Store::open(&d).unwrap();
        assert_eq!(s.channels().unwrap().len(), 2);
    }

    /// Handsets (§V.13) are kept, changed and removed; a record from before a field was
    /// added loads with its default.
    #[test]
    fn handsets_are_kept() {
        let d = dir("handsets");
        let h: Handset = serde_json::from_value(serde_json::json!({
            "id": "habc", "name": "Truck 12", "created_ms": 1
        }))
        .unwrap();
        assert!(h.driver.is_none() && !h.revoked && h.secret_sha256.is_empty());
        {
            let s = Store::open(&d).unwrap();
            s.put_handset(&h).unwrap();
            s.put_handset(&Handset {
                id: "hdef".into(),
                name: "Truck 7".into(),
                revoked: true,
                ..h.clone()
            })
            .unwrap();
            s.remove_handset("hdef").unwrap();
        }
        let s = Store::open(&d).unwrap();
        assert_eq!(s.handsets().unwrap(), vec![h]);
        drop(s);
    }
}
