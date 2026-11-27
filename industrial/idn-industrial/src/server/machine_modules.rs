//! Modules and settings for the machines: kept, removed, given to each by scope, and
//! pushed to the connected ones.

use super::*;

impl SiteManager {
    /// Every stored module file: (scope, name, text); scope `machine:<fp>` or
    /// `tag:<key>=<value>`.
    pub fn module_files(&self) -> io::Result<Vec<(String, String, String)>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(MODULES).map_err(db_err)?;
        let mut v = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (k, val) = e.map_err(db_err)?;
            if let Some((scope, name)) = k.value().rsplit_once(':') {
                v.push((scope.to_owned(), name.to_owned(), val.value().to_owned()));
            }
        }
        Ok(v)
    }

    /// Checks and stores a module file for `scope` (`machine:<fp>` or `tag:<k>=<v>`),
    /// then sends the machines it applies to their new settings. Refused: a file that
    /// isn't valid, or names actions (only a machine's own files may).
    pub async fn put_module(&self, scope: &str, name: &str, text: &str) -> io::Result<Vec<String>> {
        let invalid = |m: String| idn_common::err::invalid_input(m);
        if !(scope.starts_with("machine:") || scope.starts_with("tag:") && scope.contains('='))
            || name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(invalid(
                "scope: machine:<fingerprint> or tag:<key>=<value>; name: letters, digits, - and _"
                    .into(),
            ));
        }
        let parsed: crate::modules::Settings =
            toml::from_str(text).map_err(|e| invalid(format!("not valid: {e}")))?;
        if !parsed.action.is_empty() {
            return Err(invalid(
                "actions can only be set in a machine's own files".into(),
            ));
        }
        let mut warnings = Vec::new();
        for d in &parsed.modbus {
            for p in d.points.iter().filter(|p| p.writable) {
                warnings.push(format!(
                    "{}/{}: writable is ignored here (only a machine's own files can allow writes)",
                    d.name, p.name
                ));
            }
        }
        if parsed.mqtt.is_some() {
            warnings.push("[mqtt] is ignored here (it is set on the machine itself)".into());
        }
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(MODULES)
            .map_err(db_err)?
            .insert(format!("{scope}:{name}").as_str(), text)
            .map_err(db_err)?;
        w.commit().map_err(db_err)?;
        self.push_settings(scope).await;
        Ok(warnings)
    }

    pub async fn remove_module(&self, scope: &str, name: &str) -> io::Result<bool> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let gone = w
            .open_table(MODULES)
            .map_err(db_err)?
            .remove(format!("{scope}:{name}").as_str())
            .map_err(db_err)?
            .is_some();
        w.commit().map_err(db_err)?;
        if gone {
            self.push_settings(scope).await;
        }
        Ok(gone)
    }

    /// The module files for a machine: those of its tags, then its own (a name of its
    /// own wins).
    pub fn modules_for(&self, m: &Machine) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let files = self.module_files().unwrap_or_default();
        for (scope, name, text) in &files {
            if let Some(tag) = scope.strip_prefix("tag:")
                && let Some((k, v)) = tag.split_once('=')
                && m.tags.get(k).is_some_and(|x| x == v)
            {
                out.insert(name.clone(), text.clone());
            }
        }
        for (scope, name, text) in &files {
            if scope.strip_prefix("machine:") == Some(m.fingerprint.as_str()) {
                out.insert(name.clone(), text.clone());
            }
        }
        out
    }

    /// What a machine is told at hello and on a change: the CRL and its modules.
    pub(super) fn config_for(&self, m: &Machine) -> serde_json::Value {
        serde_json::json!({
            "crl": b64(self.0.authority.crl_der()),
            "modules": self.modules_for(m),
        })
    }

    /// Sends the connected machines a scope applies to their settings now.
    pub(super) async fn push_settings(&self, scope: &str) {
        let machines = self.machines().unwrap_or_default();
        for m in machines.into_iter().filter(|m| m.online.is_some()) {
            let applies = match scope.split_once(':') {
                Some(("machine", fp)) => fp == m.fingerprint,
                Some(("tag", kv)) => kv
                    .split_once('=')
                    .is_some_and(|(k, v)| m.tags.get(k).is_some_and(|x| x == v)),
                _ => false,
            };
            if !applies {
                continue;
            }
            let Some(link) = self
                .0
                .online
                .lock_ok()
                .get(&m.fingerprint)
                .map(|(l, _, _)| l.clone())
            else {
                continue;
            };
            let it = Item::new(
                wire::OP_CONFIG,
                "idn.config",
                serde_json::to_vec(&self.config_for(&m)).unwrap_or_default(),
            );
            let r = tokio::time::timeout(Duration::from_secs(10), async {
                let (mut s, mut r) = link.open().await?;
                wire::write_item(&mut s, &it).await?;
                tokio::io::AsyncWriteExt::shutdown(&mut s).await?;
                wire::read_answer(&mut r).await
            })
            .await;
            if !matches!(r, Ok(Ok((wire::ST_OK, _)))) {
                // It gets them at its next connection.
                idn_open_protocol::log::log(&format!(
                    "site manager: settings to {}: not taken now",
                    m.name
                ));
            }
        }
    }
}
