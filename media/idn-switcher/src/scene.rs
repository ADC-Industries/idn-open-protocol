//! Scenes (spec/extensions/idn-switcher.md §S.12): layers placed, cropped, turned, faded
//! and blended over a transparent canvas, rendered on the CPU into the scene input's
//! picture. An upright layer in the normal blend goes the fast way (scaled once, then keyed
//! over its rectangle); a turned or blended one is drawn sample by sample. Each layer has
//! a stable id, a name, a "volunteers may toggle" flag and a fade time; a layer may be a
//! scene's copy of a global layer (§S.5), which keeps its place and visibility per scene.

use crate::compose;
use crate::frame::{Frame, Ycc};
use serde_json::Value;
use std::sync::Arc;

/// Layers in a scene, at most (§S.3).
pub const MAX_ITEMS: usize = 16;
/// A layer's fade, seconds, at most.
pub const MAX_FADE: f32 = 10.0;
/// Scenes inside scenes, at most this deep.
pub const MAX_DEPTH: usize = 4;

/// How an item combines with what is beneath it (on luma; chroma mixes normally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    Normal,
    Add,
    Multiply,
    Screen,
}

impl Blend {
    pub fn name(self) -> &'static str {
        match self {
            Blend::Normal => "normal",
            Blend::Add => "add",
            Blend::Multiply => "multiply",
            Blend::Screen => "screen",
        }
    }

    fn from_name(s: &str) -> Option<Blend> {
        Some(match s {
            "normal" => Blend::Normal,
            "add" => Blend::Add,
            "multiply" => Blend::Multiply,
            "screen" => Blend::Screen,
            _ => return None,
        })
    }
}

/// What an item shows.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemSource {
    /// An input's newest picture (any input, another scene too).
    Input(u32),
    /// A flat colour.
    Colour(Ycc),
}

/// One layer of a scene (or a global layer's definition, §S.5).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Its id, stable while it stays in its scene (0: none yet, given one as the scene
    /// takes it). A global layer's definition: the global layer's id.
    pub id: u32,
    /// Its name for people ("" none).
    pub name: String,
    /// Volunteers may show and hide it (a flag for the UI; the engine doesn't check it).
    pub volunteer: bool,
    /// Locked in the scene editor (not moved or resized there by accident); the
    /// switcher only keeps it.
    pub locked: bool,
    /// Seconds it fades in and out over when shown or hidden (`scene_layer_visible`).
    pub fade: f32,
    /// The global layer this is the scene's copy of: everything but its place in the
    /// scene, its id and whether it shows comes from that definition.
    pub global: Option<u32>,
    pub source: ItemSource,
    /// x, y, width, height as fractions of the canvas.
    pub rect: (f32, f32, f32, f32),
    /// x0, y0, x1, y1 as fractions of the source.
    pub crop: (f32, f32, f32, f32),
    /// Degrees, clockwise about the item's centre.
    pub rotation: f32,
    /// 0–1.
    pub opacity: f32,
    pub blend: Blend,
    /// A border around the rectangle, 0–20 px, turning with it.
    pub border: u32,
    pub border_colour: Ycc,
    /// Rounded corners: the radius as a fraction of the rectangle's shorter side (0 square
    /// to 0.5, a round end); a border rounds with it.
    pub radius: f32,
    /// Its source's alpha keys it (`true`, the default); `false` (`"key": "none"`): drawn
    /// opaque, its alpha ignored (a picture whose alpha is not meant for keying).
    pub keyed: bool,
    pub visible: bool,
    /// Its input's sound in the scene's (§S.12), dB (−60 to +12), and muted.
    pub volume_db: f32,
    pub muted: bool,
}

impl Default for Item {
    fn default() -> Self {
        Item {
            id: 0,
            name: String::new(),
            volunteer: false,
            locked: false,
            fade: 0.5,
            global: None,
            source: ItemSource::Colour(Ycc::BLACK),
            rect: (0.0, 0.0, 1.0, 1.0),
            crop: (0.0, 0.0, 1.0, 1.0),
            rotation: 0.0,
            opacity: 1.0,
            blend: Blend::Normal,
            border: 0,
            border_colour: Ycc::WHITE,
            radius: 0.0,
            keyed: true,
            visible: true,
            volume_db: 0.0,
            muted: false,
        }
    }
}

/// `#rrggbb` as Y′CbCr (BT.709).
pub fn parse_colour(s: &str) -> Option<Ycc> {
    let h = s.strip_prefix('#')?;
    // (ASCII first: the pairs are cut by bytes.)
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some(Ycc::from_rgb(b(0)?, b(2)?, b(4)?))
}

fn four(
    v: &Value,
    what: &str,
    default: (f32, f32, f32, f32),
) -> Result<(f32, f32, f32, f32), String> {
    match v {
        Value::Null => Ok(default),
        Value::Array(a) if a.len() == 4 => {
            let n = |i: usize| {
                a[i].as_f64()
                    .filter(|x| x.is_finite())
                    .map(|x| x as f32)
                    .ok_or(format!("{what}: four numbers"))
            };
            Ok((n(0)?, n(1)?, n(2)?, n(3)?))
        }
        _ => Err(format!("{what}: four numbers")),
    }
}

