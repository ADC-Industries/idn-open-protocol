//! Video filters on an input (spec/extensions/idn-switcher.md §S.17, Wave 2): keys (chroma,
//! colour, luma), a picture as a mask, colour correction, a 3D LUT, sharpening and scrolling,
//! in order, on the input's 4:2:2 picture as it arrives — so the input is filtered wherever
//! it shows (program, scenes, mixes, the multiview). Keys and masks give the
//! picture an alpha plane (or multiply the one it has).

use crate::frame::{Frame, Ycc};
use idn_common::sync::LockOk as _;
use idn_open_protocol::shq::Plane;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Filters on an input, at most.
pub const MAX_FILTERS: usize = 16;

/// One filter, its settings as the API gives them.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Keys out a colour by its chroma (a green or blue screen): transparent within
    /// `similarity` of it (0–1, a distance on the Cb/Cr plane), fading over `smoothness`;
    /// the key colour's spill on what stays is taken out over `spill`.
    ChromaKey {
        colour: Ycc,
        similarity: f32,
        smoothness: f32,
        spill: f32,
    },
    /// Keys out a colour by luma and chroma together (a flat background of any colour).
    ColourKey {
        colour: Ycc,
        similarity: f32,
        smoothness: f32,
    },
    /// Keys by brightness (0–1): transparent below `min` and above `max`, each edge faded
    /// over its `_smooth` (graphics on black: `min` 0.1).
    LumaKey {
        min: f32,
        min_smooth: f32,
        max: f32,
        max_smooth: f32,
    },
    /// Brightness and contrast (−1–1), gamma (0.2–5, 1 none), saturation (0–3, 1 none), a
    /// hue turn (degrees) and opacity (0–1).
    Correct {
        brightness: f32,
        contrast: f32,
        gamma: f32,
        saturation: f32,
        hue: f32,
        opacity: f32,
    },
    /// A 3D or 1D LUT from a `.cube` file (a camera's log to Rec. 709, a look), `amount`
    /// of it (0–1).
    Lut { file: String, amount: f32 },
    /// Unsharp masking on luma (0–1).
    Sharpen { amount: f32 },
    /// A picture's alpha (or its brightness, when it has none) as this input's: shapes,
    /// soft edges, a cut-out. `invert` swaps what shows.
    Mask { file: String, invert: bool },
    /// Moves the picture round, wrapping, `x` and `y` frames a second (a ticker, a crawl).
    Scroll { x: f32, y: f32 },
}

/// A filter and whether it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub filter: Filter,
    pub on: bool,
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

/// What one setting of a filter is (§S.17): a number in a range, a colour, a file or a
/// check box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Setting {
    /// A number from `min` to `max` (clamped), `default` when missing; `step` is a UI's.
    Number {
        default: f32,
        min: f32,
        max: f32,
        step: f32,
    },
    /// A colour, `#rrggbb`.
    Colour { default: &'static str },
    /// A file on this machine (`about` says what it is).
    File { about: &'static str },
    /// True or false.
    Check { default: bool },
}

const fn number(default: f32, min: f32, max: f32, step: f32) -> Setting {
    Setting::Number {
        default,
        min,
        max,
        step,
    }
}

/// A filter's type, its name for people, and its settings by key.
pub type FilterSettings = (
    &'static str,
    &'static str,
    &'static [(&'static str, Setting)],
);

/// Every filter's settings: its type, its name for people, and each setting by key. The
/// parser ([`Item::from_json`]) takes its ranges and defaults from here, and the API serves
/// it (`GET /api/v1/filters_schema`): one source of truth.
pub const SCHEMA: &[FilterSettings] = &[
    (
        "chroma_key",
        "Chroma key",
        &[
            ("colour", Setting::Colour { default: "#00ff00" }),
            ("similarity", number(0.4, 0.0, 1.0, 0.01)),
            ("smoothness", number(0.08, 0.0, 1.0, 0.01)),
            ("spill", number(0.1, 0.0, 1.0, 0.01)),
        ],
    ),
    (
        "colour_key",
        "Colour key",
        &[
            ("colour", Setting::Colour { default: "#00ff00" }),
            ("similarity", number(0.3, 0.0, 1.0, 0.01)),
            ("smoothness", number(0.08, 0.0, 1.0, 0.01)),
        ],
    ),
    (
        "luma_key",
        "Luma key",
        &[
            ("min", number(0.0, 0.0, 1.0, 0.01)),
            ("min_smooth", number(0.0, 0.0, 1.0, 0.01)),
            ("max", number(1.0, 0.0, 1.0, 0.01)),
            ("max_smooth", number(0.0, 0.0, 1.0, 0.01)),
        ],
    ),
    (
        "correct",
        "Colour correction",
        &[
            ("brightness", number(0.0, -1.0, 1.0, 0.01)),
            ("contrast", number(0.0, -1.0, 1.0, 0.01)),
            ("gamma", number(1.0, 0.2, 5.0, 0.05)),
            ("saturation", number(1.0, 0.0, 3.0, 0.05)),
            ("hue", number(0.0, -180.0, 180.0, 1.0)),
            ("opacity", number(1.0, 0.0, 1.0, 0.01)),
        ],
    ),
    (
        "lut",
        "LUT",
        &[
            (
                "file",
                Setting::File {
                    about: "A .cube file on this machine",
                },
            ),
            ("amount", number(1.0, 0.0, 1.0, 0.05)),
        ],
    ),
    (
        "sharpen",
        "Sharpen",
        &[("amount", number(0.3, 0.0, 1.0, 0.05))],
    ),
    (
        "mask",
        "Mask",
        &[
            (
                "file",
                Setting::File {
                    about: "A picture file on this machine (its alpha, or its brightness)",
                },
            ),
            ("invert", Setting::Check { default: false }),
        ],
    ),
    (
        "scroll",
        "Scroll",
        &[
            ("x", number(0.0, -10.0, 10.0, 0.01)),
            ("y", number(0.0, -10.0, 10.0, 0.01)),
        ],
    ),
];

