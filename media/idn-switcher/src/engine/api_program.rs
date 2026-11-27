//! The switcher's API for the program bus: cuts, transitions, stingers, fade to black,
//! scenes and their layers, global layers, desks, outputs and the multiview, edits.

use super::*;

impl Switcher {
    /// Swaps program and preview at once (finishing a transition under way).
    pub fn cut(&self) -> Result<(), String> {
        self.edit(|s| {
            complete(s);
            Ok(())
        })
    }

    /// Runs the transition from where it is to the end.
    pub fn auto(&self) -> Result<(), String> {
        let mut clip = None;
        self.edit(|s| {
            if let Kind::Stinger(n) = s.kind {
                let st = stinger_of(s, n).ok_or(format!("stinger {n} is empty"))?;
                if !s.running && s.position <= 0.0 {
                    s.cut_done = false;
                    clip = Some(st.input);
                }
                s.running = true;
            } else if s.kind == Kind::Cut || s.duration <= 0.0 {
                complete(s);
            } else {
                s.running = true;
            }
            Ok(())
        })?;
        // A file clip plays from its start.
        if let Some(input) = clip {
            let hook = self.shared.on_stinger.lock_ok().clone();
            if let Some(f) = hook {
                f(input);
            }
        }
        Ok(())
    }

    /// Sets the transition's position by hand (the T-bar): 1.0 completes it.
    pub fn tbar(&self, position: f32) -> Result<(), String> {
        self.edit(|s| {
            // A transition started by hand starts afresh (only `auto` cleared it).
            if !s.running && s.position <= 0.0 && position > 0.0 {
                s.cut_done = false;
            }
            s.running = false;
            if position >= 1.0 {
                complete(s);
            } else {
                s.position = position.max(0.0);
                // Back at rest: a stinger's swap at its cut stands as a cut; left marked,
                // the next CUT did nothing and the next mix snapped back.
                if s.position <= 0.0 {
                    s.cut_done = false;
                }
            }
            Ok(())
        })
    }

    /// The next transition: its kind and duration in seconds (0.1–10).
    pub fn transition(&self, kind: Kind, duration: f32) -> Result<(), String> {
        if let Kind::Luma { pattern, .. } = kind {
            self.slot(pattern)?;
        }
        self.edit(|s| {
            s.kind = kind;
            s.duration = duration.clamp(0.1, 10.0);
            Ok(())
        })
    }

    /// Puts a scene straight on program (a cut; preview stays, unless it was this scene:
    /// then the old program goes to preview, as a cut would). Only scenes go on the buses
    /// (§S.3): an input is put in a scene first.
    pub fn program(&self, id: u32) -> Result<(), String> {
        self.bus_scene(id)?;
        self.edit(|s| {
            if s.preview == Some(id) {
                s.preview = s.program;
            }
            s.program = Some(id);
            s.running = false;
            s.position = 0.0;
            // A stinger's swap at its cut point ends with it: left set, the next
            // transition played out and snapped back.
            s.cut_done = false;
            Ok(())
        })
    }

    /// Puts a scene on preview (only scenes go on the buses, §S.3).
    pub fn preview(&self, id: u32) -> Result<(), String> {
        self.bus_scene(id)?;
        self.edit(|s| {
            s.preview = Some(id);
            Ok(())
        })
    }

    /// `id` as a bus takes it: a scene; an input that isn't one is refused (§S.3).
    pub(super) fn bus_scene(&self, id: u32) -> Result<(), String> {
        let slot = self.slot(id)?;
        if matches!(slot.source, Source::Scene) {
            Ok(())
        } else {
            Err(format!(
                "input {id} is not a scene: only scenes go on program, preview and the mixes (put it in a scene)"
            ))
        }
    }

