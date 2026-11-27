//! Store and forward on disk (§I.6.3), in a redb database (crash-safe transactions): the
//! Node's outbox, and the Site Manager's tokens, machines, inbox and the ids it has seen.

use crate::wire::Item;
use idn_common::sync::LockOk as _;
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::io;
use std::path::Path;

fn err(e: impl std::fmt::Display) -> io::Error {
    idn_common::err::other(format_args!("store: {e}"))
}

/// Items by sequence number (their order of arrival).
const OUTBOX: TableDefinition<u64, &[u8]> = TableDefinition::new("outbox");
/// Counters: `dropped` (items dropped over the limits), `bytes` (the queue's size),
/// `seq` (the last sequence number given).
const COUNTS: TableDefinition<&str, u64> = TableDefinition::new("counts");
/// Commands the Node has taken (§I.7.3), by item id: `0` while running, then the status
/// and the answer. A command sent again (a Site Manager restart, a retry) is never run
/// twice.
const CLAIMS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("claims");
/// The claims in the order they were made (a sequence → the id), to keep the newest.
const CLAIM_ORDER: TableDefinition<u64, &[u8]> = TableDefinition::new("claim_order");
/// Claims kept (the oldest beyond go): the spec's 10 000 (it was 5 000, deep bug run
/// 2026-11-09).
const CLAIMS_KEPT: u64 = 10_000;
/// An answer kept for the same command again, at most (a bigger one is said to have been
/// given: 10 000 × 4 KiB keeps the table under 40 MB).
const ANSWER_KEPT: usize = 4 * 1024;

/// What a command's id says when it arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// Never seen: claimed now; run it.
    New,
    /// Claimed before and never answered: it was running when the Node stopped.
    CutShort,
    /// Answered before: the same answer again.
    Answered(u8, Vec<u8>),
}

/// An item's stored form: op, id, time, type and payload as on the wire.
pub fn encode(it: &Item) -> Vec<u8> {
    let mut b = Vec::with_capacity(30 + it.kind.len() + it.payload.len());
    b.push(it.op);
    b.extend_from_slice(&it.id);
    b.extend_from_slice(&it.time_ms.to_le_bytes());
    b.extend_from_slice(&(it.kind.len() as u16).to_le_bytes());
    b.extend_from_slice(it.kind.as_bytes());
    b.extend_from_slice(&it.payload);
    b
}

pub fn decode(b: &[u8]) -> Option<Item> {
    if b.len() < 27 {
        return None;
    }
    let tl = u16::from_le_bytes([b[25], b[26]]) as usize;
    let kind = std::str::from_utf8(b.get(27..27 + tl)?).ok()?.to_owned();
    Some(Item {
        op: b[0],
        id: b[1..17].try_into().ok()?,
        time_ms: i64::from_le_bytes(b[17..25].try_into().ok()?),
        kind,
        payload: b[27 + tl..].to_vec(),
    })
}

/// The Node's outbox: what its programs handed it, until the Site Manager has stored it.
/// Bounded by size only, never by age: the machine's clock may jump (a batch computer's
/// often does), and a jump must never delete what is waiting (§I.6.3).
pub struct Outbox {
    db: Database,
    /// Bytes allowed in the queue; beyond them the oldest values go, then the oldest of
    /// the rest (§I.6.3).
    pub max_bytes: u64,
    /// Items handed over and not yet committed, each with where its answer goes: the
    /// pushes that arrive while a commit runs go in the next one together (one disk sync
    /// for many: a machine with several programs sending at once).
    waiting: std::sync::Mutex<Vec<(Vec<u8>, Answer)>>,
    /// Held by whoever commits.
    committing: std::sync::Mutex<()>,
}

/// Where a push's sequence number (or why not) goes.
type Answer = std::sync::mpsc::SyncSender<io::Result<u64>>;

/// Default (`[outbox] max_mb` in the machine's own `node.toml`): 1 GB.
pub const OUTBOX_MAX_BYTES: u64 = 1 << 30;

