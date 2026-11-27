//! The API's commands, one by one.

use super::*;

impl Service {
    /// One command (§S.8); its extra answer fields.
    /// Runs the command `cmd` with its JSON body (`POST /api/v1/<cmd>`'s): what it answers
    /// beside `ok`, or why not.
    pub fn command(&self, cmd: &str, v: &Value) -> Result<Value, String> {
        match cmd {
            // The MQTT card (§S.33): the broker, the login, the base topic, the triggers.
            "mqtt_set" => {
                return match self.mqtt.get() {
                    Some(m) => m.set(v).map(|_| json!({})),
                    None => Err("MQTT isn't started".into()),
                };
            }
            // A starter (G6, §S.32): its titles, timers and show added to what is here.
            "starter" => return self.starter(v["name"].as_str().unwrap_or("")),
            _ => {}
        }
        if cmd == "undo" || cmd == "redo" {
            return self.undo_redo(cmd == "undo");
        }
        // An edit of the setup: its way back, taken before it happens (§S.28).
        let record =
            UNDOABLE.contains(&cmd) && !self.replaying.load(std::sync::atomic::Ordering::Relaxed);
        let inverse = if record { self.inverse(cmd, v) } else { None };
        let r = self.command_inner(cmd, v);
        if record && let Ok(out) = &r {
            let step = if cmd == "add_input" {
                out["id"].as_u64().map(|id| Step {
                    what: format!("adding {}", self.label_of(id as u32)),
                    cmd: "remove_input".into(),
                    body: json!({"id": id}),
                })
            } else if cmd == "global_layer" && inverse.is_none() {
                // A new global layer: removed again.
                out["id"].as_u64().map(|id| Step {
                    what: "adding a global layer".into(),
                    cmd: "global_layer_remove".into(),
                    body: json!({"id": id}),
                })
            } else {
                inverse
            };
            if let Some(step) = step {
                let mut log = self.undo.lock_ok();
                // Edits of one cue in a row are one step back: the cue before the first.
                let same_cue = step.cmd == "cue_update"
                    && log
                        .back
                        .last()
                        .is_some_and(|b| b.cmd == "cue_update" && b.body["n"] == step.body["n"]);
                if !same_cue {
                    log.back.push(step);
                }
                let extra = log.back.len().saturating_sub(UNDO_STEPS);
                log.back.drain(..extra);
                log.forward.clear();
                drop(log);
                // The clients hear of it (the change itself may have answered them before
                // the step was kept).
                self.show_changed();
            }
        }
        if ["show_", "cue_", "pco_", "setup_"]
            .iter()
            .any(|p| cmd.starts_with(p))
        {
            self.show_changed();
        }
        // The inputs kept with the setup (§S.28).
        if r.is_ok()
            && matches!(
                cmd,
                "add_input" | "remove_input" | "rename_input" | "title" | "slideshow_set"
            )
        {
            self.save_sources();
        }
        r
    }

