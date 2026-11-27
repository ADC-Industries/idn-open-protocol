//! Titles (spec/extensions/idn-switcher.md §S.16, Wave 2): boxes and text drawn over a
//! transparent canvas — a name, a lower third, a caption — as an input. Text is shaped and
//! drawn with cosmic-text (any installed font, other scripts through font fallback, right-
//! to-left too), with an outline and a soft shadow, then turned into the switcher's 4:2:2
//! with alpha. Sizes are fractions of the canvas, so a title looks the same at any size.

use crate::frame::{Frame, Ycc};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight,
    Wrap,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Layers in a title, at most.
pub const MAX_LAYERS: usize = 32;
/// Characters in a text layer, at most.
pub const MAX_TEXT: usize = 4000;

/// The fallback faces, for a machine with no fonts (a server): DejaVu Sans (Bitstream Vera
/// licence, `fonts/LICENSE-DejaVu.txt`).
const DEJAVU: &[u8] = include_bytes!("../fonts/DejaVuSans.ttf");
const DEJAVU_BOLD: &[u8] = include_bytes!("../fonts/DejaVuSans-Bold.ttf");

/// A colour with alpha, full-range RGB (as `#rrggbb` or `#rrggbbaa`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const WHITE: Rgba = Rgba(255, 255, 255, 255);
    pub const BLACK: Rgba = Rgba(0, 0, 0, 255);

    pub fn parse(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#')?;
        if !h.is_ascii() || (h.len() != 6 && h.len() != 8) {
            return None;
        }
        let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(Rgba(
            b(0)?,
            b(2)?,
            b(4)?,
            if h.len() == 8 { b(6)? } else { 255 },
        ))
    }

    pub fn to_hex(self) -> String {
        if self.3 == 255 {
            format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.0, self.1, self.2, self.3)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Centre,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

/// A soft shadow under text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    /// Right and down, as fractions of the text's size.
    pub offset: (f32, f32),
    /// Its softness, a fraction of the text's size (0: hard).
    pub blur: f32,
    pub colour: Rgba,
}

/// A text layer: laid out in its rectangle, wrapped at its width when `wrap`.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub text: String,
    /// A font family by name; empty: the default sans serif.
    pub font: String,
    /// The text's size, a fraction of the canvas height (0.05: a twentieth).
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub colour: Rgba,
    pub align: HAlign,
    pub valign: VAlign,
    pub wrap: bool,
    /// Line spacing, times the size.
    pub line: f32,
    /// An outline around the letters, a fraction of the size (0: none).
    pub outline: f32,
    pub outline_colour: Rgba,
    pub shadow: Option<Shadow>,
}

impl Default for Text {
    fn default() -> Self {
        Text {
            text: String::new(),
            font: String::new(),
            size: 0.06,
            bold: false,
            italic: false,
            colour: Rgba::WHITE,
            align: HAlign::Left,
            valign: VAlign::Middle,
            wrap: true,
            line: 1.2,
            outline: 0.0,
            outline_colour: Rgba::BLACK,
            shadow: None,
        }
    }
}

/// What a layer draws.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerKind {
    /// A filled rectangle, its corners rounded by `radius` (a fraction of its height,
    /// 0–0.5).
    Box {
        colour: Rgba,
        radius: f32,
    },
    Text(Text),
    /// A picture file (a logo), fitted whole into the rectangle and centred, `opacity`
    /// (0–1) of it.
    Picture {
        file: String,
        opacity: f32,
    },
}

/// One layer of a title, drawn bottom to top.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// x, y, width, height as fractions of the canvas.
    pub rect: (f32, f32, f32, f32),
    pub kind: LayerKind,
    pub visible: bool,
}

/// A title: its layers, bottom to top, and the values of its fields: a text's `{name}` is
/// the field `name` (a name, a score), `{clock}` the time and so on (§S.19).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Title {
    pub layers: Vec<Layer>,
    pub fields: BTreeMap<String, String>,
    /// Where its `{Column}` values come from, a row at a time (§S.20).
    pub data: Option<crate::data::Source>,
}

/// Fields in a title, at most; characters in a field's value, at most.
pub const MAX_FIELDS: usize = 64;
pub const MAX_FIELD: usize = 1000;

