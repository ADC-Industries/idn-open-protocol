//! What goes down to a machine: stored, sent in order while its link lasts, and done.

use super::*;

impl SiteManager {
    pub(super) fn downlink_notify(&self, fp: &str) -> Arc<tokio::sync::Notify> {
        self.0
            .downlink
            .lock_ok()
            .entry(fp.to_owned())
            .or_default()
            .clone()
    }

    /// Stores an item for a machine; it goes when the machine is connected, in order.
    pub fn send_down(&self, fp: &str, it: &Item) -> io::Result<()> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        {
            let mut c = w.open_table(COUNTS).map_err(db_err)?;
            let seq = c.get("outbound").map_err(db_err)?.map_or(0, |v| v.value()) + 1;
            c.insert("outbound", seq).map_err(db_err)?;
            let mut key = fp.as_bytes().to_vec();
            key.extend_from_slice(&seq.to_be_bytes());
            w.open_table(OUTBOUND)
                .map_err(db_err)?
                .insert(key.as_slice(), store::encode(it).as_slice())
                .map_err(db_err)?;
        }
        w.commit().map_err(db_err)?;
        self.downlink_notify(fp).notify_one();
        Ok(())
    }

    /// The oldest item waiting for `fp`: (key, item).
    pub(super) fn next_down(&self, fp: &str) -> io::Result<Option<(Vec<u8>, Item)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(OUTBOUND).map_err(db_err)?;
        let from = fp.as_bytes().to_vec();
        let mut to = from.clone();
        to.extend_from_slice(&[0xff; 8]);
        for e in t.range(from.as_slice()..=to.as_slice()).map_err(db_err)? {
            let (k, v) = e.map_err(db_err)?;
            if let Some(it) = store::decode(v.value()) {
                return Ok(Some((k.value().to_vec(), it)));
            }
        }
        Ok(None)
    }

    pub(super) fn done_down(&self, key: &[u8]) -> io::Result<()> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(OUTBOUND)
            .map_err(db_err)?
            .remove(key)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    /// Items waiting for a machine.
    pub fn waiting_down(&self, fp: &str) -> usize {
        let Ok(r) = self.0.db.begin_read() else {
            return 0;
        };
        let Ok(t) = r.open_table(OUTBOUND) else {
            return 0;
        };
        let from = fp.as_bytes().to_vec();
        let mut to = from.clone();
        to.extend_from_slice(&[0xff; 8]);
        t.range(from.as_slice()..=to.as_slice())
            .map_or(0, |i| i.count())
    }

    /// Sends what waits for the machine, in order, while its link lasts.
    pub(super) async fn deliver_down(self, fp: String, link: Link) {
        let wake = self.downlink_notify(&fp);
        loop {
            let next = {
                let me = self.clone();
                let fp = fp.clone();
                tokio::task::spawn_blocking(move || me.next_down(&fp)).await
            };
            let Ok(Ok(next)) = next else { return };
            let Some((key, it)) = next else {
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = link.closed() => return,
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                continue;
            };
            let sent = async {
                let (mut s, mut r) = link.open().await?;
                wire::write_item(&mut s, &it).await?;
                tokio::io::AsyncWriteExt::shutdown(&mut s).await?;
                tokio::time::timeout(Duration::from_secs(60), wire::read_answer(&mut r))
                    .await
                    .map_err(|_| err("no answer"))?
            }
            .await;
            match sent {
                // Taken (or refused for good: not sent again, so one item can't block
                // the rest).
                Ok((st, _)) if st != wire::ST_ERROR => {
                    let _ = self.done_down(&key);
                }
                _ => return,
            }
        }
    }
}