impl Item {
    /// From the API's JSON (§S.12); missing fields take their defaults.
    pub fn from_json(v: &Value) -> Result<Item, String> {
        let d = Item::default();
        let global = match &v["global"] {
            Value::Null => None,
            g => Some(
                g.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .ok_or("global: a global layer's id")?,
            ),
        };
        let source = match &v["source"] {
            // A global layer's copy takes its source from the definition.
            Value::Null if global.is_some() => d.source.clone(),
            Value::Number(n) => ItemSource::Input(
                n.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or("source: an input id")?,
            ),
            s if s["colour"].is_string() => ItemSource::Colour(
                parse_colour(s["colour"].as_str().unwrap_or("")).ok_or("colour: #rrggbb")?,
            ),
            _ => return Err("source: an input id or {\"colour\": \"#rrggbb\"}".into()),
        };
        let rect = four(&v["rect"], "rect", d.rect)?;
        let crop = four(&v["crop"], "crop", d.crop)?;
        let crop = (
            crop.0.clamp(0.0, 1.0),
            crop.1.clamp(0.0, 1.0),
            crop.2.clamp(0.0, 1.0),
            crop.3.clamp(0.0, 1.0),
        );
        if crop.2 <= crop.0 || crop.3 <= crop.1 {
            return Err("crop: x1 > x0 and y1 > y0".into());
        }
        let blend = match &v["blend"] {
            Value::Null => d.blend,
            b => Blend::from_name(b.as_str().unwrap_or(""))
                .ok_or("blend: normal, add, multiply or screen")?,
        };
        let border_colour = match &v["border_colour"] {
            Value::Null => d.border_colour,
            c => parse_colour(c.as_str().unwrap_or("")).ok_or("border_colour: #rrggbb")?,
        };
        let keyed = match &v["key"] {
            Value::Null => d.keyed,
            k => match k.as_str() {
                Some("alpha") => true,
                Some("none") => false,
                _ => return Err("key: alpha or none".into()),
            },
        };
        let name = v["name"].as_str().unwrap_or("");
        if name.chars().count() > 64 {
            return Err("name: 64 characters at most".into());
        }
        Ok(Item {
            id: v["id"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            name: name.to_owned(),
            volunteer: v["volunteer"].as_bool().unwrap_or(d.volunteer),
            locked: v["locked"].as_bool().unwrap_or(d.locked),
            fade: v["fade"]
                .as_f64()
                .filter(|x| x.is_finite())
                .map_or(d.fade, |x| (x as f32).clamp(0.0, MAX_FADE)),
            global,
            source,
            rect: (rect.0, rect.1, rect.2.max(0.0), rect.3.max(0.0)),
            crop,
            rotation: v["rotation"]
                .as_f64()
                .filter(|r| r.is_finite())
                .map_or(d.rotation, |r| r as f32),
            opacity: v["opacity"]
                .as_f64()
                .map_or(d.opacity, |o| (o as f32).clamp(0.0, 1.0)),
            blend,
            border: v["border"].as_u64().map_or(d.border, |b| b.min(20) as u32),
            border_colour,
            radius: v["radius"]
                .as_f64()
                .filter(|x| x.is_finite())
                .map_or(d.radius, |x| (x as f32).clamp(0.0, 0.5)),
            keyed,
            visible: v["visible"].as_bool().unwrap_or(d.visible),
            volume_db: v["volume_db"]
                .as_f64()
                .filter(|x| x.is_finite())
                .map_or(d.volume_db, |x| (x as f32).clamp(-60.0, 12.0)),
            muted: v["muted"].as_bool().unwrap_or(d.muted),
        })
    }

    /// Its input's sound's level in the scene: none while muted or hidden, else its volume.
    pub fn sound_gain(&self) -> f32 {
        if self.muted || !self.visible {
            0.0
        } else {
            10f32.powf(self.volume_db / 20.0)
        }
    }

    /// As the API gives it (the state's [`Layer`](crate::service::state_schema::Layer)).
    pub fn to_json(&self) -> Value {
        serde_json::to_value(crate::service::state_schema::Layer::from(self)).unwrap_or_default()
    }
}

/// The items of a JSON list (at most [`MAX_ITEMS`]).
pub fn items_from_json(v: &Value) -> Result<Vec<Item>, String> {
    let a = v.as_array().ok_or("items: a list")?;
    if a.len() > MAX_ITEMS {
        return Err(format!("a scene has at most {MAX_ITEMS} layers"));
    }
    a.iter()
        .enumerate()
        .map(|(i, x)| Item::from_json(x).map_err(|e| format!("layer {}: {e}", i + 1)))
        .collect()
}

/// A layer of a scene: by its place in the list (from 0) or by its stable id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerRef {
    Index(usize),
    Id(u32),
}

impl LayerRef {
    /// Its place in `items`.
    pub fn find(self, items: &[Item]) -> Option<usize> {
        match self {
            LayerRef::Index(i) => (i < items.len()).then_some(i),
            LayerRef::Id(id) => items.iter().position(|it| it.id == id && id != 0),
        }
    }
}

/// Gives `new` its layers' ids (§S.12): a layer whose id is set and not taken by an
/// earlier one in the list keeps it (as `scene_items` sends back the list it read); the
/// others get new ones, above every id in `old` and `new` (a removed layer's id isn't
/// given again while the scene has it in its history of the list).
pub fn assign_ids(old: &[Item], new: &mut [Item]) {
    let mut next = old
        .iter()
        .chain(new.iter())
        .map(|i| i.id)
        .max()
        .unwrap_or(0)
        + 1;
    let mut seen = std::collections::BTreeSet::new();
    for it in new.iter_mut() {
        if it.id == 0 || !seen.insert(it.id) {
            it.id = next;
            seen.insert(next);
            next += 1;
        }
    }
}

/// A scene's copy of global layer `def` (§S.5): everything from the definition but the
/// copy's own id and visibility (and its place, which is its place in the list).
pub fn apply_global(layer: &mut Item, def: &Item) {
    let (id, visible) = (layer.id, layer.visible);
    *layer = Item {
        id,
        visible,
        global: Some(def.id),
        ..def.clone()
    };
}

