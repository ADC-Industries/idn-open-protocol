//! Routing presets (§A.11) and the site's templates (§N.21): saved, deleted, recalled,
//! and sent to machines.

use super::*;

/// Saves of presets and templates take turns: two at once each read the list, and the
/// second's write lost the first's change (F6-6).
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The list `key` of a saved file (none when there is no file yet).
fn read_list(path: &std::path::Path, key: &str) -> Vec<serde_json::Value> {
    idn_common::fsutil::load_saved(path, |b| {
        serde_json::from_slice::<serde_json::Value>(b)
            .ok()
            .and_then(|v| v[key].as_array().cloned())
    })
    .unwrap_or_else(|e| {
        idn_open_protocol::log::log(&format!("site manager: {e}"));
        None
    })
    .unwrap_or_default()
}

use crate::settings::MACHINE_OWN;

impl State {
    /// The routing presets (N3): `[{name, saved, receivers: [...]}]`, from their file.
    /// (A file that can't be read is set aside and said, not taken as none and written
    /// over by the next save; bug round F, F6-6.)
    pub(super) fn presets(&self) -> Vec<serde_json::Value> {
        read_list(&self.dir.join(presets::FILE), "presets")
    }

    pub(super) fn write_presets(&self, list: &[serde_json::Value]) -> Result<(), String> {
        let body = serde_json::json!({ "presets": list }).to_string();
        idn_open_protocol::trust::pki::write_private(&self.dir.join(presets::FILE), body.as_bytes())
            .map_err(|e| e.to_string())
    }

    /// Saves every receiver's routes now as `name` (replacing one of that name): how
    /// many receivers it holds.
    pub(super) fn save_preset(&self, name: &str) -> Result<usize, String> {
        let _turn = SAVING.lock_ok();
        if name.is_empty() || name.chars().count() > 60 || name.chars().any(char::is_control) {
            return Err("a preset's name: 1 to 60 characters".into());
        }
        let receivers = presets::snapshot(&self.authority.node_reports());
        if receivers.is_empty() {
            return Err("no receivers in the site to save".into());
        }
        let mut list = self.presets();
        if list.len() >= 100 && !list.iter().any(|p| p["name"] == name) {
            return Err("at most 100 presets".into());
        }
        list.retain(|p| p["name"].as_str() != Some(name));
        let saved = idn_common::epoch::secs();
        let n = receivers.len();
        list.push(serde_json::json!({"name": name, "saved": saved, "receivers": receivers}));
        list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        self.write_presets(&list)?;
        self.log(&format!(
            "panel: routing preset {name:?} saved ({n} receivers)"
        ));
        Ok(n)
    }

    pub(super) fn delete_preset(&self, name: &str) -> Result<(), String> {
        let _turn = SAVING.lock_ok();
        let mut list = self.presets();
        let before = list.len();
        list.retain(|p| p["name"].as_str() != Some(name));
        if list.len() == before {
            return Err(format!("no preset {name:?}"));
        }
        self.write_presets(&list)?;
        self.log(&format!("panel: routing preset {name:?} deleted"));
        Ok(())
    }