    pub(super) fn command_inner(&self, cmd: &str, v: &Value) -> Result<Value, String> {
        let sw = &self.sw;
        // An input by id, or (for `id`) by its label: `{"input": "Camera 1"}` (or
        // `"camera"`, or a label as `id`), as cues and MQTT commands send it (lab 396-B).
        let id = |k: &str| -> Result<u32, String> {
            if let Some(i) = v[k].as_u64() {
                // (Out of range is an error, not another input: 2^32 + 5 was input 5.)
                return u32::try_from(i).map_err(|_| format!("{k}: no such input"));
            }
            let label = if k == "id" {
                v["id"]
                    .as_str()
                    .or_else(|| v["input"].as_str())
                    .or_else(|| v["camera"].as_str())
            } else if k == "scene" {
                v["scene"].as_str()
            } else {
                None
            };
            let Some(label) = label else {
                return Err(format!("{k}: a number"));
            };
            sw.state()["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["label"] == label)
                .and_then(|i| i["id"].as_u64())
                .map(|i| i as u32)
                .ok_or_else(|| format!("no input {label:?}"))
        };
        let f = |k: &str, d: f64| v[k].as_f64().unwrap_or(d) as f32;
        match cmd {
            // A stream (§S.22): SRT, RTMP (as a server too), RTSP, HLS… as an input.
            "add_input" if v["stream"].is_string() => {
                let url = v["stream"].as_str().unwrap_or_default().trim();
                let label = v["label"].as_str().map(str::to_owned).unwrap_or_else(|| stream_label(url));
                let (addr, ingest) = self.media.open_stream(url, &label)?;
                let i = sw.add_input(&label, Source::Network(addr)).map_err(|e| e.to_string())?;
                self.media.streams_in.lock_ok().insert(i, (url.to_owned(), ingest));
                self.save_sources();
                return Ok(json!({"id": i}));
            }
            // A list (§S.18): videos and pictures one after another, as one input.
            #[cfg(feature = "ffmpeg")]
            "add_input" if v["list"].is_object() => {
                let (addr, player) = self.media.open_list(&v["list"])?;
                let label = v["label"].as_str().unwrap_or("List");
                let i = sw.add_input(label, Source::Network(addr)).map_err(|e| e.to_string())?;
                let made = list_made(&v["list"], &self.media)?;
                self.media.lists.lock_ok().insert(i, (made, player));
                self.save_sources();
                return Ok(json!({"id": i}));
            }
            #[cfg(feature = "ffmpeg")]
            "list" => {
                let i = id("id")?;
                let lists = self.media.lists.lock_ok();
                let (_, p) = lists.get(&i).ok_or("not a list")?;
                match v["action"].as_str().unwrap_or("") {
                    "next" => p.next(),
                    "previous" => p.previous(),
                    "play" => p.resume(),
                    "pause" => p.pause(),
                    "restart" => p.restart(),
                    "goto" => p.goto(v["n"].as_u64().ok_or("n: an item's place, from 0")? as usize),
                    _ => return Err("action: next, previous, play, pause, restart or goto".into()),
                }
                Ok(())
            }
            #[cfg(feature = "ffmpeg")]
            "list_items" => {
                let i = id("id")?;
                if !self.media.lists.lock_ok().contains_key(&i) {
                    return Err("not a list".into());
                }
                // Resolved (library files fetched) before the lists are locked: the lock held
                // through a download froze the state, its long poll and MQTT (deep bug run
                // 2026-11-09).
                let (items, made_items) = (
                    list_items(v, &self.media)?,
                    list_made(v, &self.media)?["items"].clone(),
                );
                {
                    let mut lists = self.media.lists.lock_ok();
                    let (made, p) = lists.get_mut(&i).ok_or("not a list")?;
                    p.set_items(items)?;
                    made["items"] = made_items;
                }
                self.save_sources();
                Ok(())
            }
            #[cfg(feature = "ffmpeg")]
            "add_input" if v["file"].is_string() => {
                // The name as given is what the setup keeps (a relative or library name
                // goes with the show to another machine); the file it points to is played.
                let path = v["file"].as_str().unwrap_or_default();
                let file = self.media.resolve(path)?;
                let label = v["label"].as_str().map(str::to_owned).unwrap_or_else(|| {
                    std::path::Path::new(path).file_name().map_or(path.into(), |n| n.to_string_lossy().into_owned())
                });
                // A picture: decoded once and held (not played as a one-frame video).
                if is_picture(&file) {
                    let frame = Arc::new(load_picture(&file).map_err(|e| format!("{path}: {e}"))?);
                    let source = Source::Still { path: path.to_owned(), frame };
                    let i = sw.add_input(&label, source).map_err(|e| e.to_string())?;
                    return Ok(json!({"id": i}));
                }
                let (addr, player) = self.media.open(&file, v["loop"].as_bool().unwrap_or(false))?;
                let i = sw.add_input(&label, Source::Network(addr)).map_err(|e| e.to_string())?;
                self.media.players.lock_ok().insert(i, (path.to_owned(), player));
                if v["loop"].as_bool() == Some(true) {
                    self.media.looping.lock_ok().insert(i);
                }
                return Ok(json!({"id": i}));
            }
            #[cfg(feature = "ffmpeg")]
            "media" => {
                let i = id("id")?;
                let players = self.media.players.lock_ok();
                let (_, p) = players.get(&i).ok_or("not a media input")?;
                match v["action"].as_str().unwrap_or("") {
                    "play" => p.resume(),
                    "pause" => p.pause(),
                    // Not past what a Duration holds (1e300 panicked with the lock held).
                    "seek" => p.seek(Duration::try_from_secs_f64(v["position"].as_f64().unwrap_or(0.0).max(0.0)).map_err(|_| "position: seconds")?),
                    "restart" => {
                        p.seek(Duration::ZERO);
                        p.resume();
                    }
                    _ => return Err("action: play, pause, seek or restart".into()),
                }
                Ok(())
            }
            // A title from a template by name (a saved one, or a style built in: lower, two,
            // caption, centre, corner, clock, count, score) with `text` in it.
            "add_input" if v["template"].is_string() => {
                let (title, made_label) = rules::template_title(
                    v["template"].as_str().unwrap_or_default(),
                    v["text"].as_str().unwrap_or(""),
                    &self.saved_templates(),
                )?;
                let label = v["label"].as_str().map_or(made_label, str::to_owned);
                return self.command_inner("add_input", &json!({"title": title, "label": label}));
            }
            "add_input" => {
                let source = if let Some(name) = v["network"].as_str() {
                    Source::Network(name.into())
                } else if let Some(c) = v["colour"].as_str() {
                    Source::Colour(parse_colour(c).ok_or("colour: #rrggbb")?)
                } else if v["scene"].as_bool() == Some(true) {
                    Source::Scene
                } else if v["title"].is_object() {
                    Source::Title
                } else if v["slideshow"].is_object() {
                    Source::Slideshow(slideshow_from_json(&v["slideshow"], &self.media)?)
                } else if v["ptz"].is_object() {
                    Source::Ptz {
                        camera: v["ptz"]["camera"].as_u64().ok_or("ptz camera: an input id")? as u32,
                        preset: v["ptz"]["preset"].as_u64().ok_or("ptz preset: 1 to 16")?.min(255) as u8,
                    }
                } else if v["virtual"].is_object() {
                    Source::Virtual {
                        input: v["virtual"]["input"].as_u64().ok_or("virtual input: an input id")? as u32,
                        view: view_from_json(&v["virtual"]["view"]),
                    }
                } else {
                    return Err("give network (a source name or address), file (a video, sound or picture file), colour, scene: true, title or slideshow".into());
                };
                // A title's layers checked before it is added.
                let title = match &source {
                    Source::Title => Some(crate::title::Title::from_json(&v["title"])?),
                    _ => None,
                };
                let label = v["label"].as_str().map(str::to_owned).unwrap_or_else(|| match &source {
                    Source::Network(n) => n.clone(),
                    Source::Colour(_) => v["colour"].as_str().unwrap_or("Colour").to_owned(),
                    Source::Still { path, .. } => path.clone(),
                    Source::Feed => "Feed".to_owned(),
                    Source::Delay { input, .. } => format!("Delay of {input}"),
                    Source::Scene => "Scene".to_owned(),
                    Source::Title => "Title".to_owned(),
                    Source::Slideshow(_) => "Slideshow".to_owned(),
                    Source::Ptz { camera, preset } => format!("{} · P{preset}", self.label_of(*camera)),
                    Source::Virtual { input, .. } => format!("{} · shot", self.label_of(*input)),
                });
                let scene = matches!(source, Source::Scene);
                let kept = matches!(
                    source,
                    Source::Title | Source::Slideshow(_) | Source::Ptz { .. } | Source::Virtual { .. }
                );
                let i = sw.add_input(&label, source).map_err(|e| e.to_string())?;
                if let Some(t) = title
                    && let Err(e) = sw.set_title(i, t)
                {
                    let _ = sw.remove_input(i);
                    return Err(e);
                }
                if scene {
                    self.save_scenes();
                }
                // A countdown title needs its timer: five minutes, unless there is one.
                if v["title"].to_string().contains("{timer:countdown}")
                    && !sw.state()["timers"].as_array().into_iter().flatten().any(|t| t["name"] == "countdown")
                    && sw.timer("countdown", TimerAction::Set(TimerMode::Down(Duration::from_secs(300)))).is_ok()
                {
                    self.save_timers();
                }
                if kept {
                    self.save_sources();
                }
                return Ok(json!({"id": i}));
            }
            // An input's filters, as a whole, in order (§S.17).
            "filters" => {
                let f = crate::filter::items_from_json(&v["filters"])?;
                sw.set_filters(id("id")?, f)
            }
            // A title's layers, as a whole (§S.16).
            "title" => {
                let t = crate::title::Title::from_json(v)?;
                sw.set_title(id("id")?, t)?;
                self.save_sources();
                Ok(())
            }
            // Some of a title's fields, live (§S.19): {"id", "fields": {"name": value}}.
            "title_fields" => {
                let f = crate::title::fields_from_json(&v["fields"])?;
                sw.set_title_fields(id("id")?, f)?;
                self.save_sources();
                Ok(())
            }
            // A camera's control (§S.21): {"id", "control": "network" | {"visca": ip|tcp|
            // udp, "address"} | null}.
            "camera" => {
                let c = match &v["control"] {
                    Value::Null => None,
                    c => Some(crate::ptz::Control::from_json(c)?),
                };
                sw.set_camera(id("id")?, c)
            }
            // A camera moved: {"id" (the camera or a preset input), "move": drive|zoom|
            // stop|home|autofocus|recall|store, "pan", "tilt", "speed", "preset"}.
            "ptz" => sw.ptz(id("id")?, crate::ptz::Move::from_json(v)?),
            // A virtual shot moved: {"id", "view": {"x", "y", "zoom"}, "seconds"}.
            "virtual_view" => {
                sw.virtual_view(id("id")?, view_from_json(&v["view"]), f("seconds", 0.0))?;
                self.save_sources();
                Ok(())
            }
            // A title's data moved on a row (§S.20): {"id", "action": next|previous|goto|
            // play|pause, "n"}; "read" reads it again now.
            "title_row" => {
                let action = match v["action"].as_str().unwrap_or("") {
                    "next" => SlideAction::Next,
                    "previous" => SlideAction::Previous,
                    "play" => SlideAction::Play,
                    "pause" => SlideAction::Pause,
                    "goto" => SlideAction::Goto(v["n"].as_u64().ok_or("n: a row, from 0")? as usize),
                    "read" => return sw.title_data_read(id("id")?).map(|()| json!({})),
                    _ => return Err("action: next, previous, goto, play, pause or read".into()),
                };
                sw.title_row(id("id")?, action)
            }
            // A number added to a title's field (a score): {"id", "name", "by": 1}.
            "title_field_add" => {
                let name = v["name"].as_str().ok_or("name: a field")?;
                let value = sw.add_title_field(id("id")?, name, v["by"].as_f64().unwrap_or(1.0))?;
                self.save_sources();
                return Ok(json!({ "value": value }));
            }
            // A title kept as a template: {"name", "title": {"layers", "fields"}}; with
            // "title": null, removed.
            "title_template" => {
                let name = v["name"].as_str().filter(|n| !n.is_empty() && n.len() <= 64).ok_or("name: 1–64 characters")?;
                let path = state_dir().join(TEMPLATES_FILE);
                // (One that can't be read is set aside, not saved over; bug review 3.)
                let mut all: serde_json::Map<String, Value> =
                    super::saved::load_saved_value(TEMPLATES_FILE)
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default();
                if v["title"].is_null() {
                    all.remove(name);
                } else {
                    let t = crate::title::Title::from_json(&v["title"])?;
                    if all.len() >= 100 && !all.contains_key(name) {
                        return Err("at most 100 templates".into());
                    }
                    all.insert(name.to_owned(), t.to_json());
                }
                std::fs::create_dir_all(state_dir())
                    .and_then(|()| idn_common::fsutil::write(&path, Value::Object(all).to_string()))
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            // A timer (§S.19): {"name", "action": set|start|stop|reset|remove, "mode":
            // down|up|until, "seconds", "until": "HH:MM"}.
            "timer" => {
                let name = v["name"].as_str().unwrap_or("");
                let action = match v["action"].as_str().unwrap_or("") {
                    "set" => TimerAction::Set(timer_mode(v)?),
                    "start" => TimerAction::Start,
                    "stop" => TimerAction::Stop,
                    "reset" => TimerAction::Reset,
                    "remove" => TimerAction::Remove,
                    _ => return Err("action: set, start, stop, reset or remove".into()),
                };
                sw.timer(name, action)?;
                if matches!(action, TimerAction::Set(_) | TimerAction::Remove) {
                    self.save_timers();
                }
                Ok(())
            }
            // A slideshow moved on: {"id", "action": next|previous|play|pause|goto, "n"}.
            "slideshow" => {
                let action = match v["action"].as_str().unwrap_or("") {
                    "next" => SlideAction::Next,
                    "previous" => SlideAction::Previous,
                    "play" => SlideAction::Play,
                    "pause" => SlideAction::Pause,
                    "goto" => SlideAction::Goto(v["n"].as_u64().ok_or("n: a picture's place, from 0")? as usize),
                    _ => return Err("action: next, previous, play, pause or goto".into()),
                };
                sw.slideshow(id("id")?, action)
            }
            // The slideshow or list on program, else on preview, else the first, moved on:
            // {"action": next|previous|play|pause|goto (|restart, a list's), "n"}.
            "slide" => {
                #[cfg(feature = "ffmpeg")]
                let lists: Vec<u32> = self.media.lists.lock_ok().keys().copied().collect();
                #[cfg(not(feature = "ffmpeg"))]
                let lists: Vec<u32> = Vec::new();
                let (id, list) = rules::pick_slides(&sw.state(), &lists).ok_or("no slideshow or list")?;
                let mut body = v.clone();
                body["id"] = json!(id);
                let cmd = if list { "list" } else { "slideshow" };
                self.command_inner(cmd, &body)?;
                return Ok(json!({"id": id, "kind": cmd}));
            }
            // A scene layer's sound (§S.12): the layer by `layer` (its id) or `item` (its
            // place, from 0).
            "scene_item_sound" => {
                let layer = match (v["layer"].as_u64(), v["item"].as_u64()) {
                    (Some(l), _) => crate::scene::LayerRef::Id(u32::try_from(l).map_err(|_| "layer: no such layer")?),
                    (None, Some(i)) => crate::scene::LayerRef::Index(i as usize),
                    _ => return Err("layer: the layer's id (or item: its place, from 0)".into()),
                };
                let volume = v["volume_db"].as_f64().map(|x| x as f32);
                sw.scene_item_sound(id("id")?, layer, volume, v["muted"].as_bool())?;
                self.save_scenes();
                Ok(())
            }
            // A scene's layers, as a whole (§S.12).
            "scene_items" => {
                let items = crate::scene::items_from_json(&v["items"])?;
                sw.scene_items(id("id")?, items)?;
                self.save_scenes();
                Ok(())
            }
            // One layer of a scene shown or hidden, with its fade (§S.12): live, not undone.
            "scene_layer_visible" => {
                let layer = v["layer"].as_u64().and_then(|l| u32::try_from(l).ok()).ok_or("layer: the layer's id")?;
                let on = v["on"].as_bool().unwrap_or(true);
                match v.get("fade").filter(|f| !f.is_null()) {
                    // This once over `fade` seconds (0: a cut), not the layer's own.
                    Some(f) => sw.scene_layer_visible_fade(
                        id("scene")?,
                        layer,
                        on,
                        f.as_f64().ok_or("fade: seconds")? as f32,
                    )?,
                    None => sw.scene_layer_visible(id("scene")?, layer, on)?,
                }
                self.save_scenes();
                Ok(())
            }
            // A global layer defined or changed (§S.5): {"layer": {...}, "add_to_scenes"}.
            "global_layer" => {
                let def = crate::scene::Item::from_json(&v["layer"])?;
                self.unplaced_globals.lock_ok().remove(&u64::from(def.id));
                let gid = sw.set_global_layer(def, v["add_to_scenes"].as_bool().unwrap_or(false))?;
                self.save_globals();
                self.save_scenes();
                return Ok(json!({"id": gid}));
            }
            "global_layer_remove" => {
                let gid = v["id"].as_u64().and_then(|g| u32::try_from(g).ok()).ok_or("id: a global layer's")?;
                sw.remove_global_layer(gid)?;
                self.save_globals();
                self.save_scenes();
                Ok(())
            }
            // An input kept receiving while not in use (§S.3).
            "keep_active" => sw.keep_active(id("id")?, v["on"].as_bool().unwrap_or(true)),
            // A scene's desk snapshot recalled as it goes live: {id, desk, snapshot}
            // (snapshot "" or null: none).
            "scene_snapshot" => {
                let to = match (v["desk"].as_str(), v["snapshot"].as_str()) {
                    (Some(d), Some(s)) if !d.trim().is_empty() && !s.trim().is_empty() => {
                        Some((d.trim().to_owned(), s.trim().to_owned()))
                    }
                    _ => None,
                };
                sw.scene_snapshot(id("id")?, to)
            }
            // The multiview's layout (§S.7): its tiles, or null for the default.
            "multiview_layout" => {
                let tiles = match &v["tiles"] {
                    Value::Null => None,
                    t => Some(crate::multiview::layout_from_json(t)?),
                };
                sw.set_multiview_layout(tiles)?;
                self.save_multiview();
                Ok(())
            }
            // The multiview sent on the network or not (§S.7).
            "multiview" => {
                let on = v["output"].as_bool().ok_or("output: true or false")?;
                sw.multiview_output(on)?;
                self.save_multiview();
                Ok(())
            }
            "remove_input" => {
                let i = id("id")?;
                let r = sw.remove_input(i);
                // Taken out, then dropped with no lock held: a stream input's drop joins
                // its supervisor, and every client's state waited on the lock meanwhile.
                #[cfg(feature = "ffmpeg")]
                let gone = (
                    self.media.players.lock_ok().remove(&i),
                    self.media.looping.lock_ok().remove(&i),
                    self.media.lists.lock_ok().remove(&i),
                );
                let stream = self.media.streams_in.lock_ok().remove(&i);
                #[cfg(feature = "ffmpeg")]
                drop(gone);
                drop(stream);
                self.save_scenes();
                self.save_sources();
                r
            }
            "rename_input" => {
                let r = sw.rename_input(id("id")?, v["label"].as_str().unwrap_or(""));
                self.save_scenes();
                self.save_sources();
                r
            }
            "audio_offset" => sw.input_audio_offset(
                id("id")?,
                v["ms"].as_u64().ok_or("ms: 0 to 500")?.min(500) as u32,
            ),
            "preview" => sw.preview(id("id")?),
            "program" => sw.program(id("id")?),
            "cut" => sw.cut(),
            "auto" => sw.auto(),
            "tbar" => sw.tbar(f("position", 0.0)),
            "transition" => {
                let name = v["kind"].as_str().unwrap_or("mix");
                let kind = if name == "luma" {
                    Kind::Luma {
                        pattern: id("pattern")?,
                        soft: f("soft", 0.05).clamp(0.0, 1.0),
                        invert: v["invert"].as_bool().unwrap_or(false),
                    }
                } else {
                    Kind::parse(name, f("soft", 0.05)).ok_or("kind: cut, mix, fade, wipe-left|right|up|down|circle, zoom, luma, stinger-1…8")?
                };
                sw.transition(kind, f("duration", 1.0))
            }
            // A stinger slot (§S.14): {"n": 1–8, "input": id, "duration": s, "cut": s}, or
            // {"n": …, "input": null} to empty it.
            "stinger" => {
                let n = v["n"].as_u64().ok_or("n: 1 to 8")? as usize;
                let slot = v["input"].as_u64().map(|i| crate::Stinger {
                    input: i as u32,
                    duration: f("duration", 1.0),
                    cut: f("cut", 0.5),
                });
                sw.stinger(n, slot)
            }
            "ftb" => sw.ftb(),
            // Without `source` (§S.26): `take_feed` and `denoise`, each when given (true
            // or false; anything else refused), the source, gain and delay as they are.
            "audio" if v.get("source").is_none() && (v.get("take_feed").is_some() || v.get("denoise").is_some()) => {
                let flag = |k: &str| match v.get(k) {
                    None => Ok(None),
                    Some(Value::Bool(b)) => Ok(Some(*b)),
                    Some(_) => Err(format!("{k}: true or false")),
                };
                let (take, denoise) = (flag("take_feed")?, flag("denoise")?);
                if v.get("gain_db").is_some() || v.get("delay_ms").is_some() {
                    return Err("gain_db and delay_ms go with source".into());
                }
                if let Some(on) = take {
                    sw.take_feed(on)?;
                }
                if let Some(on) = denoise {
                    sw.audio_denoise(on)?;
                }
                Ok(())
            }
            "audio" => {
                let source = match &v["source"] {
                    Value::String(s) if s == "none" => AudioSource::None,
                    Value::String(s) if s == "program" => AudioSource::FollowProgram,
                    Value::String(s) if s == "routed" => AudioSource::Routed,
                    // A sound card: {"card": DEVICE or null (the default), "first": 1…}.
                    x if x.get("card").is_some() => AudioSource::Card {
                        device: x["card"].as_str().filter(|d| !d.is_empty() && *d != "default").map(str::to_owned),
                        first: (x["first"].as_u64().unwrap_or(1).max(1) - 1) as u16,
                    },
                    x => match x["input"].as_u64() {
                        Some(i) => AudioSource::Input(i as u32),
                        None => return Err("source: \"none\", \"program\", \"routed\", {\"input\": id} or {\"card\": device}".into()),
                    },
                };
                // A value not given stays as it is (MQTT and cue actions send only the
                // source: the stream's trim and lip-sync delay were reset; F7-5).
                let (gain_now, delay_now) = sw.audio_trim();
                sw.audio(
                    source,
                    f("gain_db", f64::from(gain_now)),
                    v["delay_ms"].as_u64().map_or(delay_now, |d| d.min(u32::MAX as u64) as u32),
                )
                    .and_then(|()| match v["denoise"].as_bool() {
                        Some(on) => sw.audio_denoise(on),
                        None => Ok(()),
                    })
                    .and_then(|()| match v["take_feed"].as_bool() {
                        Some(on) => sw.take_feed(on),
                        None => Ok(()),
                    })
            }
            // The monitor (§S.24): {"preview", "solo": [ids], "volume_db", "mute",
            // "card": device ("" the default) or null (none)}, each optional.
            "monitor" => {
                let solo = match &v["solo"] {
                    Value::Array(a) => Some(a.iter().filter_map(|i| i.as_u64()).map(|i| i as u32).collect()),
                    _ => None,
                };
                let card = v.get("card").map(|c| c.as_str().map(str::to_owned));
                sw.monitor(
                    v["preview"].as_bool(),
                    solo,
                    v["volume_db"].as_f64().map(|v| v as f32),
                    v["mute"].as_bool(),
                    card,
                )?;
                self.save_monitor();
                Ok(())
            }
            "monitor_solo" => sw.monitor_solo(id("input")?, v["on"].as_bool().unwrap_or(true)),
            // A screen of this machine (idn-display §D.3): {"screen": "1" | "kms:HDMI-A-1",
            // "output": full name, or "" to turn it off, "audio": a sound card or null}.
            "screen" => {
                let screen = v["screen"].as_str().filter(|s| !s.is_empty()).ok_or("screen: its number or name")?;
                let mut list: Vec<_> = self.displays.screens().into_iter().filter(|s| s.screen != screen).collect();
                if let Some(output) = v["output"].as_str().filter(|o| !o.is_empty()) {
                    if !sw.outputs().iter().any(|o| o.1 == output) {
                        return Err(format!("no output {output:?}"));
                    }
                    list.push(crate::displays::Screen {
                        screen: screen.to_owned(),
                        output: output.to_owned(),
                        audio: v["audio"].as_str().map(str::to_owned),
                    });
                }
                self.displays.set(list);
                self.save_screens();
                Ok(())
            }
            // Closed captions on the program (§S.29): a line; on/off and when they clear.
            "caption" => sw.caption(v["text"].as_str().unwrap_or("")),
            "captions" => {
                sw.captions(v["on"].as_bool(), v["clear_after"].as_f64());
                // The visible captions with them (one CC button): a global layer
                // shown or hidden in every scene as the closed captions go on or off.
                if let (Some(on), Some(g)) = (v["on"].as_bool(), v["layer"].as_u64()) {
                    sw.global_layer_visible(
                        u32::try_from(g).map_err(|_| "layer: a global layer's id")?,
                        on,
                    )?;
                    self.save_scenes();
                }
                // From a captions source (G2), or "" for none.
                if let Some(f) = v["from"].as_str() {
                    sw.captions_from(Some(f.to_owned()));
                }
                Ok(())
            }
            // Setups (§S.28).
            "setup_save" => self.setup_save(v["name"].as_str().unwrap_or("")),
            "setup_load" => self.setup_load(v["name"].as_str().unwrap_or("")),
            "setup_delete" => {
                std::fs::remove_file(Self::setup_path(v["name"].as_str().unwrap_or(""))?).map_err(|e| format!("deleting the setup: {e}"))
            }
            // The show (§S.27).
            "cue_go" => {
                let next = self.show.lock_ok().next;
                // (From the follow-on: only if that cue is still the next.)
                if let Some(want) = v["if_next"].as_u64()
                    && next != Some(want as usize)
                {
                    return Ok(json!({"errors": []}));
                }
                match next {
                    Some(k) => return Ok(json!({"errors": self.fire_cue(k)})),
                    None => Err("the show is at its end".into()),
                }
            }
            "cue_back" => {
                let prev = {
                    let r = self.show.lock_ok();
                    r.current.and_then(|(cur, _)| (0..cur).rev().find(|&i| !r.show.cues[i].header()))
                };
                match prev {
                    Some(k) => return Ok(json!({"errors": self.fire_cue(k)})),
                    None => Err("no cue before this one".into()),
                }
            }
            "cue_standby" | "cue_fire" => {
                // `n`, or `cue` as the spec and the MQTT notes give it (§S.33).
                let n = v["n"]
                    .as_u64()
                    .or_else(|| v["cue"].as_u64())
                    .filter(|&n| n >= 1)
                    .ok_or("n: a cue, from 1")? as usize;
                let len = self.show.lock_ok().show.cues.len();
                if n > len {
                    return Err(format!("cues are 1 to {len}"));
                }
                // A header is a heading in the list: nothing to fire or stand by on.
                if self.show.lock_ok().show.cues.get(n - 1).is_some_and(|c| c.header()) {
                    return Err(format!("cue {n} is a header"));
                }
                if cmd == "cue_fire" {
                    return Ok(json!({"errors": self.fire_cue(n - 1)}));
                } else {
                    self.show.lock_ok().standby(n - 1)
                }
            }
            "show_set" => {
                let show = crate::show::Show::from_json(&v["show"])?;
                // (`if_version`: only over the show it was read from.)
                let mut r = self.show.lock_ok();
                Self::check_show_version(&r.show, v)?;
                Self::write_state_file(SHOW_FILE, &show.to_json())?;
                r.replace(show);
                return Ok(json!({"version": rules::show_version(&r.show)}));
            }
            // One cue's edits (each with `if_version`, optional): added at `at` (from 1; the
            // end if missing), changed (the keys given), deleted, moved to `to`.
            "cue_add" => {
                let mut cue = match &v["cue"] {
                    Value::Null => crate::show::Cue { kind: "item".into(), ..Default::default() },
                    c => crate::show::Cue::from_json(c)?,
                };
                return self.edit_cues(v, |cues| {
                    let at = match v["at"].as_u64() {
                        Some(a) if a >= 1 && a as usize <= cues.len() + 1 => a as usize - 1,
                        Some(_) => return Err(format!("at: 1 to {}", cues.len() + 1)),
                        None => cues.len(),
                    };
                    if v["cue"]["name"].is_null() {
                        cue.name = format!("Cue {}", cues.len() + 1);
                    }
                    cues.insert(at, (None, cue));
                    Ok(json!({"n": at + 1}))
                });
            }
            "cue_update" | "cue_delete" | "cue_move" => {
                let n = v["n"].as_u64().filter(|&n| n >= 1).ok_or("n: a cue, from 1")? as usize;
                return self.edit_cues(v, |cues| {
                    if n > cues.len() {
                        return Err(format!("cues are 1 to {}", cues.len()));
                    }
                    match cmd {
                        "cue_update" => {
                            let mut c = cues[n - 1].1.to_json();
                            for (k, x) in v["cue"].as_object().ok_or("cue: the keys to change")? {
                                c[k] = x.clone();
                            }
                            cues[n - 1].1 = crate::show::Cue::from_json(&c)?;
                            Ok(json!({"n": n}))
                        }
                        "cue_delete" => {
                            cues.remove(n - 1);
                            Ok(json!({}))
                        }
                        _ => {
                            let to = v["to"].as_u64().filter(|&t| t >= 1 && t as usize <= cues.len());
                            let to = to.ok_or(format!("to: 1 to {}", cues.len()))? as usize;
                            let c = cues.remove(n - 1);
                            cues.insert(to - 1, c);
                            Ok(json!({"n": to}))
                        }
                    }
                });
            }
            "show_new" | "show_load" => {
                let name = v["name"].as_str().unwrap_or("").trim().to_owned();
                let show = if cmd == "show_new" {
                    crate::show::Show { name, ..Default::default() }
                } else {
                    let saved = self.saved_shows();
                    let v = saved.get(&name).ok_or(format!("no show {name:?}"))?;
                    crate::show::Show::from_json(v)?
                };
                Self::write_state_file(SHOW_FILE, &show.to_json())?;
                *self.show.lock_ok() = crate::show::Runner::new(show);
                Ok(())
            }
            "show_save" | "show_delete" => {
                let name = v["name"].as_str().unwrap_or("").trim().to_owned();
                if name.is_empty() {
                    return Err("name: the show's".into());
                }
                let mut saved = self.saved_shows();
                if cmd == "show_save" {
                    let mut show = self.show.lock_ok().show.clone();
                    show.name = name.clone();
                    saved.insert(name, show.to_json());
                    self.show.lock_ok().show.name = show.name;
                } else {
                    saved.remove(&name).ok_or(format!("no show {name:?}"))?;
                }
                Self::write_state_file(SHOWS_FILE, &Value::Object(saved))
            }
            // Planning Center (§S.27): the user's personal access token, kept on this
            // machine only.
            "pco_login" => {
                let pco = crate::show::Pco {
                    app_id: v["app_id"].as_str().unwrap_or("").trim().to_owned(),
                    secret: v["secret"].as_str().unwrap_or("").trim().to_owned(),
                };
                if pco.app_id.is_empty() || pco.secret.is_empty() {
                    return Err("app_id and secret: a personal access token".into());
                }
                pco.plans()?;
                // Readable by this user only, as every file holding a password (a token
                // is one); its set-aside copies keep that mode.
                let body = json!({"app_id": pco.app_id, "secret": pco.secret}).to_string();
                idn_common::fsutil::write_private(&state_dir().join(PCO_FILE), body.as_bytes())
                    .map_err(|e| format!("saving {PCO_FILE}: {e}"))
            }
            "pco_logout" => {
                // The token gone from the disk: the file, and any copy of it set aside as
                // damaged.
                let path = state_dir().join(PCO_FILE);
                let _ = std::fs::remove_file(&path);
                idn_common::fsutil::remove_set_asides(&path);
                Ok(())
            }
            "pco_plans" => {
                let pco = self.pco().ok_or("no Planning Center token yet")?;
                return Ok(json!({"plans": pco.plans()?}));
            }
            "pco_import" => {
                let pco = self.pco().ok_or("no Planning Center token yet")?;
                let show = pco.import(
                    v["service_type"].as_str().ok_or("service_type")?,
                    v["plan"].as_str().ok_or("plan")?,
                )?;
                Self::write_state_file(SHOW_FILE, &show.to_json())?;
                *self.show.lock_ok() = crate::show::Runner::new(show);
                Ok(())
            }
            "stream_add" => return self.streams.add(sw, v),
            // Saved destinations (their keys kept here, never in the state).
            // The program format, from now (§S.38; not while live).
            "format" => return self.set_format(v),
            "destination_save" => return self.destination_save(v),
            "destination_remove" => self.destination_remove(v),
            "go_live" => return self.go_live(v),
            "iso_add" => {
                // No file: the input's label as its name (in Videos/IDN Recordings).
                let mut v = v.clone();
                if v["file"].as_str().is_none_or(|f| f.trim().is_empty()) {
                    let input = v["input"].as_u64().ok_or("input: an input id")?;
                    let label = self.label_of(u32::try_from(input).map_err(|_| "input: no such input")?);
                    v["file"] = json!(rules::iso_file_name(&label));
                }
                // Its answer names the file written (" (2)" after a name taken, its folder).
                return self.streams.iso_add(sw, &v);
            }
            // Independent mixes (§S.15).
            "add_mix" => {
                let (w, h) = match v["format"].as_str() {
                    Some(f) => {
                        // A size only ("1080x1920", "720p"): a mix runs at the program's rate.
                        let f = if f.ends_with('p') {
                            format!("{f}30")
                        } else if !f.contains('@') {
                            format!("{f}@30")
                        } else {
                            f.to_owned()
                        };
                        let (w, h, _) = parse_format(&f).ok_or("format: WxH, 1080p, 2160p…")?;
                        (w, h)
                    }
                    None => (
                        v["width"].as_u64().ok_or("width")? as usize,
                        v["height"].as_u64().ok_or("height")? as usize,
                    ),
                };
                let name = v["name"].as_str().unwrap_or("").trim();
                // `follow`: a version of the program in another shape (vertical, square).
                let id = if v["follow"].as_bool().unwrap_or(false) {
                    sw.add_following_mix(name, w, h)
                } else {
                    sw.add_mix(name, w, h)
                }
                .map_err(|e| e.to_string())?;
                self.save_mixes();
                return Ok(json!({"id": id}));
            }
            "remove_mix" => {
                let r = sw.remove_mix(id("mix")?);
                self.save_mixes();
                r
            }
            "mix_program" => sw.mix_program(id("mix")?, id("id")?),
            "mix_preview" => sw.mix_preview(id("mix")?, id("id")?),
            "mix_cut" => sw.mix_cut(id("mix")?),
            "mix_auto" => sw.mix_auto(id("mix")?),
            "mix_transition" => {
                let name = v["kind"].as_str().unwrap_or("mix");
                let kind = if name == "luma" {
                    Kind::Luma {
                        pattern: id("pattern")?,
                        soft: f("soft", 0.05).clamp(0.0, 1.0),
                        invert: v["invert"].as_bool().unwrap_or(false),
                    }
                } else {
                    Kind::parse(name, f("soft", 0.05)).ok_or("kind: cut, mix, fade, wipe-…, luma")?
                };
                sw.mix_transition(id("mix")?, kind, f("duration", 1.0))
            }
            "stream_pause" => {
                self.streams.pause(
                    v["id"].as_u64().ok_or("id: a number")?,
                    v["paused"].as_bool().unwrap_or(true),
                );
                Ok(())
            }
            "stream_remove" => {
                self.streams.remove(v["id"].as_u64().ok_or("id: a number")?);
                Ok(())
            }
            other => return Err(format!("no command {other:?}")),
        }
        .map(|()| json!({}))
    }
}
