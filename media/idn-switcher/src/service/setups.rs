//! Setups: saved and loaded, inputs restored, undo.

use super::*;

impl Service {
    /// Loads setup `name` (§S.28): the inputs it has and the switcher has the same stay as
    /// they are (cameras keep their connection); the rest go, its own come; its scenes,
    /// mixes, timers, filters, cameras' control, sound offsets and show replace these, then
    /// its program's transition and stingers (a setup without these: left as they are).
    /// Undo starts again.
    pub(super) fn setup_load(&self, name: &str) -> Result<(), String> {
        // (A damaged one is set aside, and the answer says so.)
        let doc: Value = idn_common::fsutil::load_json(&Self::setup_path(name)?)
            .map_err(|e| format!("setup {name:?}: {e}"))?
            .ok_or(format!("no setup {name:?}"))?;
        let files = &doc["files"];
        let target: Vec<Value> = files[SOURCES_FILE].as_array().cloned().unwrap_or_default();
        // The inputs' sound offsets: the setup's; without them they stay as they are
        // (taken before its inputs go).
        let offsets = match files[AUDIO_OFFSETS_FILE].as_object() {
            Some(m) => m.clone(),
            None => self.audio_offsets_now(),
        };
        self.replaying
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // The mixes go (made again from the setup).
        for m in self.sw.state()["mixes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(id) = m["id"].as_u64() {
                let _ = self.command_inner("remove_mix", &json!({"mix": id}));
            }
        }
        // The inputs: those the setup has as they are stay, the others go.
        let st = self.sw.state();
        let mut left = target.clone();
        for i in st["inputs"].as_array().cloned().unwrap_or_default() {
            let keep = self
                .saved_input(&st, &i)
                .and_then(|b| left.iter().position(|t| *t == b));
            match keep {
                Some(k) => {
                    left.remove(k);
                }
                None => {
                    let _ = self.command_inner("remove_input", &json!({"id": i["id"]}));
                }
            }
        }
        // Its files, then what is made from them.
        for f in SETUP_FILES {
            let v = match files.get(*f) {
                _ if *f == AUDIO_OFFSETS_FILE => Value::Object(offsets.clone()),
                Some(v) => v.clone(),
                None if [FILTERS_FILE, CAMERAS_FILE, MULTIVIEW_FILE, KEEP_ACTIVE_FILE]
                    .contains(f) =>
                {
                    json!({})
                }
                None => json!([]),
            };
            if *f != SHOW_FILE || !v.is_array() {
                let _ = Self::write_state_file(f, &v);
            }
        }
        // What is kept by label read again now, before the inputs come (each new one then
        // takes its own as at start).
        self.setup_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for t in self.sw.state()["timers"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(n) = t["name"].as_str() {
                let _ = self.sw.timer(n, TimerAction::Remove);
            }
        }
        self.restore_timers();
        for x in &left {
            if let Err(e) = self.restore_input(x) {
                idn_open_protocol::log::log(&format!(
                    "switcher: the setup's input {}: {e}",
                    x["label"].as_str().unwrap_or("?")
                ));
            }
        }
        self.restore_globals();
        self.restore_scenes();
        self.restore_mixes();
        self.restore_multiview(false);
        // The offsets at once (not a moment after, by the by-label thread), then the
        // program's transition and the stingers it keeps (a setup without them: left as
        // they are).
        self.apply_audio_offsets(&offsets);
        let notes = self.apply_live(&doc);
        if !notes.is_empty() {
            idn_open_protocol::log::log(&format!("switcher: setup {name}: {}", notes.join("; ")));
        }
        let (show, notes) = crate::show::Show::from_saved(&files[SHOW_FILE]);
        if !notes.is_empty() {
            idn_open_protocol::log::log(&format!(
                "switcher: setup {name}: its show without what this build can't read: {}",
                notes.join("; ")
            ));
        }
        *self.show.lock_ok() = crate::show::Runner::new(show);
        self.replaying
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *self.undo.lock_ok() = UndoLog::default();
        self.save_sources();
        self.show_changed();
        let _ = idn_common::fsutil::write(state_dir().join(SETUP_CURRENT), name.trim());
        Ok(())
    }

    /// Makes an input again from how it is kept (`sources.json`, §S.28): its id (`None`: a
    /// source the command line added already, or a shot waiting for its camera).
    pub(super) fn restore_input(&self, x: &Value) -> Result<Option<u32>, String> {
        let label = x["label"].as_str().unwrap_or("Title");
        // Files, pictures, colours and network sources: as `add_input` adds them; a source
        // the command line added already isn't added twice.
        if x["file"].is_string() || x["colour"].is_string() || x["network"].is_string() {
            if let Some(n) = x["network"].as_str() {
                let have = self.sw.state()["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|i| i["source"]["network"].as_str() == Some(n));
                if have {
                    return Ok(None);
                }
            }
            let r = self.command_inner("add_input", x)?;
            return Ok(r["id"].as_u64().map(|i| i as u32));
        }
        if let Some(url) = x["stream"].as_str() {
            let (addr, ingest) = self.media.open_stream(url, label)?;
            let id = self
                .sw
                .add_input(label, Source::Network(addr))
                .map_err(|e| e.to_string())?;
            self.media
                .streams_in
                .lock_ok()
                .insert(id, (url.to_owned(), ingest));
            return Ok(Some(id));
        }
        #[cfg(feature = "ffmpeg")]
        if x["list"].is_object() {
            let (addr, player) = self.media.open_list(&x["list"])?;
            let id = self
                .sw
                .add_input(label, Source::Network(addr))
                .map_err(|e| e.to_string())?;
            self.media
                .lists
                .lock_ok()
                .insert(id, (x["list"].clone(), player));
            return Ok(Some(id));
        }
        if x["ptz"].is_object() || x["virtual"].is_object() {
            if !self.make_shot(x) {
                self.pending_shots.lock_ok().push(x.clone());
            }
            return Ok(None);
        }
        if x["title"].is_object() {
            let t = crate::title::Title::from_json(&x["title"])?;
            let id = self
                .sw
                .add_input(label, Source::Title)
                .map_err(|e| e.to_string())?;
            self.sw.set_title(id, t)?;
            return Ok(Some(id));
        }
        if x["slideshow"].is_object() {
            let show = slideshow_from_json(&x["slideshow"], &self.media)?;
            let id = self
                .sw
                .add_input(label, Source::Slideshow(show))
                .map_err(|e| e.to_string())?;
            return Ok(Some(id));
        }
        Err("not an input the setup keeps".into())
    }

    /// Keeps a per-input setting in the state directory by label (`file`): each input's
    /// `field` in the state (as `pick` keeps it), given back with `apply` to an input of
    /// that label when it appears (at start, or added later): checked at each change. An
    /// input without one (null, an empty list, a zero) isn't kept.
    pub(super) fn keep_by_label(
        self: &Arc<Self>,
        file: &'static str,
        field: &'static str,
        pick: fn(&Value) -> Value,
        apply: fn(&Switcher, u32, &Value) -> Result<(), String>,
    ) {
        let weak = Arc::downgrade(self);
        let path = state_dir().join(file);
        // (One that can't be read or parsed: set aside and said, not saved over with
        // nothing; deep bug run 2026-11-09.)
        let load = |path: &std::path::Path| -> serde_json::Map<String, Value> {
            super::saved::load_state(path).unwrap_or_default()
        };
        let mut saved = load(&path);
        std::thread::Builder::new()
            .name(format!("{field}-save"))
            .spawn(move || {
                let mut seen = std::collections::BTreeSet::new();
                let mut version = 0;
                let mut loaded = 0;
                while let Some(api) = weak.upgrade() {
                    // A setup loaded (§S.28): its file read again, and given to every input
                    // by label (filters cleared where it has none).
                    let now_gen = api.setup_gen.load(std::sync::atomic::Ordering::Relaxed);
                    if now_gen != loaded {
                        loaded = now_gen;
                        saved = load(&path);
                        seen.clear();
                        for i in api.sw.state()["inputs"].as_array().into_iter().flatten() {
                            let (Some(id), Some(label)) = (i["id"].as_u64(), i["label"].as_str())
                            else {
                                continue;
                            };
                            seen.insert(id);
                            let x = match saved.get(label) {
                                Some(x) => x.clone(),
                                None if field == "filters" => json!([]),
                                None if field == "audio_offset_ms" => json!(0),
                                None if field == "keep_active" => json!(false),
                                None => continue,
                            };
                            if let Err(e) = apply(&api.sw, id as u32, &x) {
                                idn_open_protocol::log::log(&format!(
                                    "switcher: {label}'s {field}: {e}"
                                ));
                            }
                        }
                    }
                    let before = saved.clone();
                    for i in api.sw.state()["inputs"].as_array().into_iter().flatten() {
                        let (Some(id), Some(label)) = (i["id"].as_u64(), i["label"].as_str())
                        else {
                            continue;
                        };
                        let v = &i[field];
                        // (None: null, an empty list, a zero offset, or false.)
                        let has = !v.is_null()
                            && v.as_array().is_none_or(|a| !a.is_empty())
                            && v.as_u64() != Some(0)
                            && v.as_bool() != Some(false);
                        if seen.insert(id) && !has {
                            // New here: what its label had.
                            if let Some(x) = saved.get(label)
                                && let Err(e) = apply(&api.sw, id as u32, x)
                            {
                                idn_open_protocol::log::log(&format!(
                                    "switcher: {label}'s saved {field}: {e}"
                                ));
                            }
                            continue;
                        }
                        if has {
                            saved.insert(label.to_owned(), pick(v));
                        } else {
                            saved.remove(label);
                        }
                    }
                    // Not over a setup loaded meanwhile.
                    if saved != before
                        && api.setup_gen.load(std::sync::atomic::Ordering::Relaxed) == loaded
                    {
                        let _ = std::fs::create_dir_all(state_dir()).and_then(|()| {
                            idn_common::fsutil::write(
                                &path,
                                Value::Object(saved.clone()).to_string(),
                            )
                        });
                    }
                    // (Waited without the service held: dropped by its host, it goes then,
                    // not up to 5 s later on this thread.)
                    drop(api);
                    let Some(v) = wait_change_weak(&weak, version, Duration::from_secs(5)) else {
                        return;
                    };
                    version = v;
                }
            })
            .expect("a thread");
    }

    /// The way back from `cmd` with `v`, as things are now (§S.28).
    pub(super) fn inverse(&self, cmd: &str, v: &Value) -> Option<Step> {
        let st = self.sw.state();
        let id = v["id"].as_u64();
        let input = || {
            st["inputs"]
                .as_array()?
                .iter()
                .find(|i| i["id"].as_u64() == id)
                .cloned()
        };
        let step = |what: String, cmd: &str, body: Value| {
            Some(Step {
                what,
                cmd: cmd.into(),
                body,
            })
        };
        match cmd {
            "title" => {
                let i = input()?;
                let mut body = i["source"]["title"].clone();
                body["id"] = json!(id?);
                step(format!("the title {}", i["label"].as_str()?), "title", body)
            }
            "scene_items" => {
                let i = input()?;
                step(
                    format!("the scene {}", i["label"].as_str()?),
                    "scene_items",
                    json!({"id": id?, "items": i["source"]["scene"]["items"]}),
                )
            }
            // A global layer changed: its definition as it was; a new one: removed (§S.5).
            "global_layer" => {
                let want = v["layer"]["id"].as_u64()?;
                let old = st["global_layers"]
                    .as_array()?
                    .iter()
                    .find(|g| g["id"].as_u64() == Some(want))?
                    .clone();
                step(
                    format!("the global layer {}", old["name"].as_str().unwrap_or("")),
                    "global_layer",
                    json!({"layer": old}),
                )
            }
            "global_layer_remove" => {
                let want = v["id"].as_u64()?;
                let old = st["global_layers"]
                    .as_array()?
                    .iter()
                    .find(|g| g["id"].as_u64() == Some(want))?
                    .clone();
                step(
                    format!("the global layer {}", old["name"].as_str().unwrap_or("")),
                    "global_layer",
                    json!({"layer": old}),
                )
            }
            "filters" => {
                let i = input()?;
                let old = if i["filters"].is_array() {
                    i["filters"].clone()
                } else {
                    json!([])
                };
                step(
                    format!("the filters of {}", i["label"].as_str()?),
                    "filters",
                    json!({"id": id?, "filters": old}),
                )
            }
            "rename_input" => {
                let i = input()?;
                step(
                    format!("renaming {}", i["label"].as_str()?),
                    "rename_input",
                    json!({"id": id?, "label": i["label"]}),
                )
            }
            "show_set" => step(
                "the show's edit".into(),
                "show_set",
                json!({"show": self.show.lock_ok().show.to_json()}),
            ),
            // A cue's edit: the cue edit that puts it back (the places followed both ways,
            // as a whole show set back would not: the next cue moved, a follow time fired).
            "cue_add" | "cue_update" | "cue_delete" | "cue_move" => {
                let r = self.show.lock_ok();
                let len = r.show.cues.len();
                if cmd == "cue_add" {
                    let at = v["at"].as_u64().map_or(len + 1, |a| a as usize);
                    return step(format!("adding cue {at}"), "cue_delete", json!({"n": at}));
                }
                let n = v["n"].as_u64().filter(|&n| n >= 1).map(|n| n as usize)?;
                let cue = r.show.cues.get(n - 1)?;
                match cmd {
                    "cue_update" => step(
                        format!("changing cue {n} ({})", cue.name),
                        "cue_update",
                        json!({"n": n, "cue": cue.to_json()}),
                    ),
                    "cue_delete" => step(
                        format!("deleting cue {n} ({})", cue.name),
                        "cue_add",
                        json!({"at": n, "cue": cue.to_json()}),
                    ),
                    _ => {
                        let to = v["to"].as_u64()?;
                        step(
                            format!("moving cue {n} ({})", cue.name),
                            "cue_move",
                            json!({"n": to, "to": n}),
                        )
                    }
                }
            }
            "remove_input" => {
                let i = input()?;
                let label = i["label"].as_str()?.to_owned();
                // (Its sound offset back with it, at once.)
                let offset = &i["audio_offset_ms"];
                if i["source"]["scene"].is_object() {
                    step(
                        format!("removing {label}"),
                        "restore_scene",
                        json!({"old_id": id?, "label": label, "items": i["source"]["scene"]["items"],
                            "audio_offset_ms": offset}),
                    )
                } else {
                    let body = self.saved_input(&st, &i)?;
                    step(
                        format!("removing {label}"),
                        "restore_input",
                        json!({"old_id": id?, "input": body, "audio_offset_ms": offset}),
                    )
                }
            }
            _ => None,
        }
    }

    /// A setup's file name from its name (letters, digits, spaces and `-_.()`; 60 at most).
    pub(super) fn setup_path(name: &str) -> Result<std::path::PathBuf, String> {
        let n = name.trim();
        if n.is_empty()
            || n.chars().count() > 60
            || n.starts_with('.')
            || !n
                .chars()
                .all(|c| c.is_alphanumeric() || " -_.()".contains(c))
        {
            return Err("a setup's name: letters, digits, spaces and - _ . ( ), 60 at most".into());
        }
        Ok(state_dir().join(SETUPS_DIR).join(format!("{n}.json")))
    }

    /// Adds starter `name` (§S.32): its inputs (an input of the same label here is kept),
    /// its timers, and its show if this one has no cues. Nothing here is taken away.
    pub(super) fn starter(&self, name: &str) -> Result<Value, String> {
        let all: Value = serde_json::from_str(STARTERS).map_err(|e| e.to_string())?;
        let s = all
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == name))
            .ok_or(format!("no starter {name:?}"))?;
        let have: Vec<String> = self.sw.state()["inputs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i["label"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let (mut added, mut kept) = (Vec::new(), Vec::new());
        for i in s["inputs"].as_array().into_iter().flatten() {
            let label = i["label"].as_str().unwrap_or("").to_owned();
            if have.contains(&label) {
                kept.push(label);
                continue;
            }
            self.command_inner("add_input", i)?;
            added.push(label);
        }
        for t in s["timers"].as_array().into_iter().flatten() {
            self.command_inner("timer", t)?;
        }
        let empty = self.show.lock_ok().show.to_json()["cues"]
            .as_array()
            .is_none_or(|c| c.is_empty());
        if empty {
            // Its cues put its titles on program: each in a scene of its own (§S.3).
            let mut show = s["show"].clone();
            for a in show["cues"]
                .as_array_mut()
                .into_iter()
                .flatten()
                .flat_map(|c| c["actions"].as_array_mut().into_iter().flatten())
            {
                if a["do"] == "switcher"
                    && let Some(cmd) = a["command"].as_str().map(str::to_owned)
                {
                    self.bus_body_in_scene(&cmd, &mut a["body"]);
                }
            }
            self.command_inner("show_set", &json!({"show": show}))?;
        }
        self.show_version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({"added": added, "kept": kept, "show": if empty { "set" } else { "kept" }}))
    }

    /// The setups saved, by name.
    pub(super) fn setups(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(state_dir().join(SETUPS_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".json")
                    .map(str::to_owned)
            })
            .collect();
        v.sort();
        v
    }

    /// Saves the setup as it is now under `name` (§S.28): its files, and the program's
    /// transition and stingers ([`saved_live`](Self::saved_live)).
    pub(super) fn setup_save(&self, name: &str) -> Result<(), String> {
        let path = Self::setup_path(name)?;
        self.save_sources();
        self.save_globals();
        self.save_scenes();
        self.save_mixes();
        self.save_multiview();
        self.save_timers();
        Self::write_state_file(SHOW_FILE, &self.show.lock_ok().show.to_json())?;
        // What the by-label threads keep is written as it changes (a moment after).
        std::thread::sleep(Duration::from_millis(300));
        let mut files = serde_json::Map::new();
        for f in SETUP_FILES {
            if let Some(v) = std::fs::read(state_dir().join(f))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                files.insert((*f).to_owned(), v);
            }
        }
        // The offsets as they are now (always, none too: loading it sets them).
        files.insert(
            AUDIO_OFFSETS_FILE.to_owned(),
            Value::Object(self.audio_offsets_now()),
        );
        let mut doc = json!({"name": name.trim(), "files": files});
        for (k, v) in self.saved_live() {
            doc[k] = v;
        }
        std::fs::create_dir_all(state_dir().join(SETUPS_DIR))
            .and_then(|()| idn_common::fsutil::write(&path, doc.to_string()))
            .and_then(|()| idn_common::fsutil::write(state_dir().join(SETUP_CURRENT), name.trim()))
            .map_err(|e| format!("saving the setup: {e}"))
    }

    /// The inputs' sound sync offsets by label (`audio-offsets.json`) as they are now: the
    /// file's (inputs not here keep theirs), with every input here as it is (0: left out).
    pub(super) fn audio_offsets_now(&self) -> serde_json::Map<String, Value> {
        let mut m = super::saved::load_saved_value(AUDIO_OFFSETS_FILE)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        for i in self.sw.state()["inputs"].as_array().into_iter().flatten() {
            let Some(label) = i["label"].as_str() else {
                continue;
            };
            match i["audio_offset_ms"].as_u64() {
                Some(ms) if ms > 0 => m.insert(label.to_owned(), json!(ms)),
                _ => m.remove(label),
            };
        }
        m
    }

    /// Gives every input here its sound offset from `offsets` by label (none: 0).
    fn apply_audio_offsets(&self, offsets: &serde_json::Map<String, Value>) {
        for i in self.sw.state()["inputs"].as_array().into_iter().flatten() {
            let (Some(id), Some(label)) = (i["id"].as_u64(), i["label"].as_str()) else {
                continue;
            };
            let ms = offsets
                .get(label)
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(500);
            if i["audio_offset_ms"].as_u64() != Some(ms) {
                let _ = self.sw.input_audio_offset(id as u32, ms as u32);
            }
        }
    }

    /// What a setup keeps of the program besides its files (§S.28), inputs by label: the
    /// program's `transition` and the 8 `stingers` (null: empty). (The global layers are a
    /// file of the setup, §S.5.)
    fn saved_live(&self) -> serde_json::Map<String, Value> {
        let st = self.sw.state();
        let label = |id: &Value| -> Value {
            st["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| !id.is_null() && i["id"] == *id)
                .map_or(Value::Null, |i| i["label"].clone())
        };
        let stingers: Vec<Value> = st["stingers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| match label(&s["input"]) {
                Value::Null => Value::Null,
                l => json!({"input": l, "duration": s["duration"], "cut": s["cut"]}),
            })
            .collect();
        let mut m = serde_json::Map::new();
        m.insert(
            "transition".into(),
            saved_transition(&st["transition"], &label),
        );
        m.insert("stingers".into(), Value::Array(stingers));
        m
    }

    /// Applies what [`saved_live`](Self::saved_live) kept, each part only if the setup has
    /// it: what couldn't be (an input not here: that stinger emptied, a luma wipe a mix).
    fn apply_live(&self, doc: &Value) -> Vec<String> {
        let st = self.sw.state();
        let by_label = |l: &Value| -> Option<u64> {
            st["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| l.is_string() && i["label"] == *l)
                .and_then(|i| i["id"].as_u64())
        };
        let num = |v: &Value, d: f32| v.as_f64().map_or(d, |x| x as f32);
        let mut notes = Vec::new();
        for (k, s) in doc["stingers"].as_array().into_iter().flatten().enumerate() {
            let slot = if s.is_object() {
                match by_label(&s["input"]) {
                    Some(input) => Some(crate::Stinger {
                        input: input as u32,
                        duration: num(&s["duration"], 1.0),
                        cut: num(&s["cut"], 0.5),
                    }),
                    None => {
                        notes.push(format!("stinger {}: no input {}", k + 1, s["input"]));
                        None
                    }
                }
            } else {
                None
            };
            if let Err(e) = self.sw.stinger(k + 1, slot) {
                notes.push(format!("stinger {}: {e}", k + 1));
            }
        }
        let t = &doc["transition"];
        if t.is_object() {
            let kind = transition_kind(t, &by_label);
            if t["kind"] == "luma" && !matches!(kind, Kind::Luma { .. }) {
                notes.push(format!(
                    "the transition: no luma pattern {}, a mix",
                    t["luma"]["pattern"]
                ));
            }
            if let Err(e) = self.sw.transition(kind, num(&t["duration"], 1.0)) {
                notes.push(format!("the transition: {e}"));
            }
        }
        notes
    }

    /// Undoes (or redoes) the last edit of the setup: what it was.
    pub(super) fn undo_redo(&self, undo: bool) -> Result<Value, String> {
        let step = {
            let mut log = self.undo.lock_ok();
            let list = if undo {
                &mut log.back
            } else {
                &mut log.forward
            };
            list.pop().ok_or(if undo {
                "nothing to undo"
            } else {
                "nothing to redo"
            })?
        };
        // Its input as it is now (made again by an undo since).
        let mut body = step.body.clone();
        if let Some(mut id) = body["id"].as_u64() {
            let log = self.undo.lock_ok();
            while let Some(&n) = log.ids.get(&id) {
                id = n;
            }
            body["id"] = json!(id);
        }
        let counter = if step.cmd.starts_with("restore_") {
            None
        } else {
            self.inverse(&step.cmd, &body)
        };
        self.replaying
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let r = self.run_step(&step.cmd, &body);
        self.replaying
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let back = |list: &mut Vec<Step>, s: Step| list.push(s);
        match r {
            Ok(out) => {
                let counter = counter.or_else(|| {
                    out["id"].as_u64().map(|id| Step {
                        what: step.what.clone(),
                        cmd: "remove_input".into(),
                        body: json!({"id": id}),
                    })
                });
                let mut log = self.undo.lock_ok();
                if let (Some(old), Some(new)) = (step.body["old_id"].as_u64(), out["id"].as_u64()) {
                    log.ids.insert(old, new);
                }
                if let Some(mut c) = counter {
                    c.what = step.what.clone();
                    if undo {
                        back(&mut log.forward, c)
                    } else {
                        back(&mut log.back, c)
                    }
                }
                drop(log);
                self.show_changed();
                Ok(json!({"done": step.what}))
            }
            Err(e) => Err(format!("{}: {e}", step.what)),
        }
    }

    /// Runs an undo step: a command, or an input made again.
    pub(super) fn run_step(&self, cmd: &str, body: &Value) -> Result<Value, String> {
        match cmd {
            "restore_input" => {
                let id = self.restore_input(&body["input"])?;
                self.save_sources();
                if let (Some(id), Some(ms)) = (id, body["audio_offset_ms"].as_u64()) {
                    self.sw.input_audio_offset(id, ms.min(500) as u32)?;
                }
                Ok(json!({"id": id}))
            }
            "restore_scene" => {
                let id = self
                    .sw
                    .add_input(body["label"].as_str().unwrap_or("Scene"), Source::Scene)
                    .map_err(|e| e.to_string())?;
                let items = crate::scene::items_from_json(&body["items"])?;
                self.sw.scene_items(id, items)?;
                self.save_scenes();
                if let Some(ms) = body["audio_offset_ms"].as_u64() {
                    self.sw.input_audio_offset(id, ms.min(500) as u32)?;
                }
                Ok(json!({"id": id}))
            }
            _ => self.command(cmd, body),
        }
    }
}