/// Checks that no scene in `defs` (scene id → items) contains itself, directly or through
/// others, and that none nests more than [`MAX_DEPTH`] deep.
pub fn check_nesting(defs: &std::collections::BTreeMap<u32, Vec<Item>>) -> Result<(), String> {
    fn depth(
        id: u32,
        defs: &std::collections::BTreeMap<u32, Vec<Item>>,
        path: &mut Vec<u32>,
    ) -> Result<usize, String> {
        if path.contains(&id) {
            return Err(format!("scene {id} would contain itself"));
        }
        path.push(id);
        let mut d = 1;
        for it in defs.get(&id).into_iter().flatten() {
            if let ItemSource::Input(src) = it.source
                && defs.contains_key(&src)
            {
                d = d.max(1 + depth(src, defs, path)?);
            }
        }
        path.pop();
        Ok(d)
    }
    for &id in defs.keys() {
        if depth(id, defs, &mut Vec::new())? > MAX_DEPTH {
            return Err(format!("scenes nest at most {MAX_DEPTH} deep"));
        }
    }
    Ok(())
}

/// The inputs a scene shows, through the scenes inside it too (not the scenes themselves).
pub fn members(
    id: u32,
    defs: &std::collections::BTreeMap<u32, Vec<Item>>,
    out: &mut std::collections::BTreeSet<u32>,
) {
    fn walk(
        id: u32,
        defs: &std::collections::BTreeMap<u32, Vec<Item>>,
        out: &mut std::collections::BTreeSet<u32>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        for it in defs.get(&id).into_iter().flatten().filter(|i| i.visible) {
            if let ItemSource::Input(src) = it.source {
                if defs.contains_key(&src) {
                    walk(src, defs, out, depth + 1);
                } else {
                    out.insert(src);
                }
            }
        }
    }
    walk(id, defs, out, 0);
}

/// The inputs a scene holds, shown or hidden, through the scenes inside it too (with the
/// scenes): what is received while the scene is on a bus (§S.3: a hidden layer shown is
/// ready at once).
pub fn holds(
    id: u32,
    defs: &std::collections::BTreeMap<u32, Vec<Item>>,
    out: &mut std::collections::BTreeSet<u32>,
) {
    fn walk(
        id: u32,
        defs: &std::collections::BTreeMap<u32, Vec<Item>>,
        out: &mut std::collections::BTreeSet<u32>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        for it in defs.get(&id).into_iter().flatten() {
            if let ItemSource::Input(src) = it.source
                && out.insert(src)
                && defs.contains_key(&src)
            {
                walk(src, defs, out, depth + 1);
            }
        }
    }
    walk(id, defs, out, 0);
}

/// The one input a scene passes through as it is (§S.12): a single visible layer over the
/// whole canvas, upright, uncropped, opaque, in the normal blend, no border, showing an
/// input (not a colour). The scene is then that input's own picture, fitted where it shows
/// as an input's is (its shape kept), not one drawn from it.
pub fn pass_through(items: &[Item]) -> Option<u32> {
    let mut shown = items.iter().filter(|i| i.visible && i.opacity > 0.0);
    let it = shown.next()?;
    if shown.next().is_some() {
        return None;
    }
    let full = it.rect == (0.0, 0.0, 1.0, 1.0) && it.crop == (0.0, 0.0, 1.0, 1.0);
    match it.source {
        ItemSource::Input(src)
            if full
                && it.opacity >= 1.0
                && it.blend == Blend::Normal
                && it.border == 0
                && it.rotation.rem_euclid(360.0) == 0.0 =>
        {
            Some(src)
        }
        _ => None,
    }
}

/// A transparent canvas: black, alpha 0.
pub fn canvas(w: usize, h: usize) -> Frame {
    let cw = Frame::chroma_width(w);
    Frame {
        width: w,
        height: h,
        y: vec![Ycc::BLACK.0; w * h],
        cb: vec![128; cw * h],
        cr: vec![128; cw * h],
        a: Some(vec![0; w * h]),
        coefs: None,
        gpu: None,
    }
}

/// Whether `items` cover the whole canvas: an item over all of it, upright, at full
/// opacity, normal blend, showing a colour or a picture without alpha — what is drawn on
/// it keeps it covered, so the scene's alpha is 255 everywhere and need not be kept.
pub fn opaque(items: &[Item], pic: &dyn Fn(u32) -> Option<Arc<Frame>>) -> bool {
    items.iter().any(|it| {
        let r = it.rect;
        it.visible
            && it.opacity >= 1.0
            && it.blend == Blend::Normal
            && it.rotation.rem_euclid(360.0) == 0.0
            && it.radius <= 0.0
            && r.0 <= 0.0
            && r.1 <= 0.0
            && r.0 + r.2 >= 1.0
            && r.1 + r.3 >= 1.0
            && match it.source {
                ItemSource::Colour(_) => true,
                ItemSource::Input(id) => pic(id).is_some_and(|p| p.a.is_none() || !it.keyed),
            }
    })
}

