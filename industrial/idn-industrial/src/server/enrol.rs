//! Enrolment by token (§I.5): tokens minted, listed and revoked, joins checked against
//! them, machines listed and revoked.

use super::*;

impl SiteManager {
    /// Mints a token valid for `ttl`, giving the machine `name` (if set) and `tags`.
    pub fn mint_token(
        &self,
        name: Option<String>,
        tags: BTreeMap<String, String>,
        ttl: Duration,
    ) -> io::Result<(String, TokenRecord)> {
        let t = Token::new(&self.ca_fingerprint(), self.0.cfg.public_urls.clone())?;
        let now = wire::now_ms();
        let rec = TokenRecord {
            id: t.id_hex(),
            secret: b64(t.secret),
            name: name.filter(|n| !n.trim().is_empty()),
            tags,
            created_ms: now,
            expires_ms: now + ttl.as_millis() as i64,
            used_by: None,
        };
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(TOKENS)
            .map_err(db_err)?
            .insert(
                rec.id.as_str(),
                serde_json::to_vec(&rec).map_err(err)?.as_slice(),
            )
            .map_err(db_err)?;
        w.commit().map_err(db_err)?;
        Ok((t.encode(), rec))
    }

    /// Every token, newest first.
    pub fn tokens(&self) -> io::Result<Vec<TokenRecord>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(TOKENS).map_err(db_err)?;
        let mut v: Vec<TokenRecord> = t
            .iter()
            .map_err(db_err)?
            .filter_map(|e| e.ok())
            .filter_map(|(_, v)| serde_json::from_slice(v.value()).ok())
            .collect();
        v.sort_by_key(|t: &TokenRecord| -t.created_ms);
        Ok(v)
    }

    /// Withdraws an unused token.
    pub fn revoke_token(&self, id: &str) -> io::Result<bool> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let gone = w
            .open_table(TOKENS)
            .map_err(db_err)?
            .remove(id)
            .map_err(db_err)?
            .is_some();
        w.commit().map_err(db_err)?;
        Ok(gone)
    }

    /// Checks a join against its token and enrols the machine (§I.5).
    pub(super) async fn redeem(&self, it: &Item) -> io::Result<Vec<u8>> {
        if it.op != wire::OP_JOIN {
            return Err(err("not a join"));
        }
        let j: serde_json::Value =
            serde_json::from_slice(&it.payload).map_err(|_| err("bad join"))?;
        let field = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        if field("edition") != crate::node::EDITION {
            return Err(err(format!(
                "this is an IDN Site Manager | Industrial; a \"{}\" Node joins an IDN Site Manager",
                field("edition")
            )));
        }
        let spki = unb64(&field("key")).ok_or_else(|| err("bad key"))?;
        let mac = unb64(&field("mac")).ok_or_else(|| err("bad proof"))?;
        let id = field("token");
        let ca_spki =
            idn_open_protocol::trust::pki::Anchor::new(self.0.authority.ca_der())?.ca_der_spki()?;
        // The token checked and used up in one transaction: one machine per token.
        let w = self.0.db.begin_write().map_err(db_err)?;
        let rec = {
            let mut t = w.open_table(TOKENS).map_err(db_err)?;
            let mut rec: TokenRecord = t
                .get(id.as_str())
                .map_err(db_err)?
                .and_then(|v| serde_json::from_slice(v.value()).ok())
                .ok_or_else(|| err("unknown token (withdrawn?)"))?;
            if rec.used_by.is_some() {
                return Err(err("token already used"));
            }
            if wire::now_ms() > rec.expires_ms {
                return Err(err("token expired"));
            }
            let secret: [u8; 32] = unb64(&rec.secret)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| err("token damaged"))?;
            if !token::same(&mac, &token::mac(&secret, "device", &spki, &ca_spki)) {
                return Err(err("the token's proof didn't match"));
            }
            rec.used_by = Some(idn_open_protocol::trust::pki::fingerprint(&spki));
            t.insert(
                id.as_str(),
                serde_json::to_vec(&rec).map_err(err)?.as_slice(),
            )
            .map_err(db_err)?;
            (rec, secret)
        };
        let name = rec.0.name.clone().unwrap_or_else(|| field("name"));
        let (cert, dev) = self.0.authority.enrol_approved(&spki, &name, "app")?;
        w.commit().map_err(db_err)?;
        let now = wire::now_ms();
        let topic = self.unique_topic(&dev.name, &dev.fingerprint);
        let m = Machine {
            fingerprint: dev.fingerprint.clone(),
            name: dev.name.clone(),
            topic,
            edition: crate::node::EDITION.into(),
            tags: rec.0.tags.clone(),
            enrolled_ms: now,
            last_seen_ms: now,
            version: String::new(),
            revoked: false,
            online: None,
        };
        self.put_machine(&m)?;
        let _ = self.0.events.send(Event::Enrolled(Box::new(m.clone())));
        self.audit(
            "machine",
            &m.topic,
            "machine.enrol",
            serde_json::json!({ "fingerprint": m.fingerprint, "name": m.name,
                "token": rec.0.id, "tags": m.tags }),
        );
        idn_open_protocol::log::log(&format!(
            "site manager: {} joined with token {}",
            m.name, rec.0.id
        ));
        let answer = serde_json::json!({
            "cert": b64(&cert),
            "crl": b64(self.0.authority.crl_der()),
            "mac": b64(token::mac(&rec.1, "authority", &spki, &ca_spki)),
            "name": m.name,
            "tags": m.tags,
        });
        serde_json::to_vec(&answer).map_err(err)
    }

    pub(super) async fn join(&self, link: &Link) -> io::Result<()> {
        let (mut s, mut r) = tokio::time::timeout(Duration::from_secs(10), link.accept())
            .await
            .map_err(|_| err("no join in time"))??;
        let it = tokio::time::timeout(Duration::from_secs(10), wire::read_item(&mut r))
            .await
            .map_err(|_| err("no join in time"))??;
        match self.redeem(&it).await {
            Ok(answer) => wire::write_answer(&mut s, wire::ST_OK, &answer).await,
            Err(e) => {
                idn_open_protocol::log::log(&format!("site manager: join refused: {e}"));
                wire::write_answer(&mut s, wire::ST_REFUSED, e.to_string().as_bytes()).await
            }
        }
    }

    /// Every machine, with whether it is connected now.
    pub fn machines(&self) -> io::Result<Vec<Machine>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(MACHINES).map_err(db_err)?;
        let online = self.0.online.lock_ok();
        let mut v: Vec<Machine> = t
            .iter()
            .map_err(db_err)?
            .filter_map(|e| e.ok())
            .filter_map(|(_, v)| serde_json::from_slice::<Machine>(v.value()).ok())
            .map(|mut m| {
                m.online = online.get(&m.fingerprint).map(|(_, o, _)| o.clone());
                m
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub(super) fn machine(&self, fp: &str) -> Option<Machine> {
        let r = self.0.db.begin_read().ok()?;
        let t = r.open_table(MACHINES).ok()?;
        let v = t.get(fp).ok()??;
        serde_json::from_slice(v.value()).ok()
    }

    /// Changes a machine's record in one write (read in it): a record read, then put back
    /// after a wait on the network, undid a revoke made meanwhile (deep bug run
    /// 2026-11-09). `None`: no such machine.
    pub(super) fn update_machine(
        &self,
        fp: &str,
        change: impl FnOnce(&mut Machine),
    ) -> io::Result<Option<Machine>> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let out = {
            let mut t = w.open_table(MACHINES).map_err(db_err)?;
            let m: Option<Machine> = t
                .get(fp)
                .map_err(db_err)?
                .and_then(|v| serde_json::from_slice(v.value()).ok());
            match m {
                Some(mut m) => {
                    change(&mut m);
                    t.insert(fp, serde_json::to_vec(&m).map_err(err)?.as_slice())
                        .map_err(db_err)?;
                    Some(m)
                }
                None => None,
            }
        };
        w.commit().map_err(db_err)?;
        Ok(out)
    }

    pub(super) fn put_machine(&self, m: &Machine) -> io::Result<()> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(MACHINES)
            .map_err(db_err)?
            .insert(
                m.fingerprint.as_str(),
                serde_json::to_vec(m).map_err(err)?.as_slice(),
            )
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    /// Gives a machine new tags (`plant=PD`, …): its status says them at once, settings
    /// pushed by tag follow them, and what it sends from now on carries them; items already
    /// waiting keep the tags they arrived with (§I.7.2).
    pub async fn set_tags(&self, fp: &str, tags: BTreeMap<String, String>) -> io::Result<()> {
        for (k, v) in &tags {
            let ok = |x: &str| {
                !x.is_empty()
                    && x.len() <= 64
                    && x.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            };
            if !ok(k) || !ok(v) {
                return Err(idn_common::err::invalid_input(
                    "a tag's key and value: letters, digits, -, _ and . (64 at most)",
                ));
            }
        }
        self.update_machine(fp, |m| m.tags = tags)?
            .ok_or_else(|| idn_common::err::invalid_input("no such machine"))?;
        self.publish_status(fp);
        self.push_settings(&format!("machine:{fp}")).await;
        Ok(())
    }

    /// Revokes a machine: its certificate refused from now on, its link closed at once.
    pub fn revoke(&self, fp: &str) -> io::Result<()> {
        self.0.authority.revoke(fp)?;
        self.update_machine(fp, |m| m.revoked = true)?;
        if let Some((l, _, _)) = self.0.online.lock_ok().remove(fp) {
            l.close("revoked");
        }
        let _ = self.0.events.send(Event::Revoked(fp.to_owned()));
        Ok(())
    }
}