/// A text with its `{placeholders}` filled in by `look` (a field, the clock, a timer);
/// one `look` doesn't know stays as written, so a designer sees it. `{{` is a brace.
pub fn expand(text: &str, look: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(a) = after.strip_prefix('{') {
            out.push('{');
            rest = a;
            continue;
        }
        match after.find('}') {
            Some(j) if !after[..j].contains('{') => {
                let key = after[..j].trim();
                match look(key) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&rest[i..i + j + 2]),
                }
                rest = &after[j + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A placeholder that changes by itself: the clock, the date, a timer (so a title showing
/// one is drawn again as it changes).
pub fn is_moving(key: &str) -> bool {
    let k = key.split(':').next().unwrap_or("");
    matches!(k, "clock" | "date" | "timer")
}

fn num(v: &Value, what: &str, d: f32, lo: f32, hi: f32) -> Result<f32, String> {
    match v {
        Value::Null => Ok(d),
        v => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(|x| (x as f32).clamp(lo, hi))
            .ok_or(format!("{what}: a number")),
    }
}

fn colour(v: &Value, what: &str, d: Rgba) -> Result<Rgba, String> {
    match v {
        Value::Null => Ok(d),
        v => Rgba::parse(v.as_str().unwrap_or("")).ok_or(format!("{what}: #rrggbb or #rrggbbaa")),
    }
}

fn pair(v: &Value, what: &str, d: (f32, f32)) -> Result<(f32, f32), String> {
    match v {
        Value::Null => Ok(d),
        Value::Array(a) if a.len() == 2 => {
            let n = |i: usize| num(&a[i], what, 0.0, -1.0, 1.0);
            Ok((n(0)?, n(1)?))
        }
        _ => Err(format!("{what}: two numbers")),
    }
}

/// A fraction for the JSON: four places (not an f32's float noise).
fn r4(x: f32) -> f64 {
    (x as f64 * 1e4).round() / 1e4
}

impl Layer {
    /// From the API's JSON (§S.16); missing fields take their defaults.
    pub fn from_json(v: &Value) -> Result<Layer, String> {
        let rect = match &v["rect"] {
            Value::Null => (0.0, 0.0, 1.0, 1.0),
            Value::Array(a) if a.len() == 4 => {
                let n = |i: usize| num(&a[i], "rect", 0.0, -1.0, 2.0);
                (n(0)?, n(1)?, n(2)?.max(0.0), n(3)?.max(0.0))
            }
            _ => return Err("rect: four numbers".into()),
        };
        let kind = match v["type"].as_str().unwrap_or("text") {
            "box" => LayerKind::Box {
                colour: colour(&v["colour"], "colour", Rgba(0, 0, 0, 160))?,
                radius: num(&v["radius"], "radius", 0.0, 0.0, 0.5)?,
            },
            "text" => {
                let d = Text::default();
                let text = v["text"].as_str().unwrap_or("");
                if text.chars().count() > MAX_TEXT {
                    return Err(format!("text: at most {MAX_TEXT} characters"));
                }
                let shadow = match &v["shadow"] {
                    Value::Null | Value::Bool(false) => None,
                    s => Some(Shadow {
                        offset: pair(&s["offset"], "shadow offset", (0.04, 0.04))?,
                        blur: num(&s["blur"], "shadow blur", 0.08, 0.0, 1.0)?,
                        colour: colour(&s["colour"], "shadow colour", Rgba(0, 0, 0, 180))?,
                    }),
                };
                LayerKind::Text(Text {
                    text: text.to_owned(),
                    font: v["font"].as_str().unwrap_or("").to_owned(),
                    size: num(&v["size"], "size", d.size, 0.005, 1.0)?,
                    bold: v["bold"].as_bool().unwrap_or(d.bold),
                    italic: v["italic"].as_bool().unwrap_or(d.italic),
                    colour: colour(&v["colour"], "colour", d.colour)?,
                    align: match v["align"].as_str().unwrap_or("left") {
                        "left" => HAlign::Left,
                        "centre" | "center" => HAlign::Centre,
                        "right" => HAlign::Right,
                        _ => return Err("align: left, centre or right".into()),
                    },
                    valign: match v["valign"].as_str().unwrap_or("middle") {
                        "top" => VAlign::Top,
                        "middle" => VAlign::Middle,
                        "bottom" => VAlign::Bottom,
                        _ => return Err("valign: top, middle or bottom".into()),
                    },
                    wrap: v["wrap"].as_bool().unwrap_or(d.wrap),
                    line: num(&v["line"], "line", d.line, 0.5, 4.0)?,
                    outline: num(&v["outline"], "outline", d.outline, 0.0, 0.3)?,
                    outline_colour: colour(
                        &v["outline_colour"],
                        "outline_colour",
                        d.outline_colour,
                    )?,
                    shadow,
                })
            }
            "picture" => LayerKind::Picture {
                file: v["file"]
                    .as_str()
                    .filter(|f| !f.is_empty())
                    .ok_or("file: a picture's path")?
                    .to_owned(),
                opacity: num(&v["opacity"], "opacity", 1.0, 0.0, 1.0)?,
            },
            _ => return Err("type: text, box or picture".into()),
        };
        Ok(Layer {
            rect,
            kind,
            visible: v["visible"].as_bool().unwrap_or(true),
        })
    }

    pub fn to_json(&self) -> Value {
        let r = self.rect;
        let mut v = match &self.kind {
            LayerKind::Box { colour, radius } => {
                json!({"type": "box", "colour": colour.to_hex(), "radius": r4(*radius)})
            }
            LayerKind::Text(t) => json!({
                "type": "text", "text": t.text, "font": t.font, "size": r4(t.size),
                "bold": t.bold, "italic": t.italic, "colour": t.colour.to_hex(),
                "align": match t.align { HAlign::Left => "left", HAlign::Centre => "centre", HAlign::Right => "right" },
                "valign": match t.valign { VAlign::Top => "top", VAlign::Middle => "middle", VAlign::Bottom => "bottom" },
                "wrap": t.wrap, "line": r4(t.line), "outline": r4(t.outline),
                "outline_colour": t.outline_colour.to_hex(),
                "shadow": t.shadow.map_or(Value::Null, |s| json!({
                    "offset": [r4(s.offset.0), r4(s.offset.1)], "blur": r4(s.blur), "colour": s.colour.to_hex(),
                })),
            }),
            LayerKind::Picture { file, opacity } => {
                json!({"type": "picture", "file": file, "opacity": r4(*opacity)})
            }
        };
        v["rect"] = json!([r4(r.0), r4(r.1), r4(r.2), r4(r.3)]);
        v["visible"] = json!(self.visible);
        v
    }
}

/// Fields from JSON: `{"name": "value", …}` (numbers taken as text).
pub fn fields_from_json(v: &Value) -> Result<BTreeMap<String, String>, String> {
    let Some(m) = v.as_object() else {
        return match v {
            Value::Null => Ok(BTreeMap::new()),
            _ => Err("fields: {\"name\": \"value\", …}".into()),
        };
    };
    if m.len() > MAX_FIELDS {
        return Err(format!("at most {MAX_FIELDS} fields"));
    }
    m.iter()
        .map(|(k, v)| {
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                _ => return Err(format!("field {k}: text or a number")),
            };
            if v.chars().count() > MAX_FIELD || k.contains(['{', '}']) {
                return Err(format!(
                    "field {k}: at most {MAX_FIELD} characters, no braces in its name"
                ));
            }
            Ok((k.clone(), v))
        })
        .collect()
}

impl Title {
    /// From the API's JSON: `{"layers": [...]}` (at most [`MAX_LAYERS`]).
    pub fn from_json(v: &Value) -> Result<Title, String> {
        let a = v["layers"].as_array().ok_or("layers: a list")?;
        if a.len() > MAX_LAYERS {
            return Err(format!("at most {MAX_LAYERS} layers"));
        }
        let layers = a
            .iter()
            .enumerate()
            .map(|(i, x)| Layer::from_json(x).map_err(|e| format!("layer {}: {e}", i + 1)))
            .collect::<Result<_, _>>()?;
        Ok(Title {
            layers,
            fields: fields_from_json(&v["fields"])?,
            data: match &v["data"] {
                Value::Null => None,
                d => Some(crate::data::Source::from_json(d)?),
            },
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "layers": self.layers.iter().map(Layer::to_json).collect::<Vec<_>>(),
            "fields": self.fields,
            "data": self.data.as_ref().map(crate::data::Source::to_json),
        })
    }

    /// The pictures its layers show.
    pub fn pictures(&self) -> Vec<&str> {
        self.layers
            .iter()
            .filter_map(|l| match &l.kind {
                LayerKind::Picture { file, .. } => Some(file.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Its texts as they show now (placeholders filled in by `look`).
    pub fn texts(&self, look: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        self.layers
            .iter()
            .filter_map(|l| match &l.kind {
                LayerKind::Text(t) => Some(expand(&t.text, look)),
                _ => None,
            })
            .collect()
    }

    /// It shows something that changes by itself (a clock, a timer).
    pub fn is_moving(&self) -> bool {
        let moving = std::cell::Cell::new(false);
        let look = |k: &str| {
            moving.set(moving.get() | is_moving(k));
            None
        };
        let _ = self.texts(&look);
        moving.get()
    }

    /// A title of one text layer (a text source): the text in `rect`, over a box when
    /// `backing` has a colour.
    pub fn text(text: Text, rect: (f32, f32, f32, f32), backing: Option<Rgba>) -> Title {
        let mut layers = Vec::new();
        if let Some(colour) = backing {
            layers.push(Layer {
                rect,
                kind: LayerKind::Box {
                    colour,
                    radius: 0.0,
                },
                visible: true,
            });
        }
        layers.push(Layer {
            rect,
            kind: LayerKind::Text(text),
            visible: true,
        });
        Title {
            layers,
            fields: BTreeMap::new(),
            data: None,
        }
    }
}

/// The fonts, loaded once (the machine's, and the fallback face) with their glyph cache.
pub struct Fonts {
    system: FontSystem,
    cache: SwashCache,
}

impl Default for Fonts {
    fn default() -> Self {
        Fonts::new()
    }
}

impl Fonts {
    /// Loads the machine's fonts (a moment on a machine with many) and the fallback face.
    pub fn new() -> Fonts {
        let mut system = FontSystem::new();
        let db = system.db_mut();
        db.load_font_data(DEJAVU.to_vec());
        db.load_font_data(DEJAVU_BOLD.to_vec());
        // No sans serif face on the machine under the usual name: the fallback face.
        let sans = cosmic_text::fontdb::Query {
            families: &[Family::SansSerif],
            ..Default::default()
        };
        if db.query(&sans).is_none() {
            db.set_sans_serif_family("DejaVu Sans");
        }
        Fonts {
            system,
            cache: SwashCache::new(),
        }
    }

    /// The font families there are, sorted.
    pub fn families(&self) -> Vec<String> {
        let mut f: Vec<String> = self
            .system
            .db()
            .faces()
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .collect();
        f.sort_by_key(|n| n.to_lowercase());
        f.dedup();
        f
    }
}

/// A premultiplied RGBA canvas, drawn on layer by layer.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<[u8; 4]>,
}

impl Canvas {
    /// `colour` over the canvas where `mask` covers (`mask` is `mw` wide at `(x0, y0)`,
    /// clipped to the canvas).
    fn fill(&mut self, mask: &[u8], mw: usize, x0: i64, y0: i64, colour: Rgba) {
        if colour.3 == 0 || mw == 0 {
            return;
        }
        let mh = mask.len() / mw;
        for my in 0..mh {
            let y = y0 + my as i64;
            if y < 0 || y >= self.h as i64 {
                continue;
            }
            let row = &mask[my * mw..(my + 1) * mw];
            for (mx, &m) in row.iter().enumerate() {
                let x = x0 + mx as i64;
                if m == 0 || x < 0 || x >= self.w as i64 {
                    continue;
                }
                let a = (m as u32 * colour.3 as u32 + 127) / 255;
                let d = &mut self.px[y as usize * self.w + x as usize];
                let inv = 255 - a;
                let over = |s: u8, d: u8| ((s as u32 * a + d as u32 * inv + 127) / 255) as u8;
                *d = [
                    over(colour.0, d[0]),
                    over(colour.1, d[1]),
                    over(colour.2, d[2]),
                    (a + (d[3] as u32 * inv + 127) / 255) as u8,
                ];
            }
        }
    }

    /// A picture fitted whole into `r` (pixels: x0, y0, x1, y1), centred, `opacity` of it.
    fn picture(&mut self, p: &Frame, r: (f32, f32, f32, f32), opacity: f32) {
        let (bw, bh) = (r.2 - r.0, r.3 - r.1);
        if bw < 1.0 || bh < 1.0 || p.width == 0 || p.height == 0 || opacity <= 0.0 {
            return;
        }
        let s = (bw / p.width as f32).min(bh / p.height as f32);
        let (w, h) = (
            ((p.width as f32 * s).round() as usize).max(1),
            ((p.height as f32 * s).round() as usize).max(1),
        );
        let x0 = (r.0 + (bw - w as f32) / 2.0).round() as i64;
        let y0 = (r.1 + (bh - h as f32) / 2.0).round() as i64;
        let f = p.scaled((0.0, 0.0, 1.0, 1.0), w, h);
        let cw = Frame::chroma_width(w);
        let o = (opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
        for row in 0..h {
            let y = y0 + row as i64;
            if y < 0 || y >= self.h as i64 {
                continue;
            }
            for col in 0..w {
                let x = x0 + col as i64;
                if x < 0 || x >= self.w as i64 {
                    continue;
                }
                let a = (f.alpha(col, row) as u32 * o + 127) / 255;
                if a == 0 {
                    continue;
                }
                let (l, pb, pr) = (
                    (f.y[row * w + col] as f32 - 16.0) / 219.0,
                    (f.cb[row * cw + col / 2] as f32 - 128.0) / 224.0,
                    (f.cr[row * cw + col / 2] as f32 - 128.0) / 224.0,
                );
                let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
                let rgb = [
                    q(l + 1.5748 * pr),
                    q(l - 0.1873 * pb - 0.4681 * pr),
                    q(l + 1.8556 * pb),
                ];
                let d = &mut self.px[y as usize * self.w + x as usize];
                let inv = 255 - a;
                let over = |s: u32, d: u8| ((s * a + d as u32 * inv + 127) / 255) as u8;
                *d = [
                    over(rgb[0], d[0]),
                    over(rgb[1], d[1]),
                    over(rgb[2], d[2]),
                    (a + (d[3] as u32 * inv + 127) / 255) as u8,
                ];
            }
        }
    }

    /// The canvas as 4:2:2 with alpha: colours un-premultiplied, a chroma sample from its
    /// two pixels weighted by their coverage.
    fn into_frame(self) -> Frame {
        let (w, h) = (self.w, self.h);
        let cw = Frame::chroma_width(w);
        let mut f = Frame {
            width: w,
            height: h,
            y: vec![16; w * h],
            cb: vec![128; cw * h],
            cr: vec![128; cw * h],
            a: Some(vec![0; w * h]),
            coefs: None,
            gpu: None,
        };
        let a_plane = f.a.as_mut().unwrap();
        for row in 0..h {
            for cx in 0..cw {
                let (mut cb, mut cr, mut wsum) = (0u32, 0u32, 0u32);
                for x in [2 * cx, 2 * cx + 1] {
                    if x >= w {
                        continue;
                    }
                    let p = self.px[row * w + x];
                    if p[3] == 0 {
                        continue;
                    }
                    let un =
                        |c: u8| ((c as u32 * 255 + p[3] as u32 / 2) / p[3] as u32).min(255) as u8;
                    let c = Ycc::from_rgb(un(p[0]), un(p[1]), un(p[2]));
                    f.y[row * w + x] = c.0;
                    a_plane[row * w + x] = p[3];
                    cb += c.1 as u32 * p[3] as u32;
                    cr += c.2 as u32 * p[3] as u32;
                    wsum += p[3] as u32;
                }
                // Neither pixel covered: the neutral chroma stays.
                if let (Some(b), Some(r)) = (
                    (cb + wsum / 2).checked_div(wsum),
                    (cr + wsum / 2).checked_div(wsum),
                ) {
                    f.cb[row * cw + cx] = b as u8;
                    f.cr[row * cw + cx] = r as u8;
                }
            }
        }
        f
    }
}

/// A rectangle in pixels from fractions of the canvas: x0, y0, x1, y1 (not clipped).
fn pixels(rect: (f32, f32, f32, f32), w: usize, h: usize) -> (f32, f32, f32, f32) {
    let (w, h) = (w as f32, h as f32);
    (
        rect.0 * w,
        rect.1 * h,
        (rect.0 + rect.2) * w,
        (rect.1 + rect.3) * h,
    )
}

/// A rounded rectangle's coverage, anti-aliased, over the pixels it touches.
fn box_mask(r: (f32, f32, f32, f32), radius: f32) -> (Vec<u8>, usize, i64, i64) {
    let (x0, y0) = (r.0.floor() as i64, r.1.floor() as i64);
    let (x1, y1) = (r.2.ceil() as i64, r.3.ceil() as i64);
    let (mw, mh) = ((x1 - x0).max(0) as usize, (y1 - y0).max(0) as usize);
    let mut m = vec![0u8; mw * mh];
    let (hw, hh) = ((r.2 - r.0) / 2.0, (r.3 - r.1) / 2.0);
    let (cx, cy) = (r.0 + hw, r.1 + hh);
    let rad = radius.min(hw).min(hh).max(0.0);
    for my in 0..mh {
        for mx in 0..mw {
            let (px, py) = (x0 as f32 + mx as f32 + 0.5, y0 as f32 + my as f32 + 0.5);
            // Signed distance to the rounded rectangle.
            let qx = (px - cx).abs() - (hw - rad);
            let qy = (py - cy).abs() - (hh - rad);
            let out =
                (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad;
            m[my * mw + mx] = ((0.5 - out).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    (m, mw, x0, y0)
}

/// `mask` grown by `r` pixels in every direction (round): an outline's coverage. From
/// the exact distance to the nearest covered pixel (half covered or more; Felzenszwalb and
/// Huttenlocher's transform, by columns then rows): the same time whatever `r`. A disc of
/// offsets for every pixel took tens of seconds for a thick outline at 2160p.
fn dilate(mask: &[u8], mw: usize, r: f32) -> Vec<u8> {
    let mh = mask.len() / mw.max(1);
    if mw == 0 || mh == 0 {
        return mask.to_vec();
    }
    const FAR: f64 = 1e12;
    let mut d: Vec<f64> = mask
        .iter()
        .map(|&v| if v >= 128 { 0.0 } else { FAR })
        .collect();
    let n = mw.max(mh);
    let (mut f, mut out, mut v, mut z) = (
        vec![0f64; n],
        vec![0f64; n],
        vec![0usize; n],
        vec![0f64; n + 1],
    );
    for x in 0..mw {
        for y in 0..mh {
            f[y] = d[y * mw + x];
        }
        distance_1d(&f[..mh], &mut out[..mh], &mut v, &mut z);
        for y in 0..mh {
            d[y * mw + x] = out[y];
        }
    }
    for y in 0..mh {
        let row = &mut d[y * mw..(y + 1) * mw];
        f[..mw].copy_from_slice(row);
        distance_1d(&f[..mw], &mut out[..mw], &mut v, &mut z);
        row.copy_from_slice(&out[..mw]);
    }
    mask.iter()
        .zip(&d)
        .map(|(&m, &dd)| {
            let k = ((r + 0.5) as f64 - dd.sqrt()).clamp(0.0, 1.0);
            m.max((k * 255.0).round() as u8)
        })
        .collect()
}

/// One line's squared distances (`f`: 0 where covered, far elsewhere, or the last pass's)
/// into `d`; `v` and `z` are scratch of the line's length (and one more).
fn distance_1d(f: &[f64], d: &mut [f64], v: &mut [usize], z: &mut [f64]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let sq = |q: usize| (q * q) as f64;
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    let cross = |q: usize, p: usize| ((f[q] + sq(q)) - (f[p] + sq(p))) / (2.0 * (q - p) as f64);
    for q in 1..n {
        // (z[0] is −∞: k never goes below 0.)
        let mut s = cross(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = cross(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    let mut k = 0usize;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let p = v[k];
        *out = sq(q.abs_diff(p)) + f[p];
    }
}

/// A text's outline, and the room around it for outline and shadow, at most (pixels).
const MAX_OUTLINE_PX: f32 = 128.0;
const MAX_PAD_PX: i64 = 512;

/// A box blur of radius `r`, three passes each way (close to a Gaussian).
fn blur(mask: &mut [u8], mw: usize, r: usize) {
    if r == 0 || mw == 0 {
        return;
    }
    let mh = mask.len() / mw;
    let mut tmp = vec![0u8; mask.len()];
    let pass = |src: &[u8], dst: &mut [u8], n: usize, len: usize, stride: usize, step: usize| {
        for line in 0..n {
            let base = line * stride;
            let at =
                |i: i64| -> u32 { src[base + (i.clamp(0, len as i64 - 1) as usize) * step] as u32 };
            let win = 2 * r as u32 + 1;
            let mut sum: u32 = (-(r as i64)..=r as i64).map(at).sum();
            for i in 0..len {
                dst[base + i * step] = ((sum + win / 2) / win) as u8;
                sum += at(i as i64 + r as i64 + 1);
                sum -= at(i as i64 - r as i64);
            }
        }
    };
    for _ in 0..3 {
        pass(mask, &mut tmp, mh, mw, mw, 1);
        pass(&tmp, mask, mw, mh, 1, mw);
    }
}

/// A text layer's coverage, in an area around its rectangle wide enough for its outline
/// and shadow: (mask, width, x, y on the canvas).
fn text_mask(
    t: &Text,
    r: (f32, f32, f32, f32),
    pad: i64,
    canvas_h: usize,
    fonts: &mut Fonts,
) -> (Vec<u8>, usize, i64, i64) {
    let px = (t.size * canvas_h as f32).max(1.0);
    let (bw, bh) = ((r.2 - r.0).max(1.0), (r.3 - r.1).max(1.0));
    let mut buf = Buffer::new(&mut fonts.system, Metrics::new(px, px * t.line));
    buf.set_wrap(if t.wrap {
        Wrap::WordOrGlyph
    } else {
        Wrap::None
    });
    buf.set_size(Some(bw), None);
    let family = if t.font.is_empty() {
        Family::SansSerif
    } else {
        Family::Name(&t.font)
    };
    let attrs = Attrs::new()
        .family(family)
        .weight(if t.bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if t.italic {
            Style::Italic
        } else {
            Style::Normal
        });
    let align = match t.align {
        HAlign::Left => Align::Left,
        HAlign::Centre => Align::Center,
        HAlign::Right => Align::Right,
    };
    buf.set_text(&t.text, &attrs, Shaping::Advanced, Some(align));
    buf.shape_until_scroll(&mut fonts.system, false);
    let content_h = buf
        .layout_runs()
        .map(|run| run.line_top + run.line_height)
        .fold(0.0f32, f32::max);
    let dy = match t.valign {
        VAlign::Top => 0.0,
        VAlign::Middle => (bh - content_h) / 2.0,
        VAlign::Bottom => bh - content_h,
    };
    let (x0, y0) = (r.0.floor() as i64 - pad, r.1.floor() as i64 - pad);
    let (mw, mh) = (
        bw.ceil() as usize + 2 * pad as usize + 1,
        bh.ceil() as usize + 2 * pad as usize + 1,
    );
    let mut m = vec![0u8; mw * mh];
    // The text's origin in the mask.
    let (ox, oy) = (r.0 - x0 as f32, r.1 + dy - y0 as f32);
    let (ox, oy) = (ox.round() as i64, oy.round() as i64);
    buf.draw(
        &mut fonts.system,
        &mut fonts.cache,
        Color::rgba(255, 255, 255, 255),
        |x, y, w, h, c| {
            for yy in 0..h as i64 {
                for xx in 0..w as i64 {
                    let (mx, my) = (ox + x as i64 + xx, oy + y as i64 + yy);
                    if mx < 0 || my < 0 || mx >= mw as i64 || my >= mh as i64 {
                        continue;
                    }
                    let p = &mut m[my as usize * mw + mx as usize];
                    *p = (*p).max(c.a());
                }
            }
        },
    );
    (m, mw, x0, y0)
}

/// What drawing a title looks up besides its fields: placeholders (the clock, timers) and
/// its pictures (read beforehand: drawing reads no files).
pub struct Lookups<'a> {
    pub value: &'a dyn Fn(&str) -> Option<String>,
    pub picture: &'a dyn Fn(&str) -> Option<Arc<Frame>>,
}

/// Draws a title at `width` × `height` (its fields filled in; no clock, no pictures):
/// transparent where it has nothing.
pub fn render(title: &Title, width: usize, height: usize, fonts: &mut Fonts) -> Frame {
    render_with(
        title,
        width,
        height,
        fonts,
        &Lookups {
            value: &|_| None,
            picture: &|_| None,
        },
    )
}

/// Draws a title, its placeholders and pictures from `look`.
pub fn render_with(
    title: &Title,
    width: usize,
    height: usize,
    fonts: &mut Fonts,
    look: &Lookups,
) -> Frame {
    let value = |k: &str| title.fields.get(k).cloned().or_else(|| (look.value)(k));
    let mut c = Canvas {
        w: width,
        h: height,
        px: vec![[0; 4]; width * height],
    };
    for l in title.layers.iter().filter(|l| l.visible) {
        let r = pixels(l.rect, width, height);
        match &l.kind {
            LayerKind::Box { colour, radius } => {
                let (m, mw, x0, y0) = box_mask(r, radius * (r.3 - r.1));
                c.fill(&m, mw, x0, y0, *colour);
            }
            LayerKind::Picture { file, opacity } => {
                if let Some(p) = (look.picture)(file) {
                    c.picture(&p, r, *opacity);
                }
            }
            LayerKind::Text(t) => {
                let t = &Text {
                    text: expand(&t.text, &value),
                    ..t.clone()
                };
                if t.text.trim().is_empty() {
                    continue;
                }
                let px = t.size * height as f32;
                // In pixels at most so much, whatever the fractions (extreme values made
                // masks of gigabytes).
                let outline = (t.outline * px).min(MAX_OUTLINE_PX);
                let pad = (match t.shadow {
                    Some(s) => {
                        (outline + s.blur * px * 1.5 + s.offset.0.abs().max(s.offset.1.abs()) * px)
                            .ceil() as i64
                    }
                    None => outline.ceil() as i64,
                } + 2)
                    .min(MAX_PAD_PX);
                let (m, mw, x0, y0) = text_mask(t, r, pad, height, fonts);
                let shape = if outline > 0.0 {
                    dilate(&m, mw, outline)
                } else {
                    m.clone()
                };
                if let Some(s) = t.shadow {
                    let mut sm = shape.clone();
                    blur(&mut sm, mw, ((s.blur * px / 2.0).round() as usize).min(256));
                    let (dx, dy) = (
                        (s.offset.0 * px).round() as i64,
                        (s.offset.1 * px).round() as i64,
                    );
                    c.fill(&sm, mw, x0 + dx, y0 + dy, s.colour);
                }
                if outline > 0.0 {
                    c.fill(&shape, mw, x0, y0, t.outline_colour);
                }
                c.fill(&m, mw, x0, y0, t.colour);
            }
        }
    }
    c.into_frame()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distance transform's outline is the disc's, as the brute-force one made it
    /// (to within a level of rounding, where both are exact).
    #[test]
    fn an_outline_is_a_round_dilation() {
        let (mw, mh) = (41usize, 31usize);
        let mut mask = vec![0u8; mw * mh];
        for (y, row) in mask.chunks_mut(mw).enumerate() {
            for (x, m) in row.iter_mut().enumerate() {
                if (18..23).contains(&x) && (12..19).contains(&y) || (x == 5 && y == 5) {
                    *m = 255;
                }
            }
        }
        let r = 6.0f32;
        let got = dilate(&mask, mw, r);
        for y in 0..mh as i64 {
            for x in 0..mw as i64 {
                let mut want = 0f32;
                for sy in 0..mh as i64 {
                    for sx in 0..mw as i64 {
                        if mask[sy as usize * mw + sx as usize] == 255 {
                            let d = (((x - sx).pow(2) + (y - sy).pow(2)) as f32).sqrt();
                            want = want.max((r + 0.5 - d).clamp(0.0, 1.0));
                        }
                    }
                }
                let g = got[y as usize * mw + x as usize] as f32 / 255.0;
                assert!((g - want).abs() < 0.01, "({x},{y}): {g} against {want}");
            }
        }
    }

    #[test]
    fn json_round_trip() {
        let v = json!({"layers": [
            {"type": "box", "rect": [0.05, 0.75, 0.5, 0.12], "colour": "#10203080", "radius": 0.2},
            {"type": "text", "rect": [0.07, 0.75, 0.46, 0.12], "text": "Pastor Sam", "bold": true,
             "size": 0.05, "colour": "#ffffff", "align": "centre", "valign": "bottom",
             "outline": 0.05, "shadow": {"offset": [0.03, 0.03], "blur": 0.1, "colour": "#000000c0"}},
        ]});
        let t = Title::from_json(&v).unwrap();
        assert_eq!(Title::from_json(&t.to_json()).unwrap(), t);
        assert!(Title::from_json(&json!({"layers": [{"type": "circle"}]})).is_err());
        assert!(Title::from_json(&json!({"layers": [{"colour": "red"}]})).is_err());
    }

    #[test]
    fn text_is_drawn_where_it_is_placed() {
        let mut fonts = Fonts::new();
        let title = Title::text(
            Text {
                text: "Welcome".into(),
                size: 0.1,
                bold: true,
                align: HAlign::Centre,
                outline: 0.06,
                shadow: Some(Shadow {
                    offset: (0.05, 0.05),
                    blur: 0.1,
                    colour: Rgba(0, 0, 0, 200),
                }),
                ..Default::default()
            },
            (0.25, 0.4, 0.5, 0.2),
            Some(Rgba(0, 0, 128, 255)),
        );
        let f = render(&title, 640, 360, &mut fonts);
        let a = f.a.as_ref().unwrap();
        // Outside the box: transparent; inside it: opaque.
        assert_eq!(a[10 * 640 + 10], 0);
        assert_eq!(a[(360 / 2) * 640 + 170], 255);
        // White letters in the middle: some samples near white, inside the box only.
        let bright = (144..216)
            .flat_map(|y| (160..480).map(move |x| (x, y)))
            .filter(|&(x, y)| f.y[y * 640 + x] > 200)
            .count();
        assert!(bright > 200, "{bright} bright samples");
        let outside = (0..360)
            .flat_map(|y| (0..640).map(move |x| (x, y)))
            .filter(|&(x, y)| !(150..490).contains(&x) || !(134..226).contains(&y))
            .filter(|&(x, y)| a[y * 640 + x] > 0)
            .count();
        assert_eq!(outside, 0);
        // The fallback font is there whatever the machine has.
        assert!(fonts.families().iter().any(|f| f == "DejaVu Sans"));
    }

    #[test]
    fn placeholders_fields_and_pictures() {
        let look = |k: &str| match k {
            "name" => Some("Sam".to_owned()),
            "clock" => Some("10:30".to_owned()),
            _ => None,
        };
        assert_eq!(
            expand("Hi {name}, {clock} {{x}} {nope}", &look),
            "Hi Sam, 10:30 {x}} {nope}"
        );
        assert_eq!(expand("{ name }", &look), "Sam");
        assert_eq!(expand("open {", &look), "open {");
        let mut t = Title::from_json(&json!({
            "layers": [{"type": "text", "text": "{home} – {away}"}, {"type": "picture", "file": "logo.png", "rect": [0, 0, 0.5, 0.5]}],
            "fields": {"home": 2, "away": "1"},
        }))
        .unwrap();
        assert_eq!(t.fields["home"], "2");
        assert!(!t.is_moving());
        assert_eq!(t.pictures(), ["logo.png"]);
        assert_eq!(Title::from_json(&t.to_json()).unwrap(), t);
        t.layers[0].kind = LayerKind::Text(Text {
            text: "Starts in {timer:service}".into(),
            ..Default::default()
        });
        assert!(t.is_moving());
        // A red picture fitted into the top-left quarter (centred: 16:9 into 16:9).
        let red = Arc::new(Frame::solid(32, 18, Ycc::from_rgb(255, 0, 0)));
        let mut fonts = Fonts::new();
        let f = render_with(
            &t,
            320,
            180,
            &mut fonts,
            &Lookups {
                value: &|_| None,
                picture: &|p| (p == "logo.png").then(|| red.clone()),
            },
        );
        let a = f.a.as_ref().unwrap();
        assert_eq!((a[40 * 320 + 40], a[150 * 320 + 300]), (255, 0));
        let want = Ycc::from_rgb(255, 0, 0);
        assert!((f.y[40 * 320 + 40] as i32 - want.0 as i32).abs() <= 2);
        assert!((f.cr[40 * 160 + 20] as i32 - want.2 as i32).abs() <= 2);
    }

    #[test]
    fn rounded_box_corners_are_soft() {
        let (m, mw, _, _) = box_mask((10.0, 10.0, 110.0, 60.0), 20.0);
        // The corner pixel is outside the rounding, the centre inside.
        assert_eq!(m[0], 0);
        assert_eq!(m[25 * mw + 50], 255);
        // Some partly covered pixels along the curve.
        assert!(m.iter().any(|&v| v > 0 && v < 255));
    }
}
