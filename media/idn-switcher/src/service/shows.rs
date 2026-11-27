//! Shows (§S.27): saved by name, their cues fired, Planning Center.

use super::*;

impl Service {
    /// The shows saved by name (`shows.json`).
    /// (One that can't be read is set aside and said, not saved over with the one show
    /// saved next; bug review 3.)
    pub(super) fn saved_shows(&self) -> serde_json::Map<String, Value> {
        super::saved::load_saved_value(SHOWS_FILE)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    /// The Planning Center token, if one is kept.
    pub(super) fn pco(&self) -> Option<crate::show::Pco> {
        let v = super::saved::load_saved_value(PCO_FILE)?;
        Some(crate::show::Pco {
            app_id: v["app_id"].as_str()?.to_owned(),
            secret: v["secret"].as_str()?.to_owned(),
        })
    }

    /// Fires cue `k` of the show (§S.27): the switcher's own actions here and now, in
    /// order; a mixer's, web and OSC ones on a thread of their own (a slow device never
    /// holds up the cut). The errors of its switcher and desk actions are answered; its
    /// web and OSC ones run after, into the show's errors (the state's) only.
    pub(super) fn fire_cue(&self, k: usize) -> Vec<String> {
        let cue = {
            let mut r = self.show.lock_ok();
            // `k` was found under an earlier lock: a show set meanwhile may be shorter (a
            // panic here poisoned the show for the whole API).
            let Some(cue) = r.show.cues.get(k).cloned() else {
                return vec![format!("cue {} is gone (the show changed)", k + 1)];
            };
            r.fire(k);
            let _ = Self::write_state_file(SHOW_FILE, &r.show.to_json());
            cue
        };
        let mut external = Vec::new();
        let mut failed = Vec::new();
        self.show_errors.lock_ok().retain(|(c, _)| *c != k);
        for a in cue.actions {
            match a {
                crate::show::Action::Switcher { command, body } => {
                    if let Err(e) = self.run_switcher_action(&command, body) {
                        failed.push(format!("{command}: {e}"));
                    }
                }
                crate::show::Action::Mixer { desk, snapshot, .. } if !desk.is_empty() => {
                    if let Err(e) = self.sw.desk_snapshot(&desk, &snapshot) {
                        failed.push(e);
                    }
                }
                other => external.push(other),
            }
        }
        self.show_errors
            .lock_ok()
            .extend(failed.iter().map(|e| (k, e.clone())));
        self.show_changed();
        if !external.is_empty() {
            let (errors, version) = (self.show_errors.clone(), self.show_version.clone());
            std::thread::spawn(move || {
                for a in external {
                    if let Err(e) = crate::show::run_external(&a) {
                        let mut list = errors.lock_ok();
                        list.push((k, e));
                        // The last few only.
                        let extra = list.len().saturating_sub(5);
                        list.drain(..extra);
                        version.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
        failed
    }

    /// The show's `if_version` (if given) against its version now: a UI that read an
    /// older show doesn't write over another's edit.
    pub(super) fn check_show_version(show: &crate::show::Show, v: &Value) -> Result<(), String> {
        match &v["if_version"] {
            Value::Null => Ok(()),
            want => {
                let now = rules::show_version(show);
                if want.as_u64() == Some(now) {
                    Ok(())
                } else {
                    Err(format!(
                        "the show changed meanwhile (its version is {now}): read it again"
                    ))
                }
            }
        }
    }

    /// An edit of the show's cues, whole under one lock (`if_version` checked): `edit`
    /// changes the cues, each with where it was (`None`: a new one), and answers what the
    /// command answers. Kept, the places followed; the answer gets the show's new version.
    pub(super) fn edit_cues(
        &self,
        v: &Value,
        edit: impl FnOnce(&mut Vec<(Option<usize>, crate::show::Cue)>) -> Result<Value, String>,
    ) -> Result<Value, String> {
        let mut r = self.show.lock_ok();
        Self::check_show_version(&r.show, v)?;
        let mut cues: Vec<_> = r
            .show
            .cues
            .iter()
            .cloned()
            .enumerate()
            .map(|(k, c)| (Some(k), c))
            .collect();
        let mut out = edit(&mut cues)?;
        if cues.len() > crate::show::MAX_CUES {
            return Err(format!("at most {} cues", crate::show::MAX_CUES));
        }
        let old_of_new: Vec<Option<usize>> = cues.iter().map(|c| c.0).collect();
        let show = crate::show::Show {
            cues: cues.into_iter().map(|c| c.1).collect(),
            ..r.show.clone()
        };
        Self::write_state_file(SHOW_FILE, &show.to_json())?;
        r.rearrange(show, &old_of_new);
        // The cues' errors follow their cues (a removed cue's go).
        self.show_errors.lock_ok().retain_mut(|(k, _)| {
            match old_of_new.iter().position(|o| *o == Some(*k)) {
                Some(j) => {
                    *k = j;
                    true
                }
                None => false,
            }
        });
        out["version"] = json!(rules::show_version(&r.show));
        Ok(out)
    }

    /// The show changed: the clients' long poll answers.
    pub(super) fn show_changed(&self) {
        self.show_version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A cue's switcher command: `input` or `camera` given by label become the `id`.
    pub(super) fn run_switcher_action(&self, command: &str, mut body: Value) -> Result<(), String> {
        for key in ["input", "camera"] {
            if let Some(label) = body[key].as_str().map(str::to_owned) {
                let id = self.sw.state()["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|i| i["label"] == label.as_str())
                    .and_then(|i| i["id"].as_u64())
                    .ok_or(format!("no input {label:?}"))?;
                body["id"] = json!(id);
                if let Some(o) = body.as_object_mut() {
                    o.remove(key);
                }
            }
        }
        if command.starts_with("cue_")
            || command.starts_with("show_")
            || command.starts_with("pco_")
        {
            return Err("a cue can't run the show's own commands".into());
        }
        self.command(command, &body).map(|_| ())
    }

    /// The scene an input goes on a bus in (§S.3): itself if it is a scene; else the
    /// scene made for it once ("LABEL (scene)": the input over the whole picture, the
    /// global layers on it), found again by that label. For a starter's cues, which put
    /// its titles on program; the API refuses an input that isn't a scene.
    pub(super) fn scene_for(&self, id: u32) -> Result<u32, String> {
        let st = self.sw.state();
        let inputs = st["inputs"].as_array().cloned().unwrap_or_default();
        let input = inputs
            .iter()
            .find(|i| i["id"] == id)
            .ok_or(format!("no input {id}"))?;
        if input["source"]["scene"].is_object() {
            return Ok(id);
        }
        let label = format!("{} (scene)", input["label"].as_str().unwrap_or(""));
        let found = inputs.iter().find(|i| {
            i["label"] == label.as_str()
                && i["source"]["scene"]["items"].as_array().is_some_and(|l| {
                    l.iter()
                        .any(|it| it["source"] == id && it["global"].is_null())
                })
        });
        if let Some(scene) = found.and_then(|i| i["id"].as_u64()) {
            return Ok(scene as u32);
        }
        let scene = self
            .sw
            .add_input(&label, Source::Scene)
            .map_err(|e| e.to_string())?;
        let mut items = self.sw.scene(scene).unwrap_or_default();
        items.insert(
            0,
            crate::scene::Item {
                source: crate::scene::ItemSource::Input(id),
                name: input["label"].as_str().unwrap_or("").to_owned(),
                ..Default::default()
            },
        );
        items.truncate(crate::scene::MAX_ITEMS);
        self.sw.scene_items(scene, items)?;
        self.save_scenes();
        idn_open_protocol::log::log(&format!(
            "switcher: {} put on a bus by a starter's cue: given the scene {label:?}",
            input["label"]
        ));
        Ok(scene)
    }

    /// A bus command's body (`program`, `preview`, `mix_program`, `mix_preview`) of a
    /// starter's cue whose input isn't a scene: that input's scene in its place (by
    /// label). Whether it changed.
    pub(super) fn bus_body_in_scene(&self, command: &str, body: &mut Value) -> bool {
        if !matches!(
            command,
            "program" | "preview" | "mix_program" | "mix_preview"
        ) {
            return false;
        }
        let st = self.sw.state();
        let inputs = st["inputs"].as_array().cloned().unwrap_or_default();
        let by = |pick: &dyn Fn(&Value) -> bool| inputs.iter().find(|i| pick(i)).cloned();
        let found = if let Some(n) = body["id"].as_u64() {
            by(&|i| i["id"] == n)
        } else {
            ["id", "input", "camera"]
                .iter()
                .find_map(|k| body[*k].as_str())
                .and_then(|l| by(&|i| i["label"] == l))
        };
        let Some(input) = found else { return false };
        if input["source"]["scene"].is_object() {
            return false;
        }
        let Some(id) = input["id"].as_u64() else {
            return false;
        };
        match self.scene_for(id as u32) {
            Ok(scene) => {
                if let Some(o) = body.as_object_mut() {
                    for k in ["id", "input", "camera"] {
                        o.remove(k);
                    }
                    o.insert("input".into(), json!(self.label_of(scene)));
                }
                true
            }
            Err(e) => {
                idn_open_protocol::log::log(&format!(
                    "switcher: a scene for {}: {e}",
                    input["label"]
                ));
                false
            }
        }
    }
}
