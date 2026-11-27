//! The machines' links, either transport: each served, and what it sends stored (unless
//! a repeat) before it is acknowledged; the inbox read back.

use super::*;

impl SiteManager {
    /// A link, either transport: joining, or an enrolled machine.
    pub async fn link(&self, link: Link) {
        if link.alpn == crate::tls::ALPN_JOIN {
            if let Err(e) = self.join(&link).await {
                idn_open_protocol::log::log(&format!(
                    "site manager: join from {}: {e}",
                    link.remote
                ));
            }
            // The machine hangs up once it has the answer; closing first would throw the
            // answer away unsent.
            let _ = tokio::time::timeout(Duration::from_secs(5), link.closed()).await;
            link.close("done");
            return;
        }
        let anchor = self.0.authority.server_creds().anchor.clone();
        let info = match crate::tls::peer(&anchor, link.peer_certs.as_deref()) {
            Ok(Some(i)) => i,
            Ok(None) => {
                link.close("no certificate");
                return;
            }
            Err(e) => {
                link.close(&e.to_string());
                return;
            }
        };
        self.machine_link(link, info.fingerprint).await;
    }

    pub(super) async fn machine_link(&self, link: Link, fp: String) {
        // Hello first: the edition checked, the settings answered.
        let hello = async {
            let (s, mut r) = link.accept().await?;
            let it = wire::read_item(&mut r).await?;
            Ok::<_, io::Error>((s, it))
        };
        let (mut s, it) = match tokio::time::timeout(Duration::from_secs(10), hello).await {
            Ok(Ok(x)) => x,
            _ => {
                link.close("no hello");
                return;
            }
        };
        let h: serde_json::Value = serde_json::from_slice(&it.payload).unwrap_or_default();
        let edition = h.get("edition").and_then(|e| e.as_str()).unwrap_or("media");
        if it.op != wire::OP_HELLO || edition != crate::node::EDITION {
            let _ =
                wire::write_answer(&mut s, wire::ST_REFUSED, b"not an IDN Node | Industrial").await;
            link.close("edition");
            return;
        }
        let Some(mut m) = self.machine(&fp) else {
            let _ = wire::write_answer(&mut s, wire::ST_REFUSED, b"unknown machine").await;
            link.close("unknown");
            return;
        };
        let cfg = self.config_for(&m);
        if wire::write_answer(
            &mut s,
            wire::ST_OK,
            &serde_json::to_vec(&cfg).unwrap_or_default(),
        )
        .await
        .is_err()
        {
            return;
        }
        let version = h
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let seen = wire::now_ms();
        match self.update_machine(&fp, |r| {
            r.version = version;
            r.last_seen_ms = seen;
        }) {
            Ok(Some(fresh)) => m = fresh,
            Ok(None) => {
                link.close("unknown");
                return;
            }
            Err(e) => {
                idn_open_protocol::log::log(&format!("site manager: {}'s record: {e}", m.name))
            }
        }
        if m.revoked {
            link.close("revoked");
            return;
        }
        let clock_offset_ms = h
            .get("time_ms")
            .and_then(|t| t.as_i64())
            .map_or(0, |t| wire::now_ms() - t);
        if clock_offset_ms.abs() > 2000 {
            idn_open_protocol::log::log(&format!(
                "site manager: {}'s clock is {:.1} s {} the Site Manager's",
                m.name,
                clock_offset_ms.abs() as f64 / 1000.0,
                if clock_offset_ms > 0 {
                    "behind"
                } else {
                    "ahead of"
                }
            ));
        }
        let online = Online {
            transport: link.transport.to_owned(),
            address: link.remote.clone(),
            since_ms: wire::now_ms(),
            clock_offset_ms,
        };
        // A machine connected twice (a reconnect racing the old link): the newer wins.
        static LINKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let me_id = LINKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some((old, _, _)) = self
            .0
            .online
            .lock_ok()
            .insert(fp.clone(), (link.clone(), online, me_id))
        {
            old.close("replaced by a newer link");
        }
        let _ = self.0.events.send(Event::Online(fp.clone()));
        tokio::spawn(self.clone().deliver_down(fp.clone(), link.clone()));
        loop {
            let (mut s, mut r) = match link.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let me = self.clone();
            let fp = fp.clone();
            tokio::spawn(async move {
                let Ok(Ok(first)) =
                    tokio::time::timeout(Duration::from_secs(60), wire::read_item(&mut r)).await
                else {
                    return;
                };
                // A screen stream (§I.11.3): its first item names the session; the rest
                // of the stream is pictures, relayed to the browsers watching.
                if first.op == wire::OP_SCREEN {
                    let session = String::from_utf8_lossy(&first.payload).into_owned();
                    me.relay_screen(&fp, session, r).await;
                    return;
                }
                if !storable(first.op) {
                    let (st, p) = me.handle_item(&fp, first).await;
                    let _ = wire::write_answer(&mut s, st, &p).await;
                    return;
                }
                // A batch: the rest of the stream, stored in order, a status for each.
                let mut items = vec![first];
                let rest = tokio::time::timeout(Duration::from_secs(120), async {
                    loop {
                        match wire::read_item(&mut r).await {
                            Ok(it) if storable(it.op) && items.len() < 1000 => items.push(it),
                            Ok(_) => return Err(err("a batch holds items to store only")),
                            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                            Err(e) => return Err(e),
                        }
                    }
                })
                .await;
                if !matches!(rest, Ok(Ok(()))) {
                    return;
                }
                let statuses = me.handle_batch(&fp, items).await;
                let _ = wire::write_answer(&mut s, wire::ST_OK, &statuses).await;
            });
        }
        let mut on = self.0.online.lock_ok();
        let mine = on.get(&fp).is_some_and(|(_, _, id)| *id == me_id);
        if mine {
            on.remove(&fp);
        }
        drop(on);
        if !mine {
            return; // a newer link took over; it is still online
        }
        let _ = self.update_machine(&fp, |m| m.last_seen_ms = wire::now_ms());
        let _ = self.0.events.send(Event::Offline(fp));
    }

