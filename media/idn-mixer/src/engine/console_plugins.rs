//! The desk's plugins: inserted, set, their editors, names, parameters and levels.

use super::*;

impl Console {
    /// Changes a strip's inserts (`{"channel": N}` or `{"bus": …}`, with `op`):
    /// `add` (`key`, optional `name`, `at`), `remove`, `move` (`index`, `to`), `bypass`
    /// (`index`, `on`). A plugin that can't be loaded isn't added.
    pub fn insert(&self, v: &Value) -> Result<(), String> {
        let index = || {
            v["index"]
                .as_u64()
                .map(|i| i as usize)
                .ok_or("index: which plugin, from 0")
        };
        // The plugin's name, from the list (which may mean a scan: not while the desk waits).
        let name = match (v["op"].as_str(), v["name"].as_str(), v["key"].as_str()) {
            (Some("add"), None, Some(key)) => {
                let list = self.shared.plugins.list(false).unwrap_or_default();
                list.as_array()
                    .and_then(|l| l.iter().find(|p| p["key"] == key))
                    .and_then(|p| p["name"].as_str())
                    .unwrap_or(key)
                    .to_owned()
            }
            (_, name, _) => name.unwrap_or("").to_owned(),
        };
        let mut added = None;
        self.edit(false, |s| {
            let (list, chain) = plugins::strip(s, v)?;
            match v["op"].as_str().unwrap_or("") {
                "add" => {
                    let key = v["key"]
                        .as_str()
                        .filter(|k| !k.is_empty())
                        .ok_or("key: the plugin, from the plugin list")?;
                    let name = name.clone();
                    let at = v["at"]
                        .as_u64()
                        .map_or(list.len(), |a| (a as usize).min(list.len()));
                    list.insert(
                        at,
                        Insert {
                            key: key.to_owned(),
                            name,
                            ..Default::default()
                        },
                    );
                    added = Some((chain, at, key.to_owned()));
                }
                "remove" => {
                    let i = index()?;
                    (i < list.len())
                        .then(|| list.remove(i))
                        .ok_or("no such plugin")?;
                }
                "move" => {
                    let i = index()?;
                    let to = v["to"].as_u64().ok_or("to: the new place, from 0")? as usize;
                    if i >= list.len() {
                        return Err("no such plugin".into());
                    }
                    let p = list.remove(i);
                    list.insert(to.min(list.len()), p);
                }
                "bypass" => {
                    let i = index()?;
                    list.get_mut(i).ok_or("no such plugin")?.bypass =
                        v["on"].as_bool().ok_or("on: true or false")?;
                }
                other => return Err(format!("op: add, remove, move or bypass, not {other:?}")),
            }
            Ok(())
        })?;
        // Added but not loaded (not a plugin, or it failed): taken out again.
        if let Some((chain, at, key)) = added
            && let Some(e) = self
                .shared
                .plugins
                .errors()
                .iter()
                .find(|e| e["strip"] == plugins::strip_name(chain) || e["strip"] == "host")
        {
            let e = e["error"].as_str().unwrap_or("").to_owned();
            // Only if it is still there (the chain may have changed in the seconds the
            // plugin took to fail: removing by place took out another plugin, or past the
            // end panicked; deep bug run 2026-11-09).
            self.edit(false, |s| {
                let list = plugins::strip(s, v)?.0;
                let i = if list.get(at).is_some_and(|p| p.key == key) {
                    Some(at)
                } else {
                    list.iter().rposition(|p| p.key == key)
                };
                if let Some(i) = i {
                    list.remove(i);
                }
                Ok(())
            })?;
            return Err(e);
        }
        Ok(())
    }

    /// Sets a plugin's parameter (`param`: its id, `value`: in its range).
    pub fn plugin_set(&self, v: &Value) -> Result<(), String> {
        let (chain, index) = self.chain_of(v)?;
        let param = v["param"].as_u64().ok_or("param: the parameter's id")? as u32;
        let value = v["value"].as_f64().ok_or("value: a number")?;
        if self
            .shared
            .plugins
            .set(chain, index, param, value)?
            .is_some()
        {
            // Its latency changed: the other strips line up again. (The chains asked for
            // before the desk is locked: they wait while plugins are built, seconds, and
            // the clock waited with them.)
            let chains = self.shared.plugins.chains();
            self.shared.desk.lock_ok().set_chains(chains);
        }
        Ok(())
    }

    /// A plugin's own window (§M.10), on this machine's desktop: `{"channel": N}` or
    /// `{"bus": …}`, `index`; `open` true or false (absent: the other way round from now);
    /// `parent`: the application's window to show it in (`x11:ID`, `hwnd:ID`; absent: a
    /// window of its own). Its knobs then move the plugin as the panel's do; the strip's
    /// saved state follows.
    pub fn plugin_editor(&self, v: &Value) -> Result<Value, String> {
        let (chain, index) = self.chain_of(v)?;
        let open = v["open"]
            .as_bool()
            .unwrap_or_else(|| !self.shared.plugins.editor_open(chain, index));
        let title = format!(
            "{} - {}",
            self.plugin_name(chain, index)
                .unwrap_or_else(|| "Plugin".into()),
            plugins::strip_name(chain)
        );
        self.shared
            .plugins
            .editor(chain, index, open, v["parent"].as_str(), &title)
    }

    pub(super) fn plugin_name(&self, chain: u32, index: usize) -> Option<String> {
        self.shared
            .plugins
            .params(chain, index)
            .ok()
            .and_then(|p| p["name"].as_str().map(str::to_owned))
    }

    /// A plugin's parameters (`{"channel": N}` or `{"bus": …}`, `index`).
    pub fn plugin_params(&self, v: &Value) -> Result<Value, String> {
        let (chain, index) = self.chain_of(v)?;
        self.shared.plugins.params(chain, index)
    }

    /// The plugins on this machine (`rescan`: look again).
    pub fn plugin_list(&self, rescan: bool) -> Result<Value, String> {
        self.shared.plugins.list(rescan)
    }

    /// A strip's plugins' output levels since last asked (`{"channel": N}` or a bus; dBFS,
    /// one per plugin): the rack's per-slot meters.
    pub fn plugin_levels(&self, v: &Value) -> Result<Value, String> {
        let mut s = self.settings();
        let (_, chain) = plugins::strip(&mut s, v)?;
        self.shared.plugins.levels(chain)
    }

    pub(super) fn chain_of(&self, v: &Value) -> Result<(u32, usize), String> {
        let mut s = self.settings();
        let (_, chain) = plugins::strip(&mut s, v)?;
        Ok((
            chain,
            v["index"].as_u64().ok_or("index: which plugin, from 0")? as usize,
        ))
    }
}
