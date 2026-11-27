//! Dead letters (§I.7.5): an item an application refused (`idn/<machine>/nack/<id>` or
//! `POST /api/dead-letters`), kept with the reason, never dropped, on the web UI and the
//! API, until someone resolves it (it stays, marked) or sends it again.
//!
//! So a refusal that comes after the item was handed over still has the item, messages
//! and files the application's side took are kept among the recent ones, the newest
//! 256 MB.

use super::*;

/// How much of what was handed over is kept for a later refusal.
const RECENT_MAX_BYTES: u64 = 256 << 20;

/// A dead letter as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dead {
    /// The item's id (hex).
    id: String,
    machine: Option<String>,
    fingerprint: Option<String>,
    kind: Option<String>,
    /// The item's op (a message, a file, …): sent again as it came (`None` with the item
    /// not kept).
    #[serde(default)]
    op: Option<u8>,
    /// The machine's tags when the item was refused, to send it again with.
    #[serde(default)]
    tags: Option<String>,
    time_ms: Option<i64>,
    arrived_ms: Option<i64>,
    /// The item's payload (base64), if it was still kept.
    payload: Option<String>,
    /// The application's reason: a code and words (e.g. `unsupported_version`,
    /// "unsupported version 2 for scada.silos").
    code: Option<String>,
    reason: String,
    by: String,
    at_ms: i64,
    /// Refused again after being sent again.
    times: u32,
    retries: u32,
    resolved: Option<Resolved>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Resolved {
    by: String,
    at_ms: i64,
    note: String,
}

fn rec_parts(rec: &[u8]) -> Option<(String, i64, Item)> {
    if rec.len() < 72 {
        return None;
    }
    let fp = String::from_utf8_lossy(&rec[..64]).into_owned();
    let at = i64::from_le_bytes(rec[64..72].try_into().ok()?);
    Some((fp, at, store::decode(&rec[72..])?))
}

impl SiteManager {
    /// Keeps a handed-over message or file among the recent ones (values aren't: they
    /// are retained topics already), the oldest going beyond the limit.
    pub(super) fn keep_recent(&self, w: &redb::WriteTransaction, rec: &[u8]) -> io::Result<()> {
        let Some((_, _, it)) = rec_parts(rec) else {
            return Ok(());
        };
        if it.op == wire::OP_VALUES {
            return Ok(());
        }
        let mut t = w.open_table(RECENT).map_err(db_err)?;
        let mut ids = w.open_table(RECENT_IDS).map_err(db_err)?;
        let mut c = w.open_table(COUNTS).map_err(db_err)?;
        let seq = c.get("recent").map_err(db_err)?.map_or(0, |v| v.value()) + 1;
        c.insert("recent", seq).map_err(db_err)?;
        let mut bytes = c
            .get("recent_bytes")
            .map_err(db_err)?
            .map_or(0, |v| v.value());
        t.insert(seq, rec).map_err(db_err)?;
        ids.insert(it.id.as_slice(), seq).map_err(db_err)?;
        bytes += rec.len() as u64;
        while bytes > RECENT_MAX_BYTES {
            let Some((k, v)) = t.pop_first().map_err(db_err)? else {
                break;
            };
            let old = v.value().to_vec();
            drop(k);
            bytes = bytes.saturating_sub(old.len() as u64);
            if let Some((_, _, o)) = rec_parts(&old) {
                ids.remove(o.id.as_slice()).map_err(db_err)?;
            }
        }
        c.insert("recent_bytes", bytes).map_err(db_err)?;
        Ok(())
    }

    /// The stored record of an item by id: among the recent ones, else still in the inbox.
    fn find_item(&self, id: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        if let Some(seq) = r
            .open_table(RECENT_IDS)
            .map_err(db_err)?
            .get(id)
            .map_err(db_err)?
            .map(|v| v.value())
            && let Some(v) = r
                .open_table(RECENT)
                .map_err(db_err)?
                .get(seq)
                .map_err(db_err)?
        {
            return Ok(Some(v.value().to_vec()));
        }
        let t = r.open_table(INBOX).map_err(db_err)?;
        for e in t.iter().map_err(db_err)? {
            let (_, v) = e.map_err(db_err)?;
            if let Some((_, _, it)) = rec_parts(v.value())
                && it.id.as_slice() == id
            {
                return Ok(Some(v.value().to_vec()));
            }
        }
        Ok(None)
    }