    pub(super) async fn handle_item(&self, fp: &str, it: Item) -> (u8, Vec<u8>) {
        match it.op {
            wire::OP_PING => (wire::ST_OK, Vec::new()),
            wire::OP_MESSAGE | wire::OP_VALUES | wire::OP_FILE => {
                if !wire::valid_kind(&it.kind) {
                    return (wire::ST_REFUSED, b"bad type".to_vec());
                }
                // A machine's audit messages (a write, an action, a session) go to the
                // 2-year audit as well as to the inbox (§I.11.6): once, when stored (a
                // batch sent again after a lost acknowledgement isn't a second write).
                let audit = it.kind.strip_prefix("audit.").map(|rest| {
                    let detail: serde_json::Value =
                        serde_json::from_slice(&it.payload).unwrap_or_default();
                    (rest.to_owned(), detail)
                });
                let me = self.clone();
                let fp = fp.to_owned();
                let fp2 = fp.clone();
                match tokio::task::spawn_blocking(move || me.store(&fp2, &it)).await {
                    Ok(Ok(Some(_))) => {
                        if let Some((rest, detail)) = audit {
                            let machine = self.machine(&fp).map(|m| m.topic).unwrap_or_default();
                            let user = detail
                                .get("user")
                                .and_then(|u| u.as_str())
                                .unwrap_or("machine")
                                .to_owned();
                            self.audit(&user, &machine, &rest, detail);
                        }
                        (wire::ST_OK, Vec::new())
                    }
                    Ok(Ok(None)) => (wire::ST_DUPLICATE, Vec::new()),
                    Ok(Err(e)) => (wire::ST_ERROR, e.to_string().into_bytes()),
                    Err(e) => (wire::ST_ERROR, e.to_string().into_bytes()),
                }
            }
            // An update's pieces (§I.17): answered here, not by the application.
            wire::OP_REQUEST if it.kind == "idn.update.piece" => {
                let me = self.clone();
                tokio::task::spawn_blocking(move || me.update_piece(&it))
                    .await
                    .unwrap_or((wire::ST_ERROR, Vec::new()))
            }
            wire::OP_REQUEST => self.machine_request(fp, it).await,
            _ => (wire::ST_UNKNOWN, Vec::new()),
        }
    }