    /// Recalls preset `name`: each of its receivers set to its saved routes, one message
    /// a machine; each machine's result (`ok`, `unchanged`, `offline` or the error).
    pub(super) fn recall_preset(&self, name: &str) -> Result<serde_json::Value, String> {
        let p = self
            .presets()
            .into_iter()
            .find(|p| p["name"].as_str() == Some(name))
            .ok_or_else(|| format!("no preset {name:?}"))?;
        let reports = self.authority.node_reports();
        // The changes, by machine.
        let mut by_fp: std::collections::BTreeMap<
            String,
            (String, Vec<idn_open_protocol::node::routing::RouteChange>),
        > = Default::default();
        for rx in p["receivers"].as_array().into_iter().flatten() {
            let fp = rx["fingerprint"].as_str().unwrap_or("").to_owned();
            let receiver = rx["receiver"].as_str().unwrap_or("");
            let entry = by_fp
                .entry(fp.clone())
                .or_insert_with(|| (rx["machine"].as_str().unwrap_or("").to_owned(), Vec::new()));
            let now = reports
                .iter()
                .find(|r| r.fingerprint == fp)
                .map(|r| presets::current(r, receiver))
                .unwrap_or_default();
            entry
                .1
                .extend(presets::changes(receiver, &now, &presets::saved_routes(rx)));
        }
        let mut results = Vec::new();
        let mut total = 0;
        for (fp, (machine, changes)) in by_fp {
            let result = if !reports.iter().any(|r| r.fingerprint == fp) {
                "offline".to_owned()
            } else if changes.is_empty() {
                "unchanged".to_owned()
            } else {
                match self
                    .authority
                    .route_node(&fp, &changes, std::time::Duration::from_secs(5))
                {
                    Ok(reply) if reply.attr("ok") == Some("true") => {
                        total += changes.len();
                        "ok".to_owned()
                    }
                    Ok(reply) => reply
                        .attr("error")
                        .unwrap_or("the machine refused")
                        .to_owned(),
                    Err(e) => e.to_string(),
                }
            };
            results
                .push(serde_json::json!({"fingerprint": fp, "machine": machine, "result": result}));
        }
        self.log(&format!(
            "panel: routing preset {name:?} recalled ({total} route changes)"
        ));
        Ok(serde_json::json!({"ok": true, "changes": total, "results": results}))
    }

    /// The site's templates (§N.21): `[{name, settings}]`, from `templates.json`.
    pub(super) fn templates(&self) -> Vec<serde_json::Value> {
        read_list(&self.dir.join(TEMPLATES), "templates")
    }