    fn dead_all(&self) -> io::Result<Vec<(u64, Dead)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(DEAD).map_err(db_err)?;
        let mut v = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (k, d) = e.map_err(db_err)?;
            if let Ok(d) = serde_json::from_slice::<Dead>(d.value()) {
                v.push((k.value(), d));
            }
        }
        Ok(v)
    }

    fn dead_put(&self, seq: Option<u64>, d: &Dead) -> io::Result<u64> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let seq = {
            let mut c = w.open_table(COUNTS).map_err(db_err)?;
            let seq = match seq {
                Some(s) => s,
                None => {
                    let s = c.get("dead").map_err(db_err)?.map_or(0, |v| v.value()) + 1;
                    c.insert("dead", s).map_err(db_err)?;
                    s
                }
            };
            w.open_table(DEAD)
                .map_err(db_err)?
                .insert(seq, serde_json::to_vec(d).unwrap_or_default().as_slice())
                .map_err(db_err)?;
            seq
        };
        w.commit().map_err(db_err)?;
        self.dead_announce();
        Ok(seq)
    }

    /// The open count, retained on `idn/site/dead-letters`.
    fn dead_announce(&self) {
        let open = self
            .dead_all()
            .map(|v| v.iter().filter(|(_, d)| d.resolved.is_none()).count())
            .unwrap_or(0);
        let _ = self.0.broker.publish(&Msg::new(
            "idn/site/dead-letters",
            serde_json::to_vec(&serde_json::json!({ "open": open })).unwrap_or_default(),
            1,
            true,
        ));
    }

    /// An application refused the item `id_hex` (`{"code", "reason"}`). Kept as a dead
    /// letter (with the item, if it is still kept); a second refusal of an open one counts
    /// on it. Returns its number.
    pub fn refuse(&self, id_hex: &str, body: &serde_json::Value, by: &str) -> io::Result<u64> {
        let id = idn_common::hex::unhex(id_hex)
            .filter(|b| b.len() == 16)
            .ok_or_else(|| idn_common::err::invalid_input("an item id is 32 hex digits"))?;
        let code = body.get("code").and_then(|v| v.as_str()).map(str::to_owned);
        let reason = body
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(2000)
            .collect::<String>();
        let id_hex = idn_common::hex::hex(&id);
        if let Some((seq, mut d)) = self
            .dead_all()?
            .into_iter()
            .find(|(_, d)| d.id == id_hex && d.resolved.is_none())
        {
            d.times += 1;
            d.code = code.or(d.code);
            d.reason = reason;
            d.at_ms = wire::now_ms();
            return self.dead_put(Some(seq), &d);
        }
        let found = self.find_item(&id)?.and_then(|r| rec_parts(&r));
        let m = found.as_ref().and_then(|(fp, _, _)| self.machine(fp));
        let tags = m.as_ref().map(|m| tags_text(&m.tags));
        let machine = m.map(|m| m.topic);
        let d = Dead {
            id: id_hex.clone(),
            machine,
            fingerprint: found.as_ref().map(|f| f.0.clone()),
            kind: found.as_ref().map(|f| f.2.kind.clone()),
            op: found.as_ref().map(|f| f.2.op),
            tags,
            time_ms: found.as_ref().map(|f| f.2.time_ms),
            arrived_ms: found.as_ref().map(|f| f.1),
            payload: found.as_ref().map(|f| b64(&f.2.payload)),
            code,
            reason,
            by: by.to_owned(),
            at_ms: wire::now_ms(),
            times: 1,
            retries: 0,
            resolved: None,
        };
        let seq = self.dead_put(None, &d)?;
        let ev = serde_json::json!({
            "event": "dead_letter", "dead_letter": seq, "id": d.id, "machine": d.machine,
            "type": d.kind, "code": d.code, "reason": d.reason, "time_ms": d.at_ms,
        });
        let _ = self.0.broker.publish(&Msg::new(
            "idn/site/events",
            serde_json::to_vec(&ev).unwrap_or_default(),
            1,
            false,
        ));
        self.audit(
            by,
            d.fingerprint.as_deref().unwrap_or(""),
            "dead_letter",
            ev,
        );
        Ok(seq)
    }

    fn dead_view(seq: u64, d: &Dead, with_payload: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "n": seq, "id": d.id, "machine": d.machine, "type": d.kind,
            "time_ms": d.time_ms, "arrived_ms": d.arrived_ms, "code": d.code,
            "reason": d.reason, "by": d.by, "at_ms": d.at_ms, "times": d.times,
            "retries": d.retries, "resolved": d.resolved.as_ref().map(|r| serde_json::json!({
                "by": r.by, "at_ms": r.at_ms, "note": r.note })),
            "kept": d.payload.is_some(),
        });
        if with_payload && let Some(p) = d.payload.as_deref().and_then(unb64) {
            match String::from_utf8(p) {
                Ok(t) => v["text"] = serde_json::Value::String(t),
                Err(e) => v["payload"] = serde_json::Value::String(b64(e.as_bytes())),
            }
        }
        v
    }

    /// Dead letters, newest first (open ones only unless `all`), without their items.
    pub fn dead_letters(&self, all: bool) -> io::Result<Vec<serde_json::Value>> {
        let mut v = self.dead_all()?;
        v.reverse();
        Ok(v.iter()
            .filter(|(_, d)| all || d.resolved.is_none())
            .take(1000)
            .map(|(s, d)| Self::dead_view(*s, d, false))
            .collect())
    }

    /// One dead letter with its item (`text`, or `payload` in base64).
    pub fn dead_letter(&self, seq: u64) -> io::Result<Option<serde_json::Value>> {
        Ok(self
            .dead_all()?
            .into_iter()
            .find(|(s, _)| *s == seq)
            .map(|(s, d)| Self::dead_view(s, &d, true)))
    }

    /// Marks a dead letter resolved (it stays, marked).
    pub fn resolve_dead(&self, seq: u64, by: &str, note: &str) -> io::Result<bool> {
        let Some((_, mut d)) = self.dead_all()?.into_iter().find(|(s, _)| *s == seq) else {
            return Ok(false);
        };
        d.resolved = Some(Resolved {
            by: by.to_owned(),
            at_ms: wire::now_ms(),
            note: note.chars().take(2000).collect(),
        });
        self.dead_put(Some(seq), &d)?;
        self.audit(
            by,
            d.fingerprint.as_deref().unwrap_or(""),
            "dead_letter.resolve",
            serde_json::json!({ "dead_letter": seq, "id": d.id, "note": note }),
        );
        Ok(true)
    }

    /// Publishes a dead letter's item again, as it was (with `retry` = n among its user
    /// properties), for the application to take after it was fixed.
    pub fn retry_dead(&self, seq: u64, by: &str) -> io::Result<bool> {
        let Some((_, mut d)) = self.dead_all()?.into_iter().find(|(s, _)| *s == seq) else {
            return Ok(false);
        };
        let (Some(fp), Some(payload), Some(op)) = (
            d.fingerprint.clone(),
            d.payload.as_deref().and_then(unb64),
            d.op,
        ) else {
            return Err(idn_common::err::invalid_input(
                "the item isn't kept any more; ask the machine's program to send it again",
            ));
        };
        let m = self
            .machine(&fp)
            .ok_or_else(|| idn_common::err::invalid_input("the machine is gone"))?;
        let mut it = Item::new(op, d.kind.as_deref().unwrap_or("x"), payload);
        if let Some(id) = idn_common::hex::unhex(&d.id).filter(|b| b.len() == 16) {
            it.id.copy_from_slice(&id);
        }
        it.time_ms = d.time_ms.unwrap_or(0);
        d.retries += 1;
        let tags = d.tags.clone().unwrap_or_else(|| tags_text(&m.tags));
        for mut msg in Self::to_mqtt(&m, d.arrived_ms.unwrap_or(0), &it, &tags) {
            msg.props.user.push(("retry".into(), d.retries.to_string()));
            let _ = self.0.broker.publish(&msg);
        }
        self.dead_put(Some(seq), &d)?;
        self.audit(
            by,
            &fp,
            "dead_letter.retry",
            serde_json::json!({ "dead_letter": seq, "id": d.id, "retry": d.retries }),
        );
        Ok(true)
    }
}