/// Renders `items` over a transparent `w` × `h` canvas. `pic(id)` gives an input's newest
/// picture (`None`: the item shows nothing).
pub fn render(
    w: usize,
    h: usize,
    items: &[Item],
    pic: &dyn Fn(u32) -> Option<Arc<Frame>>,
) -> Frame {
    let mut c = canvas(w, h);
    for it in items.iter().filter(|i| i.visible && i.opacity > 0.0) {
        let place = (
            it.rect.0 as f64 * w as f64,
            it.rect.1 as f64 * h as f64,
            it.rect.2 as f64 * w as f64,
            it.rect.3 as f64 * h as f64,
        );
        if place.2 < 1.0 || place.3 < 1.0 {
            continue;
        }
        let src = match &it.source {
            ItemSource::Input(id) => match pic(*id) {
                Some(p) => p,
                None => continue,
            },
            ItemSource::Colour(col) => Arc::new(Frame::solid(2, 2, *col)),
        };
        let radius = corner_radius(it, place);
        if it.border > 0 {
            let b = it.border as f64;
            draw(
                &mut c,
                &Frame::solid(2, 2, it.border_colour),
                (
                    place.0 - b,
                    place.1 - b,
                    place.2 + 2.0 * b,
                    place.3 + 2.0 * b,
                ),
                (0.0, 0.0, 1.0, 1.0),
                it.rotation,
                Look {
                    opacity: it.opacity,
                    blend: Blend::Normal,
                    radius: if radius > 0.0 { radius + b } else { 0.0 },
                    keyed: true,
                },
            );
        }
        draw(
            &mut c,
            &src,
            place,
            it.crop,
            it.rotation,
            Look {
                opacity: it.opacity,
                blend: it.blend,
                radius,
                keyed: it.keyed,
            },
        );
    }
    c
}

/// An item's corner radius in canvas pixels at `place` (its `radius` of the shorter side).
pub(crate) fn corner_radius(it: &Item, place: (f64, f64, f64, f64)) -> f64 {
    it.radius.clamp(0.0, 0.5) as f64 * place.2.min(place.3)
}

/// How an item is drawn: opacity (0–1), blend, corner radius (canvas pixels, 0 square),
/// keyed by its source's alpha.
#[derive(Debug, Clone, Copy)]
struct Look {
    opacity: f32,
    blend: Blend,
    radius: f64,
    keyed: bool,
}

/// The coverage (0–255) rounded corners of `radius` leave at canvas point (`px`, `py`) of
/// an item `iw` × `ih` centred at (`cx`, `cy`), turned by (`cos`, `sin`): 255 but in the
/// corners, there an antialiased quarter circle (the GPU's `corner`).
#[allow(clippy::too_many_arguments)]
#[inline]
fn corner(px: f64, py: f64, cx: f64, cy: f64, cos: f64, sin: f64, iw: f64, ih: f64, r: f64) -> u32 {
    if r <= 0.0 {
        return 255;
    }
    let (dx, dy) = (px - cx, py - cy);
    let lx = dx * cos + dy * sin + iw / 2.0;
    let ly = -dx * sin + dy * cos + ih / 2.0;
    let ox = (r - lx).max(lx - (iw - r)).max(0.0);
    let oy = (r - ly).max(ly - (ih - r)).max(0.0);
    if ox == 0.0 || oy == 0.0 {
        return 255;
    }
    ((r - (ox * ox + oy * oy).sqrt() + 0.5).clamp(0.0, 1.0) * 255.0).round() as u32
}

/// Coverage `a` (0–255) cut by `k` (0–255).
#[inline]
fn cut(a: u32, k: u32) -> u32 {
    (a * k + 127) / 255
}

/// The 0–256 weight of a sample with coverage `a` (0–255) at opacity `o` (0–256).
#[inline]
fn weight(a: u32, o: u32) -> u32 {
    (a * o + 127) / 255
}

/// `a` + (`b` − `a`) × `w` / 256.
#[inline]
fn lerp(a: u8, b: u8, w: u32) -> u8 {
    ((a as u32 * (256 - w) + b as u32 * w + 128) >> 8) as u8
}

/// The luma an item's sample `s` makes over `d` by `blend`, before the mix by coverage.
#[inline]
fn blend_y(d: u8, s: u8, blend: Blend) -> u8 {
    let (d, s) = (d as i32, s as i32);
    match blend {
        Blend::Normal => s as u8,
        Blend::Add => (d + (s - 16)).clamp(16, 235) as u8,
        Blend::Multiply => (16 + (d - 16).max(0) * (s - 16).max(0) / 219).clamp(16, 235) as u8,
        Blend::Screen => (235 - (235 - d).max(0) * (235 - s).max(0) / 219).clamp(16, 235) as u8,
    }
}

/// The plane `pl` (`pw` × `ph`) at (`fx`, `fy`) in sample coordinates, bilinear with 8-bit
/// weights (the compositor's taps, S.9).
#[inline]
fn bilinear(pl: &[u8], pw: usize, ph: usize, fx: f64, fy: f64) -> u8 {
    let fx = fx.clamp(0.0, (pw - 1) as f64);
    let fy = fy.clamp(0.0, (ph - 1) as f64);
    let (xa, ya) = (fx as usize, fy as usize);
    let wx = ((fx - xa as f64) * 256.0).round() as u32;
    let wy = ((fy - ya as f64) * 256.0).round() as u32;
    let (xb, yb) = ((xa + 1).min(pw - 1), (ya + 1).min(ph - 1));
    let (ra, rb) = (ya * pw, yb * pw);
    let top = pl[ra + xa] as u32 * (256 - wx) + pl[ra + xb] as u32 * wx;
    let bot = pl[rb + xa] as u32 * (256 - wx) + pl[rb + xb] as u32 * wx;
    ((top * (256 - wy) + bot * wy + 32768) >> 16) as u8
}

