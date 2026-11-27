//! Where the picture goes on a screen (the display service, idn-display.md §D.8): turned
//! for a screen hung on its side, kept inside a TV's overscan, fitted, filled or
//! stretched, and on a video wall cut to this screen's tile with the part behind the
//! bezels left out. Pure arithmetic: the renderer draws what [`place`] says, and the tests
//! check it without a window.

use serde::{Deserialize, Serialize};

/// How the picture meets the screen (or the whole wall).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
// (`fit` is the API's word, and `Fit::Fit` reads as it says.)
#[allow(clippy::enum_variant_names)]
pub enum Fit {
    /// All of it, its aspect kept, black bars where the shapes differ.
    #[default]
    Fit,
    /// The screen covered, its aspect kept, the edges that don't fit cut off.
    Fill,
    /// The screen covered, its aspect changed to the screen's.
    Stretch,
}

/// A gap between two screens' pictures, in millimetres: the bezels, both screens' together.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Gap {
    /// Between two screens side by side.
    #[serde(default)]
    pub h: f64,
    /// Between two screens one above the other.
    #[serde(default)]
    pub v: f64,
}

/// A screen's picture area in millimetres (its visible part, without the bezels).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SizeMm {
    pub w: f64,
    pub h: f64,
}

/// This screen's place in a video wall of identical screens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Wall {
    /// The wall's rows and columns of screens (1 to 16 each).
    pub rows: u32,
    pub cols: u32,
    /// This screen's row and column, from 0 (top left).
    pub row: u32,
    pub col: u32,
    /// The gaps between the screens' pictures (the bezels): the picture's part behind
    /// them is not shown, so lines run straight across. Needs `screen_mm`.
    #[serde(default)]
    pub bezel_mm: Gap,
    /// Each screen's picture area. Left out: the screen's pixels are the unit, no gaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_mm: Option<SizeMm>,
    /// The `sync/1` group the wall's screens present in step in (§E.14). Left out:
    /// `wall:` and the feed's name, the same on every screen of the wall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// How the picture is shown on one screen.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct View {
    /// Turned clockwise by 0, 90, 180 or 270 degrees (a screen hung on its side).
    #[serde(default)]
    pub rotation: u16,
    /// The part of each edge the screen hides (a TV's overscan), in % (0 to 20): the
    /// picture is kept inside the rest.
    #[serde(default)]
    pub overscan: f64,
    #[serde(default)]
    pub fit: Fit,
    /// This screen's tile of a video wall; none: the picture is this screen's alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall: Option<Wall>,
}

/// Where to draw: the part of the screen (`viewport`: x, y, width, height in the
/// screen's pixels) and the part of the turned picture shown there (`uv`: left, top,
/// right, bottom, 0 to 1, across and down the picture as turned), and the turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub viewport: [f32; 4],
    pub uv: [f32; 4],
    pub rotation: u16,
}

/// A rectangle: x, y, width, height.
type Rect = (f64, f64, f64, f64);

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.0.max(b.0), a.1.max(b.1));
    let (x1, y1) = ((a.0 + a.2).min(b.0 + b.2), (a.1 + a.3).min(b.1 + b.3));
    (x1 - x0 > 1e-9 && y1 - y0 > 1e-9).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// The most overscan taken, in %.
pub const MAX_OVERSCAN: f64 = 20.0;