/// A number setting's default, minimum and maximum from [`SCHEMA`].
fn number_setting(ty: &str, key: &str) -> (f32, f32, f32) {
    SCHEMA
        .iter()
        .find(|f| f.0 == ty)
        .and_then(|f| f.2.iter().find(|s| s.0 == key))
        .and_then(|s| match s.1 {
            Setting::Number {
                default, min, max, ..
            } => Some((default, min, max)),
            _ => None,
        })
        .unwrap_or_else(|| unreachable!("filter {ty}: no number setting {key}"))
}

/// [`SCHEMA`] as JSON: `{"filters": [{"type", "name", "settings": [{"key", "kind": number|
/// colour|file|check, "default", "min", "max", "step", "about"}]}]}`.
pub fn schema_json() -> Value {
    let filters: Vec<Value> = SCHEMA
        .iter()
        .map(|(ty, name, settings)| {
            let settings: Vec<Value> = settings
                .iter()
                .map(|(key, s)| match *s {
                    Setting::Number {
                        default,
                        min,
                        max,
                        step,
                    } => json!({"key": key, "kind": "number", "default": r4(default),
                        "min": r4(min), "max": r4(max), "step": r4(step)}),
                    Setting::Colour { default } => {
                        json!({"key": key, "kind": "colour", "default": default})
                    }
                    Setting::File { about } => {
                        json!({"key": key, "kind": "file", "default": "", "about": about})
                    }
                    Setting::Check { default } => {
                        json!({"key": key, "kind": "check", "default": default})
                    }
                })
                .collect();
            json!({"type": ty, "name": name, "settings": settings})
        })
        .collect();
    json!({ "filters": filters, "max": MAX_FILTERS })
}

fn r4(x: f32) -> f64 {
    (x as f64 * 1e4).round() / 1e4
}

impl Item {
    /// From the API's JSON (§S.17); missing settings take their defaults.
    pub fn from_json(v: &Value) -> Result<Item, String> {
        let colour = |d: Ycc| -> Result<Ycc, String> {
            match &v["colour"] {
                Value::Null => Ok(d),
                c => crate::scene::parse_colour(c.as_str().unwrap_or(""))
                    .ok_or("colour: #rrggbb".into()),
            }
        };
        let file = || -> Result<String, String> {
            v["file"]
                .as_str()
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .ok_or("file: a path".into())
        };
        let ty = v["type"].as_str().unwrap_or("");
        // A number setting of this filter: its range and default from [`SCHEMA`].
        let n = |key: &str| -> Result<f32, String> {
            let (d, lo, hi) = number_setting(ty, key);
            num(&v[key], key, d, lo, hi)
        };
        let filter = match ty {
            "chroma_key" => Filter::ChromaKey {
                colour: colour(Ycc::from_rgb(0, 255, 0))?,
                similarity: n("similarity")?,
                smoothness: n("smoothness")?,
                spill: n("spill")?,
            },
            "colour_key" => Filter::ColourKey {
                colour: colour(Ycc::from_rgb(0, 255, 0))?,
                similarity: n("similarity")?,
                smoothness: n("smoothness")?,
            },
            "luma_key" => Filter::LumaKey {
                min: n("min")?,
                min_smooth: n("min_smooth")?,
                max: n("max")?,
                max_smooth: n("max_smooth")?,
            },
            "correct" => Filter::Correct {
                brightness: n("brightness")?,
                contrast: n("contrast")?,
                gamma: n("gamma")?,
                saturation: n("saturation")?,
                hue: n("hue")?,
                opacity: n("opacity")?,
            },
            "lut" => Filter::Lut {
                file: file()?,
                amount: n("amount")?,
            },
            "sharpen" => Filter::Sharpen {
                amount: n("amount")?,
            },
            "mask" => Filter::Mask {
                file: file()?,
                invert: v["invert"].as_bool().unwrap_or(false),
            },
            "scroll" => Filter::Scroll {
                x: n("x")?,
                y: n("y")?,
            },
            _ => {
                return Err(
                    "type: chroma_key, colour_key, luma_key, correct, lut, sharpen, \
                            mask or scroll"
                        .into(),
                );
            }
        };
        Ok(Item {
            filter,
            on: v["on"].as_bool().unwrap_or(true),
        })
    }

    pub fn to_json(&self) -> Value {
        let mut v = match &self.filter {
            Filter::ChromaKey {
                colour,
                similarity,
                smoothness,
                spill,
            } => json!({"type": "chroma_key", "colour": colour.to_hex(),
                "similarity": r4(*similarity), "smoothness": r4(*smoothness), "spill": r4(*spill)}),
            Filter::ColourKey {
                colour,
                similarity,
                smoothness,
            } => json!({"type": "colour_key", "colour": colour.to_hex(),
                "similarity": r4(*similarity), "smoothness": r4(*smoothness)}),
            Filter::LumaKey {
                min,
                min_smooth,
                max,
                max_smooth,
            } => json!({"type": "luma_key", "min": r4(*min), "min_smooth": r4(*min_smooth),
                "max": r4(*max), "max_smooth": r4(*max_smooth)}),
            Filter::Correct {
                brightness,
                contrast,
                gamma,
                saturation,
                hue,
                opacity,
            } => json!({"type": "correct", "brightness": r4(*brightness),
                "contrast": r4(*contrast), "gamma": r4(*gamma), "saturation": r4(*saturation),
                "hue": r4(*hue), "opacity": r4(*opacity)}),
            Filter::Lut { file, amount } => {
                json!({"type": "lut", "file": file, "amount": r4(*amount)})
            }
            Filter::Sharpen { amount } => json!({"type": "sharpen", "amount": r4(*amount)}),
            Filter::Mask { file, invert } => {
                json!({"type": "mask", "file": file, "invert": invert})
            }
            Filter::Scroll { x, y } => json!({"type": "scroll", "x": r4(*x), "y": r4(*y)}),
        };
        v["on"] = json!(self.on);
        v
    }
}