    /// Sets stinger slot `n` (1–8): its clip input, length and cut point (seconds), or
    /// empties it (`None`).
    pub fn stinger(&self, n: usize, slot: Option<Stinger>) -> Result<(), String> {
        if !(1..=STINGERS).contains(&n) {
            return Err(format!("stingers are 1 to {STINGERS}"));
        }
        if let Some(st) = slot {
            self.slot(st.input)?;
            if !(0.1..=10.0).contains(&st.duration) {
                return Err("duration: 0.1 to 10 s".into());
            }
            if !(0.0..=st.duration).contains(&st.cut) {
                return Err("cut: within the duration".into());
            }
        }
        self.edit(|s| {
            s.stingers[n - 1] = slot;
            Ok(())
        })
    }

    /// Fade to black, or back.
    pub fn ftb(&self) -> Result<(), String> {
        self.edit(|s| {
            s.ftb = !s.ftb;
            Ok(())
        })
    }

    /// A scene's layers (`None` if the input isn't a scene).
    pub fn scene(&self, id: u32) -> Option<Vec<crate::scene::Item>> {
        self.shared.scenes.lock_ok().get(&id).cloned()
    }

    /// Replaces a scene's layers (§S.12), as a whole: bottom to top, at most
    /// [`crate::scene::MAX_ITEMS`]. A layer whose `id` it already has keeps it (send back
    /// the list as read); a new one gets a new id. A layer with `global` set is that global
    /// layer's copy: all but its id and visibility come from the definition. Every layer's
    /// input must exist; no scene may come to contain itself, and scenes nest at most
    /// [`crate::scene::MAX_DEPTH`] deep.
    pub fn scene_items(&self, id: u32, mut items: Vec<crate::scene::Item>) -> Result<(), String> {
        let slot = self.slot(id)?;
        if !matches!(slot.source, Source::Scene) {
            return Err(format!("input {id} is not a scene"));
        }
        if items.len() > crate::scene::MAX_ITEMS {
            return Err(format!(
                "a scene has at most {} layers",
                crate::scene::MAX_ITEMS
            ));
        }
        {
            let globals = self.shared.globals.lock_ok();
            for it in items.iter_mut() {
                if let Some(g) = it.global {
                    let def = globals.get(&g).ok_or(format!("no global layer {g}"))?;
                    crate::scene::apply_global(it, def);
                }
            }
        }
        {
            let inputs = self.shared.inputs.lock_ok();
            for it in &items {
                if let crate::scene::ItemSource::Input(src) = it.source
                    && !inputs.contains_key(&src)
                {
                    return Err(format!("no input {src}"));
                }
            }
        }
        let mut scenes = self.shared.scenes.lock_ok();
        crate::scene::assign_ids(scenes.get(&id).map_or(&[][..], |v| v), &mut items);
        let mut trial = scenes.clone();
        trial.insert(id, items.clone());
        crate::scene::check_nesting(&trial)?;
        // A layer gone (or its visibility set here) stops fading.
        self.shared
            .layer_fades
            .lock_ok()
            .retain(|(s, l), _| *s != id || items.iter().any(|it| it.id == *l));
        scenes.insert(id, items);
        drop(scenes);
        self.edit(|_| Ok(()))
    }

    /// The level of a layer of scene `id`'s sound (§S.12): its volume in dB (−60 to +12)
    /// and whether it is muted, either or both; heard at once, ramped. The layer by its
    /// place or its id.
    pub fn scene_item_sound(
        &self,
        id: u32,
        layer: crate::scene::LayerRef,
        volume_db: Option<f32>,
        muted: Option<bool>,
    ) -> Result<(), String> {
        {
            let mut scenes = self.shared.scenes.lock_ok();
            let items = scenes
                .get_mut(&id)
                .ok_or(format!("input {id} is not a scene"))?;
            let k = layer.find(items).ok_or(match layer {
                crate::scene::LayerRef::Index(_) => format!(
                    "the scene has layers 0 to {}",
                    items.len().saturating_sub(1)
                ),
                crate::scene::LayerRef::Id(l) => format!("the scene has no layer {l}"),
            })?;
            let it = &mut items[k];
            if let Some(v) = volume_db.filter(|v| v.is_finite()) {
                it.volume_db = v.clamp(-60.0, 12.0);
            }
            if let Some(m) = muted {
                it.muted = m;
            }
        }
        self.edit(|_| Ok(()))
    }

