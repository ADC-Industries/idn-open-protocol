//! The multiview (spec/extensions/idn-switcher.md §S.7): tiles placed by a layout (by
//! default preview and program large across the top, every input small below), each
//! labelled, with red and green borders for what is on program and preview. Sent as an
//! IDN output (switched on and off at run time) and served as a JPEG to the API.

use crate::font;
use crate::frame::{Frame, Ycc};
use std::sync::Arc;

/// The multiview's size.
pub const WIDTH: usize = 1920;
pub const HEIGHT: usize = 1080;

const RED: Ycc = Ycc(63, 102, 240);
const GREEN: Ycc = Ycc(145, 54, 34);
const GREY: Ycc = Ycc(60, 128, 128);

/// A place on the multiview, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// What a tile shows (§S.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileKind {
    /// The program as sent.
    Program,
    /// The preview bus's scene.
    Preview,
    /// An input, by id (received at the tile's size while nothing else uses it).
    Input(u32),
    /// A scene, by id, drawn at the tile's size.
    Scene(u32),
    /// An extra mix (§S.15), by id: its program's scene.
    Mix(u32),
}

/// One tile of a multiview layout: what it shows, where (x, y, width, height as fractions
/// of the multiview), and whether it has its label strip and its tally border.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileSpec {
    pub kind: TileKind,
    pub rect: (f32, f32, f32, f32),
    pub label: bool,
    pub tally: bool,
}

/// Tiles in a layout, at most.
pub const MAX_TILES: usize = 128;

impl TileSpec {
    /// From the API's JSON: `{"kind": "program"|"preview"|"input"|"scene"|"mix", "id",
    /// "rect": [x, y, w, h], "label": true, "tally": true}`.
    pub fn from_json(v: &serde_json::Value) -> Result<TileSpec, String> {
        let id = || -> Result<u32, String> {
            v["id"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| "a tile's id: a number".to_owned())
        };
        let kind = match v["kind"].as_str().unwrap_or("") {
            "program" => TileKind::Program,
            "preview" => TileKind::Preview,
            "input" => TileKind::Input(id()?),
            "scene" => TileKind::Scene(id()?),
            "mix" => TileKind::Mix(id()?),
            _ => return Err("a tile's kind: program, preview, input, scene or mix".into()),
        };
        let r: Vec<f32> = v["rect"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_f64().filter(|x| x.is_finite()))
                    .map(|x| x as f32)
                    .collect()
            })
            .unwrap_or_default();
        let [x, y, w, h] = r[..] else {
            return Err("a tile's rect: [x, y, width, height], fractions".into());
        };
        let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        let (w, h) = (w.clamp(0.0, 1.0 - x), h.clamp(0.0, 1.0 - y));
        if w * (WIDTH as f32) < 32.0 || h * (HEIGHT as f32) < 24.0 {
            return Err("a tile: at least 32 × 24 pixels of the multiview".into());
        }
        Ok(TileSpec {
            kind,
            rect: (x, y, w, h),
            label: v["label"].as_bool().unwrap_or(true),
            tally: v["tally"].as_bool().unwrap_or(true),
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        let (kind, id) = match self.kind {
            TileKind::Program => ("program", None),
            TileKind::Preview => ("preview", None),
            TileKind::Input(i) => ("input", Some(i)),
            TileKind::Scene(i) => ("scene", Some(i)),
            TileKind::Mix(i) => ("mix", Some(i)),
        };
        let r = |x: f32| (x as f64 * 1e5).round() / 1e5;
        serde_json::json!({
            "kind": kind,
            "id": id,
            "rect": [r(self.rect.0), r(self.rect.1), r(self.rect.2), r(self.rect.3)],
            "label": self.label,
            "tally": self.tally,
        })
    }

    /// Its place in multiview pixels (x even, inside the multiview).
    pub fn pixels(&self) -> Rect {
        let (x, y, w, h) = self.rect;
        let px = ((x * WIDTH as f32).round() as usize).min(WIDTH - 32) & !1;
        let py = ((y * HEIGHT as f32).round() as usize).min(HEIGHT - 24);
        Rect {
            x: px,
            y: py,
            w: ((w * WIDTH as f32).round() as usize).clamp(32, WIDTH - px),
            h: ((h * HEIGHT as f32).round() as usize).clamp(24, HEIGHT - py),
        }
    }
}