/// A list of filters from JSON (at most [`MAX_FILTERS`]).
pub fn items_from_json(v: &Value) -> Result<Vec<Item>, String> {
    let a = v.as_array().ok_or("filters: a list")?;
    if a.len() > MAX_FILTERS {
        return Err(format!("at most {MAX_FILTERS} filters"));
    }
    a.iter()
        .enumerate()
        .map(|(i, x)| Item::from_json(x).map_err(|e| format!("filter {}: {e}", i + 1)))
        .collect()
}

/// A filter made ready to run: its tables built, its files read.
enum Step {
    /// By `cb << 8 | cr`: the coverage, and the chroma with the spill taken out.
    ChromaKey(Vec<[u8; 3]>),
    /// Coverage by (Y/4, Cb/4, Cr/4).
    ColourKey(Vec<u8>),
    /// Coverage by luma sample.
    LumaKey(Box<[u8; 256]>),
    Correct {
        luma: Box<[u8; 256]>,
        /// (Cb, Cr) → (Cb′, Cr′), by `cb << 8 | cr`.
        chroma: Vec<[u8; 2]>,
        opacity: u32,
    },
    /// (Y, Cb/4, Cr/4) → (Y′, Cb′, Cr′).
    Lut(Vec<[u8; 3]>),
    Sharpen(f32),
    Mask {
        picture: Arc<Frame>,
        invert: bool,
        /// Its coverage at the size last asked for.
        scaled: Mutex<Option<ScaledMask>>,
    },
    Scroll(f32, f32),
}

/// A mask's coverage at a size: width, height, samples.
type ScaledMask = (usize, usize, Arc<Vec<u8>>);

/// An input's filters, ready to run on its pictures.
pub struct Chain {
    items: Vec<Item>,
    steps: Vec<Step>,
    /// Its steps as the GPU runs them, made on first use (`None`: one runs only here).
    gpu: std::sync::OnceLock<Option<Arc<Vec<GpuStep>>>>,
}

/// A step as the GPU runs it (`gpu_filter.wgsl`): what it does, its opacity (a
/// correction's), and its table packed a value to a word.
#[derive(Clone)]
pub struct GpuStep {
    /// 0 correct, 1 LUT, 2 chroma key, 3 colour key, 4 luma key, 5 sharpen.
    pub op: u32,
    pub opacity: u32,
    pub table: Arc<Vec<u32>>,
}

/// Reads a picture file (the switcher's picture loader).
pub type Load<'a> = &'a dyn Fn(&str) -> Result<Frame, String>;