    /// Shows or hides one layer of a scene (§S.12), by its id, fading over the layer's
    /// `fade` seconds (a fade under way turns back from where it is). Live, as a cut is:
    /// not a setup edit.
    pub fn scene_layer_visible(&self, scene: u32, layer: u32, on: bool) -> Result<(), String> {
        layer_visible(&self.shared, scene, layer, on, None)?;
        self.edit(|_| Ok(()))
    }

    /// Shows or hides global layer `global` in every scene that has it (each copy with its
    /// fade): the visible captions turned on with the closed captions (one CC button).
    /// How many copies changed.
    pub fn global_layer_visible(&self, global: u32, on: bool) -> Result<usize, String> {
        if !self.shared.globals.lock_ok().contains_key(&global) {
            return Err(format!("no global layer {global}"));
        }
        let copies: Vec<(u32, u32)> = self
            .shared
            .scenes
            .lock_ok()
            .iter()
            .flat_map(|(s, items)| {
                items
                    .iter()
                    .filter(|i| i.global == Some(global))
                    .map(move |i| (*s, i.id))
            })
            .collect();
        for &(scene, layer) in &copies {
            layer_visible(&self.shared, scene, layer, on, None)?;
        }
        self.edit(|_| Ok(()))?;
        Ok(copies.len())
    }

    /// As [`Switcher::scene_layer_visible`], over `fade` seconds this once (0: a cut)
    /// instead of the layer's own.
    pub fn scene_layer_visible_fade(
        &self,
        scene: u32,
        layer: u32,
        on: bool,
        fade: f32,
    ) -> Result<(), String> {
        layer_visible(&self.shared, scene, layer, on, Some(fade))?;
        self.edit(|_| Ok(()))
    }

    /// The global layers' definitions (§S.5), in their order.
    pub fn global_layers(&self) -> Vec<crate::scene::Item> {
        self.shared.globals.lock_ok().values().cloned().collect()
    }

    /// Defines a global layer (§S.5) — a logo, captions — or changes one: `def.id` 0 (or
    /// an id not in use) makes a new one (with that id, or the next), else it replaces
    /// that one's definition, and every scene's copy of it takes the change but keeps its
    /// place and visibility there. A new one is added to every scene made from now on
    /// (shown as `def.visible` says), and with `to_scenes` to every scene there is too, on
    /// top (where a scene has room). Its id.
    pub fn set_global_layer(
        &self,
        mut def: crate::scene::Item,
        to_scenes: bool,
    ) -> Result<u32, String> {
        let sh = &self.shared;
        if def.global.is_some() {
            return Err("a global layer can't be a copy of another".into());
        }
        if let crate::scene::ItemSource::Input(src) = def.source {
            let slot = self.slot(src)?;
            if matches!(slot.source, Source::Scene) {
                // (A scene in every scene would hold itself.)
                return Err("a global layer shows an input, not a scene".into());
            }
        }
        let mut globals = sh.globals.lock_ok();
        let new = def.id == 0 || !globals.contains_key(&def.id);
        if new && globals.len() >= MAX_GLOBAL_LAYERS {
            return Err(format!("at most {MAX_GLOBAL_LAYERS} global layers"));
        }
        if def.id == 0 {
            def.id = sh.next_global.fetch_add(1, Ordering::Relaxed) as u32;
        } else {
            sh.next_global
                .fetch_max(u64::from(def.id) + 1, Ordering::Relaxed);
        }
        let id = def.id;
        globals.insert(id, def.clone());
        drop(globals);
        let mut scenes = sh.scenes.lock_ok();
        for items in scenes.values_mut() {
            let mut has = false;
            for it in items.iter_mut().filter(|i| i.global == Some(id)) {
                crate::scene::apply_global(it, &def);
                has = true;
            }
            if new && to_scenes && !has && items.len() < crate::scene::MAX_ITEMS {
                let mut it = crate::scene::Item {
                    visible: def.visible,
                    ..Default::default()
                };
                crate::scene::apply_global(&mut it, &def);
                it.id = 0;
                let old = items.clone();
                items.push(it);
                crate::scene::assign_ids(&old, items);
            }
        }
        drop(scenes);
        self.edit(|_| Ok(()))?;
        Ok(id)
    }