/// A transition as a setup and `mixes.json` keep it (§S.28), from the state's: `kind`,
/// `duration`, a wipe's `soft` edge, a luma wipe's `luma` {`pattern` (its input's label),
/// `soft`, `invert`}.
pub(super) fn saved_transition(t: &Value, label: &dyn Fn(&Value) -> Value) -> Value {
    let mut out = json!({"kind": t["kind"], "duration": t["duration"]});
    if t["soft"].is_number() {
        out["soft"] = t["soft"].clone();
    }
    if t["luma"].is_object() {
        out["luma"] = json!({"pattern": label(&t["luma"]["pattern"]), "soft": t["luma"]["soft"],
            "invert": t["luma"]["invert"]});
    }
    out
}

/// A kept transition's kind ([`saved_transition`]'s, or `mixes.json`'s before it had a
/// soft edge or luma: the wipe's default edge, a mix): a luma wipe's pattern found by label;
/// one whose pattern isn't here, a mix.
pub(super) fn transition_kind(t: &Value, by_label: &dyn Fn(&Value) -> Option<u64>) -> Kind {
    let soft = |v: &Value| v.as_f64().map_or(0.05, |x| x as f32);
    match t["kind"].as_str().unwrap_or("mix") {
        "luma" => by_label(&t["luma"]["pattern"]).map_or(Kind::Mix, |p| Kind::Luma {
            pattern: p as u32,
            soft: soft(&t["luma"]["soft"]).clamp(0.0, 1.0),
            invert: t["luma"]["invert"].as_bool().unwrap_or(false),
        }),
        name => Kind::parse(name, soft(&t["soft"])).unwrap_or(Kind::Mix),
    }
}