impl Chain {
    /// Builds the filters that are on: tables computed, LUT files and mask pictures read.
    pub fn new(items: Vec<Item>, load: Option<Load>) -> Result<Chain, String> {
        if items.len() > MAX_FILTERS {
            return Err(format!("at most {MAX_FILTERS} filters"));
        }
        let mut steps = Vec::new();
        for it in items.iter().filter(|i| i.on) {
            steps.push(match &it.filter {
                Filter::ChromaKey {
                    colour,
                    similarity,
                    smoothness,
                    spill,
                } => {
                    let (kb, kr) = (colour.1 as f32, colour.2 as f32);
                    let (smooth, spill) = (smoothness.max(1e-3), spill.max(1e-3));
                    let mut t = vec![[0u8; 3]; 65536];
                    for (i, o) in t.iter_mut().enumerate() {
                        let (b, c) = ((i >> 8) as f32, (i & 255) as f32);
                        let d = ((b - kb).powi(2) + (c - kr).powi(2)).sqrt() / 224.0;
                        // Chroma near the key taken towards grey.
                        let s = ramp(d, *similarity, spill);
                        *o = [
                            (ramp(d, *similarity, smooth) * 255.0).round() as u8,
                            (128.0 + (b - 128.0) * s).round() as u8,
                            (128.0 + (c - 128.0) * s).round() as u8,
                        ];
                    }
                    Step::ChromaKey(t)
                }
                Filter::ColourKey {
                    colour,
                    similarity,
                    smoothness,
                } => {
                    let key = (colour.0 as f32, colour.1 as f32, colour.2 as f32);
                    let smooth = smoothness.max(1e-3);
                    let mut t = vec![0u8; 64 * 64 * 64];
                    for (i, o) in t.iter_mut().enumerate() {
                        let at = |k: usize| ((i >> k) & 63) as f32 * 4.0 + 1.5;
                        let (dy, db, dc) = (
                            (at(12) - key.0) / 219.0 * 0.5,
                            (at(6) - key.1) / 224.0,
                            (at(0) - key.2) / 224.0,
                        );
                        let d = (dy * dy + db * db + dc * dc).sqrt();
                        *o = (ramp(d, *similarity, smooth) * 255.0).round() as u8;
                    }
                    Step::ColourKey(t)
                }
                Filter::LumaKey {
                    min,
                    min_smooth,
                    max,
                    max_smooth,
                } => {
                    let mut t = Box::new([0u8; 256]);
                    for (v, o) in t.iter_mut().enumerate() {
                        let l = ((v as f32 - 16.0) / 219.0).clamp(0.0, 1.0);
                        let up = ramp(l, *min, *min_smooth);
                        // Transparent only above `max` (white stays at `max` 1).
                        let down = if *max_smooth <= 0.0 {
                            if l > *max { 0.0 } else { 1.0 }
                        } else {
                            1.0 - ramp(l, max - max_smooth, *max_smooth)
                        };
                        *o = (up * down * 255.0).round() as u8;
                    }
                    Step::LumaKey(t)
                }
                Filter::Correct {
                    brightness,
                    contrast,
                    gamma,
                    saturation,
                    hue,
                    opacity,
                } => {
                    let mut luma = Box::new([0u8; 256]);
                    let k = 1.0 + contrast * if *contrast > 0.0 { 2.0 } else { 1.0 };
                    for (v, o) in luma.iter_mut().enumerate() {
                        let l = ((v as f32 - 16.0) / 219.0).clamp(0.0, 1.0).powf(*gamma);
                        let l = (l - 0.5) * k + 0.5 + brightness;
                        *o = (16.0 + 219.0 * l).round().clamp(16.0, 235.0) as u8;
                    }
                    let (s, c) = hue.to_radians().sin_cos();
                    let mut chroma = vec![[128u8; 2]; 65536];
                    for (i, o) in chroma.iter_mut().enumerate() {
                        let (x, y) = ((i >> 8) as f32 - 128.0, (i & 255) as f32 - 128.0);
                        let (x, y) = ((x * c - y * s) * saturation, (x * s + y * c) * saturation);
                        let q = |v: f32| (128.0 + v).round().clamp(16.0, 240.0) as u8;
                        *o = [q(x), q(y)];
                    }
                    Step::Correct {
                        luma,
                        chroma,
                        opacity: (opacity * 255.0).round() as u32,
                    }
                }
                Filter::Lut { file, amount } => {
                    let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
                    let cube = Cube::parse(&text).map_err(|e| format!("{file}: {e}"))?;
                    Step::Lut(cube.ycc_table(*amount))
                }
                Filter::Sharpen { amount } => Step::Sharpen(amount * 2.0),
                Filter::Mask { file, invert } => {
                    let load = load.ok_or("no picture loader for the mask")?;
                    let picture = load(file).map_err(|e| format!("{file}: {e}"))?;
                    Step::Mask {
                        picture: Arc::new(picture.pixels()),
                        invert: *invert,
                        scaled: Mutex::new(None),
                    }
                }
                Filter::Scroll { x, y } => Step::Scroll(*x, *y),
            });
        }
        Ok(Chain {
            items,
            steps,
            gpu: std::sync::OnceLock::new(),
        })
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Its steps for the GPU, when every one runs there (correction, LUT, the keys, a
    /// sharpen); `None` with a mask or scroll, which run here only.
    pub fn gpu_steps(&self) -> Option<Arc<Vec<GpuStep>>> {
        self.gpu
            .get_or_init(|| {
                let step = |op: u32, opacity: u32, table: Vec<u32>| {
                    Some(GpuStep {
                        op,
                        opacity,
                        table: Arc::new(table),
                    })
                };
                let three = |v: &[u8; 3]| v[0] as u32 | (v[1] as u32) << 8 | (v[2] as u32) << 16;
                self.steps
                    .iter()
                    .map(|s| match s {
                        Step::Correct {
                            luma,
                            chroma,
                            opacity,
                        } => step(
                            0,
                            *opacity,
                            luma.iter()
                                .map(|&v| v as u32)
                                .chain(chroma.iter().map(|c| c[0] as u32 | (c[1] as u32) << 8))
                                .collect(),
                        ),
                        Step::Lut(t) => step(1, 255, t.iter().map(three).collect()),
                        Step::ChromaKey(t) => step(2, 255, t.iter().map(three).collect()),
                        Step::ColourKey(t) => step(3, 255, t.iter().map(|&v| v as u32).collect()),
                        Step::LumaKey(t) => step(4, 255, t.iter().map(|&v| v as u32).collect()),
                        // The amount × 256, as `sharpen` takes it.
                        Step::Sharpen(k) => step(5, 255, vec![(k * 256.0) as i32 as u32]),
                        Step::Mask { .. } | Step::Scroll(..) => None,
                    })
                    .collect::<Option<Vec<_>>>()
                    .map(Arc::new)
            })
            .clone()
    }

    /// Nothing to do (no filter on).
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// It changes with time (a scroll), so a still picture is filtered every frame.
    pub fn animated(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Scroll(x, y) if *x != 0.0 || *y != 0.0))
    }

    /// `f` through the filters, at `t` seconds (for a scroll).
    pub fn apply(&self, f: &Frame, t: f64) -> Frame {
        let mut f = copy(f);
        for s in &self.steps {
            match s {
                Step::ChromaKey(t) => chroma_key(&mut f, t),
                Step::ColourKey(t) => key_by(&mut f, |y, b, c| {
                    t[(y as usize >> 2) << 12 | (b as usize >> 2) << 6 | c as usize >> 2]
                }),
                Step::LumaKey(t) => key_by(&mut f, |y, _, _| t[y as usize]),
                Step::Correct {
                    luma,
                    chroma,
                    opacity,
                } => {
                    par_rows(&mut f, |r| {
                        for v in r.y.iter_mut() {
                            *v = luma[*v as usize];
                        }
                        for (b, c) in r.cb.iter_mut().zip(r.cr.iter_mut()) {
                            let [nb, nc] = chroma[(*b as usize) << 8 | *c as usize];
                            *b = nb;
                            *c = nc;
                        }
                    });
                    if *opacity < 255 {
                        let o = *opacity;
                        let a = f.a.get_or_insert_with(|| filled(f.width * f.height, 255));
                        for v in a.iter_mut() {
                            *v = ((*v as u32 * o + 127) / 255) as u8;
                        }
                    }
                }
                Step::Lut(table) => lut(&mut f, table),
                Step::Sharpen(k) => sharpen(&mut f, *k),
                Step::Mask {
                    picture,
                    invert,
                    scaled,
                } => {
                    let m = {
                        let mut c = scaled.lock_ok();
                        match c.as_ref() {
                            Some((w, h, m)) if (*w, *h) == (f.width, f.height) => m.clone(),
                            _ => {
                                let p = picture.scaled((0.0, 0.0, 1.0, 1.0), f.width, f.height);
                                let m: Vec<u8> = match &p.a {
                                    Some(a) => a.clone(),
                                    None => {
                                        p.y.iter()
                                            .map(|&y| {
                                                ((y.clamp(16, 235) as u32 - 16) * 255 / 219) as u8
                                            })
                                            .collect()
                                    }
                                };
                                let m = Arc::new(m);
                                *c = Some((f.width, f.height, m.clone()));
                                m
                            }
                        }
                    };
                    let inv = *invert;
                    let a = f.a.get_or_insert_with(|| filled(f.width * f.height, 255));
                    for (v, &k) in a.iter_mut().zip(m.iter()) {
                        let k = if inv { 255 - k } else { k } as u32;
                        *v = ((*v as u32 * k + 127) / 255) as u8;
                    }
                }
                Step::Scroll(x, y) => scroll(&mut f, *x as f64 * t, *y as f64 * t),
            }
        }
        f
    }
}