    /// Removes a global layer (§S.5): its definition, and its copy from every scene.
    pub fn remove_global_layer(&self, id: u32) -> Result<(), String> {
        let sh = &self.shared;
        sh.globals
            .lock_ok()
            .remove(&id)
            .ok_or(format!("no global layer {id}"))?;
        let mut scenes = sh.scenes.lock_ok();
        for items in scenes.values_mut() {
            items.retain(|i| i.global != Some(id));
        }
        drop(scenes);
        self.edit(|_| Ok(()))
    }

    /// The multiview's layout (§S.7): its tiles as set, or the default for the inputs
    /// there are now.
    pub fn multiview_layout(&self) -> Vec<crate::multiview::TileSpec> {
        let inputs = self.shared.inputs.lock_ok().clone();
        layout_of(&self.shared, &inputs)
    }

    /// Sets the multiview's layout (§S.7): its tiles, in drawing order (at most
    /// [`crate::multiview::MAX_TILES`]); `None`: the default (preview and program across
    /// the top, every input and scene below, growing as there are more). A tile's input,
    /// scene or mix must exist.
    pub fn set_multiview_layout(
        &self,
        tiles: Option<Vec<crate::multiview::TileSpec>>,
    ) -> Result<(), String> {
        use crate::multiview::TileKind;
        if let Some(t) = &tiles {
            if t.len() > crate::multiview::MAX_TILES {
                return Err(format!(
                    "a multiview has at most {} tiles",
                    crate::multiview::MAX_TILES
                ));
            }
            let s = self.shared.state.lock_ok().clone();
            for tile in t {
                match tile.kind {
                    TileKind::Input(i) => {
                        self.slot(i)?;
                    }
                    TileKind::Scene(i) => {
                        if !matches!(self.slot(i)?.source, Source::Scene) {
                            return Err(format!("input {i} is not a scene"));
                        }
                    }
                    TileKind::Mix(m) if !s.mixes.contains_key(&m) => {
                        return Err(format!("no mix {m}"));
                    }
                    _ => {}
                }
            }
        }
        *self.shared.multiview_layout.lock_ok() = tiles;
        self.edit(|_| Ok(()))
    }

    /// Sends the multiview on the network as `name (Multiview)` (§S.7), or stops it — at
    /// run time; the API's multiview picture works either way.
    pub fn multiview_output(&self, on: bool) -> Result<(), String> {
        let sh = &self.shared;
        let mut out = sh.multiview_out.lock_ok();
        if on && out.is_none() {
            let o = Output::new(
                &sh.publisher,
                OutputConfig::new(&format!("{} (Multiview)", sh.cfg.name)),
            )
            .map_err(|e| format!("the multiview's output: {e}"))?;
            o.set_frame_rate(multiview_rate(sh.cfg.fps));
            *out = Some(Arc::new(o));
        } else if !on {
            // (Dropped here: withdrawn from the network.)
            *out = None;
        }
        drop(out);
        self.edit(|_| Ok(()))
    }

    /// Recalls snapshot `snapshot` on the desk `desk` (its source name, "HOST (Mixer)")
    /// over the switcher's connection to it (§S.27): `<idn_desk_snapshot name/>`, which
    /// the desk takes from devices of its site (idn-mixer §M.21).
    pub fn desk_snapshot(&self, desk: &str, snapshot: &str) -> Result<(), String> {
        send_desk_snapshot(&self.shared, desk, snapshot)
    }

    /// The desks (mixers) on the network, by source name, and whether each was heard in
    /// the last 5 s (§S.27), found on the network (mDNS or the Discovery Server).
    pub fn desks(&self) -> Vec<(String, bool)> {
        self.shared
            .desks
            .lock_ok()
            .iter()
            .map(|(n, (_, at))| (n.clone(), heard_lately(*at)))
            .collect()
    }