    /// Stored items after `after`, at most `n`: (sequence, machine fingerprint, arrival
    /// ms, item).
    pub fn inbox(&self, after: u64, n: usize) -> io::Result<Vec<(u64, String, i64, Item)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(INBOX).map_err(db_err)?;
        let mut out = Vec::new();
        for e in t.range(after + 1..).map_err(db_err)? {
            let (k, v) = e.map_err(db_err)?;
            let b = v.value();
            if b.len() < 72 {
                continue;
            }
            let fp = String::from_utf8_lossy(&b[..64]).into_owned();
            let at = i64::from_le_bytes(b[64..72].try_into().unwrap());
            if let Some(it) = store::decode(&b[72..]) {
                out.push((k.value(), fp, at, it));
            }
            if out.len() >= n {
                break;
            }
        }
        Ok(out)
    }

    /// The machine's tags when inbox item `seq` arrived (kept with each item; `None` if
    /// they can't be read: its machine's tags now are used).
    pub fn inbox_tags(&self, seq: u64) -> Option<String> {
        let r = self.0.db.begin_read().ok()?;
        let t = r.open_table(INBOX_TAGS).ok()?;
        t.get(seq).ok()?.map(|v| v.value().to_owned())
    }

    /// Forgets an inbox item the application's side has taken; a message or file is kept
    /// a while longer among the recent ones, so a later refusal still has it (§I.7.5).
    pub fn inbox_done(&self, seq: u64) -> io::Result<()> {
        self.inbox_done_all(&[seq])
    }

    /// [`inbox_done`](Self::inbox_done) for several, in one commit.
    pub fn inbox_done_all(&self, seqs: &[u64]) -> io::Result<()> {
        if seqs.is_empty() {
            return Ok(());
        }
        let w = self.0.db.begin_write().map_err(db_err)?;
        for &seq in seqs {
            let gone = w
                .open_table(INBOX)
                .map_err(db_err)?
                .remove(seq)
                .map_err(db_err)?
                .map(|v| v.value().to_vec());
            w.open_table(INBOX_TAGS)
                .map_err(db_err)?
                .remove(seq)
                .map_err(db_err)?;
            if let Some(rec) = gone {
                self.keep_recent(&w, &rec)?;
            }
        }
        w.commit().map_err(db_err)
    }

    /// Stores an item from `fp` unless its id was seen; `Ok(None)`: a repeat.
    pub(super) fn store(&self, fp: &str, it: &Item) -> io::Result<Option<u64>> {
        Ok(self.store_all(fp, vec![it.clone()])?.pop().flatten())
    }

    /// Stores items from `fp`, in order, each unless its id was seen: its inbox sequence,
    /// or `None` for a repeat. Returns once they are on disk. Batches from machines arriving
    /// while a commit runs are stored together in the next one (group commit): one disk
    /// sync for many machines' batches, each still answered only once it is stored.
    pub(super) fn store_all(&self, fp: &str, items: Vec<Item>) -> io::Result<Vec<Option<u64>>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.0
            .store_waiting
            .lock_ok()
            .push((fp.to_owned(), items, tx));
        let _c = self.0.store_committing.lock_ok();
        if let Ok(r) = rx.try_recv() {
            return r;
        }
        let batches = std::mem::take(&mut *self.0.store_waiting.lock_ok());
        match self.store_batches(&batches) {
            Ok(results) => {
                for ((_, _, tx), r) in batches.iter().zip(results) {
                    let _ = tx.send(Ok(r));
                }
            }
            Err(e) => {
                for (_, _, tx) in &batches {
                    let _ = tx.send(Err(io::Error::new(e.kind(), e.to_string())));
                }
            }
        }
        rx.recv()
            .unwrap_or_else(|_| Err(io::Error::other("the store's commit went away")))
    }

    /// Machines' batches stored in one transaction.
    #[allow(clippy::type_complexity)]
    fn store_batches(
        &self,
        batches: &[(
            String,
            Vec<Item>,
            std::sync::mpsc::SyncSender<io::Result<Vec<Option<u64>>>>,
        )],
    ) -> io::Result<Vec<Vec<Option<u64>>>> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let mut out = Vec::with_capacity(batches.len());
        {
            let mut seen = w.open_table(SEEN).map_err(db_err)?;
            let mut inbox = w.open_table(INBOX).map_err(db_err)?;
            let mut inbox_tags = w.open_table(INBOX_TAGS).map_err(db_err)?;
            // Never reused once the inbox has emptied: delivery keeps its place by it.
            let mut c = w.open_table(COUNTS).map_err(db_err)?;
            let last = inbox.last().map_err(db_err)?.map_or(0, |(k, _)| k.value());
            let mut seq = c
                .get("inbox")
                .map_err(db_err)?
                .map_or(0, |v| v.value())
                .max(last);
            for (fp, items, _) in batches {
                // The machine's tags now, kept with each item (§I.7.2).
                let tags = self
                    .machine(fp)
                    .map(|m| tags_text(&m.tags))
                    .unwrap_or_default();
                let mut r = Vec::with_capacity(items.len());
                for it in items {
                    let mut key = fp.as_bytes().to_vec();
                    key.extend_from_slice(&it.id);
                    if seen.get(key.as_slice()).map_err(db_err)?.is_some() {
                        r.push(None);
                        continue;
                    }
                    seen.insert(key.as_slice(), wire::now_ms())
                        .map_err(db_err)?;
                    seq += 1;
                    let mut b = fp.as_bytes().to_vec();
                    b.extend_from_slice(&wire::now_ms().to_le_bytes());
                    b.extend_from_slice(&store::encode(it));
                    inbox.insert(seq, b.as_slice()).map_err(db_err)?;
                    inbox_tags.insert(seq, tags.as_str()).map_err(db_err)?;
                    r.push(Some(seq));
                }
                out.push(r);
            }
            c.insert("inbox", seq).map_err(db_err)?;
        }
        w.commit().map_err(db_err)?;
        for s in out.iter().flatten().flatten() {
            let _ = self.0.events.send(Event::Stored(*s));
        }
        Ok(out)
    }

    /// A batch of items to store (messages, values, files) from `fp`: a status for each,
    /// the valid ones stored in one transaction (§I.6: each answered only once stored).
    pub(super) async fn handle_batch(&self, fp: &str, items: Vec<Item>) -> Vec<u8> {
        let mut statuses = vec![wire::ST_OK; items.len()];
        let mut keep = Vec::with_capacity(items.len());
        for (i, it) in items.into_iter().enumerate() {
            if wire::valid_kind(&it.kind) {
                keep.push((i, it));
            } else {
                statuses[i] = wire::ST_REFUSED;
            }
        }
        let me = self.clone();
        let fp2 = fp.to_owned();
        let (keep, stored) = tokio::task::spawn_blocking(move || {
            let r = me.store_all(&fp2, keep.iter().map(|(_, it)| it.clone()).collect());
            (keep, r)
        })
        .await
        .unwrap_or_else(|e| (Vec::new(), Err(idn_common::err::other(e))));
        match stored {
            Ok(seqs) => {
                let machine = self.machine(fp).map(|m| m.topic).unwrap_or_default();
                for ((i, it), seq) in keep.iter().zip(seqs) {
                    if seq.is_none() {
                        statuses[*i] = wire::ST_DUPLICATE;
                        continue;
                    }
                    // A machine's audit messages (a write, an action, a session) go to the
                    // 2-year audit as well as to the inbox (§I.11.6): once, when stored (a
                    // batch sent again after a lost acknowledgement isn't a second write).
                    if let Some(rest) = it.kind.strip_prefix("audit.") {
                        let detail: serde_json::Value =
                            serde_json::from_slice(&it.payload).unwrap_or_default();
                        let user = detail
                            .get("user")
                            .and_then(|u| u.as_str())
                            .unwrap_or("machine")
                            .to_owned();
                        self.audit(&user, &machine, rest, detail);
                    }
                }
            }
            Err(_) => {
                for (i, _) in &keep {
                    statuses[*i] = wire::ST_ERROR;
                }
                // (A join failure lost `keep`: every valid item errs.)
                if keep.is_empty() {
                    for s in statuses.iter_mut().filter(|s| **s == wire::ST_OK) {
                        *s = wire::ST_ERROR;
                    }
                }
            }
        }
        statuses
    }

    pub(super) fn prune_seen(&self) -> io::Result<()> {
        let cutoff = wire::now_ms() - SEEN_FOR_MS;
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(SEEN)
            .map_err(db_err)?
            .retain(|_, t| t >= cutoff)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    /// Sends a request to a connected machine's program and waits for its answer
    /// (status, payload).
    pub async fn command(
        &self,
        fp: &str,
        kind: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<(u8, Vec<u8>)> {
        let it = Item::new(wire::OP_REQUEST, kind, payload);
        self.command_item(fp, &it, timeout).await
    }

    /// Sends a command item (its id fixed, so the machine can tell it again) to a
    /// connected machine and waits for the answer.
    pub async fn command_item(
        &self,
        fp: &str,
        it: &Item,
        timeout: Duration,
    ) -> io::Result<(u8, Vec<u8>)> {
        let link = self
            .0
            .online
            .lock_ok()
            .get(fp)
            .map(|(l, _, _)| l.clone())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "the machine isn't connected")
            })?;
        tokio::time::timeout(timeout, async {
            let (mut s, mut r) = link.open().await?;
            wire::write_item(&mut s, it).await?;
            wire::read_answer(&mut r).await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no answer in time"))?
    }
}