/// A copy to filter, in pooled memory (reconstructed first if it came as coefficients).
fn copy(f: &Frame) -> Frame {
    if f.deferred() {
        return f.pixels();
    }
    let take = |s: &[u8]| {
        let mut v = Plane::buffer(s.len());
        v.copy_from_slice(s);
        v
    };
    Frame {
        width: f.width,
        height: f.height,
        y: take(&f.y),
        cb: take(&f.cb),
        cr: take(&f.cr),
        a: f.a.as_deref().map(take),
        coefs: None,
        gpu: None,
    }
}

/// `len` bytes of `v`, in pooled memory.
fn filled(len: usize, v: u8) -> Vec<u8> {
    let mut b = Plane::buffer(len);
    b.fill(v);
    b
}

/// 0 below `at`, 1 from `at + width`, smoothly between.
fn ramp(v: f32, at: f32, width: f32) -> f32 {
    if width <= 0.0 {
        return if v >= at { 1.0 } else { 0.0 };
    }
    let t = ((v - at) / width).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A picture's rows, a band of them to a thread.
struct Rows<'a> {
    width: usize,
    y: &'a mut [u8],
    cb: &'a mut [u8],
    cr: &'a mut [u8],
    /// Empty when the picture has no alpha.
    a: &'a mut [u8],
}

/// Runs `work` on bands of rows in parallel (one band on a small picture).
fn par_rows(f: &mut Frame, work: impl Fn(Rows) + Sync) {
    let (w, h) = (f.width, f.height);
    let cw = Frame::chroma_width(w);
    let threads = if w * h < 400_000 {
        1
    } else {
        crate::par::threads()
    };
    let band = h.div_ceil(threads).max(1);
    let mut no_alpha: Vec<u8> = Vec::new();
    let a: &mut [u8] = f.a.as_deref_mut().unwrap_or(&mut no_alpha);
    let has_a = !a.is_empty();
    let work = &work;
    crate::par::scope(|s| {
        let mut ys = f.y.chunks_mut(band * w);
        let mut cbs = f.cb.chunks_mut(band * cw);
        let mut crs = f.cr.chunks_mut(band * cw);
        let mut as_ = a.chunks_mut(band * w);
        while let (Some(y), Some(cb), Some(cr)) = (ys.next(), cbs.next(), crs.next()) {
            let a = if has_a {
                as_.next().unwrap_or_default()
            } else {
                &mut []
            };
            let r = Rows {
                width: w,
                y,
                cb,
                cr,
                a,
            };
            if threads == 1 {
                work(r);
            } else {
                s.spawn(move || work(r));
            }
        }
    });
}

/// The picture's alpha times `cover(y, cb, cr)` per pixel (each pixel with its chroma
/// sample's chroma); a picture without alpha takes the coverage as it.
fn key_by(f: &mut Frame, cover: impl Fn(u8, u8, u8) -> u8 + Sync) {
    let had = f.a.is_some();
    if !had {
        f.a = Some(Plane::buffer(f.width * f.height));
    }
    par_rows(f, |r| {
        let w = r.width;
        let cw = Frame::chroma_width(w);
        for row in 0..r.y.len() / w {
            let (cb, cr) = (
                &r.cb[row * cw..(row + 1) * cw],
                &r.cr[row * cw..(row + 1) * cw],
            );
            let (y, a) = (
                &r.y[row * w..(row + 1) * w],
                &mut r.a[row * w..(row + 1) * w],
            );
            for (x, (a, &y)) in a.iter_mut().zip(y).enumerate() {
                let k = cover(y, cb[x / 2], cr[x / 2]);
                *a = if had {
                    ((*a as u32 * k as u32 + 127) / 255) as u8
                } else {
                    k
                };
            }
        }
    });
}

/// A chroma key by its table: coverage at each chroma sample (between two for an odd
/// pixel), the spill taken out of the chroma.
fn chroma_key(f: &mut Frame, t: &[[u8; 3]]) {
    let had = f.a.is_some();
    if !had {
        f.a = Some(Plane::buffer(f.width * f.height));
    }
    par_rows(f, |r| {
        let w = r.width;
        let cw = Frame::chroma_width(w);
        let mut cover = vec![0u8; cw];
        for row in 0..r.y.len() / w {
            let (cb, cr) = (
                &mut r.cb[row * cw..(row + 1) * cw],
                &mut r.cr[row * cw..(row + 1) * cw],
            );
            for ((k, b), c) in cover.iter_mut().zip(cb.iter_mut()).zip(cr.iter_mut()) {
                let [a, nb, nc] = t[(*b as usize) << 8 | *c as usize];
                *k = a;
                *b = nb;
                *c = nc;
            }
            let a = &mut r.a[row * w..(row + 1) * w];
            for (x, a) in a.iter_mut().enumerate() {
                let c = x / 2;
                let k = if x % 2 == 1 && c + 1 < cw {
                    (cover[c] as u16 + cover[c + 1] as u16).div_ceil(2) as u8
                } else {
                    cover[c]
                };
                *a = if had {
                    ((*a as u32 * k as u32 + 127) / 255) as u8
                } else {
                    k
                };
            }
        }
    });
}