/// A layout's tiles from the API's JSON list (at most [`MAX_TILES`]).
pub fn layout_from_json(v: &serde_json::Value) -> Result<Vec<TileSpec>, String> {
    let a = v.as_array().ok_or("tiles: a list")?;
    if a.len() > MAX_TILES {
        return Err(format!("a multiview has at most {MAX_TILES} tiles"));
    }
    a.iter()
        .enumerate()
        .map(|(k, t)| TileSpec::from_json(t).map_err(|e| format!("tile {}: {e}", k + 1)))
        .collect()
}

/// The layout used when none is set: preview and program large across the top, then a
/// tile for each of `inputs` (id, whether it is a scene) below, in order: 4 × 2 for up to
/// 8, 8 × 4 for up to 16, and smaller tiles (16:9) as there are more.
pub fn default_layout(inputs: &[(u32, bool)]) -> Vec<TileSpec> {
    let tile = |kind, rect| TileSpec {
        kind,
        rect,
        label: true,
        tally: true,
    };
    let mut out = vec![
        tile(TileKind::Preview, (0.0, 0.0, 0.5, 0.5)),
        tile(TileKind::Program, (0.5, 0.0, 0.5, 0.5)),
    ];
    let n = inputs.len();
    // The fewest columns whose 16:9 tiles' rows fit the bottom half.
    let (cols, rows) = [4usize, 8, 12, 16, 24, 32]
        .into_iter()
        .map(|c| (c, (HEIGHT / 2) / ((WIDTH / c) * 9 / 16)))
        .find(|(c, r)| c * r >= n)
        .unwrap_or((32, (HEIGHT / 2) / ((WIDTH / 32) * 9 / 16)));
    let (tw, th) = (
        1.0 / cols as f32,
        ((WIDTH / cols) * 9 / 16) as f32 / HEIGHT as f32,
    );
    for (k, &(id, scene)) in inputs.iter().take(cols * rows).enumerate() {
        let kind = if scene {
            TileKind::Scene(id)
        } else {
            TileKind::Input(id)
        };
        out.push(tile(
            kind,
            ((k % cols) as f32 * tw, 0.5 + (k / cols) as f32 * th, tw, th),
        ));
    }
    out
}

/// One tile as drawn: its place, its picture, its label strip's text (`None`: no strip)
/// and its border's colour by tally.
pub struct Drawn {
    pub rect: Rect,
    pub picture: Option<Arc<Frame>>,
    pub label: Option<String>,
    pub tally: Tally,
}

/// A tile's tally border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tally {
    /// Grey: neither (or the tile shows no tally).
    Off,
    /// Red: on air.
    Program,
    /// Green: on preview.
    Preview,
}

fn fill(f: &mut Frame, r: Rect, c: Ycc) {
    let x = r.x & !1;
    let w = (r.w + (r.x - x)).min(f.width - x);
    f.paste(&Frame::solid(w, r.h.min(f.height - r.y), c), x, r.y);
}

fn border(f: &mut Frame, r: Rect, t: usize, c: Ycc) {
    fill(f, Rect { h: t, ..r }, c);
    fill(
        f,
        Rect {
            y: r.y + r.h - t,
            h: t,
            ..r
        },
        c,
    );
    fill(f, Rect { w: t + 1, ..r }, c);
    fill(
        f,
        Rect {
            x: (r.x + r.w).saturating_sub(t + 1),
            w: t + 1,
            ..r
        },
        c,
    );
}

/// The size a picture is drawn at in tile `r` (inside its margin).
pub fn picture_size(r: Rect) -> (usize, usize) {
    ((r.w - 8) & !1, r.h - 8)
}

/// A picture in a tile: fitted inside a margin (unless already that size), with a label
/// strip at the bottom (unless `label` is `None`).
fn tile(f: &mut Frame, r: Rect, pic: Option<&Frame>, label: Option<&str>, frame: Option<Ycc>) {
    let m = 4;
    let inner = Rect {
        x: r.x + m,
        y: r.y + m,
        w: r.w - 2 * m,
        h: r.h - 2 * m,
    };
    let (pw, ph) = picture_size(r);
    match pic {
        Some(p) if (p.width, p.height) == (pw, ph) => f.paste(p, inner.x & !1, inner.y),
        Some(p) => f.paste(&p.fitted(pw, ph), inner.x & !1, inner.y),
        None => fill(f, inner, Ycc::BLACK),
    }
    let Some(label) = label else {
        border(f, r, m.min(3), frame.unwrap_or(GREY));
        return;
    };
    let scale = if r.h >= 300 {
        3
    } else if r.h >= 200 {
        2
    } else {
        1
    };
    let strip = 7 * scale + 2 * scale;
    let s = Rect {
        x: inner.x,
        y: inner.y + inner.h - strip,
        w: inner.w,
        h: strip,
    };
    fill(f, s, Ycc(30, 128, 128));
    font::draw(
        f,
        label,
        s.x + 2 * scale,
        s.y + scale,
        scale,
        235,
        s.w.saturating_sub(4 * scale),
    );
    border(f, r, m.min(3), frame.unwrap_or(GREY));
}