impl Outbox {
    pub fn open(path: &Path) -> io::Result<Outbox> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let db = Database::create(path).map_err(err)?;
        let w = db.begin_write().map_err(err)?;
        w.open_table(OUTBOX).map_err(err)?;
        w.open_table(COUNTS).map_err(err)?;
        w.open_table(CLAIMS).map_err(err)?;
        w.open_table(CLAIM_ORDER).map_err(err)?;
        w.commit().map_err(err)?;
        Ok(Outbox {
            db,
            max_bytes: OUTBOX_MAX_BYTES,
            waiting: std::sync::Mutex::new(Vec::new()),
            committing: std::sync::Mutex::new(()),
        })
    }

    /// Stores an item; returns its sequence number. Returns only once it is on disk. Items
    /// pushed at the same time share a commit (group commit): each caller still returns
    /// only once its own item is on disk, and items keep the order they were handed over.
    pub fn push(&self, it: &Item) -> io::Result<u64> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.waiting.lock_ok().push((encode(it), tx));
        // One committer at a time; whoever gets here commits everything waiting, so a
        // caller may find its item already committed by the one before.
        let _c = self.committing.lock_ok();
        if let Ok(r) = rx.try_recv() {
            return r;
        }
        let batch = std::mem::take(&mut *self.waiting.lock_ok());
        match self.push_all(batch.iter().map(|(e, _)| e.as_slice())) {
            Ok((seqs, bytes)) => {
                for ((_, tx), seq) in batch.iter().zip(seqs) {
                    let _ = tx.send(Ok(seq));
                }
                if bytes > self.max_bytes
                    && let Err(e) = self.trim()
                {
                    idn_open_protocol::log::log(&format!("industrial: outbox trim: {e}"));
                }
            }
            Err(e) => {
                for (_, tx) in &batch {
                    let _ = tx.send(Err(io::Error::new(e.kind(), e.to_string())));
                }
            }
        }
        rx.recv()
            .unwrap_or_else(|_| Err(io::Error::other("the outbox's commit went away")))
    }

    /// Stores encoded items in one transaction: their sequence numbers, and the bytes
    /// queued after.
    fn push_all<'a>(&self, items: impl Iterator<Item = &'a [u8]>) -> io::Result<(Vec<u64>, u64)> {
        let w = self.db.begin_write().map_err(err)?;
        let mut seqs = Vec::new();
        let bytes = {
            let mut t = w.open_table(OUTBOX).map_err(err)?;
            let mut c = w.open_table(COUNTS).map_err(err)?;
            // Never reused, even once the queue has emptied: the sender keeps its place by
            // sequence.
            let last = t.last().map_err(err)?.map_or(0, |(k, _)| k.value());
            let mut seq = c
                .get("seq")
                .map_err(err)?
                .map_or(0, |v| v.value())
                .max(last);
            let mut bytes = c.get("bytes").map_err(err)?.map_or(0, |v| v.value());
            for enc in items {
                seq += 1;
                t.insert(seq, enc).map_err(err)?;
                bytes += enc.len() as u64;
                seqs.push(seq);
            }
            c.insert("seq", seq).map_err(err)?;
            c.insert("bytes", bytes).map_err(err)?;
            bytes
        };
        w.commit().map_err(err)?;
        Ok((seqs, bytes))
    }

    /// The oldest `n` items after `after` (sequence numbers), in order.
    pub fn peek(&self, after: u64, n: usize) -> io::Result<Vec<(u64, Item)>> {
        let r = self.db.begin_read().map_err(err)?;
        let t = r.open_table(OUTBOX).map_err(err)?;
        let mut out = Vec::new();
        for e in t.range(after + 1..).map_err(err)? {
            let (k, v) = e.map_err(err)?;
            if let Some(it) = decode(v.value()) {
                out.push((k.value(), it));
            }
            if out.len() >= n {
                break;
            }
        }
        Ok(out)
    }

    /// Forgets an item the Site Manager has stored.
    pub fn remove(&self, seq: u64) -> io::Result<()> {
        self.remove_all(&[seq])
    }

    /// Forgets items the Site Manager has stored, in one commit.
    pub fn remove_all(&self, seqs: &[u64]) -> io::Result<()> {
        if seqs.is_empty() {
            return Ok(());
        }
        let w = self.db.begin_write().map_err(err)?;
        {
            let mut t = w.open_table(OUTBOX).map_err(err)?;
            let mut gone = 0;
            for &seq in seqs {
                gone += t
                    .remove(seq)
                    .map_err(err)?
                    .map_or(0, |v| v.value().len() as u64);
            }
            let mut c = w.open_table(COUNTS).map_err(err)?;
            let bytes = c.get("bytes").map_err(err)?.map_or(0, |v| v.value());
            c.insert("bytes", bytes.saturating_sub(gone)).map_err(err)?;
        }
        w.commit().map_err(err)
    }

    /// Items waiting.
    pub fn len(&self) -> io::Result<u64> {
        let r = self.db.begin_read().map_err(err)?;
        r.open_table(OUTBOX).map_err(err)?.len().map_err(err)
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Claims a command by its id before it runs (on disk once this returns).
    pub fn claim(&self, id: &[u8; 16]) -> io::Result<Claim> {
        let w = self.db.begin_write().map_err(err)?;
        let out = {
            let mut t = w.open_table(CLAIMS).map_err(err)?;
            let before = t
                .get(id.as_slice())
                .map_err(err)?
                .map(|v| v.value().to_vec());
            match before {
                Some(b) if b.len() >= 2 && b[0] == 1 => Claim::Answered(b[1], b[2..].to_vec()),
                Some(_) => Claim::CutShort,
                None => {
                    t.insert(id.as_slice(), [0u8].as_slice()).map_err(err)?;
                    let mut o = w.open_table(CLAIM_ORDER).map_err(err)?;
                    let mut c = w.open_table(COUNTS).map_err(err)?;
                    let seq = c.get("claim").map_err(err)?.map_or(0, |v| v.value()) + 1;
                    c.insert("claim", seq).map_err(err)?;
                    o.insert(seq, id.as_slice()).map_err(err)?;
                    // The oldest beyond the limit go.
                    if seq > CLAIMS_KEPT
                        && let Some(old) = o.remove(seq - CLAIMS_KEPT).map_err(err)?
                    {
                        let old = old.value().to_vec();
                        t.remove(old.as_slice()).map_err(err)?;
                    }
                    Claim::New
                }
            }
        };
        w.commit().map_err(err)?;
        Ok(out)
    }

    /// Keeps a command's answer, for the same command sent again.
    pub fn answered(&self, id: &[u8; 16], status: u8, payload: &[u8]) -> io::Result<()> {
        let w = self.db.begin_write().map_err(err)?;
        {
            let mut t = w.open_table(CLAIMS).map_err(err)?;
            if t.get(id.as_slice()).map_err(err)?.is_some() {
                let mut v = vec![1, status];
                if payload.len() <= ANSWER_KEPT {
                    v.extend_from_slice(payload);
                } else {
                    v.extend_from_slice(b"(answered before; the answer was too big to keep)");
                }
                t.insert(id.as_slice(), v.as_slice()).map_err(err)?;
            }
        }
        w.commit().map_err(err)
    }

    /// The queue's size in bytes.
    pub fn bytes(&self) -> u64 {
        self.db
            .begin_read()
            .ok()
            .and_then(|r| r.open_table(COUNTS).ok())
            .and_then(|t| t.get("bytes").ok().flatten().map(|v| v.value()))
            .unwrap_or(0)
    }

    /// Items dropped over the limits so far.
    pub fn dropped(&self) -> u64 {
        self.db
            .begin_read()
            .ok()
            .and_then(|r| r.open_table(COUNTS).ok())
            .and_then(|t| t.get("dropped").ok().flatten().map(|v| v.value()))
            .unwrap_or(0)
    }

    /// Holds the queue within `max_bytes`: the oldest values first, then the oldest of
    /// the rest.
    fn trim(&self) -> io::Result<()> {
        let w = self.db.begin_write().map_err(err)?;
        let mut dropped = 0u64;
        {
            let mut t = w.open_table(OUTBOX).map_err(err)?;
            let mut total: u64 = 0;
            let mut entries = Vec::new();
            for e in t.iter().map_err(err)? {
                let (k, v) = e.map_err(err)?;
                total += v.value().len() as u64;
                entries.push((
                    k.value(),
                    v.value().len() as u64,
                    v.value().first() == Some(&crate::wire::OP_VALUES),
                ));
            }
            if total > self.max_bytes {
                for values_first in [true, false] {
                    for (k, len, is_values) in &entries {
                        if total <= self.max_bytes {
                            break;
                        }
                        if *is_values == values_first && t.remove(*k).map_err(err)?.is_some() {
                            total -= len;
                            dropped += 1;
                        }
                    }
                }
            }
        }
        {
            // The size recounted, and the drops counted.
            let t = w.open_table(OUTBOX).map_err(err)?;
            let mut bytes = 0u64;
            for e in t.iter().map_err(err)? {
                bytes += e.map_err(err)?.1.value().len() as u64;
            }
            let mut c = w.open_table(COUNTS).map_err(err)?;
            c.insert("bytes", bytes).map_err(err)?;
            let before = c.get("dropped").map_err(err)?.map_or(0, |v| v.value());
            c.insert("dropped", before + dropped).map_err(err)?;
        }
        w.commit().map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{OP_MESSAGE, OP_VALUES};

    #[test]
    fn the_outbox_keeps_order_across_a_reopen_and_drops_values_first() {
        let dir = idn_common::testing::TempDir::new("outbox");
        let path = dir.join("outbox.redb");
        let a = Item::new(OP_MESSAGE, "a.b", vec![1; 100]);
        let b = Item::new(OP_VALUES, "values", vec![2; 100]);
        let c = Item::new(OP_MESSAGE, "a.c", vec![3; 100]);
        {
            let o = Outbox::open(&path).unwrap();
            for it in [&a, &b, &c] {
                o.push(it).unwrap();
            }
        }
        let mut o = Outbox::open(&path).unwrap();
        let got: Vec<Item> = o.peek(0, 10).unwrap().into_iter().map(|(_, i)| i).collect();
        assert_eq!(got, vec![a.clone(), b.clone(), c.clone()]);
        let first = o.peek(0, 1).unwrap()[0].0;
        o.remove(first).unwrap();
        assert_eq!(o.len().unwrap(), 2);
        // Over the limit: the values item goes before the message.
        o.max_bytes = 200;
        o.push(&Item::new(OP_MESSAGE, "a.d", vec![4; 10])).unwrap();
        let kinds: Vec<String> = o
            .peek(0, 10)
            .unwrap()
            .into_iter()
            .map(|(_, i)| i.kind)
            .collect();
        assert_eq!(kinds, ["a.c", "a.d"]);
        assert_eq!(o.dropped(), 1);
        // Emptied, then a new item: a number after every earlier one.
        for (seq, _) in o.peek(0, 10).unwrap() {
            o.remove(seq).unwrap();
        }
        let n = o.push(&Item::new(OP_MESSAGE, "a.e", vec![5])).unwrap();
        assert!(n > 4, "{n}");
    }

    #[test]
    fn a_clock_jump_never_drops_anything() {
        let dir = idn_common::testing::TempDir::new("outbox-clock");
        let o = Outbox::open(&dir.join("outbox.redb")).unwrap();
        // Items timed in 1970, now, and far in the future (a clock set wrong, then fixed).
        for t in [1, crate::wire::now_ms(), i64::MAX / 2] {
            let mut it = Item::new(OP_MESSAGE, "scada.loads", vec![7; 100]);
            it.time_ms = t;
            o.push(&it).unwrap();
        }
        assert_eq!(o.len().unwrap(), 3, "under the size, all kept");
        assert_eq!(o.dropped(), 0);
        assert!(o.bytes() > 300);
    }

    #[test]
    fn a_command_is_claimed_once() {
        let dir = idn_common::testing::TempDir::new("claims");
        let path = dir.join("outbox.redb");
        let (a, b) = ([1u8; 16], [2u8; 16]);
        {
            let o = Outbox::open(&path).unwrap();
            assert_eq!(o.claim(&a).unwrap(), Claim::New);
            o.answered(&a, 0, b"done").unwrap();
            assert_eq!(o.claim(&b).unwrap(), Claim::New);
            // b never answered: the Node stops here.
        }
        let o = Outbox::open(&path).unwrap();
        assert_eq!(o.claim(&a).unwrap(), Claim::Answered(0, b"done".to_vec()));
        assert_eq!(o.claim(&b).unwrap(), Claim::CutShort);
    }
}