/// Through a table by (Y, Cb/4, Cr/4): each pixel its luma (an odd pixel with the chroma
/// between its two samples), each chroma sample the mean of its two pixels'.
fn lut(f: &mut Frame, t: &[[u8; 3]]) {
    let at = |y: u8, b: u8, c: u8| t[(y as usize) << 12 | (b as usize >> 2) << 6 | c as usize >> 2];
    par_rows(f, |r| {
        let w = r.width;
        let cw = Frame::chroma_width(w);
        for row in 0..r.y.len() / w {
            let (cb, cr) = (
                &mut r.cb[row * cw..(row + 1) * cw],
                &mut r.cr[row * cw..(row + 1) * cw],
            );
            let y = &mut r.y[row * w..(row + 1) * w];
            for c in 0..cw {
                let x0 = 2 * c;
                let (b0, c0) = (cb[c], cr[c]);
                let (b1, c1) = if c + 1 < cw {
                    (
                        (b0 as u16 + cb[c + 1] as u16).div_ceil(2) as u8,
                        (c0 as u16 + cr[c + 1] as u16).div_ceil(2) as u8,
                    )
                } else {
                    (b0, c0)
                };
                let p = at(y[x0], b0, c0);
                y[x0] = p[0];
                if x0 + 1 < w {
                    let q = at(y[x0 + 1], b1, c1);
                    y[x0 + 1] = q[0];
                    cb[c] = (p[1] as u16 + q[1] as u16).div_ceil(2) as u8;
                    cr[c] = (p[2] as u16 + q[2] as u16).div_ceil(2) as u8;
                } else {
                    cb[c] = p[1];
                    cr[c] = p[2];
                }
            }
        }
    });
}

/// Unsharp masking on luma: each sample pushed away from its 3×3 mean by `k`.
fn sharpen(f: &mut Frame, k: f32) {
    let (w, h) = (f.width, f.height);
    if w < 3 || h < 3 {
        return;
    }
    let src = {
        let mut v = Plane::buffer(f.y.len());
        v.copy_from_slice(&f.y);
        v
    };
    let kk = (k * 256.0) as i32;
    let rows_each = h.div_ceil(8).max(1);
    crate::par::scope(|s| {
        for (band, out) in f.y.chunks_mut(rows_each * w).enumerate() {
            let src = &src;
            s.spawn(move || {
                // Each row's sums of three across, then three of those down.
                let across = |y: usize, o: &mut [i32]| {
                    let r = &src[y * w..(y + 1) * w];
                    for x in 1..w - 1 {
                        o[x] = r[x - 1] as i32 + r[x] as i32 + r[x + 1] as i32;
                    }
                };
                // Rolled down a row at a time: each row summed across once.
                let (mut up, mut mid, mut down) = (vec![0i32; w], vec![0i32; w], vec![0i32; w]);
                let mut have = None;
                for (i, row) in out.chunks_mut(w).enumerate() {
                    let y = band * rows_each + i;
                    if y == 0 || y == h - 1 {
                        continue;
                    }
                    if have == Some(y - 1) {
                        std::mem::swap(&mut up, &mut mid);
                        std::mem::swap(&mut mid, &mut down);
                    } else {
                        across(y - 1, &mut up);
                        across(y, &mut mid);
                    }
                    across(y + 1, &mut down);
                    have = Some(y);
                    let s = &src[y * w..(y + 1) * w];
                    for x in 1..w - 1 {
                        let v = s[x] as i32;
                        let d = v - (up[x] + mid[x] + down[x] + 4) / 9;
                        row[x] = (v + ((d * kk) >> 8)).clamp(1, 254) as u8;
                    }
                }
            });
        }
    });
    Plane {
        width: 0,
        height: 0,
        data: src,
    }
    .recycle();
}

/// Moved round by `fx`, `fy` frames (wrapping), an even number of pixels across.
fn scroll(f: &mut Frame, fx: f64, fy: f64) {
    let (w, h) = (f.width, f.height);
    if w == 0 || h == 0 {
        return;
    }
    let cw = Frame::chroma_width(w);
    let dx = ((fx.rem_euclid(1.0) * w as f64) as usize / 2 * 2) % w.max(1);
    let dy = (fy.rem_euclid(1.0) * h as f64) as usize % h;
    let shift = |p: &mut Vec<u8>, pw: usize, sx: usize| {
        if sx == 0 && dy == 0 {
            return;
        }
        let mut out = Plane::buffer(p.len());
        for row in 0..h {
            let to = (row + dy) % h;
            let (s, d) = (
                &p[row * pw..(row + 1) * pw],
                &mut out[to * pw..(to + 1) * pw],
            );
            d[sx..].copy_from_slice(&s[..pw - sx]);
            d[..sx].copy_from_slice(&s[pw - sx..]);
        }
        let old = std::mem::replace(p, out);
        Plane {
            width: 0,
            height: 0,
            data: old,
        }
        .recycle();
    };
    shift(&mut f.y, w, dx);
    shift(&mut f.cb, cw, (dx / 2).min(cw));
    shift(&mut f.cr, cw, (dx / 2).min(cw));
    if let Some(a) = f.a.as_mut() {
        shift(a, w, dx);
    }
}

/// A `.cube` LUT: its table of RGB values (red fastest), 3D or 1D, over its domain.
struct Cube {
    size: usize,
    three_d: bool,
    min: [f32; 3],
    max: [f32; 3],
    data: Vec<[f32; 3]>,
}