/// Where `picture` (as received, width × height) goes on a screen of `surface` pixels as
/// `view` says. `None`: none of it on this screen (a tile the picture doesn't reach).
pub fn place(surface: (u32, u32), picture: (u32, u32), view: &View) -> Option<Placement> {
    let (sw, sh) = (surface.0.max(1) as f64, surface.1.max(1) as f64);
    let (pw, ph) = (picture.0.max(1) as f64, picture.1.max(1) as f64);
    let rotation = match view.rotation {
        90 | 180 | 270 => view.rotation,
        _ => 0,
    };
    let (rw, rh) = if rotation % 180 == 90 {
        (ph, pw)
    } else {
        (pw, ph)
    };
    // The wall in its own units (millimetres, or this screen's pixels), the screens and
    // the gaps between them; one screen alone is a wall of one.
    let (tw, th, gh, gv, cols, rows, col, row) = match &view.wall {
        Some(w) => {
            let (cols, rows) = (w.cols.max(1), w.rows.max(1));
            let (col, row) = (w.col.min(cols - 1), w.row.min(rows - 1));
            match w.screen_mm.filter(|s| s.w > 0.0 && s.h > 0.0) {
                Some(s) => (
                    s.w,
                    s.h,
                    w.bezel_mm.h.max(0.0),
                    w.bezel_mm.v.max(0.0),
                    cols,
                    rows,
                    col,
                    row,
                ),
                None => (sw, sh, 0.0, 0.0, cols, rows, col, row),
            }
        }
        None => (sw, sh, 0.0, 0.0, 1, 1, 0, 0),
    };
    let canvas = (
        cols as f64 * tw + (cols - 1) as f64 * gh,
        rows as f64 * th + (rows - 1) as f64 * gv,
    );
    // The picture on the wall.
    let p: Rect = match view.fit {
        Fit::Stretch => (0.0, 0.0, canvas.0, canvas.1),
        f => {
            let (kx, ky) = (canvas.0 / rw, canvas.1 / rh);
            let k = if f == Fit::Fit {
                kx.min(ky)
            } else {
                kx.max(ky)
            };
            let (w, h) = (rw * k, rh * k);
            ((canvas.0 - w) / 2.0, (canvas.1 - h) / 2.0, w, h)
        }
    };
    // This screen, and what its signal covers: the screen shows only the middle of its
    // signal (overscan), so the signal reaches past its edges by that much.
    let t: Rect = (col as f64 * (tw + gh), row as f64 * (th + gv), tw, th);
    let o = view.overscan.clamp(0.0, MAX_OVERSCAN) / 100.0;
    let (ex, ey) = (t.2 * o / (1.0 - 2.0 * o), t.3 * o / (1.0 - 2.0 * o));
    let s: Rect = (t.0 - ex, t.1 - ey, t.2 + 2.0 * ex, t.3 + 2.0 * ey);
    let i = intersect(p, s)?;
    let viewport = [
        ((i.0 - s.0) / s.2 * sw) as f32,
        ((i.1 - s.1) / s.3 * sh) as f32,
        (i.2 / s.2 * sw) as f32,
        (i.3 / s.3 * sh) as f32,
    ];
    let uv = [
        ((i.0 - p.0) / p.2) as f32,
        ((i.1 - p.1) / p.3) as f32,
        ((i.0 + i.2 - p.0) / p.2) as f32,
        ((i.1 + i.3 - p.1) / p.3) as f32,
    ];
    Some(Placement {
        viewport,
        uv,
        rotation,
    })
}

/// A point of the turned picture (`a` across, `b` down, 0 to 1) in the picture as
/// received (`u`, `v`): what the shader does (its test's mirror of it).
#[cfg(test)]
pub fn unturn(a: f32, b: f32, rotation: u16) -> (f32, f32) {
    match rotation {
        90 => (b, 1.0 - a),
        180 => (1.0 - a, 1.0 - b),
        270 => (1.0 - b, a),
        _ => (a, b),
    }
}

/// The `sync/1` group of a wall showing `feed` (§E.14): its own, or `wall:FEED`.
pub fn sync_group(wall: &Wall, feed: &str) -> String {
    match wall
        .group
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        Some(g) => g.to_owned(),
        None => format!("wall:{feed}"),
    }
}

