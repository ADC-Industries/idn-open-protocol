//! What the broker keeps on disk: clients' sessions, messages queued for them, retained messages.

use super::*;

impl Broker {
    pub(super) fn save_session(&self, client: &str, s: Option<&SessionRec>) -> io::Result<()> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        {
            let mut t = w.open_table(SESSIONS).map_err(db_err)?;
            match s {
                Some(s) if s.expiry_s > 0 => {
                    t.insert(client, serde_json::to_vec(s).map_err(err)?.as_slice())
                        .map_err(db_err)?;
                }
                _ => {
                    t.remove(client).map_err(db_err)?;
                    let mut q = w.open_table(QUEUE).map_err(db_err)?;
                    let from = queue_key(client, 0);
                    let to = queue_key(client, u64::MAX);
                    let keys: Vec<Vec<u8>> = q
                        .range(from.as_slice()..=to.as_slice())
                        .map_err(db_err)?
                        .filter_map(|e| e.ok().map(|(k, _)| k.value().to_vec()))
                        .collect();
                    for k in keys {
                        q.remove(k.as_slice()).map_err(db_err)?;
                    }
                }
            }
        }
        w.commit().map_err(db_err)
    }

    pub(super) fn expire(&self) -> io::Result<()> {
        let now = now_ms();
        let gone: Vec<String> = {
            let st = self.0.state.lock_ok();
            st.sessions
                .iter()
                .filter(|(c, s)| {
                    !st.live.contains_key(*c)
                        && s.gone_ms
                            .is_some_and(|g| now - g > s.expiry_s as i64 * 1000)
                })
                .map(|(c, _)| c.clone())
                .collect()
        };
        for c in gone {
            self.0.state.lock_ok().sessions.remove(&c);
            self.save_session(&c, None)?;
        }
        Ok(())
    }

    /// Up to `n` stored messages for `client` after `after`: (sequence, message).
    pub(super) fn queued(&self, client: &str, after: u64, n: usize) -> io::Result<Vec<(u64, Msg)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(QUEUE).map_err(db_err)?;
        let from = queue_key(client, after + 1);
        let to = queue_key(client, u64::MAX);
        let mut out = Vec::new();
        for e in t.range(from.as_slice()..=to.as_slice()).map_err(db_err)? {
            let (k, v) = e.map_err(db_err)?;
            let seq = u64::from_be_bytes(k.value()[k.value().len() - 8..].try_into().unwrap());
            if let Some(m) = Msg::decode(v.value()) {
                out.push((seq, m));
            }
            if out.len() >= n {
                break;
            }
        }
        Ok(out)
    }

    pub(super) fn unqueue(&self, client: &str, seq: u64) -> io::Result<()> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(QUEUE)
            .map_err(db_err)?
            .remove(queue_key(client, seq).as_slice())
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    /// Retained messages matching `filter` (a machine's latest values for the web UI).
    pub fn retained_matching(&self, filter: &str) -> Vec<Msg> {
        self.retained(filter)
    }

    pub(super) fn retained(&self, filter: &str) -> Vec<Msg> {
        let Ok(r) = self.0.db.begin_read() else {
            return Vec::new();
        };
        let Ok(t) = r.open_table(RETAINED) else {
            return Vec::new();
        };
        let Ok(it) = t.iter() else { return Vec::new() };
        it.filter_map(|e| e.ok())
            .filter(|(k, _)| mb::matches(k.value(), filter))
            .filter_map(|(_, v)| Msg::decode(v.value()))
            .filter(|m| !m.expired())
            .collect()
    }
}
