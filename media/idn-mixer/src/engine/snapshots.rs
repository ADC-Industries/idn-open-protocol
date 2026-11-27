//! Snapshots and presets: saved, recalled (whole or scoped), deleted; channels copied.

use super::*;

impl Console {
    /// Puts preset `name` on channel `n` (its processing; name, input, fader, pan, sends
    /// and routing stay). On a linked pair's second: on the pair (its first).
    pub fn preset_load(&self, name: &str, n: usize) -> Result<(), String> {
        let p = self
            .shared
            .presets
            .lock_ok()
            .get(name)
            .cloned()
            .ok_or("no such preset")?;
        self.edit_loading(|s| {
            let i = lead_index(s, n)?;
            p.apply(&mut s.channels[i]);
            Ok(())
        })
    }

    /// Saves channel `n`'s processing as preset `name` (§M.11).
    pub fn preset_save(&self, name: &str, n: usize) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a preset needs a name".into());
        }
        store_states(&self.shared, true);
        let s = self.settings();
        // (A pair's second: the pair's, its plugins being on the first.)
        let c = &s.channels[lead_index(&s, n)?];
        self.shared
            .presets
            .lock_ok()
            .insert(name.to_owned(), ChannelPreset::of(c));
        self.save_presets()?;
        self.changed();
        Ok(())
    }

    pub fn preset_delete(&self, name: &str) -> Result<(), String> {
        self.shared
            .presets
            .lock_ok()
            .remove(name)
            .ok_or("no such preset")?;
        self.save_presets()?;
        self.changed();
        Ok(())
    }

    pub(super) fn save_presets(&self) -> Result<(), String> {
        let Some(dir) = &self.shared.cfg.state_dir else {
            return Ok(());
        };
        let b = serde_json::to_vec_pretty(&*self.shared.presets.lock_ok())
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir)
            .and_then(|()| idn_common::fsutil::write_atomic(&dir.join(PRESETS_FILE), &b))
            .map_err(|e| e.to_string())
    }

    /// Saves the desk as snapshot `name` (with every plugin's state as it is now), without
    /// the IEM mixes.
    pub fn snapshot_save(&self, name: &str) -> Result<(), String> {
        self.snapshot_save_with(name, false)
    }

    /// Saves the desk as snapshot `name`; with `include_iem`, its IEM mixes and marks
    /// too (§M.26), recalled with it; without, a recall leaves the desk's.
    pub fn snapshot_save_with(&self, name: &str, include_iem: bool) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("a snapshot needs a name".into());
        }
        store_states(&self.shared, true);
        let mut s = self.settings();
        // The X32 is the desk's, not a snapshot's (§M.23.1).
        s.x32 = Default::default();
        s.includes_iem = include_iem;
        if !include_iem {
            s.iem_mixes.clear();
        }
        self.shared
            .snapshots
            .lock_ok()
            .insert(name.trim().to_owned(), s);
        self.save_snapshots()?;
        self.set_snapshot(name.trim());
        self.changed();
        Ok(())
    }

    /// Recalls snapshot `name` (gains move over 0.2 s).
    pub fn snapshot_recall(&self, name: &str) -> Result<(), String> {
        self.snapshot_recall_scoped(name, &SnapshotRecallScope::default())
    }

    /// Recalls a snapshot within `scope` (§M.15): outside it, and on recall-safe channels and
    /// buses, the desk stays as it is.
    pub fn snapshot_recall_scoped(
        &self,
        name: &str,
        scope: &SnapshotRecallScope,
    ) -> Result<(), String> {
        let s = self
            .shared
            .snapshots
            .lock_ok()
            .get(name)
            .cloned()
            .ok_or("no such snapshot")?;
        if let Some(f) = scope.fade_s {
            if !(0.0..=10.0).contains(&f) {
                return Err("fade_s: 0 to 10 seconds".into());
            }
            self.shared.desk.lock_ok().next_fade(f);
        }
        self.edit(true, |cur| {
            *cur = recalled(&s, cur, scope);
            Ok(())
        })?;
        self.set_snapshot(name);
        self.changed();
        Ok(())
    }

    /// `snapshot_order {names}` (§M.28): the snapshots listed in this order (names not
    /// among them are left out of it; snapshots not named come after, by name).
    pub fn snapshot_order(&self, names: &[String]) -> Result<(), String> {
        let have = self
            .shared
            .snapshots
            .lock_ok()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut order: Vec<String> = Vec::new();
        for n in names {
            if have.contains(n) && !order.contains(n) {
                order.push(n.clone());
            }
        }
        if let Some(dir) = &self.shared.cfg.state_dir {
            let b = serde_json::to_vec_pretty(&order).map_err(|e| e.to_string())?;
            idn_common::fsutil::write_atomic(&dir.join(SNAPSHOT_ORDER_FILE), &b)
                .map_err(|e| e.to_string())?;
        }
        *self.shared.snapshot_order.lock_ok() = order;
        self.changed();
        Ok(())
    }

    pub fn snapshot_delete(&self, name: &str) -> Result<(), String> {
        self.shared
            .snapshots
            .lock_ok()
            .remove(name)
            .ok_or("no such snapshot")?;
        self.save_snapshots()?;
        self.changed();
        Ok(())
    }

    pub(super) fn save_snapshots(&self) -> Result<(), String> {
        let Some(dir) = &self.shared.cfg.state_dir else {
            return Ok(());
        };
        let s = serde_json::to_vec_pretty(&*self.shared.snapshots.lock_ok())
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir)
            // (Whole or not at all: a power cut mid-write lost every snapshot.)
            .and_then(|()| idn_common::fsutil::write_atomic(&dir.join(SNAPSHOTS_FILE), &s))
            .map_err(|e| e.to_string())
    }

    /// The snapshot last saved or recalled, kept for the next start.
    pub(super) fn set_snapshot(&self, name: &str) {
        *self.shared.snapshot.lock_ok() = Some(name.to_owned());
        if let Some(dir) = &self.shared.cfg.state_dir {
            let _ = std::fs::create_dir_all(dir).and_then(|()| {
                idn_common::fsutil::write_atomic(&dir.join(SNAPSHOT_FILE), name.as_bytes())
            });
        }
    }

    /// Copies channel `from`'s processing to channel `to` (as a preset would: plugins with
    /// their state; name, input, fader, pan, sends and routing stay). A linked pair's
    /// second stands for the pair (its first).
    pub fn copy_channel(&self, from: usize, to: usize) -> Result<(), String> {
        store_states(&self.shared, true);
        self.edit_loading(|s| {
            let p = ChannelPreset::of(&s.channels[lead_index(s, from)?]);
            let to = lead_index(s, to)?;
            p.apply(&mut s.channels[to]);
            Ok(())
        })
    }
}

