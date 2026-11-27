//! Rules that lived in the switcher's panel and are the API's now (the UI is the
//! client's): the title styles built in, a title's summary, what is this switcher's own
//! output, times as "m:ss", "folder or files", the show's version, a stream's sent-rate
//! history, an ISO recording's default name and which slideshow or list `slide` moves.

use serde_json::{Map, Value, json};
use std::collections::VecDeque;

/// The title styles built in: name, label for a title made from it, and what it is.
pub const BUILT_IN_TITLES: &[(&str, &str, &str)] = &[
    (
        "lower",
        "Title",
        "A lower third: a box and a line of bold text",
    ),
    (
        "two",
        "Title",
        "A lower third with two lines: {name} and {role}",
    ),
    ("caption", "Title", "A caption across the bottom, outlined"),
    ("centre", "Title", "Large text in the centre"),
    ("corner", "Title", "Small text in the top-left corner"),
    ("clock", "Clock", "The time of day, top right"),
    (
        "count",
        "Countdown",
        "Text and the \"countdown\" timer, in the centre",
    ),
    (
        "score",
        "Scoreboard",
        "A scoreboard: home and away, their names and scores",
    ),
];

/// The styles that need no text (a clock, a countdown, a scoreboard).
fn needs_no_text(style: &str) -> bool {
    matches!(style, "clock" | "count" | "score")
}

/// A built-in style's title (`{"layers", "fields"}`) with `text` in it, or `None` if no
/// style has that name.
pub fn builtin_title(style: &str, text: &str) -> Option<Value> {
    if !BUILT_IN_TITLES.iter().any(|b| b.0 == style) {
        return None;
    }
    let t = |rect: [f64; 4], more: Value| {
        let mut l = json!({"type": "text", "rect": rect, "text": text, "size": 0.05,
            "colour": "#ffffff", "align": "left", "valign": "middle", "wrap": true});
        if let (Some(l), Value::Object(m)) = (l.as_object_mut(), more) {
            l.extend(m);
        }
        l
    };
    let shadow = json!({"offset": [0.04, 0.04], "blur": 0.1, "colour": "#000000b4"});
    let bx = |rect: [f64; 4], colour: &str| json!({"type": "box", "rect": rect, "colour": colour, "radius": 0.1});
    let (layers, fields) = match style {
        "two" => (
            vec![
                bx([0.05, 0.74, 0.5, 0.16], "#000000b0"),
                t(
                    [0.07, 0.75, 0.46, 0.08],
                    json!({"text": "{name}", "bold": true, "valign": "bottom"}),
                ),
                t(
                    [0.07, 0.83, 0.46, 0.06],
                    json!({"text": "{role}", "size": 0.035, "colour": "#d8e4ff", "valign": "top"}),
                ),
            ],
            json!({"name": text, "role": "Role"}),
        ),
        "caption" => (
            vec![t(
                [0.1, 0.78, 0.8, 0.16],
                json!({"align": "centre", "outline": 0.06, "shadow": shadow}),
            )],
            json!({}),
        ),
        "centre" => (
            vec![t(
                [0.1, 0.3, 0.8, 0.4],
                json!({"size": 0.08, "align": "centre", "shadow": shadow}),
            )],
            json!({}),
        ),
        "corner" => (
            vec![t(
                [0.03, 0.04, 0.45, 0.08],
                json!({"size": 0.04, "shadow": shadow}),
            )],
            json!({}),
        ),
        "clock" => (
            vec![t(
                [0.75, 0.04, 0.22, 0.08],
                json!({"text": "{clock}", "size": 0.045, "align": "right", "shadow": shadow}),
            )],
            json!({}),
        ),
        "count" => {
            let lead = if text.is_empty() { "Starting in" } else { text };
            (
                vec![t(
                    [0.1, 0.35, 0.8, 0.3],
                    json!({"text": format!("{lead} {{timer:countdown}}"),
                    "size": 0.08, "align": "centre", "shadow": shadow}),
                )],
                json!({}),
            )
        }
        "score" => (
            vec![
                bx([0.03, 0.04, 0.36, 0.07], "#101820d0"),
                t(
                    [0.04, 0.04, 0.12, 0.07],
                    json!({"text": "{home_name}", "size": 0.035, "bold": true}),
                ),
                t(
                    [0.16, 0.04, 0.09, 0.07],
                    json!({"text": "{home} – {away}", "size": 0.04, "bold": true, "align": "centre"}),
                ),
                t(
                    [0.26, 0.04, 0.12, 0.07],
                    json!({"text": "{away_name}", "size": 0.035, "bold": true, "align": "right"}),
                ),
            ],
            json!({"home_name": "HOME", "away_name": "AWAY", "home": "0", "away": "0"}),
        ),
        // "lower"
        _ => (
            vec![
                bx([0.05, 0.76, 0.5, 0.14], "#000000b0"),
                t([0.07, 0.76, 0.46, 0.14], json!({"bold": true})),
            ],
            json!({}),
        ),
    };
    Some(json!({"layers": layers, "fields": fields}))
}

