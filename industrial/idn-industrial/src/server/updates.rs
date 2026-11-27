//! Updates of the machines (§I.17): signed releases of IDN Industrial
//! kept on the volume (`releases/<version>.idnrelease`, each checked against this
//! build's release keys and its product as it comes), offered to one machine, a tag or
//! every machine (the command `idn.update.offer`, held and claimed like any command, for a
//! day), the pieces each machine fetches over its link (`idn.update.piece`, answered
//! here), rollback by hand (`idn.update.rollback`), and machines pinned to what they run.

use super::*;
use idn_open_protocol::release::{self, Release};

/// The longest piece a machine is sent.
const PIECE: usize = 1 << 20;
/// How long an offer waits for a machine that isn't connected.
const OFFER_FOR_MS: i64 = 24 * 3600 * 1000;

use idn_common::err::invalid_input as invalid;

impl SiteManager {
    /// The releases kept (`releases/`): IDN Industrial's only (another product's is refused
    /// as it comes, and not offered if put there by hand), each checked against this
    /// build's release keys whenever it is opened; pieces of at most [`PIECE`].
    fn release_store(&self) -> release::Store {
        release::Store::new(self.0.cfg.data_dir.join("releases"))
            .only_product(release::INDUSTRIAL)
            .max_piece(PIECE)
    }

    fn release(&self, version: &str) -> io::Result<Release> {
        self.release_store().open(version)
    }

    /// Takes a release file (uploaded to `path`): kept if it is signed by a key this build
    /// trusts and is IDN Industrial's (else removed, `InvalidInput` with why). Its manifest.
    pub fn add_release(&self, path: &std::path::Path, by: &str) -> io::Result<serde_json::Value> {
        let m = self.release_store().add(path)?;
        self.audit(
            by,
            "",
            "release.add",
            serde_json::json!({ "version": m.version }),
        );
        Ok(m.to_json())
    }