/// Why a view can't be used, if so (the API's check).
pub fn check(v: &View) -> Result<(), String> {
    if !matches!(v.rotation, 0 | 90 | 180 | 270) {
        return Err(format!("rotation {}: 0, 90, 180 or 270", v.rotation));
    }
    if !(0.0..=MAX_OVERSCAN).contains(&v.overscan) || v.overscan.is_nan() {
        return Err(format!("overscan {}: 0 to {MAX_OVERSCAN} (%)", v.overscan));
    }
    if let Some(w) = &v.wall {
        if !(1..=16).contains(&w.rows) || !(1..=16).contains(&w.cols) {
            return Err("wall: 1 to 16 rows and columns".into());
        }
        if w.row >= w.rows || w.col >= w.cols {
            return Err(format!(
                "wall: row {} and column {} are counted from 0, inside {} × {}",
                w.row, w.col, w.rows, w.cols
            ));
        }
        let mm = |x: f64| x.is_finite() && (0.0..=10_000.0).contains(&x);
        if !mm(w.bezel_mm.h) || !mm(w.bezel_mm.v) {
            return Err("wall: bezel_mm 0 to 10000".into());
        }
        match w.screen_mm {
            Some(s) if !(mm(s.w) && mm(s.h) && s.w > 0.0 && s.h > 0.0) => {
                return Err("wall: screen_mm, each above 0".into());
            }
            None if w.bezel_mm.h > 0.0 || w.bezel_mm.v > 0.0 => {
                return Err("wall: bezel_mm needs screen_mm (the screens' picture size)".into());
            }
            _ => {}
        }
        if let Some(g) = &w.group
            && g.chars().count() > idn_open_protocol::sync::MAX_GROUP
        {
            return Err(format!(
                "wall: group at most {} characters",
                idn_open_protocol::sync::MAX_GROUP
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01)
    }

    #[test]
    fn fit_fill_and_stretch() {
        let v = View::default();
        // Fitted: as the renderer always did (16:9 on 4:3: bars above and below).
        let p = place((1024, 768), (1920, 1080), &v).unwrap();
        assert!(close(p.viewport, [0.0, 96.0, 1024.0, 576.0]), "{p:?}");
        assert!(close(p.uv, [0.0, 0.0, 1.0, 1.0]));
        // Filled: 4:3 on 16:9, the whole screen, the top and bottom eighth cut.
        let fill = View {
            fit: Fit::Fill,
            ..Default::default()
        };
        let p = place((1920, 1080), (1440, 1080), &fill).unwrap();
        assert!(close(p.viewport, [0.0, 0.0, 1920.0, 1080.0]), "{p:?}");
        assert!(close(p.uv, [0.0, 0.125, 1.0, 0.875]), "{p:?}");
        // Stretched: everything, everywhere.
        let st = View {
            fit: Fit::Stretch,
            ..Default::default()
        };
        let p = place((1920, 1080), (1440, 1080), &st).unwrap();
        assert!(close(p.viewport, [0.0, 0.0, 1920.0, 1080.0]));
        assert!(close(p.uv, [0.0, 0.0, 1.0, 1.0]));
    }

    #[test]
    fn turned_pictures_fill_a_screen_on_its_side() {
        // A landscape picture turned for a portrait screen: it fills it.
        let v = View {
            rotation: 90,
            ..Default::default()
        };
        let p = place((1080, 1920), (1920, 1080), &v).unwrap();
        assert!(close(p.viewport, [0.0, 0.0, 1080.0, 1920.0]), "{p:?}");
        assert_eq!(p.rotation, 90);
        // Turned clockwise: the picture's top left corner is at the screen's top right.
        assert_eq!(unturn(1.0, 0.0, 90), (0.0, 0.0));
        assert_eq!(unturn(1.0, 1.0, 90), (1.0, 0.0));
        assert_eq!(unturn(0.0, 1.0, 270), (0.0, 0.0));
        assert_eq!(unturn(0.0, 0.0, 180), (1.0, 1.0));
        assert_eq!(unturn(0.3, 0.6, 0), (0.3, 0.6));
        // Not turned, the same landscape picture on that screen: bars above and below.
        let p = place((1080, 1920), (1920, 1080), &View::default()).unwrap();
        assert!(close(p.viewport, [0.0, 656.25, 1080.0, 607.5]), "{p:?}");
    }

    #[test]
    fn overscan_keeps_the_picture_inside_what_the_screen_shows() {
        let v = View {
            overscan: 5.0,
            ..Default::default()
        };
        let p = place((1920, 1080), (1920, 1080), &v).unwrap();
        // 5% of each edge is hidden: the picture in the middle 90%.
        assert!(close(p.viewport, [96.0, 54.0, 1728.0, 972.0]), "{p:?}");
        assert!(close(p.uv, [0.0, 0.0, 1.0, 1.0]));
    }

    #[test]
    fn a_wall_cuts_the_picture_into_tiles_without_the_part_behind_the_bezels() {
        // 2 × 2 screens of 1000 × 562.5 mm, 10 mm between pictures; a 16:9 picture.
        let wall = |row, col| View {
            wall: Some(Wall {
                rows: 2,
                cols: 2,
                row,
                col,
                bezel_mm: Gap { h: 10.0, v: 10.0 },
                screen_mm: Some(SizeMm {
                    w: 1000.0,
                    h: 562.5,
                }),
                group: None,
            }),
            fit: Fit::Stretch,
            ..Default::default()
        };
        // Stretched over the wall's 2010 × 1135 mm.
        let tl = place((1920, 1080), (3840, 2160), &wall(0, 0)).unwrap();
        let tr = place((1920, 1080), (3840, 2160), &wall(0, 1)).unwrap();
        let bl = place((1920, 1080), (3840, 2160), &wall(1, 0)).unwrap();
        for p in [tl, tr, bl] {
            assert!(close(p.viewport, [0.0, 0.0, 1920.0, 1080.0]), "{p:?}");
        }
        let (w, h) = (1000.0 / 2010.0, 562.5 / 1135.0);
        assert!(close(tl.uv, [0.0, 0.0, w, h]), "{tl:?}");
        // The 10 mm behind the bezels is skipped: the next tile starts 10 mm on.
        assert!(close(tr.uv, [1010.0 / 2010.0, 0.0, 1.0, h]), "{tr:?}");
        assert!(close(bl.uv, [0.0, 572.5 / 1135.0, w, 1.0]), "{bl:?}");
        assert!((tr.uv[0] - tl.uv[2] - 10.0 / 2010.0).abs() < 1e-4);
        // Fitted: a 21:9 picture on the 16:9 wall leaves bands; the top row's tiles
        // start below the band.
        let mut v = wall(0, 0);
        v.fit = Fit::Fit;
        let p = place((1920, 1080), (2520, 1080), &v).unwrap();
        let ph = 2010.0 * 1080.0 / 2520.0;
        let top = (1135.0 - ph) / 2.0;
        assert!(
            close(
                p.viewport,
                [
                    0.0,
                    (top / 562.5 * 1080.0) as f32,
                    1920.0,
                    ((562.5 - top) / 562.5 * 1080.0) as f32
                ]
            ),
            "{p:?}"
        );
        // Without the screens' size, pixels are the unit and there are no gaps.
        let mut v = wall(0, 1);
        if let Some(w) = &mut v.wall {
            w.screen_mm = None;
            w.bezel_mm = Gap::default();
        }
        let p = place((1920, 1080), (3840, 2160), &v).unwrap();
        assert!(close(p.uv, [0.5, 0.0, 1.0, 0.5]), "{p:?}");
    }

    #[test]
    fn views_are_checked() {
        assert!(check(&View::default()).is_ok());
        let bad = |f: &dyn Fn(&mut View)| {
            let mut v = View {
                wall: Some(Wall {
                    rows: 2,
                    cols: 3,
                    row: 1,
                    col: 2,
                    bezel_mm: Gap::default(),
                    screen_mm: None,
                    group: None,
                }),
                ..Default::default()
            };
            assert!(check(&v).is_ok());
            f(&mut v);
            check(&v).is_err()
        };
        assert!(bad(&|v| v.rotation = 45));
        assert!(bad(&|v| v.overscan = 25.0));
        assert!(bad(&|v| v.overscan = f64::NAN));
        assert!(bad(&|v| v.wall.as_mut().unwrap().row = 2));
        assert!(bad(&|v| v.wall.as_mut().unwrap().cols = 0));
        assert!(bad(&|v| v.wall.as_mut().unwrap().bezel_mm.h = 5.0));
        assert!(bad(
            &|v| v.wall.as_mut().unwrap().group = Some("x".repeat(200))
        ));
        let w = Wall {
            rows: 1,
            cols: 2,
            row: 0,
            col: 0,
            bezel_mm: Gap::default(),
            screen_mm: None,
            group: None,
        };
        assert_eq!(sync_group(&w, "BOOTH (Program)"), "wall:BOOTH (Program)");
        assert_eq!(
            sync_group(
                &Wall {
                    group: Some("lobby".into()),
                    ..w
                },
                "x"
            ),
            "lobby"
        );
    }
}
