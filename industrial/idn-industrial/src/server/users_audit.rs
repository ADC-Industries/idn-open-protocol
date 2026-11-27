//! Users and the audit, kept append-only and published on `idn/site/events`.

use super::*;

impl SiteManager {
    /// Checks a login; `None` if the name or password is wrong. The built-in `admin`
    /// uses the bootstrap password (`admin.hash`, set by `IDN_ADMIN_PASSWORD` or at first
    /// visit) and is always an administrator.
    pub fn verify_user(&self, name: &str, password: &str) -> Option<Role> {
        if name.is_empty() || name == "admin" {
            let hash = std::fs::read_to_string(self.data_dir().join("admin.hash")).ok()?;
            return (!hash.is_empty()
                && idn_open_protocol::web::verify_password(hash.trim(), password))
            .then_some(Role::Admin);
        }
        let r = self.0.db.begin_read().ok()?;
        let t = r.open_table(USERS).ok()?;
        let v = t.get(name).ok().flatten()?;
        let j: serde_json::Value = serde_json::from_str(v.value()).ok()?;
        let hash = j.get("hash")?.as_str()?;
        if idn_open_protocol::web::verify_password(hash, password) {
            Role::parse(j.get("role").and_then(|r| r.as_str()).unwrap_or("viewer"))
        } else {
            None
        }
    }

    /// Adds or changes a user.
    pub fn set_user(&self, name: &str, password: &str, role: Role) -> io::Result<()> {
        let name = name.trim();
        if name.is_empty() || name == "admin" {
            return Err(idn_common::err::invalid_input(
                "pick a name (not the built-in \"admin\")",
            ));
        }
        if password.len() < 8 {
            return Err(idn_common::err::invalid_input(
                "the password needs 8 characters or more",
            ));
        }
        let hash = idn_open_protocol::web::hash_password(password)?;
        let rec = serde_json::json!({ "hash": hash, "role": role.as_str() }).to_string();
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(USERS)
            .map_err(db_err)?
            .insert(name, rec.as_str())
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    /// The users (name, role); the built-in administrator isn't listed.
    pub fn users(&self) -> io::Result<Vec<(String, Role)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(USERS).map_err(db_err)?;
        let mut out = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (k, v) = e.map_err(db_err)?;
            let role = serde_json::from_str::<serde_json::Value>(v.value())
                .ok()
                .and_then(|j| {
                    Role::parse(j.get("role").and_then(|r| r.as_str()).unwrap_or("viewer"))
                })
                .unwrap_or(Role::Viewer);
            out.push((k.value().to_owned(), role));
        }
        Ok(out)
    }

    pub fn remove_user(&self, name: &str) -> io::Result<bool> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let gone = w
            .open_table(USERS)
            .map_err(db_err)?
            .remove(name)
            .map_err(db_err)?
            .is_some();
        w.commit().map_err(db_err)?;
        Ok(gone)
    }

    /// Records one audit entry, kept append-only (two years), and publishes it on
    /// `idn/site/events`.
    pub fn audit(&self, user: &str, machine: &str, kind: &str, detail: serde_json::Value) {
        let rec = serde_json::json!({
            "time_ms": wire::now_ms(),
            "user": user,
            "machine": machine,
            "kind": kind,
            "detail": detail,
        });
        let bytes = serde_json::to_vec(&rec).unwrap_or_default();
        if let Err(e) = self.audit_write(&bytes) {
            idn_open_protocol::log::log(&format!("site manager: audit: {e}"));
        }
        let _ = self.0.broker.publish(&Msg::new(
            "idn/site/events",
            serde_json::to_vec(&serde_json::json!({ "event": "audit", "audit": rec }))
                .unwrap_or_default(),
            1,
            false,
        ));
    }

    pub(super) fn audit_write(&self, bytes: &[u8]) -> io::Result<()> {
        const TWO_YEARS_MS: i64 = 2 * 365 * 24 * 3600 * 1000;
        let w = self.0.db.begin_write().map_err(db_err)?;
        {
            let mut c = w.open_table(COUNTS).map_err(db_err)?;
            let seq = c.get("audit").map_err(db_err)?.map_or(0, |v| v.value()) + 1;
            c.insert("audit", seq).map_err(db_err)?;
            let mut t = w.open_table(AUDIT).map_err(db_err)?;
            t.insert(seq, bytes).map_err(db_err)?;
            // Drop entries older than two years.
            let cutoff = wire::now_ms() - TWO_YEARS_MS;
            let old: Vec<u64> = t
                .iter()
                .map_err(db_err)?
                .filter_map(|e| e.ok())
                .take_while(|(_, v)| {
                    serde_json::from_slice::<serde_json::Value>(v.value())
                        .ok()
                        .and_then(|j| j.get("time_ms").and_then(|t| t.as_i64()))
                        .is_some_and(|t| t < cutoff)
                })
                .map(|(k, _)| k.value())
                .collect();
            for k in old {
                t.remove(k).map_err(db_err)?;
            }
        }
        w.commit().map_err(db_err)
    }

    /// Audit entries after sequence `after`, newest first, at most `n`.
    pub fn audit_log(&self, after: u64, n: usize) -> io::Result<Vec<(u64, serde_json::Value)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(AUDIT).map_err(db_err)?;
        // From the newest back, stopping at `n`: the table holds two years.
        let mut out = Vec::new();
        for e in t.range(after.saturating_add(1)..).map_err(db_err)?.rev() {
            if out.len() >= n {
                break;
            }
            let (k, v) = e.map_err(db_err)?;
            if let Ok(j) = serde_json::from_slice(v.value()) {
                out.push((k.value(), j));
            }
        }
        Ok(out)
    }
}