impl Cube {
    fn parse(text: &str) -> Result<Cube, String> {
        let (mut size, mut three_d) = (0usize, true);
        let (mut min, mut max) = ([0.0f32; 3], [1.0f32; 3]);
        let mut data = Vec::new();
        let three = |it: &mut std::str::SplitWhitespace| -> Result<[f32; 3], String> {
            let mut v = [0.0; 3];
            for x in &mut v {
                *x = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("a line of three numbers")?;
            }
            Ok(v)
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            match it.next() {
                Some("LUT_3D_SIZE") => {
                    size = it
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or("LUT_3D_SIZE")?;
                    three_d = true;
                }
                Some("LUT_1D_SIZE") => {
                    size = it
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or("LUT_1D_SIZE")?;
                    three_d = false;
                }
                Some("DOMAIN_MIN") => min = three(&mut it)?,
                Some("DOMAIN_MAX") => max = three(&mut it)?,
                Some("TITLE") | Some("LUT_3D_INPUT_RANGE") | Some("LUT_1D_INPUT_RANGE") => {}
                Some(first) if first.parse::<f32>().is_ok() => {
                    let mut it = line.split_whitespace();
                    data.push(three(&mut it)?);
                }
                Some(_) => {}
                None => {}
            }
        }
        let want = if three_d {
            size.checked_pow(3)
        } else {
            Some(size)
        };
        if !(2..=256).contains(&size) || want != Some(data.len()) {
            return Err(format!(
                "not a .cube LUT (size {size}, {} entries)",
                data.len()
            ));
        }
        Ok(Cube {
            size,
            three_d,
            min,
            max,
            data,
        })
    }