/// Covers the canvas alpha under `src` keyed at (`x`, `y`) by opacity `o` (0–256), in
/// bands of rows in parallel; an opaque source at full opacity just fills.
fn cover(c: &mut Frame, src: &Frame, x: usize, y: usize, o: u32) {
    let (w, h) = (c.width, c.height);
    let Some(ca) = c.a.as_mut() else { return };
    let (rows, n) = (
        src.height.min(h.saturating_sub(y)),
        src.width.min(w.saturating_sub(x)),
    );
    if rows == 0 || n == 0 {
        return;
    }
    let area = &mut ca[y * w..(y + rows) * w];
    let bands = crate::par::threads();
    let per = rows.div_ceil(bands).max(1);
    crate::par::scope(|s| {
        for (k, band) in area.chunks_mut(per * w).enumerate() {
            s.spawn(move || {
                for (r, row) in band.chunks_mut(w).enumerate() {
                    let sr = k * per + r;
                    let d = &mut row[x..x + n];
                    match src.a.as_ref() {
                        None if o >= 256 => d.fill(255),
                        sa => {
                            for (i, d) in d.iter_mut().enumerate() {
                                let a = sa.map_or(255, |a| a[sr * src.width + i] as u32);
                                let k = weight(a, o);
                                *d = (*d as u32 + (((255 - *d as u32) * k + 128) >> 8)) as u8;
                            }
                        }
                    }
                }
            });
        }
    });
}

/// Blends `src` (already at its rectangle's size) over the canvas at (`x`, `y`) (`x` even)
/// by `blend` on luma, mixed by its coverage at opacity `o` (0–256); chroma mixes normally.
/// Bands of rows in parallel; clipped at the right and bottom.
fn key_blend(c: &mut Frame, src: &Frame, x: usize, y: usize, o: u32, blend: Blend) {
    let (w, h) = (c.width, c.height);
    let (cw, scw) = (Frame::chroma_width(w), Frame::chroma_width(src.width));
    let rows = src.height.min(h.saturating_sub(y));
    let n = src.width.min(w.saturating_sub(x));
    let cn = scw.min(cw.saturating_sub(x / 2));
    if rows == 0 || n == 0 {
        return;
    }
    let sa = src.a.as_deref();
    let sw = src.width;
    let bands = crate::par::threads();
    let per = rows.div_ceil(bands).max(1);
    let Frame { y: cy, cb, cr, .. } = c;
    crate::par::scope(|s| {
        for (k, band) in cy[y * w..(y + rows) * w].chunks_mut(per * w).enumerate() {
            s.spawn(move || {
                for (r, line) in band.chunks_mut(w).enumerate() {
                    let sr = k * per + r;
                    let sv = &src.y[sr * sw..sr * sw + n];
                    for (i, (d, &v)) in line[x..x + n].iter_mut().zip(sv).enumerate() {
                        let a = sa.map_or(255, |a| a[sr * sw + i] as u32);
                        *d = lerp(*d, blend_y(*d, v, blend), weight(a, o));
                    }
                }
            });
        }
        for (plane, sp) in [(cb, &src.cb), (cr, &src.cr)] {
            for (k, band) in plane[y * cw..(y + rows) * cw]
                .chunks_mut(per * cw)
                .enumerate()
            {
                s.spawn(move || {
                    for (r, line) in band.chunks_mut(cw).enumerate() {
                        let sr = k * per + r;
                        let sv = &sp[sr * scw..sr * scw + cn];
                        for (i, (d, &v)) in line[x / 2..x / 2 + cn].iter_mut().zip(sv).enumerate() {
                            // A chroma sample covers two pixels: their mean coverage.
                            let a = sa.map_or(255, |a| {
                                let l = a[sr * sw + 2 * i] as u32;
                                let r = *a.get(sr * sw + 2 * i + 1).unwrap_or(&a[sr * sw + 2 * i])
                                    as u32;
                                (l + r).div_ceil(2)
                            });
                            *d = lerp(*d, v, weight(a, o));
                        }
                    }
                });
            }
        }
    });
}