    pub fn program_output(&self) -> &Output {
        &self.shared.program_out
    }

    /// Every output, for screens and other local viewers (Wave 3, idn-display): what it
    /// is (`program`, `preview`, `multiview`, `monitor`, `mix`), its full name and its
    /// port on this machine.
    pub fn outputs(&self) -> Vec<(&'static str, String, u16)> {
        let sh = &self.shared;
        let mut v = vec![(
            "program",
            sh.program_out.name().to_owned(),
            sh.program_out.port(),
        )];
        if let Some(o) = &sh.preview_out {
            v.push(("preview", o.name().to_owned(), o.port()));
        }
        if let Some(o) = sh.multiview_out.lock_ok().as_ref() {
            v.push(("multiview", o.name().to_owned(), o.port()));
        }
        for o in sh.mix_outs.lock_ok().values() {
            v.push(("mix", o.name().to_owned(), o.port()));
        }
        v.push((
            "monitor",
            sh.monitor_net.name().to_owned(),
            sh.monitor_net.port(),
        ));
        v
    }

    /// Waits until the state changes past `after` (or `timeout`); the version then.
    pub fn wait_change(&self, after: u64, timeout: Duration) -> u64 {
        let s = self.shared.state.lock_ok();
        let (s, _) = self
            .shared
            .changed
            .wait_timeout_while(s, timeout, |s| s.version <= after)
            .unwrap();
        s.version
    }

    pub(super) fn edit(
        &self,
        f: impl FnOnce(&mut State) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut s = self.shared.state.lock_ok();
        f(&mut s)?;
        bump(&mut s, &self.shared.changed);
        Ok(())
    }
}

/// Shows or hides layer `layer` (its id) of scene `scene`, fading over its `fade` (a fade
/// under way turns back from where it is); the state not bumped.
pub(super) fn layer_visible(
    sh: &Shared,
    scene: u32,
    layer: u32,
    on: bool,
    fade: Option<f32>,
) -> Result<(), String> {
    let mut scenes = sh.scenes.lock_ok();
    let items = scenes
        .get_mut(&scene)
        .ok_or(format!("input {scene} is not a scene"))?;
    let it = items
        .iter_mut()
        .find(|i| i.id == layer && layer != 0)
        .ok_or(format!("the scene has no layer {layer}"))?;
    let mut fades = sh.layer_fades.lock_ok();
    let now = Instant::now();
    let level = match fades.get(&(scene, layer)).map(|f| f.level(now)) {
        Some((l, true)) => l,
        _ => f32::from(u8::from(it.visible)),
    };
    let to = f32::from(u8::from(on));
    it.visible = on;
    let secs = fade.map_or(it.fade, |f| {
        if f.is_finite() {
            f.clamp(0.0, crate::scene::MAX_FADE)
        } else {
            it.fade
        }
    });
    if secs > 0.0 && level != to {
        fades.insert(
            (scene, layer),
            LayerFade {
                from: level,
                to,
                start: now,
                secs: secs * (to - level).abs(),
            },
        );
    } else {
        fades.remove(&(scene, layer));
    }
    Ok(())
}

/// Recalls snapshot `snapshot` on desk `desk` over the switcher's connection to it.
pub(super) fn send_desk_snapshot(sh: &Shared, desk: &str, snapshot: &str) -> Result<(), String> {
    let desks = sh.desks.lock_ok();
    // Names as people type them: any case, as receivers take sources.
    let (input, at) = desks
        .get(desk)
        .or_else(|| {
            desks
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(desk))
                .map(|(_, d)| d)
        })
        .ok_or_else(|| format!("no desk {desk:?} on the network"))?;
    if !heard_lately(*at) {
        return Err(format!("the desk {desk:?} isn't answering"));
    }
    input.send_metadata(&format!(
        "<idn_desk_snapshot name=\"{}\"/>",
        idn_open_protocol::control::escape_attr(snapshot)
    ));
    Ok(())
}