    /// Saves (or, with no settings, deletes) the template `name` (a machine's own keys,
    /// [`MACHINE_OWN`], left out).
    pub(super) fn save_template(
        &self,
        name: &str,
        settings: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let _turn = SAVING.lock_ok();
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err("a template's name: 1 to 60 characters".into());
        }
        let mut list = self.templates();
        // Its PIN kept (a hash) when a save of its Simple mode group gives no new one.
        let kept_pin = list
            .iter()
            .find(|t| t["name"].as_str() == Some(name))
            .and_then(|t| t["settings"]["panel_pin"].as_str())
            .map(str::to_owned);
        list.retain(|t| t["name"].as_str() != Some(name));
        if let Some(mut s) = settings {
            // (The status shows a template's PIN as `pin_set`, never its hash: a template
            // read from it and saved back carries that, not a setting; bug run 2026-11-25.)
            let from_status = s
                .as_object_mut()
                .and_then(|o| o.remove("pin_set"))
                .is_some();
            let mut s = machine_settings(&s)?;
            // A machine's own settings never in a template (bug run K5-7: one written
            // straight to the API carried them to every machine it was applied to).
            for k in MACHINE_OWN {
                s.remove(*k);
            }
            // Its PIN kept when no new one is given with its Simple mode group, or when the
            // template comes back from the status (its PIN hidden there); not otherwise (a
            // template saved without the group dropping its PIN kept it; bug run B).
            if let Some(h) = kept_pin
                && (from_status || s.contains_key("panel_site_only"))
                && !s.contains_key("panel_pin")
            {
                s.insert("panel_pin".into(), h.into());
            }
            list.push(serde_json::json!({"name": name, "settings": s}));
            list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        }
        let body = serde_json::json!({ "templates": list }).to_string();
        idn_open_protocol::trust::pki::write_private(&self.dir.join(TEMPLATES), body.as_bytes())
            .map_err(|e| e.to_string())
    }

    /// Sends `settings` to each machine of `to` (§N.21): each one's result.
    pub(super) fn apply(&self, b: &[u8]) -> Result<serde_json::Value, String> {
        let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        let settings = machine_settings(&v["settings"])?;
        self.send_settings(&strings(&v["to"]), &settings)
    }

    /// `POST /api/v1/apply_template {name, to}`: the template's settings to the machines
    /// `to`, its machine-own keys left out whatever an older template holds (K5-7).
    pub(super) fn apply_template(&self, b: &[u8]) -> Result<serde_json::Value, String> {
        let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        let name = v["name"].as_str().unwrap_or("").trim();
        let t = self
            .templates()
            .into_iter()
            .find(|t| t["name"].as_str() == Some(name))
            .ok_or_else(|| format!("no template {name:?}"))?;
        let mut settings = machine_settings(&t["settings"])?;
        for k in MACHINE_OWN {
            settings.remove(*k);
        }
        self.send_settings(&strings(&v["to"]), &settings)
    }

    /// `POST /api/v1/copy_settings {from, to, groups, pin}`: the groups (default
    /// [`COPY_DEFAULT`](crate::settings::COPY_DEFAULT)) of the machine `from`'s settings
    /// now, onto the machines `to`. Never a machine's own keys nor its PIN (a `pin` given
    /// with the Simple mode group sets theirs).
    pub(super) fn copy_settings(&self, b: &[u8]) -> Result<serde_json::Value, String> {
        let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        let from = v["from"].as_str().unwrap_or("");
        let mut groups = strings(&v["groups"]);
        if groups.is_empty() {
            groups = crate::settings::COPY_DEFAULT
                .iter()
                .map(|g| (*g).to_owned())
                .collect();
        }
        let keys = crate::settings::copy_keys(&groups)?;
        let report = self
            .authority
            .node_reports()
            .into_iter()
            .find(|r| r.fingerprint == from)
            .ok_or("the machine to copy from isn't reporting now (offline, or no IDN Node)")?;
        let values = crate::settings::values_of(&report.report);
        let mut out: serde_json::Map<String, serde_json::Value> = keys
            .iter()
            .map(|k| ((*k).to_owned(), values.get(*k).cloned().unwrap_or_default()))
            .collect();
        if groups.iter().any(|g| g == "panel")
            && let Some(p) = v["pin"].as_str().filter(|p| !p.trim().is_empty())
        {
            out.insert("pin".into(), p.into());
        }
        let settings = machine_settings(&serde_json::Value::Object(out))?;
        let to: Vec<String> = strings(&v["to"])
            .into_iter()
            .filter(|fp| fp != from)
            .collect();
        self.send_settings(&to, &settings)
    }

    /// Sends checked `settings` to each machine of `to` at once: each one's result.
    fn send_settings(
        &self,
        to: &[String],
        settings: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        if to.is_empty() || settings.is_empty() {
            return Err("give the machines (to) and the settings".into());
        }
        let attrs: Vec<(String, String)> = settings
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned()))
            .collect();
        let refs: Vec<(&str, &str)> = attrs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // Every machine at once: one that is slow to answer doesn't hold up the others.
        let results: Vec<serde_json::Value> = std::thread::scope(|sc| {
            let hs: Vec<_> = to
                .iter()
                .map(|fp| {
                    let refs = &refs;
                    sc.spawn(move || {
                        let r = self.authority.set_node(fp, refs, Duration::from_secs(8));
                        let error = match r {
                            Ok(reply) if reply.attr("ok") == Some("true") => None,
                            Ok(reply) => Some(
                                reply
                                    .attr("error")
                                    .unwrap_or("the machine refused")
                                    .to_owned(),
                            ),
                            Err(e) => Some(e.to_string()),
                        };
                        serde_json::json!({"fingerprint": fp, "ok": error.is_none(), "error": error})
                    })
                })
                .collect();
            hs.into_iter()
                .map(|h| h.join().unwrap_or(serde_json::Value::Null))
                .collect()
        });
        let ok = results.iter().filter(|r| r["ok"] == true).count();
        self.log(&format!(
            "panel: {} setting(s) sent to {} machine(s), {ok} took them",
            attrs.len(),
            to.len()
        ));
        Ok(serde_json::json!({ "results": results }))
    }
}

/// A JSON list's strings.
fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