/// Draws the multiview: its tiles in order (a later one over an earlier).
pub fn render(tiles: &[Drawn]) -> Frame {
    let mut f = Frame::solid(WIDTH, HEIGHT, Ycc(20, 128, 128));
    for t in tiles {
        let colour = match t.tally {
            Tally::Program => Some(RED),
            Tally::Preview => Some(GREEN),
            Tally::Off => None,
        };
        tile(
            &mut f,
            t.rect,
            t.picture.as_deref(),
            t.label.as_deref(),
            colour,
        );
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_layout_grows_and_fits() {
        for n in [0usize, 3, 8, 9, 16, 17, 40, 100] {
            let ids: Vec<(u32, bool)> = (1..=n as u32).map(|i| (i, i % 3 == 0)).collect();
            let l = default_layout(&ids);
            assert_eq!(l.len(), 2 + n, "{n}");
            for t in &l {
                let r = t.pixels();
                assert!(r.x + r.w <= WIDTH && r.y + r.h <= HEIGHT, "{n}: {t:?}");
            }
            // No two input tiles overlap.
            for w in l[2..].windows(2) {
                let (ra, rb) = (w[0].pixels(), w[1].pixels());
                assert!(ra.x + ra.w <= rb.x || ra.y + ra.h <= rb.y, "{n}");
            }
        }
        // As before: 4 × 2 tiles of 480 × 270 up to 8, 8 × 4 of 240 × 135 up to 16.
        let ids: Vec<(u32, bool)> = (1..=8).map(|i| (i, false)).collect();
        assert_eq!(default_layout(&ids)[2].pixels().w, 480);
        assert_eq!(default_layout(&ids)[9].pixels().y, 810);
        let ids: Vec<(u32, bool)> = (1..=16).map(|i| (i, i == 16)).collect();
        let l = default_layout(&ids);
        assert_eq!((l[17].pixels().w, l[17].pixels().h), (240, 135));
        assert_eq!(l[17].kind, TileKind::Scene(16));
    }

    #[test]
    fn tiles_round_trip_and_take_their_colours() {
        let t = TileSpec {
            kind: TileKind::Mix(2),
            rect: (0.25, 0.5, 0.25, 0.25),
            label: false,
            tally: true,
        };
        assert_eq!(TileSpec::from_json(&t.to_json()).unwrap(), t);
        assert!(
            TileSpec::from_json(&serde_json::json!({"kind": "input", "rect": [0, 0, 1, 1]}))
                .is_err()
        );
        assert!(
            TileSpec::from_json(&serde_json::json!({"kind": "program", "rect": [0, 0, 0.001, 1]}))
                .is_err()
        );
        let red = Arc::new(Frame::solid(640, 360, Ycc::from_rgb(255, 0, 0)));
        let ids: Vec<(u32, bool)> = (1..=3).map(|i| (i, false)).collect();
        let l = default_layout(&ids);
        let drawn: Vec<Drawn> = l
            .iter()
            .enumerate()
            .map(|(k, t)| Drawn {
                rect: t.pixels(),
                picture: Some(red.clone()),
                label: (k != 4).then(|| format!("Cam {k}")),
                tally: match k {
                    2 => Tally::Program,
                    3 => Tally::Preview,
                    _ => Tally::Off,
                },
            })
            .collect();
        let f = render(&drawn);
        let at = |x: usize, y: usize| f.y[y * WIDTH + x];
        // The first input's tile: a red border, a red picture inside.
        let r = l[2].pixels();
        assert_eq!(at(r.x + 1, r.y + r.h / 2), RED.0);
        assert_eq!(at(r.x + r.w / 2, r.y + r.h / 3), Ycc::from_rgb(255, 0, 0).0);
        // The third: grey, and no label strip (the picture to its bottom).
        let r = l[4].pixels();
        assert_eq!(at(r.x + 1, r.y + r.h / 2), GREY.0);
        assert_eq!(at(r.x + r.w / 2, r.y + r.h - 6), Ycc::from_rgb(255, 0, 0).0);
    }
}
