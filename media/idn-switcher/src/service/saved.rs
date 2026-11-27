//! What the switcher keeps in its state directory between runs: scenes, mixes, inputs
//! (`sources.json`), screens, the monitor, timers; restored at start, saved shots made
//! once their camera appears.

use super::*;

impl Service {
    /// Keeps the scenes in the state directory (`scenes.json`), each item's input named by
    /// its label so they come back after a restart (§S.12).
    pub(super) fn save_scenes(&self) {
        let st = self.sw.state();
        let inputs = st["inputs"].as_array().cloned().unwrap_or_default();
        let label = |id: u64| {
            inputs
                .iter()
                .find(|i| i["id"].as_u64() == Some(id))
                .and_then(|i| i["label"].as_str())
                .map(str::to_owned)
        };
        let scenes: Vec<Value> = inputs
            .iter()
            .filter(|i| i["source"]["scene"].is_object())
            .map(|i| {
                let items: Vec<Value> = i["source"]["scene"]["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|it| {
                        let mut it = it.clone();
                        if let Some(src) = it["source"].as_u64() {
                            it["source"] = json!({"input": label(src)?});
                        }
                        Some(it)
                    })
                    .chain(
                        // Items saved for an input that wasn't here at the start.
                        i["label"]
                            .as_str()
                            .and_then(|l| self.unplaced_items.lock_ok().get(l).cloned())
                            .unwrap_or_default(),
                    )
                    .collect();
                json!({"label": i["label"], "items": items})
            })
            .collect();
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir).and_then(|()| {
            idn_common::fsutil::write(dir.join(SCENES_FILE), Value::Array(scenes).to_string())
        });
    }

    /// Keeps the global layers' definitions in the state directory (`global-layers.json`,
    /// §S.5), each one's input by its label (one that wasn't here at the start as it was
    /// saved, until the layer is set again).
    pub(super) fn save_globals(&self) {
        let st = self.sw.state();
        let label = |id: u64| {
            st["inputs"]
                .as_array()?
                .iter()
                .find(|i| i["id"].as_u64() == Some(id))?["label"]
                .as_str()
                .map(str::to_owned)
        };
        let unplaced = self.unplaced_globals.lock_ok().clone();
        let list: Vec<Value> = st["global_layers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|g| {
                if let Some(saved) = g["id"].as_u64().and_then(|id| unplaced.get(&id)) {
                    return Some(saved.clone());
                }
                let mut g = g.clone();
                if let Some(src) = g["source"].as_u64() {
                    g["source"] = json!({"input": label(src)?});
                }
                Some(g)
            })
            .collect();
        let _ = Self::write_state_file(GLOBALS_FILE, &Value::Array(list));
    }

    /// Brings the saved global layers back (before the scenes, whose copies name them),
    /// their inputs by label, under their ids; the ones there are now go first. One whose
    /// input isn't here shows nothing (kept as saved for the next save).
    pub(super) fn restore_globals(&self) {
        for g in self.sw.global_layers() {
            let _ = self.sw.remove_global_layer(g.id);
        }
        self.unplaced_globals.lock_ok().clear();
        let Some(saved) = load_saved_list(GLOBALS_FILE) else {
            return;
        };
        let st = self.sw.state();
        for g in saved {
            let mut v = g.clone();
            let mut missing = false;
            if let Some(l) = g["source"]["input"].as_str() {
                match st["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|i| i["label"].as_str() == Some(l))
                    .and_then(|i| i["id"].as_u64())
                {
                    Some(id) => v["source"] = json!(id),
                    None => {
                        // Shows nothing until set again.
                        v["source"] = json!({"colour": "#000000"});
                        v["opacity"] = json!(0.0);
                        missing = true;
                    }
                }
            }
            match crate::scene::Item::from_json(&v)
                .and_then(|def| self.sw.set_global_layer(def, false))
            {
                Ok(id) => {
                    if missing {
                        idn_open_protocol::log::log(&format!(
                            "switcher: global layer {id}: its input {} isn't here; it shows nothing",
                            g["source"]["input"]
                        ));
                        self.unplaced_globals.lock_ok().insert(u64::from(id), g);
                    }
                }
                Err(e) => {
                    idn_open_protocol::log::log(&format!("switcher: a saved global layer: {e}"))
                }
            }
        }
    }

    /// Keeps the multiview's layout and whether it is sent (`multiview.json`, §S.7): its
    /// tiles' inputs and scenes by label, mixes by name (`null`: the default layout).
    pub(super) fn save_multiview(&self) {
        let st = self.sw.state();
        let mv = &st["multiview"];
        let tiles = if mv["custom"] == true {
            let label = |id: &Value| {
                st["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|i| i["id"] == *id)
                    .map_or(Value::Null, |i| i["label"].clone())
            };
            let mix = |id: &Value| {
                st["mixes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|m| m["id"] == *id)
                    .map_or(Value::Null, |m| m["name"].clone())
            };
            Value::Array(
                mv["tiles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| {
                        let mut t = t.clone();
                        t["id"] = match t["kind"].as_str() {
                            Some("input" | "scene") => label(&t["id"]),
                            Some("mix") => mix(&t["id"]),
                            _ => Value::Null,
                        };
                        t
                    })
                    .collect(),
            )
        } else {
            Value::Null
        };
        let _ = Self::write_state_file(
            MULTIVIEW_FILE,
            &json!({"output": mv["output"], "tiles": tiles}),
        );
    }

    /// Brings the multiview back as kept (after the mixes): its layout (tiles whose input,
    /// scene or mix isn't here left out) and, if kept, whether it is sent (`at_start`: only
    /// turned on, so a program started with its multiview sent keeps it).
    pub(super) fn restore_multiview(&self, at_start: bool) {
        let Some(saved) = load_saved_value(MULTIVIEW_FILE) else {
            return;
        };
        let st = self.sw.state();
        let tiles = match &saved["tiles"] {
            Value::Array(list) => {
                let mut out = Vec::new();
                for t in list {
                    let mut t = t.clone();
                    let id = match t["kind"].as_str() {
                        Some("input" | "scene") => st["inputs"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|i| t["id"].is_string() && i["label"] == t["id"])
                            .map(|i| i["id"].clone()),
                        Some("mix") => st["mixes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|m| t["id"].is_string() && m["name"] == t["id"])
                            .map(|m| m["id"].clone()),
                        _ => Some(Value::Null),
                    };
                    match id {
                        Some(id) => {
                            t["id"] = id;
                            match crate::multiview::TileSpec::from_json(&t) {
                                Ok(tile) => out.push(tile),
                                Err(e) => idn_open_protocol::log::log(&format!(
                                    "switcher: a saved multiview tile: {e}"
                                )),
                            }
                        }
                        None => idn_open_protocol::log::log(&format!(
                            "switcher: the multiview's tile for {} left out: not here",
                            t["id"]
                        )),
                    }
                }
                Some(out)
            }
            _ => None,
        };
        if let Err(e) = self.sw.set_multiview_layout(tiles) {
            idn_open_protocol::log::log(&format!("switcher: the saved multiview: {e}"));
        }
        if let Some(on) = saved["output"].as_bool().filter(|on| *on || !at_start)
            && let Err(e) = self.sw.multiview_output(on)
        {
            idn_open_protocol::log::log(&format!("switcher: the multiview's output: {e}"));
        }
    }

    /// Keeps the mixes' names and formats in the state directory (`mixes.json`).
    pub(super) fn save_mixes(&self) {
        let mixes = saved_mixes(&self.sw.state());
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir)
            .and_then(|()| idn_common::fsutil::write(dir.join(MIXES_FILE), mixes.to_string()));
    }

    /// Brings the saved mixes back, their buses' inputs found by label, and their transition
    /// (a luma wipe's pattern by label too: one whose pattern isn't here comes back as a mix).
    pub(super) fn restore_mixes(&self) {
        // A file that can't be read or parsed is set aside and said so, not saved over
        // at the next change (deep bug run 2026-11-09).
        let saved = match load_saved_list(MIXES_FILE) {
            Some(v) => v,
            None => return,
        };
        for m in saved {
            let (w, h) = (
                m["width"].as_u64().unwrap_or(0),
                m["height"].as_u64().unwrap_or(0),
            );
            let name = m["name"].as_str().unwrap_or("");
            let made = if m["follow"].as_bool().unwrap_or(false) {
                self.sw.add_following_mix(name, w as usize, h as usize)
            } else {
                self.sw.add_mix(name, w as usize, h as usize)
            };
            if m["follow"].as_bool().unwrap_or(false) {
                // (Its buses are the program's: nothing else of it kept.)
                if let Err(e) = made {
                    idn_open_protocol::log::log(&format!("switcher: a saved mix: {e}"));
                }
                continue;
            }
            let id = match made {
                Ok(id) => id,
                Err(e) => {
                    idn_open_protocol::log::log(&format!("switcher: a saved mix: {e}"));
                    continue;
                }
            };
            let st = self.sw.state();
            let by_label = |l: &Value| {
                st["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|i| l.is_string() && i["label"] == *l)
                    .and_then(|i| i["id"].as_u64())
            };
            if let Some(p) = by_label(&m["program"]) {
                let _ = self.sw.mix_program(id, p as u32);
            }
            if let Some(p) = by_label(&m["preview"]) {
                let _ = self.sw.mix_preview(id, p as u32);
            }
            let kind = setups::transition_kind(&m, &by_label);
            let duration = m["duration"].as_f64().unwrap_or(1.0) as f32;
            let _ = self.sw.mix_transition(id, kind, duration);
        }
    }

    /// Keeps `mixes.json` up to date: written whenever the saved form of the mixes changes
    /// (a bus, a transition, an AUTO finishing), checked at each change of the state.
    pub(super) fn keep_mixes_saved(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("mixes-save".into())
            .spawn(move || {
                let (mut version, mut last) = (0, None);
                while let Some(api) = weak.upgrade() {
                    let now = saved_mixes(&api.sw.state());
                    if last.as_ref() != Some(&now) {
                        if last.is_some() {
                            api.save_mixes();
                        }
                        last = Some(now);
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

    /// Keeps the titles and slideshows in the state directory (`sources.json`), by label.
    /// How an input is kept with the setup (`sources.json`, §S.28): what `add_input` makes
    /// it again from (its camera or picture by label); `None` for one kept otherwise (a
    /// scene) or not at all (a feed, a delay).
    pub(super) fn saved_input(&self, st: &Value, i: &Value) -> Option<Value> {
        let label = |id: u64| {
            st["inputs"]
                .as_array()?
                .iter()
                .find(|i| i["id"].as_u64() == Some(id))?["label"]
                .as_str()
                .map(str::to_owned)
        };

        let src = &i["source"];
        if src["title"].is_object() {
            Some(json!({"label": i["label"], "title": src["title"]}))
        } else if src["slideshow"].is_object() {
            let mut show = src["slideshow"].clone();
            show.as_object_mut()?.remove("now");
            Some(json!({"label": i["label"], "slideshow": show}))
        } else if src["ptz"].is_object() {
            // The camera by its label.
            let camera = label(src["ptz"]["camera"].as_u64()?)?;
            Some(
                json!({"label": i["label"], "ptz": {"camera": camera, "preset": src["ptz"]["preset"]}}),
            )
        } else if src["virtual"].is_object() {
            let input = label(src["virtual"]["input"].as_u64()?)?;
            Some(
                json!({"label": i["label"], "virtual": {"input": input, "view": src["virtual"]["to"]}}),
            )
        } else {
            if let Some(url) = i["id"].as_u64().and_then(|id| {
                self.media
                    .streams_in
                    .lock_ok()
                    .get(&(id as u32))
                    .map(|(u, _)| u.clone())
            }) {
                return Some(json!({"label": i["label"], "stream": url}));
            }
            #[cfg(feature = "ffmpeg")]
            if let Some(made) = i["id"].as_u64().and_then(|id| {
                self.media
                    .lists
                    .lock_ok()
                    .get(&(id as u32))
                    .map(|(m, _)| m.clone())
            }) {
                return Some(json!({"label": i["label"], "list": made}));
            }
            // Every other input too (§S.28): files, pictures, colours, sources.
            #[cfg(feature = "ffmpeg")]
            if let Some(id) = i["id"].as_u64()
                && let Some((path, _)) = self.media.players.lock_ok().get(&(id as u32))
            {
                let looping = self.media.looping.lock_ok().contains(&(id as u32));
                return Some(json!({"label": i["label"], "file": path, "loop": looping}));
            }
            if let Some(p) = src["picture"].as_str() {
                Some(json!({"label": i["label"], "file": p}))
            } else if let Some(c) = src["colour"].as_str() {
                Some(json!({"label": i["label"], "colour": c}))
            } else {
                src["network"]
                    .as_str()
                    .map(|n| json!({"label": i["label"], "network": n}))
            }
        }
    }

    pub(super) fn save_sources(&self) {
        let st = self.sw.state();
        let kept: Vec<Value> = st["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| self.saved_input(&st, i))
            .collect();
        // And the shots still waiting for their camera.
        let mut kept = kept;
        kept.extend(self.pending_shots.lock_ok().iter().cloned());
        // And the inputs that couldn't be made at the start (unless one of their label has
        // been made since).
        let labels: Vec<String> = kept
            .iter()
            .filter_map(|x| x["label"].as_str().map(str::to_owned))
            .collect();
        kept.extend(
            self.unrestored
                .lock_ok()
                .iter()
                .filter(|x| {
                    !x["label"]
                        .as_str()
                        .is_some_and(|l| labels.iter().any(|k| k == l))
                })
                .cloned(),
        );
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir).and_then(|()| {
            idn_common::fsutil::write(dir.join(SOURCES_FILE), Value::Array(kept).to_string())
        });
    }

    /// An input's label (its id when it has none).
    /// The title templates saved by name (`templates.json`).
    pub(super) fn saved_templates(&self) -> serde_json::Map<String, Value> {
        load_saved_value(TEMPLATES_FILE)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    pub(super) fn label_of(&self, id: u32) -> String {
        self.sw.state()["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["id"] == id)
            .and_then(|i| i["label"].as_str().map(str::to_owned))
            .unwrap_or_else(|| id.to_string())
    }

    pub(super) fn write_state_file(name: &str, v: &Value) -> Result<(), String> {
        // Whole or not at all: written in place, a crash mid-write lost the inputs.
        idn_common::fsutil::write_atomic(&state_dir().join(name), v.to_string().as_bytes())
            .map_err(|e| format!("saving {name}: {e}"))
    }

    /// Keeps what the screens show in the state directory (`screens.json`).
    pub(super) fn save_screens(&self) {
        let list: Vec<Value> = self
            .displays
            .screens()
            .iter()
            .map(|s| s.to_json())
            .collect();
        let _ = std::fs::create_dir_all(state_dir()).and_then(|()| {
            idn_common::fsutil::write(
                state_dir().join(SCREENS_FILE),
                Value::Array(list).to_string(),
            )
        });
    }

    /// Puts the screens back as they were.
    pub(super) fn restore_screens(&self) {
        // A file that can't be read or parsed is set aside and said so, not saved over
        // at the next change (deep bug run 2026-11-09).
        let list = match load_saved_list(SCREENS_FILE) {
            Some(v) => v,
            None => return,
        };
        self.displays.set(
            list.iter()
                .filter_map(crate::displays::Screen::from_json)
                .collect(),
        );
    }

    /// Keeps the monitor's settings but its solos in the state directory (`monitor.json`).
    pub(super) fn save_monitor(&self) {
        let mut m = self.sw.state()["monitor"].clone();
        if let Some(o) = m.as_object_mut() {
            o.remove("solo");
            o.remove("card_status");
        }
        let _ = std::fs::create_dir_all(state_dir()).and_then(|()| {
            idn_common::fsutil::write(state_dir().join(MONITOR_FILE), m.to_string())
        });
    }

    /// Brings the monitor's settings back (a sound card that can't be opened is left off).
    pub(super) fn restore_monitor(&self) {
        let Some(m) = load_saved_value(MONITOR_FILE) else {
            return;
        };
        let card = m["card"].as_str().map(str::to_owned);
        let r = self.sw.monitor(
            m["preview"].as_bool(),
            None,
            m["volume_db"].as_f64().map(|v| v as f32),
            m["mute"].as_bool(),
            Some(card),
        );
        if let Err(e) = r {
            idn_open_protocol::log::log(&format!("switcher: the monitor: {e}"));
        }
    }

    /// Keeps the timers' names and modes in the state directory (`timers.json`).
    pub(super) fn save_timers(&self) {
        let timers: Vec<Value> = self.sw.state()["timers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| json!({"name": t["name"], "mode": t["mode"]}))
            .collect();
        let _ = std::fs::create_dir_all(state_dir()).and_then(|()| {
            idn_common::fsutil::write(
                state_dir().join(TIMERS_FILE),
                Value::Array(timers).to_string(),
            )
        });
    }

    /// Brings the saved timers back, stopped at their start.
    pub(super) fn restore_timers(&self) {
        // A file that can't be read or parsed is set aside and said so, not saved over
        // at the next change (deep bug run 2026-11-09).
        let saved = match load_saved_list(TIMERS_FILE) {
            Some(v) => v,
            None => return,
        };
        for t in saved {
            let m = &t["mode"];
            let v = if m == "up" {
                json!({"mode": "up"})
            } else if let Some(at) = m["until"].as_str() {
                json!({"mode": "until", "until": at})
            } else {
                json!({"mode": "down", "seconds": m["down"]})
            };
            if let Err(e) = timer_mode(&v).and_then(|mode| {
                self.sw
                    .timer(t["name"].as_str().unwrap_or(""), TimerAction::Set(mode))
            }) {
                idn_open_protocol::log::log(&format!("switcher: a saved timer: {e}"));
            }
        }
    }

    /// Brings the saved titles and slideshows back under their labels.
    pub(super) fn restore_sources(&self) {
        // A file that can't be read or parsed is set aside and said so, not saved over
        // at the next change (deep bug run 2026-11-09).
        let saved = match load_saved_list(SOURCES_FILE) {
            Some(v) => v,
            None => return,
        };
        for x in saved {
            if let Err(e) = self.restore_input(&x) {
                let msg = format!(
                    "switcher: the saved input {} wasn't made ({e}); kept in the saved inputs",
                    x["label"].as_str().unwrap_or("?")
                );
                idn_open_protocol::log::log(&msg);
                self.unrestored.lock_ok().push(x);
            }
        }
    }

    /// Brings the saved scenes back: each made again under its label, then its items, their
    /// inputs found by label (an item whose input isn't there is left out).
    pub(super) fn restore_scenes(&self) {
        // A file that can't be read or parsed is set aside and said so, not saved over
        // at the next change (deep bug run 2026-11-09).
        let saved = match load_saved_list(SCENES_FILE) {
            Some(v) => v,
            None => return,
        };
        let made: Vec<(u32, &Value)> = saved
            .iter()
            .filter_map(|sc| {
                let label = sc["label"].as_str().unwrap_or("Scene");
                Some((self.sw.add_input(label, Source::Scene).ok()?, &sc["items"]))
            })
            .collect();
        let st = self.sw.state();
        let by_label = |l: &str| {
            st["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["label"].as_str() == Some(l))
                .and_then(|i| i["id"].as_u64())
        };
        let globals: Vec<u32> = self.sw.global_layers().iter().map(|g| g.id).collect();
        for (id, items) in made {
            let scene_label = st["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["id"].as_u64() == Some(id as u64))
                .and_then(|i| i["label"].as_str())
                .unwrap_or("")
                .to_owned();
            let mut unplaced = Vec::new();
            let items: Vec<Value> = items
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|it| {
                    let mut it = it.clone();
                    // A copy of a global layer that is gone: left out.
                    if it["global"]
                        .as_u64()
                        .is_some_and(|g| !globals.contains(&(g as u32)))
                    {
                        return None;
                    }
                    if let Some(l) = it["source"]["input"].as_str() {
                        match by_label(l) {
                            Some(i) => it["source"] = json!(i),
                            None => {
                                // (Kept for the save, not dropped for good.)
                                unplaced.push(it.clone());
                                return None;
                            }
                        }
                    }
                    Some(it)
                })
                .collect();
            if !unplaced.is_empty() {
                idn_open_protocol::log::log(&format!(
                    "switcher: scene {scene_label}: {} item(s) whose input isn't here, kept in the saved scenes",
                    unplaced.len()
                ));
                self.unplaced_items.lock_ok().insert(scene_label, unplaced);
            }
            match crate::scene::items_from_json(&Value::Array(items))
                .and_then(|items| self.sw.scene_items(id, items))
            {
                Ok(()) => {}
                Err(e) => idn_open_protocol::log::log(&format!("switcher: a saved scene: {e}")),
            }
        }
    }

    /// Makes a saved preset input or virtual shot (§S.21) if its camera, by label, is here:
    /// whether it is now (made, or failed for good).
    pub(super) fn make_shot(&self, x: &Value) -> bool {
        let label = x["label"].as_str().unwrap_or("Shot");
        let st = self.sw.state();
        let by_label = |l: &Value| {
            st["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| l.is_string() && i["label"] == *l)
                .and_then(|i| i["id"].as_u64())
                .map(|i| i as u32)
        };
        let source = if x["ptz"].is_object() {
            by_label(&x["ptz"]["camera"]).map(|camera| Source::Ptz {
                camera,
                preset: x["ptz"]["preset"].as_u64().unwrap_or(1).min(255) as u8,
            })
        } else {
            by_label(&x["virtual"]["input"]).map(|input| Source::Virtual {
                input,
                view: view_from_json(&x["virtual"]["view"]),
            })
        };
        let Some(src) = source else {
            return false;
        };
        if let Err(e) = self.sw.add_input(label, src) {
            idn_open_protocol::log::log(&format!("switcher: a saved shot {label:?}: {e}"));
        }
        true
    }

    /// Makes the pending shots whose camera has appeared (checked at each change).
    pub(super) fn keep_pending_shots(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("shots-pending".into())
            .spawn(move || {
                let mut version = 0;
                while let Some(api) = weak.upgrade() {
                    let pending = std::mem::take(&mut *api.pending_shots.lock_ok());
                    let n = pending.len();
                    let left: Vec<Value> =
                        pending.into_iter().filter(|x| !api.make_shot(x)).collect();
                    let made = left.len() < n;
                    api.pending_shots.lock_ok().extend(left);
                    if made {
                        api.save_sources();
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
}

/// A saved file of the switcher's state: `None` when there is none, or (said in the log,
/// the file set aside) when it can't be read or parsed.
pub(super) fn load_saved_value(file: &str) -> Option<Value> {
    load_state(&state_dir().join(file))
}

/// As [`load_saved_value`], for a file that holds a list.
fn load_saved_list(file: &str) -> Option<Vec<Value>> {
    load_state(&state_dir().join(file))
}

/// A saved file read back as `T`: `None` when there is none, or (said in the log, the
/// file set aside, so the next save can't write over it) when it can't be read.
pub(crate) fn load_state<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    idn_common::fsutil::load_json(path).unwrap_or_else(|e| {
        idn_open_protocol::log::log(&format!("switcher: {e}"));
        None
    })
}