/// Draws `src`'s `crop` into `place` (x, y, width, height in canvas pixels) of the canvas,
/// turned `degrees` clockwise about its centre, as `look` says.
fn draw(
    c: &mut Frame,
    src: &Frame,
    place: (f64, f64, f64, f64),
    crop: (f32, f32, f32, f32),
    degrees: f32,
    look: Look,
) {
    let Look {
        opacity,
        blend,
        radius,
        keyed,
    } = look;
    let o = ((opacity.clamp(0.0, 1.0) * 256.0).round() as u32).min(256);
    if o == 0 {
        return;
    }
    let crop64 = (crop.0 as f64, crop.1 as f64, crop.2 as f64, crop.3 as f64);
    let turned = (degrees.rem_euclid(360.0)).abs() > 1e-3;
    let (x, y, iw, ih) = place;
    // Upright, on the canvas, square: scaled once, keyed (or blended) over its rectangle.
    if !turned && radius <= 0.0 && x >= -0.5 && y >= -0.5 {
        let (wi, hi) = (iw.round() as usize, ih.round() as usize);
        let (xi, yi) = (
            (x.round().max(0.0) as usize) & !1,
            y.round().max(0.0) as usize,
        );
        if wi == 0 || hi == 0 || xi >= c.width || yi >= c.height {
            return;
        }
        let whole = crop64 == (0.0, 0.0, 1.0, 1.0);
        let own;
        // (Its alpha ignored: the scaled copy without it.)
        let as_is = keyed || src.a.is_none();
        let shown: &Frame =
            if as_is && whole && (src.width, src.height) == (wi, hi) && !src.deferred() {
                src
            } else {
                let mut f = src.scaled(crop64, wi, hi);
                if !keyed {
                    f.a = None;
                }
                own = f;
                &own
            };
        if blend == Blend::Normal {
            compose::key_at(c, shown, xi, yi, o as f32 / 256.0);
        } else {
            key_blend(c, shown, xi, yi, o, blend);
        }
        cover(c, shown, xi, yi, o);
        return;
    }
    // Turned, blended or off the top/left edge: sample by sample.
    // Borrowed as it is; only coefficients are made samples (the
    // whole source was copied per item per frame just to own it).
    let owned;
    let src: &Frame = if src.deferred() {
        owned = src.pixels();
        &owned
    } else {
        src
    };
    let (sw, sh) = (src.width, src.height);
    let scw = Frame::chroma_width(sw);
    let (w, h) = (c.width, c.height);
    let cw = Frame::chroma_width(w);
    let th = (degrees as f64).to_radians();
    let (cos, sin) = (th.cos(), th.sin());
    let (cx, cy) = (x + iw / 2.0, y + ih / 2.0);
    // The turned rectangle's bounds on the canvas, the only rows and columns to visit.
    let corners = [(-iw, -ih), (iw, -ih), (iw, ih), (-iw, ih)].map(|(u, v)| {
        let (u, v) = (u / 2.0, v / 2.0);
        (cx + u * cos - v * sin, cy + u * sin + v * cos)
    });
    let (xs, ys) = (corners.map(|p| p.0), corners.map(|p| p.1));
    let lo = |v: [f64; 4]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = |v: [f64; 4]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (x0, x1) = (lo(xs).floor().max(0.0), hi(xs).ceil().min(w as f64));
    let (y0, y1) = (lo(ys).floor().max(0.0), hi(ys).ceil().min(h as f64));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let (x0, x1, y0, y1) = (x0 as usize, x1 as usize, y0 as usize, y1 as usize);
    // A canvas point to (x, y) fractions of the source, if it lands on the item.
    let map = |px: f64, py: f64| -> Option<(f64, f64)> {
        let (dx, dy) = (px - cx, py - cy);
        let u = (dx * cos + dy * sin) / iw + 0.5;
        let v = (-dx * sin + dy * cos) / ih + 0.5;
        if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
            return None;
        }
        Some((
            crop64.0 + u * (crop64.2 - crop64.0),
            crop64.1 + v * (crop64.3 - crop64.1),
        ))
    };
    let alpha_at = |fx: f64, fy: f64| -> u32 {
        match src.a.as_ref() {
            Some(a) if keyed => bilinear(a, sw, sh, fx, fy) as u32,
            _ => 255,
        }
    };
    let rounded = |px: f64, py: f64| corner(px, py, cx, cy, cos, sin, iw, ih, radius);
    let bands = crate::par::threads();
    let rows_per = h.div_ceil(bands).max(1);
    let Frame {
        y: cy_, cb, cr, a, ..
    } = c;
    let ca = a.as_mut().expect("a canvas has alpha");
    crate::par::scope(|s| {
        // Luma and the canvas alpha, together.
        for (k, (yb, ab)) in cy_
            .chunks_mut(rows_per * w)
            .zip(ca.chunks_mut(rows_per * w))
            .enumerate()
        {
            let (map, alpha_at, src, rounded) = (&map, &alpha_at, &src, &rounded);
            s.spawn(move || {
                for (r, (yl, al)) in yb.chunks_mut(w).zip(ab.chunks_mut(w)).enumerate() {
                    let row = k * rows_per + r;
                    if row < y0 || row >= y1 {
                        continue;
                    }
                    for i in x0..x1 {
                        let Some((sx, sy)) = map(i as f64 + 0.5, row as f64 + 0.5) else {
                            continue;
                        };
                        let (fx, fy) = (sx * sw as f64 - 0.5, sy * sh as f64 - 0.5);
                        let cov = cut(alpha_at(fx, fy), rounded(i as f64 + 0.5, row as f64 + 0.5));
                        let kw = weight(cov, o);
                        if kw == 0 {
                            continue;
                        }
                        let v = bilinear(&src.y, sw, sh, fx, fy);
                        yl[i] = lerp(yl[i], blend_y(yl[i], v, blend), kw);
                        al[i] = (al[i] as u32 + (((255 - al[i] as u32) * kw + 128) >> 8)) as u8;
                    }
                }
            });
        }
        // Each chroma plane: a sample covers two pixels, its centre between them.
        for (plane, sp) in [(cb, &src.cb), (cr, &src.cr)] {
            for (k, band) in plane.chunks_mut(rows_per * cw).enumerate() {
                let (map, alpha_at, rounded) = (&map, &alpha_at, &rounded);
                s.spawn(move || {
                    for (r, line) in band.chunks_mut(cw).enumerate() {
                        let row = k * rows_per + r;
                        if row < y0 || row >= y1 {
                            continue;
                        }
                        let (c0, c1) = (x0 / 2, x1.div_ceil(2).min(cw));
                        for (ci, d) in line.iter_mut().enumerate().take(c1).skip(c0) {
                            let Some((sx, sy)) = map(2.0 * ci as f64 + 1.0, row as f64 + 0.5)
                            else {
                                continue;
                            };
                            let fy = sy * sh as f64 - 0.5;
                            let (px, py) = (2.0 * ci as f64 + 1.0, row as f64 + 0.5);
                            let al = cut(alpha_at(sx * sw as f64 - 0.5, fy), rounded(px, py));
                            let kw = weight(al, o);
                            if kw == 0 {
                                continue;
                            }
                            let v = bilinear(sp, scw, sh, sx * scw as f64 - 0.5, fy);
                            *d = lerp(*d, v, kw);
                        }
                    }
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const W: usize = 64;
    const H: usize = 36;

    fn at(f: &Frame, x: usize, y: usize) -> (u8, u8) {
        (
            f.y[y * f.width + x],
            f.a.as_ref().map_or(255, |a| a[y * f.width + x]),
        )
    }

    fn item(source: ItemSource, rect: (f32, f32, f32, f32)) -> Item {
        Item {
            source,
            rect,
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_scene_is_transparent_black() {
        let f = render(W, H, &[], &|_| None);
        assert!(f.y.iter().all(|&v| v == 16));
        assert!(f.a.as_ref().unwrap().iter().all(|&a| a == 0));
    }

    #[test]
    fn an_upright_item_covers_its_rectangle_only() {
        let white = ItemSource::Colour(Ycc::WHITE);
        let f = render(W, H, &[item(white, (0.25, 0.25, 0.5, 0.5))], &|_| None);
        assert_eq!(at(&f, 32, 18), (235, 255), "inside");
        assert_eq!(at(&f, 2, 2), (16, 0), "outside: transparent");
        assert_eq!(at(&f, 60, 34), (16, 0));
    }

    #[test]
    fn opacity_and_layering() {
        let f = render(
            W,
            H,
            &[
                item(ItemSource::Colour(Ycc::WHITE), (0.0, 0.0, 1.0, 1.0)),
                Item {
                    opacity: 0.5,
                    ..item(ItemSource::Colour(Ycc::BLACK), (0.0, 0.0, 1.0, 1.0))
                },
            ],
            &|_| None,
        );
        let (y, a) = at(&f, 10, 10);
        assert!((124..=127).contains(&y), "half black over white: {y}");
        assert_eq!(a, 255);
    }

    #[test]
    fn a_quarter_turn_swaps_the_shape() {
        // A wide bar across the middle, turned 90°: tall instead.
        let bar = Item {
            rotation: 90.0,
            ..item(ItemSource::Colour(Ycc::WHITE), (0.25, 0.45, 0.5, 0.1))
        };
        let f = render(W, H, &[bar], &|_| None);
        // Its centre stays (32, 18); turned, it reaches up and down, not across.
        assert_eq!(at(&f, 32, 18).0, 235);
        assert_eq!(at(&f, 32, 18 - 12).0, 235, "up");
        assert_eq!(at(&f, 32 - 12, 18).0, 16, "no longer across");
    }

    #[test]
    fn blends_on_luma() {
        assert_eq!(blend_y(16, 100, Blend::Add), 100);
        assert_eq!(blend_y(200, 100, Blend::Add), 235, "clamped");
        assert_eq!(blend_y(235, 235, Blend::Multiply), 235);
        assert_eq!(blend_y(235, 16, Blend::Multiply), 16);
        assert_eq!(blend_y(16, 16, Blend::Screen), 16);
        assert_eq!(blend_y(16, 235, Blend::Screen), 235);
        // Multiply a mid grey over white: the grey.
        let f = render(
            W,
            H,
            &[
                item(ItemSource::Colour(Ycc::WHITE), (0.0, 0.0, 1.0, 1.0)),
                Item {
                    blend: Blend::Multiply,
                    ..item(ItemSource::Colour(Ycc(126, 128, 128)), (0.0, 0.0, 1.0, 1.0))
                },
            ],
            &|_| None,
        );
        assert!((125..=127).contains(&at(&f, 5, 5).0));
    }

    #[test]
    fn an_input_picture_cropped_and_placed() {
        // Left half white, right half black; the right half cropped in, top left quarter.
        let mut p = Frame::solid(40, 20, Ycc::BLACK);
        for r in 0..20 {
            for x in 0..20 {
                p.y[r * 40 + x] = 235;
            }
        }
        let p = Arc::new(p);
        let it = Item {
            crop: (0.5, 0.0, 1.0, 1.0),
            ..item(ItemSource::Input(7), (0.0, 0.0, 0.5, 0.5))
        };
        let f = render(W, H, &[it], &|id| (id == 7).then(|| p.clone()));
        assert_eq!(at(&f, 10, 5), (16, 255), "the black half, opaque");
        assert_eq!(at(&f, 50, 30).1, 0, "outside the quarter");
        // A missing input shows nothing.
        let it = item(ItemSource::Input(8), (0.0, 0.0, 1.0, 1.0));
        let f = render(W, H, &[it], &|_| None);
        assert!(f.a.as_ref().unwrap().iter().all(|&a| a == 0));
    }

    #[test]
    fn rounded_corners_and_a_layer_drawn_without_its_alpha() {
        // A white square, rounded by a quarter of its side: its corners clear, its edges'
        // middles and centre opaque; a border rounds with it.
        let it = Item {
            radius: 0.25,
            ..item(
                ItemSource::Colour(Ycc::WHITE),
                (0.25, 0.0, 36.0 / 64.0, 1.0),
            )
        };
        let f = render(W, H, std::slice::from_ref(&it), &|_| None);
        assert_eq!(at(&f, 16, 0).1, 0, "the top left corner");
        assert_eq!(at(&f, 51, 35).1, 0, "the bottom right corner");
        assert_eq!(at(&f, 16, 18), (235, 255), "the left edge's middle");
        assert_eq!(at(&f, 34, 0), (235, 255), "the top edge's middle");
        assert_eq!(at(&f, 34, 18), (235, 255), "the centre");
        // The arc antialiased: part covered between clear and opaque.
        assert!((1..255).contains(&at(&f, 18, 3).1) || (1..255).contains(&at(&f, 17, 4).1));
        let bordered = Item {
            border: 2,
            ..it.clone()
        };
        let f = render(W, H, &[bordered], &|_| None);
        assert_eq!(at(&f, 14, 18).1, 255, "the border beside the edge");
        assert_eq!(at(&f, 15, 1).1, 0, "the border rounded too");
        // Half-transparent everywhere: keyed by it, or drawn opaque with `key: none`.
        let mut p = Frame::solid(8, 8, Ycc::WHITE);
        p.a = Some(vec![128; 64]);
        let p = Arc::new(p);
        let pic = |id| (id == 7).then(|| p.clone());
        let keyed = item(ItemSource::Input(7), (0.0, 0.0, 0.125, 8.0 / 36.0));
        let f = render(W, H, std::slice::from_ref(&keyed), &pic);
        assert_eq!(at(&f, 2, 2).1, 128);
        let unkeyed = Item {
            keyed: false,
            ..keyed.clone()
        };
        let f = render(W, H, std::slice::from_ref(&unkeyed), &pic);
        assert_eq!(at(&f, 2, 2), (235, 255));
        assert!(opaque(
            &[Item {
                rect: (0.0, 0.0, 1.0, 1.0),
                ..unkeyed
            }],
            &pic
        ));
        // Turned, the same.
        let turned = Item {
            keyed: false,
            rotation: 30.0,
            ..item(ItemSource::Input(7), (0.25, 0.25, 0.5, 0.5))
        };
        assert_eq!(at(&render(W, H, &[turned], &pic), 32, 18), (235, 255));
    }

    #[test]
    fn past_the_edges_is_clipped() {
        let it = item(ItemSource::Colour(Ycc::WHITE), (-0.5, -0.5, 1.0, 1.0));
        let f = render(W, H, &[it], &|_| None);
        assert_eq!(at(&f, 5, 5).0, 235, "the part on the canvas");
        assert_eq!(at(&f, 50, 30).0, 16);
    }

    #[test]
    fn nesting_is_checked() {
        use std::collections::{BTreeMap, BTreeSet};
        let on = |id| item(ItemSource::Input(id), (0.0, 0.0, 1.0, 1.0));
        // 10 holds 11 holds camera 1.
        let mut d = BTreeMap::from([(10, vec![on(11), on(2)]), (11, vec![on(1)])]);
        assert!(check_nesting(&d).is_ok());
        let mut m = BTreeSet::new();
        members(10, &d, &mut m);
        assert_eq!(m, BTreeSet::from([1, 2]), "inputs through the nested scene");
        // 11 holding 10 makes a loop.
        d.insert(11, vec![on(10)]);
        assert!(check_nesting(&d).unwrap_err().contains("itself"));
        // Five deep is too deep.
        let chain: BTreeMap<u32, Vec<Item>> = (20..25)
            .map(|i| (i, if i < 24 { vec![on(i + 1)] } else { vec![] }))
            .collect();
        assert!(check_nesting(&chain).unwrap_err().contains("deep"));
    }

    #[test]
    fn json_round_trips_and_rejects_nonsense() {
        let v = json!([{"source": 3, "rect": [0.1, 0.2, 0.3, 0.4], "rotation": 15,
            "opacity": 0.5, "blend": "screen", "border": 4, "border_colour": "#ff0000",
            "radius": 0.9, "key": "none"},
            {"source": {"colour": "#00ff00"}}]);
        let items = items_from_json(&v).unwrap();
        assert_eq!(items[0].source, ItemSource::Input(3));
        assert_eq!(items[0].blend, Blend::Screen);
        assert_eq!((items[0].radius, items[0].keyed), (0.5, false));
        assert_eq!((items[1].radius, items[1].keyed), (0.0, true));
        assert_eq!(
            items_from_json(&json!(items.iter().map(Item::to_json).collect::<Vec<_>>())).unwrap(),
            items
        );
        assert!(items_from_json(&json!([{"source": "x"}])).is_err());
        assert!(items_from_json(&json!([{"source": 1, "crop": [0.5, 0, 0.2, 1]}])).is_err());
        assert!(items_from_json(&json!([{"source": 1, "blend": "dodge"}])).is_err());
        assert!(items_from_json(&json!([{"source": 1, "key": "luma"}])).is_err());
        assert!(items_from_json(&json!(vec![json!({"source": 1}); 17])).is_err());
        assert!(items_from_json(&json!(vec![json!({"source": 1}); 16])).is_ok());
    }

    #[test]
    fn ids_stay_and_new_layers_get_new_ones() {
        let on = |id, src| Item {
            id,
            ..item(ItemSource::Input(src), (0.0, 0.0, 1.0, 1.0))
        };
        let old = vec![on(1, 5), on(2, 6), on(7, 8)];
        // Sent back reordered, one new, one twice.
        let mut new = vec![on(7, 8), on(0, 9), on(1, 5), on(1, 6)];
        assign_ids(&old, &mut new);
        let ids: Vec<u32> = new.iter().map(|i| i.id).collect();
        assert_eq!(ids, [7, 8, 1, 9]);
        assert_eq!(LayerRef::Id(9).find(&new), Some(3));
        assert_eq!(LayerRef::Index(1).find(&new), Some(1));
        assert_eq!(LayerRef::Id(2).find(&new), None);
        // A copy of a global layer keeps its id and visibility.
        let def = Item {
            id: 3,
            name: "Logo".into(),
            opacity: 0.8,
            ..item(ItemSource::Input(4), (0.8, 0.0, 0.2, 0.2))
        };
        let mut copy = Item {
            id: 12,
            visible: false,
            ..Item::default()
        };
        apply_global(&mut copy, &def);
        assert_eq!(
            (copy.id, copy.visible, copy.global, copy.name.as_str()),
            (12, false, Some(3), "Logo")
        );
        assert_eq!(copy.rect, def.rect);
    }

    #[test]
    fn a_full_frame_layer_passes_its_input_through() {
        let full = item(ItemSource::Input(4), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(pass_through(std::slice::from_ref(&full)), Some(4));
        let hidden_logo = Item {
            visible: false,
            ..item(ItemSource::Input(9), (0.8, 0.0, 0.2, 0.2))
        };
        assert_eq!(pass_through(&[full.clone(), hidden_logo.clone()]), Some(4));
        let logo = Item {
            visible: true,
            ..hidden_logo
        };
        assert_eq!(pass_through(&[full.clone(), logo]), None);
        let half = Item {
            opacity: 0.5,
            ..full.clone()
        };
        assert_eq!(pass_through(&[half]), None);
        let colour = item(ItemSource::Colour(Ycc::WHITE), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(pass_through(&[colour]), None);
    }
}