/// The built-in styles as `GET /api/v1/title_templates` lists them: by name, each
/// `{"layers", "fields", "built_in": true, "label", "about"}` (a sample text in it).
pub fn builtin_templates() -> Map<String, Value> {
    BUILT_IN_TITLES
        .iter()
        .filter_map(|(name, label, about)| {
            let sample = if needs_no_text(name) { "" } else { "Title" };
            let mut t = builtin_title(name, sample)?;
            t["built_in"] = json!(true);
            t["label"] = json!(label);
            t["about"] = json!(about);
            Some(((*name).to_owned(), t))
        })
        .collect()
}

/// A title from a template by name (`saved` first, then the built-in styles) with `text`:
/// its `{"layers", "fields"}` and the label a new input made from it gets (`text`, else
/// the style's label or the template's name; at most 24 characters). A saved template
/// takes `text` where it has one place for it: its one field, or (no fields) its one
/// text layer with no `{…}` in it; with more than one, `text` is the label only (set
/// the rest with `title_fields`).
pub fn template_title(
    name: &str,
    text: &str,
    saved: &Map<String, Value>,
) -> Result<(Value, String), String> {
    let text = text.trim();
    let (title, label) = if let Some(t) = saved.get(name) {
        let mut title = json!({"layers": t["layers"], "fields": t["fields"]});
        if !text.is_empty() {
            fill_text(&mut title, text);
        }
        (title, name.to_owned())
    } else if let Some(&(_, label, _)) = BUILT_IN_TITLES.iter().find(|b| b.0 == name) {
        if text.is_empty() && !needs_no_text(name) {
            return Err(format!(
                "text: the title's text (the {name} style shows it)"
            ));
        }
        (
            builtin_title(name, text).unwrap_or_default(),
            label.to_owned(),
        )
    } else {
        return Err(format!("no title template {name:?}"));
    };
    let label = if text.is_empty() {
        label
    } else {
        text.to_owned()
    };
    Ok((title, cut(&label, 24)))
}

/// A saved template's one place for text filled with `text`: its one field, or (no
/// fields) its one text layer that has no `{…}` in it; nothing if it has more than one.
fn fill_text(title: &mut Value, text: &str) {
    if let Some(f) = title["fields"].as_object_mut()
        && !f.is_empty()
    {
        if f.len() == 1
            && let Some(v) = f.values_mut().next()
        {
            *v = json!(text);
        }
        return;
    }
    let mut texts = title["layers"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .filter(|l| l["type"] == "text");
    if let (Some(l), None) = (texts.next(), texts.next())
        && l["text"].as_str().is_none_or(|t| !t.contains('{'))
    {
        l["text"] = json!(text);
    }
}

/// `s` cut at `n` characters, with "…" when it was longer.
pub fn cut(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    } else {
        s.to_owned()
    }
}