impl Console {
    /// `channel_reset {n}` (§M.28): channel `n` as on a fresh desk (processing, levels,
    /// sends, groups), keeping what it is: its name, colour, inputs, failover, place on
    /// the desk and link. A linked pair's second resets the pair.
    pub fn channel_reset(&self, n: usize) -> Result<(), String> {
        store_states(&self.shared, true);
        self.edit_loading(|s| {
            let to = lead_index(s, n)?;
            let old = s.channels[to].clone();
            let mut c = Settings::new(s.channels.len()).channels[to].clone();
            (c.name, c.color, c.source) = (old.name, old.color, old.source);
            (c.input_b, c.use_b, c.failover) = (old.input_b, old.use_b, old.failover);
            (c.shown, c.link) = (old.shown, old.link);
            // Its IEM mark (the phones' flow) and recall safe are the desk's set-up, kept.
            (c.iem, c.safe) = (old.iem, old.safe);
            s.channels[to] = c;
            Ok(())
        })
    }
}

/// Channel `n`'s (from 1) index, or its pair's first's when it is a linked pair's second
/// (whose settings it is given: changed there, they would be put back).
fn lead_index(s: &Settings, n: usize) -> Result<usize, String> {
    let i = n.wrapping_sub(1);
    if i >= s.channels.len() {
        return Err("no such channel".into());
    }
    Ok(s.lead_of(i))
}