    /// An RGB (0–1) through the LUT, trilinear (or each channel through a 1D one).
    fn map(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let pos = |i: usize| {
            let d = (self.max[i] - self.min[i]).max(1e-6);
            (((rgb[i] - self.min[i]) / d).clamp(0.0, 1.0) * (n - 1) as f32).min((n - 1) as f32)
        };
        if !self.three_d {
            let mut out = [0.0; 3];
            for (i, o) in out.iter_mut().enumerate() {
                let p = pos(i);
                let (a, t) = (p.floor() as usize, p.fract());
                let b = (a + 1).min(n - 1);
                *o = self.data[a][i] * (1.0 - t) + self.data[b][i] * t;
            }
            return out;
        }
        let (pr, pg, pb) = (pos(0), pos(1), pos(2));
        let (r0, g0, b0) = (
            pr.floor() as usize,
            pg.floor() as usize,
            pb.floor() as usize,
        );
        let (r1, g1, b1) = (
            (r0 + 1).min(n - 1),
            (g0 + 1).min(n - 1),
            (b0 + 1).min(n - 1),
        );
        let (tr, tg, tb) = (pr.fract(), pg.fract(), pb.fract());
        let at = |r: usize, g: usize, b: usize| self.data[r + n * (g + n * b)];
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ]
        };
        let c00 = lerp(at(r0, g0, b0), at(r1, g0, b0), tr);
        let c10 = lerp(at(r0, g1, b0), at(r1, g1, b0), tr);
        let c01 = lerp(at(r0, g0, b1), at(r1, g0, b1), tr);
        let c11 = lerp(at(r0, g1, b1), at(r1, g1, b1), tr);
        lerp(lerp(c00, c10, tg), lerp(c01, c11, tg), tb)
    }

    /// The LUT as a table by (Y, Cb/4, Cr/4) in the switcher's Y′CbCr (BT.709, limited),
    /// `amount` of it mixed with the input.
    fn ycc_table(&self, amount: f32) -> Vec<[u8; 3]> {
        let mut t = vec![[0u8; 3]; 256 * 64 * 64];
        let rows_each = 256 / 8;
        crate::par::scope(|s| {
            for (band, out) in t.chunks_mut(rows_each * 4096).enumerate() {
                s.spawn(move || {
                    for (i, o) in out.iter_mut().enumerate() {
                        let y = (band * rows_each + i / 4096) as f32;
                        let (cb, cr) = (
                            ((i / 64) % 64) as f32 * 4.0 + 1.5,
                            (i % 64) as f32 * 4.0 + 1.5,
                        );
                        let (l, pb, pr) = (
                            (y - 16.0) / 219.0,
                            (cb - 128.0) / 224.0,
                            (cr - 128.0) / 224.0,
                        );
                        let rgb = [
                            (l + 1.5748 * pr).clamp(0.0, 1.0),
                            (l - 0.1873 * pb - 0.4681 * pr).clamp(0.0, 1.0),
                            (l + 1.8556 * pb).clamp(0.0, 1.0),
                        ];
                        let m = self.map(rgb);
                        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                        let c = Ycc::from_rgb(q(m[0]), q(m[1]), q(m[2]));
                        let mix = |a: f32, b: u8| (a + (b as f32 - a) * amount).round() as u8;
                        *o = [mix(y, c.0), mix(cb, c.1), mix(cr, c.2)];
                    }
                });
            }
        });
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_what_the_parser_takes() {
        let s = schema_json();
        let list = s["filters"].as_array().unwrap();
        assert_eq!(list.len(), 8);
        for f in list {
            // Each filter with no settings given: its defaults, as the schema says.
            let item = Item::from_json(&json!({"type": f["type"], "file": "x.cube"})).unwrap();
            let got = item.to_json();
            for st in f["settings"].as_array().unwrap() {
                let key = st["key"].as_str().unwrap();
                match st["kind"].as_str().unwrap() {
                    "number" => {
                        assert_eq!(got[key], st["default"], "{} {key}", f["type"]);
                        // Out of range: clamped to the schema's range.
                        let big =
                            Item::from_json(&json!({"type": f["type"], "file": "x", key: 1e6}))
                                .unwrap()
                                .to_json();
                        assert_eq!(big[key], st["max"], "{} {key}", f["type"]);
                    }
                    "check" => assert_eq!(got[key], st["default"]),
                    "colour" => assert_eq!(
                        got[key].as_str().unwrap().to_lowercase()[..3],
                        st["default"].as_str().unwrap()[..3]
                    ),
                    _ => {}
                }
            }
        }
    }

    fn chain(v: Value) -> Chain {
        Chain::new(items_from_json(&v).unwrap(), None).unwrap()
    }

    /// Green on the left half, a skin tone on the right.
    fn screen() -> Frame {
        let (w, h) = (64, 8);
        let mut f = Frame::solid(w, h, Ycc::from_rgb(0, 200, 0));
        let skin = Ycc::from_rgb(220, 170, 140);
        for row in 0..h {
            for x in w / 2..w {
                f.y[row * w + x] = skin.0;
            }
            for c in w / 4..w / 2 {
                f.cb[row * (w / 2) + c] = skin.1;
                f.cr[row * (w / 2) + c] = skin.2;
            }
        }
        f
    }

    #[test]
    fn chroma_key_takes_out_the_green_only() {
        let f = chain(json!([{"type": "chroma_key"}])).apply(&screen(), 0.0);
        let a = f.a.as_ref().unwrap();
        assert_eq!(a[4 * 64 + 4], 0, "the screen");
        assert_eq!(a[4 * 64 + 50], 255, "the person");
        // Off: nothing changes.
        let off = chain(json!([{"type": "chroma_key", "on": false}]));
        assert!(off.is_empty());
        assert_eq!(off.apply(&screen(), 0.0), screen());
    }

    #[test]
    fn colour_and_luma_keys() {
        let f = chain(json!([{"type": "colour_key", "colour": "#00c800"}])).apply(&screen(), 0.0);
        assert_eq!(f.a.as_ref().unwrap()[4 * 64 + 4], 0);
        // Graphics on black: the black goes.
        let mut g = Frame::black(64, 8);
        for v in &mut g.y[..32] {
            *v = 235;
        }
        let f = chain(json!([{"type": "luma_key", "min": 0.1, "min_smooth": 0.05}])).apply(&g, 0.0);
        let a = f.a.as_ref().unwrap();
        assert_eq!((a[0], a[100]), (255, 0));
    }

    #[test]
    fn correction_moves_luma_and_chroma() {
        let grey = Frame::solid(16, 4, Ycc(126, 140, 120));
        let f = chain(json!([{"type": "correct", "brightness": 0.2}])).apply(&grey, 0.0);
        assert!(f.y[0] > 160, "{}", f.y[0]);
        let f = chain(json!([{"type": "correct", "saturation": 0.0}])).apply(&grey, 0.0);
        assert_eq!((f.cb[0], f.cr[0]), (128, 128));
        let f = chain(json!([{"type": "correct", "opacity": 0.5}])).apply(&grey, 0.0);
        assert_eq!(f.a.as_ref().unwrap()[0], 128);
    }

    #[test]
    fn a_cube_lut() {
        // An identity 2×2×2 LUT, then one that inverts.
        let dir = idn_common::testing::TempDir::new("idn-lut");
        let mut id = String::from("LUT_3D_SIZE 2\n");
        let mut inv = id.clone();
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    id += &format!("{r} {g} {b}\n");
                    inv += &format!("{} {} {}\n", 1 - r, 1 - g, 1 - b);
                }
            }
        }
        std::fs::write(dir.join("id.cube"), id).unwrap();
        std::fs::write(dir.join("inv.cube"), inv).unwrap();
        let pic = Frame::solid(16, 4, Ycc::from_rgb(200, 60, 30));
        let f = chain(json!([{"type": "lut", "file": dir.join("id.cube")}])).apply(&pic, 0.0);
        let close = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 3;
        assert!(close(f.y[0], pic.y[0]) && close(f.cb[0], pic.cb[0]) && close(f.cr[0], pic.cr[0]));
        let f = chain(json!([{"type": "lut", "file": dir.join("inv.cube")}])).apply(&pic, 0.0);
        let want = Ycc::from_rgb(55, 195, 225);
        assert!(
            close(f.y[0], want.0) && close(f.cb[0], want.1) && close(f.cr[0], want.2),
            "{:?}",
            (f.y[0], f.cb[0], f.cr[0], want)
        );
        assert!(
            Chain::new(
                items_from_json(&json!([{"type": "lut", "file": dir.join("nope.cube")}])).unwrap(),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn mask_scroll_and_sharpen() {
        // A mask whose left half is white: only the left half shows.
        let mut m = Frame::black(32, 4);
        for row in 0..4 {
            for x in 0..16 {
                m.y[row * 32 + x] = 235;
            }
        }
        let load = move |_: &str| Ok(m.clone());
        let c = Chain::new(
            items_from_json(&json!([{"type": "mask", "file": "m.png"}])).unwrap(),
            Some(&load),
        )
        .unwrap();
        let f = c.apply(&Frame::solid(64, 8, Ycc::WHITE), 0.0);
        let a = f.a.as_ref().unwrap();
        assert_eq!((a[2], a[60]), (255, 0));
        // A quarter of a frame a second, after 1 s: column 0 is now column 16.
        let mut p = Frame::black(64, 2);
        p.y[0] = 200;
        let s = chain(json!([{"type": "scroll", "x": 0.25}]));
        assert!(s.animated());
        assert_eq!(s.apply(&p, 1.0).y[16], 200);
        // Sharpening steepens an edge.
        let mut e = Frame::solid(16, 8, Ycc(100, 128, 128));
        for row in 0..8 {
            for x in 8..16 {
                e.y[row * 16 + x] = 150;
            }
        }
        let f = chain(json!([{"type": "sharpen", "amount": 1.0}])).apply(&e, 0.0);
        assert!(f.y[4 * 16 + 7] < 100 && f.y[4 * 16 + 8] > 150);
    }

    #[test]
    fn json_round_trip() {
        let v = json!([
            {"type": "chroma_key", "colour": "#0000ff", "similarity": 0.3, "on": false},
            {"type": "correct", "gamma": 1.2, "hue": 10},
            {"type": "scroll", "x": -0.1},
        ]);
        let items = items_from_json(&v).unwrap();
        let back: Vec<Value> = items.iter().map(Item::to_json).collect();
        assert_eq!(items_from_json(&Value::Array(back)).unwrap(), items);
        assert!(items_from_json(&json!([{"type": "blur"}])).is_err());
    }
}