/// A title's line for a list: its first text layer's text with its fields filled in
/// (`{name}` as the field's value), at most 40 characters; `""` if it has none.
pub fn title_summary(title: &Value) -> String {
    let Some(text) = title["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|l| l["type"] == "text" && l["text"].as_str().is_some_and(|t| !t.is_empty()))
        .and_then(|l| l["text"].as_str())
    else {
        return String::new();
    };
    let fields = &title["fields"];
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let filled = after.find('}').and_then(|j| {
            let key = after[..j].trim();
            let ok =
                !key.is_empty() && !key.contains(['{', ':']) && !key.contains(char::is_whitespace);
            let value = fields.get(key).filter(|_| ok).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                v => Some(v.to_string()),
            })?;
            Some((value, j))
        });
        match filled {
            Some((value, j)) => {
                out.push_str(&value);
                rest = &after[j + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    cut(&out, 40)
}

/// Whether a network source is this switcher's own output: one of `outputs` (full names),
/// or `HOST (NAME)` / `HOST (NAME (…))` for the switcher's `name` on this machine (`host`,
/// any case): another machine's switcher of the same name is a source like any other.
pub fn own_source(source: &str, name: &str, host: &str, outputs: &[String]) -> bool {
    if outputs.iter().any(|o| o == source) {
        return true;
    }
    let Some((from, inner)) = source.split_once(" (") else {
        return false;
    };
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    from.eq_ignore_ascii_case(host) && (inner == name || inner.starts_with(&format!("{name} (")))
}

/// A length in seconds: a number, or text as `"m:ss"`, `"h:mm:ss"` or seconds (`"90"`).
pub fn seconds_of(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let s = v.as_str()?.trim();
    let parts: Vec<&str> = s.split(':').map(str::trim).collect();
    let num = |p: &str| p.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0);
    match parts.as_slice() {
        [n] => num(n),
        [m, s] => Some(num(m)? * 60.0 + num(s).filter(|s| *s < 60.0)?),
        [h, m, s] => Some(
            num(h)? * 3600.0
                + num(m).filter(|m| *m < 60.0)? * 60.0
                + num(s).filter(|s| *s < 60.0)?,
        ),
        _ => None,
    }
}

/// `"a.png | b.png"` or `"C:\Pictures"`: the files (split at `|`), or the folder when it is
/// one name that isn't a file (`is_file` tells, given that one name: a file's name by its
/// extension, after a folder of that name).
pub fn split_from(s: &str, is_file: impl Fn(&str) -> bool) -> (Vec<String>, Option<String>) {
    let files: Vec<String> = s
        .split('|')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    if files.len() > 1 || files.first().is_some_and(|f| is_file(f)) {
        (files, None)
    } else {
        (Vec::new(), files.into_iter().next())
    }
}

/// A name with an extension of 2–4 letters or digits (a media file, not a folder).
pub fn has_extension(p: &str) -> bool {
    p.rsplit_once('.').is_some_and(|(_, e)| {
        (2..=4).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// The show's version: a hash of its content (what `show_set`'s and the cue edits'
/// `if_version` is checked against), at most 2^53 (exact in JavaScript).
pub fn show_version(show: &crate::show::Show) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in show.to_json().to_string().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h & ((1 << 53) - 1)
}

/// How far back a stream's sent-rate history goes, in seconds.
pub const HISTORY_SECS: u64 = 120;

/// A sample of a stream's sent rate (`at` in seconds), the history kept to its last
/// [`HISTORY_SECS`] (and at most that many samples).
pub fn push_sample(h: &mut VecDeque<(u64, u32)>, at: u64, kbps: u32) {
    h.push_back((at, kbps));
    while h
        .front()
        .is_some_and(|(t, _)| at.saturating_sub(*t) >= HISTORY_SECS)
        || h.len() > HISTORY_SECS as usize
    {
        h.pop_front();
    }
}

/// Whether a destination is a stream (rtmp://, rtmps://, srt://) rather than a recording.
pub fn is_stream_target(target: &str) -> bool {
    ["rtmp:", "rtmps:", "srt:"]
        .iter()
        .any(|p| target.starts_with(p))
}

/// An ISO recording's file when none is given: the input's label (characters other than
/// letters, digits, `.`, `_` and `-` as `_`) and `.mp4`; a name Windows keeps for a
/// device (`Aux`, `CON`, `nul`, `COM1`…) gets `_` after it (`Aux_.mp4`).
pub fn iso_file_name(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    if out.trim_matches('_').is_empty() {
        out = "input".into();
    }
    // Windows reads the stem before the first dot (trailing dots and spaces dropped).
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        let at = out.find('.').unwrap_or(out.len());
        out.insert(at, '_');
    }
    format!("{out}.mp4")
}

/// The slideshow or list `slide` moves: the one on program (in the scene there), else on
/// preview, else the first; its id and whether it is a list (`lists`: the list inputs' ids).
pub fn pick_slides(state: &Value, lists: &[u32]) -> Option<(u32, bool)> {
    let is_list = |i: &Value| {
        i["id"]
            .as_u64()
            .is_some_and(|id| lists.contains(&(id as u32)))
    };
    let shows: Vec<&Value> = state["inputs"]
        .as_array()?
        .iter()
        .filter(|i| i["source"]["slideshow"].is_object() || is_list(i))
        .collect();
    // On program or preview: itself, or in the scene there (its tally, §S.23).
    let on = |k: &str| shows.iter().find(|i| i[k] == true);
    let pick = on("program").or_else(|| on("preview")).or(shows.first())?;
    Some((pick["id"].as_u64()? as u32, is_list(pick)))
}

/// A virtual shot's crop as `[x, y, width, height]` (fractions of its source): the
/// engine's [`crate::View::crop`] of the view `v` (`{"x", "y", "zoom"}`) on a source of
/// `sw` × `sh` (16 × 9 when not known yet) for an output of `w` × `h`.
pub fn shot_crop(v: &Value, sw: usize, sh: usize, w: usize, h: usize) -> Value {
    let f = |k: &str, d: f64| v[k].as_f64().unwrap_or(d) as f32;
    let view = crate::View {
        x: f("x", 0.5),
        y: f("y", 0.5),
        zoom: f("zoom", 1.0),
    };
    let (sw, sh) = if sw == 0 || sh == 0 {
        (16, 9)
    } else {
        (sw, sh)
    };
    let (x0, y0, x1, y1) = view.crop(sw, sh, w, h);
    let r = |x: f64| (x * 1e4).round() / 1e4;
    json!([r(x0), r(y0), r(x1 - x0), r(y1 - y0)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_styles_are_titles() {
        let all = builtin_templates();
        assert_eq!(all.len(), 8);
        for (name, t) in &all {
            crate::title::Title::from_json(t).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(t["built_in"], true);
        }
        let none = Map::new();
        let (t, label) = template_title("count", "", &none).unwrap();
        assert!(t.to_string().contains("Starting in {timer:countdown}"));
        assert_eq!(label, "Countdown");
        let (t, label) =
            template_title("two", "A very long name for a lower third", &none).unwrap();
        assert_eq!(t["fields"]["name"], "A very long name for a lower third");
        assert_eq!(label, "A very long name for a l…");
        assert!(template_title("lower", " ", &none).is_err(), "text needed");
        assert!(template_title("nope", "x", &none).is_err());
        // A saved one of the same name comes first.
        let mut saved = Map::new();
        saved.insert("lower".into(), json!({"layers": [], "fields": {"a": "1"}}));
        let (t, label) = template_title("lower", "", &saved).unwrap();
        assert_eq!(
            (t["fields"]["a"].as_str(), label.as_str()),
            (Some("1"), "lower")
        );
        // A saved one takes the text in its one field, or its one plain text layer.
        let (t, label) = template_title("lower", "Ann", &saved).unwrap();
        assert_eq!(
            (t["fields"]["a"].as_str(), label.as_str()),
            (Some("Ann"), "Ann")
        );
        saved.insert(
            "plain".into(),
            json!({"layers": [{"type": "box"}, {"type": "text", "text": "Hello"}], "fields": {}}),
        );
        let (t, _) = template_title("plain", "Bye", &saved).unwrap();
        assert_eq!(t["layers"][1]["text"], "Bye");
        // Two fields, or a layer with a field in it: the label only.
        saved.insert(
            "two".into(),
            json!({"layers": [], "fields": {"a": "1", "b": "2"}}),
        );
        let (t, label) = template_title("two", "Ann", &saved).unwrap();
        assert_eq!(
            (t["fields"]["a"].as_str(), label.as_str()),
            (Some("1"), "Ann")
        );
        saved.insert(
            "clock".into(),
            json!({"layers": [{"type": "text", "text": "{clock}"}]}),
        );
        let (t, _) = template_title("clock", "x", &saved).unwrap();
        assert_eq!(t["layers"][0]["text"], "{clock}");
    }

    #[test]
    fn a_titles_summary_fills_its_fields() {
        let (t, _) = template_title("two", "Ann", &Map::new()).unwrap();
        assert_eq!(title_summary(&t), "Ann");
        let t = json!({"layers": [{"type": "box"}, {"type": "text", "text": "{ home } – {away} {x:y} {gone}"}],
            "fields": {"home": 2, "away": "1"}});
        assert_eq!(title_summary(&t), "2 – 1 {x:y} {gone}");
        let long = json!({"layers": [{"type": "text", "text": "x".repeat(50)}]});
        assert_eq!(title_summary(&long).chars().count(), 41);
        assert_eq!(title_summary(&json!({"layers": []})), "");
    }

    #[test]
    fn own_outputs_are_known() {
        let outs = vec!["PC (Desk (Program))".to_owned()];
        assert!(own_source("PC (Desk (Program))", "Desk", "PC", &outs));
        assert!(own_source("PC (Desk)", "Desk", "pc", &[]));
        assert!(own_source("PC (Desk (Preview))", "Desk", "PC", &[]));
        assert!(!own_source("PC (Desktop)", "Desk", "PC", &[]));
        assert!(!own_source("PC (Camera 1)", "Desk", "PC", &[]));
        // Another machine's switcher of the same name.
        assert!(!own_source("LOBBY (Desk)", "Desk", "PC", &[]));
        assert!(!own_source("LOBBY (Desk (Program))", "Desk", "PC", &outs));
    }

    #[test]
    fn times_and_paths() {
        assert_eq!(seconds_of(&json!("5:00")), Some(300.0));
        assert_eq!(seconds_of(&json!("1:02:03")), Some(3723.0));
        assert_eq!(seconds_of(&json!(90)), Some(90.0));
        assert_eq!(seconds_of(&json!("90")), Some(90.0));
        assert_eq!(seconds_of(&json!("5:75")), None);
        assert_eq!(seconds_of(&json!("x")), None);
        let pic = |p: &str| super::super::is_picture(p);
        assert_eq!(
            split_from("C:\\Pics", pic),
            (vec![], Some("C:\\Pics".into()))
        );
        assert_eq!(split_from("a.png", pic), (vec!["a.png".to_owned()], None));
        assert_eq!(
            split_from(" a.png | ", pic),
            (vec!["a.png".to_owned()], None)
        );
        assert_eq!(split_from("|", pic), (vec![], None));
        assert_eq!(
            split_from(" a.png | b | ", pic),
            (vec!["a.png".to_owned(), "b".to_owned()], None)
        );
        assert!(has_extension("x/clip.mp4") && !has_extension("Videos") && !has_extension("a.b c"));
        assert_eq!(iso_file_name("Camera 1 / wide"), "Camera_1_wide.mp4");
        assert_eq!(iso_file_name("..."), "....mp4");
        assert_eq!(iso_file_name("★"), "input.mp4");
        assert_eq!(iso_file_name("Aux"), "Aux_.mp4");
        assert_eq!(iso_file_name("com1"), "com1_.mp4");
        assert_eq!(iso_file_name("Nul.x"), "Nul_.x.mp4");
        assert_eq!(iso_file_name("COM10"), "COM10.mp4");
        assert_eq!(iso_file_name("Auxiliary"), "Auxiliary.mp4");
        assert!(is_stream_target("rtmps://a/b") && is_stream_target("srt://x:1"));
        assert!(!is_stream_target("show.mp4"));
    }

    #[test]
    fn history_is_two_minutes() {
        let mut h = VecDeque::new();
        for t in (0..400).step_by(2) {
            push_sample(&mut h, t, t as u32);
        }
        assert_eq!(h.len(), 60);
        assert_eq!(h.front().unwrap().0, 400 - 2 - 118);
        // Samples closer than 2 s: still at most 120.
        for t in 0..500 {
            push_sample(&mut h, 1000, t);
        }
        assert_eq!(h.len(), 120);
    }

    #[test]
    fn slide_picks_program_then_preview_then_first() {
        // (Each input's tally: shown on program or preview through the scene there.)
        let st = json!({"inputs": [
            {"id": 1, "source": {"colour": "#000000"}},
            {"id": 2, "source": {"slideshow": {}}, "preview": true},
            {"id": 3, "source": {"network": "x"}, "program": true},
            {"id": 4, "source": {"network": "list"}},
        ]});
        assert_eq!(pick_slides(&st, &[4]), Some((2, false)));
        let mut st2 = st.clone();
        st2["inputs"][3]["program"] = json!(true);
        assert_eq!(pick_slides(&st2, &[4]), Some((4, true)));
        st2["inputs"][3]["program"] = json!(false);
        st2["inputs"][1]["preview"] = json!(false);
        assert_eq!(pick_slides(&st2, &[4]), Some((2, false)));
        assert_eq!(pick_slides(&json!({"inputs": []}), &[]), None);
    }

    #[test]
    fn shot_crop_is_the_engines() {
        // A 16:9 source on a 16:9 output at zoom 2, centred: the middle half.
        let c = shot_crop(
            &json!({"x": 0.5, "y": 0.5, "zoom": 2.0}),
            1920,
            1080,
            1280,
            720,
        );
        assert_eq!(c, json!([0.25, 0.25, 0.5, 0.5]));
        // Pushed into the picture at its edge.
        let c = shot_crop(&json!({"x": 0.0, "y": 1.0, "zoom": 2.0}), 0, 0, 1280, 720);
        assert_eq!(c, json!([0.0, 0.5, 0.5, 0.5]));
    }

    #[test]
    fn the_show_version_follows_its_content() {
        let mut s = crate::show::Show::default();
        let a = show_version(&s);
        s.name = "Sunday".into();
        assert_ne!(a, show_version(&s));
        assert!(show_version(&s) < 1 << 53);
    }
}