    /// The releases kept, newest first.
    pub fn releases(&self) -> Vec<serde_json::Value> {
        self.release_store()
            .list()
            .into_iter()
            .map(|r| {
                let m = r.manifest;
                serde_json::json!({
                    "version": m.version, "notes": m.notes, "created": m.created,
                    "platforms": m.packages.iter().map(|p| serde_json::json!({
                        "platform": p.platform, "name": p.name, "size": p.size,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    /// Removes a release kept here.
    pub fn remove_release(&self, version: &str, by: &str) -> io::Result<bool> {
        let gone = self.release_store().remove(version)?;
        if gone {
            self.audit(
                by,
                "",
                "release.remove",
                serde_json::json!({ "version": version }),
            );
        }
        Ok(gone)
    }

    /// A machine's `idn.update.piece` `{"version", "platform", "offset", "len"}`: those
    /// bytes of its package.
    pub(super) fn update_piece(&self, it: &Item) -> (u8, Vec<u8>) {
        let ask: serde_json::Value = serde_json::from_slice(&it.payload).unwrap_or_default();
        let version = ask["version"].as_str().unwrap_or("");
        let platform = ask["platform"].as_str().unwrap_or("");
        let offset = ask["offset"].as_u64().unwrap_or(0);
        let len = ask["len"].as_u64().unwrap_or(PIECE as u64) as usize;
        match self.release_store().piece(version, platform, offset, len) {
            Ok(b) => (wire::ST_OK, b),
            Err(e) => (wire::ST_ERROR, e.to_string().into_bytes()),
        }
    }

    /// The pinned machines; an error when the file is there but can't be read (left in
    /// place: an offer to a tag or every machine then waits, rather than reaching the
    /// machines held back, and no pin is written over the others; bug review 3).
    fn pinned(&self) -> io::Result<Vec<String>> {
        let path = self.0.cfg.data_dir.join("updates.json");
        let b = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let v = serde_json::from_slice::<serde_json::Value>(&b).map_err(|e| {
            idn_common::err::invalid_data(format_args!("{}: can't be read: {e}", path.display()))
        })?;
        Ok(v["pinned"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Machines kept on what they run (offers to a tag or every machine skip them).
    /// (An unreadable pins file is said, not shown as none pinned.)
    pub fn update_pins(&self) -> io::Result<Vec<String>> {
        self.pinned()
    }

    /// Pins a machine (`true`) or lets it be updated again.
    pub fn set_pin(&self, fp: &str, on: bool, by: &str) -> io::Result<()> {
        // (Read and written as one: two pins at once lost one.)
        static CHANGING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _changing = CHANGING.lock_ok();
        let mut p = self.pinned()?;
        p.retain(|x| x != fp);
        if on {
            p.push(fp.to_owned());
        }
        idn_common::fsutil::write_atomic(
            &self.0.cfg.data_dir.join("updates.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({ "pinned": p })).unwrap_or_default(),
        )?;
        self.audit(by, fp, "update.pin", serde_json::json!({ "pinned": on }));
        if on {
            // An offer held for it from before the pin isn't installed when it connects.
            self.cancel_updates_for(fp, by);
        }
        Ok(())
    }

    /// The machines a scope names: `all`, `machine:<fingerprint>` or `tag:<key>=<value>`
    /// (not revoked; a pinned one only when named itself).
    fn scope_machines(&self, scope: &str) -> io::Result<Vec<Machine>> {
        // (Only `all` and tag offers skip pinned machines: one named needs no pin list.)
        let pinned = if scope.starts_with("machine:") {
            Vec::new()
        } else {
            self.pinned()?
        };
        let all: Vec<Machine> = self
            .machines()?
            .into_iter()
            .filter(|m| !m.revoked)
            .collect();
        Ok(if let Some(fp) = scope.strip_prefix("machine:") {
            all.into_iter().filter(|m| m.fingerprint == fp).collect()
        } else if let Some(tag) = scope.strip_prefix("tag:") {
            let (k, v) = tag
                .split_once('=')
                .ok_or_else(|| invalid("a tag scope is tag:<key>=<value>"))?;
            all.into_iter()
                .filter(|m| m.tags.get(k).map(String::as_str) == Some(v))
                .filter(|m| !pinned.contains(&m.fingerprint))
                .collect()
        } else if scope == "all" {
            all.into_iter()
                .filter(|m| !pinned.contains(&m.fingerprint))
                .collect()
        } else {
            return Err(invalid(
                "a scope is all, machine:<fingerprint> or tag:<key>=<value>",
            ));
        })
    }

    /// Holds a command of the Site Manager's own for a machine (no response topic: its
    /// answer goes to the audit).
    fn hold_own(&self, m: &Machine, kind: &str, payload: Vec<u8>, for_ms: i64) {
        let held = broker_side::Held::own(
            m,
            &Item::new(wire::OP_REQUEST, kind, payload),
            wire::now_ms() + for_ms,
        );
        let me = self.clone();
        let m_name = m.name.clone();
        tokio::spawn(async move {
            let h = held.clone();
            let m2 = me.clone();
            match tokio::task::spawn_blocking(move || m2.hold_command_pub(&h)).await {
                Ok(Ok(())) => me.run_held_pub(held).await,
                // (Not held: said in the log, not dropped without a word.)
                r => idn_open_protocol::log::log(&format!(
                    "site manager: an update command for {} couldn't be held: {}",
                    m_name,
                    match r {
                        Ok(Err(e)) => e.to_string(),
                        Err(e) => e.to_string(),
                        Ok(Ok(())) => String::new(),
                    }
                )),
            }
        });
    }

    /// Offers `version` to the machines of `scope`, `when` `now` or `restart`. The
    /// machines it went to (each gets it when connected, within a day).
    pub fn offer_update(
        &self,
        version: &str,
        scope: &str,
        when: &str,
        by: &str,
    ) -> io::Result<Vec<String>> {
        if when != "now" && when != "restart" {
            return Err(invalid("when is now or restart"));
        }
        let r = self.release(version).map_err(|e| invalid(e.to_string()))?;
        let b64 = |b: &[u8]| idn_common::text::base64(b);
        let body = serde_json::json!({
            "manifest": b64(&r.manifest_bytes), "signature": b64(&r.signature), "when": when,
        });
        let machines = self.scope_machines(scope)?;
        for m in &machines {
            // The newest offer only: two held for one machine would run in any order.
            self.cancel_updates_for(&m.fingerprint, by);
            self.hold_own(
                m,
                "idn.update.offer",
                serde_json::to_vec(&body).unwrap_or_default(),
                OFFER_FOR_MS,
            );
        }
        let names: Vec<String> = machines.iter().map(|m| m.name.clone()).collect();
        self.audit(
            by,
            "",
            "update.offer",
            serde_json::json!({ "version": version, "scope": scope, "when": when, "machines": names }),
        );
        Ok(names)
    }

    /// Puts a machine's programs back as they were before its last update.
    pub fn rollback_machine(&self, fp: &str, by: &str) -> io::Result<()> {
        let m = self.machine(fp).ok_or_else(|| invalid("no such machine"))?;
        self.cancel_updates_for(fp, by);
        self.hold_own(&m, "idn.update.rollback", b"{}".to_vec(), 3600 * 1000);
        self.audit(by, fp, "update.rollback", serde_json::json!({}));
        Ok(())
    }
}
