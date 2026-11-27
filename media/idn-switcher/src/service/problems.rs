//! Failure states as typed events (§S.37, idn-node.md §N.41): each second the switcher's
//! state is compared with the last one, and what changed for the worse (or back) is an
//! event on the push channel (`{"type": "event", "event", …}`) and, rate-limited, a line in
//! the Site Manager's log through the Node; what is wrong now is the state's `problems`.
//!
//! - Inputs (network sources in use): `lost` (with `reason` and `cause`), `reconnecting`
//!   (each new `attempt`), `recovered`, `other_site` (the source belongs to another site),
//!   `gone` (no longer listed on the network for 10 s), `renamed` (listed again under
//!   another name at the same address), `format_mismatch` (its size or frame rate isn't
//!   the program's).
//! - PTZ: `camera_unreachable`, and the same with `over` once it answers.
//! - Sound: `desk_link_lost` (a desk whose feed is taken stopped answering), `desk_link`
//!   (back).
//! - The engine: `overloaded` (`part` `compositor`: frames late; `encoder`: pictures
//!   skipped), the same with `over`.
//! - Streams and recordings (streams.rs): `stream_auth_expired`, `stream_failed`,
//!   `dropped_frames`.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// A source not listed for this long is gone.
pub const GONE_AFTER: Duration = Duration::from_secs(10);
/// Late frames within a round (1 s) that make the compositor overloaded.
pub const LATE_PER_ROUND: u64 = 3;
/// Overloaded ends after this long without late frames or skipped pictures.
pub const CALM_AFTER: Duration = Duration::from_secs(10);
/// A frame rate this far (as a share) from the program's is a mismatch.
pub const FPS_TOLERANCE: f64 = 0.15;

/// One event: its name, level, text and fields.
pub type Ev = (&'static str, &'static str, String, Value);

/// What a stream's error says, typed: `auth_expired` when the platform refused the
/// stream's key or login (an RTMP or SRT refusal, HTTP 401/403), else `None`.
pub fn stream_reason(error: &str) -> Option<&'static str> {
    let e = error.to_ascii_lowercase();
    const AUTH: &[&str] = &[
        "401",
        "403",
        "unauthorized",
        "unauthorised",
        "forbidden",
        "authentication",
        "auth failed",
        "not authorized",
        "access denied",
        "invalid stream key",
        "invalid key",
        "stream key",
        "badname",
        "badauth",
        "bad secret",
        "badsecret",
        "wrong passphrase",
        "expired",
    ];
    AUTH.iter().any(|k| e.contains(k)).then_some("auth_expired")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The network inputs of a state, by id.
fn network_inputs(st: &Value) -> BTreeMap<u64, &Value> {
    st["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["source"].get("network").is_some())
        .filter_map(|i| Some((i["id"].as_u64()?, i)))
        .collect()
}

fn label(i: &Value) -> String {
    match s(i, "label") {
        "" => i["source"]["network"].as_str().unwrap_or("").to_owned(),
        l => l.to_owned(),
    }
}

/// An input's format against the program's: its size or frame rate differ.
fn format_mismatch(i: &Value, st: &Value) -> Option<Value> {
    if i["connected"] != true {
        return None;
    }
    let (w, h) = (i["width"].as_u64()?, i["height"].as_u64()?);
    if w == 0 || h == 0 {
        return None;
    }
    let f = &st["format"];
    let (cw, ch) = (f["width"].as_u64()?, f["height"].as_u64()?);
    let fps = f["fps"]
        .as_array()
        .and_then(|a| Some(a.first()?.as_f64()? / a.get(1)?.as_f64()?.max(1.0)))?;
    let in_fps = i["fps"].as_f64().unwrap_or(0.0);
    let rate_off = in_fps > 0.0 && ((in_fps - fps).abs() / fps) > FPS_TOLERANCE;
    ((w, h) != (cw, ch) || rate_off).then(|| {
        json!({"size": [w, h], "fps": in_fps, "program": {"size": [cw, ch], "fps": fps},
               "rate": rate_off})
    })
}

/// The events from one state to the next (inputs, cameras, the desks' feeds).
pub fn transitions(before: &Value, now: &Value) -> Vec<Ev> {
    let mut out: Vec<Ev> = Vec::new();
    let (b, n) = (network_inputs(before), network_inputs(now));
    for (id, i) in &n {
        let name = label(i);
        let was = b.get(id);
        let lost_now = i["lost"].as_str();
        let lost_before = was.and_then(|w| w["lost"].as_str());
        if let (Some(why), None) = (lost_now, lost_before) {
            let cause = idn_open_protocol::web::problems::loss_cause(why);
            out.push((
                "lost",
                "warning",
                format!("{name} lost: {why}"),
                json!({"input": id, "label": name, "reason": why, "cause": cause}),
            ));
            if cause == "other_site" {
                out.push((
                    "other_site",
                    "error",
                    format!("{name} belongs to another site"),
                    json!({"input": id, "label": name, "reason": why}),
                ));
            }
        }
        let attempts = i["attempts"].as_u64().unwrap_or(0);
        let attempts_before = was.and_then(|w| w["attempts"].as_u64()).unwrap_or(0);
        if lost_now.is_some() && attempts > attempts_before {
            out.push((
                "reconnecting",
                "info",
                format!("{name}: reconnecting (attempt {attempts})"),
                json!({"input": id, "label": name, "attempt": attempts, "reason": lost_now}),
            ));
        }
        if lost_before.is_some() && lost_now.is_none() && i["connected"] == true {
            out.push((
                "recovered",
                "ok",
                format!("{name} is back"),
                json!({"input": id, "label": name}),
            ));
        }
        let fm = format_mismatch(i, now);
        let fm_before = was.and_then(|w| format_mismatch(w, before));
        if let Some(f) = &fm
            && fm_before.as_ref().map(|x| (&x["size"], x["rate"].clone()))
                != Some((&f["size"], f["rate"].clone()))
        {
            let mut f = f.clone();
            f["input"] = json!(id);
            f["label"] = json!(name);
            out.push((
                "format_mismatch",
                if f["rate"] == true { "warning" } else { "info" },
                format!(
                    "{name} sends {}x{} at {:.2}/s; the program is {}x{} at {:.2}/s",
                    f["size"][0],
                    f["size"][1],
                    f["fps"].as_f64().unwrap_or(0.0),
                    f["program"]["size"][0],
                    f["program"]["size"][1],
                    f["program"]["fps"].as_f64().unwrap_or(0.0)
                ),
                f,
            ));
        }
    }
    // PTZ cameras (§S.21): an error is the camera not answering.
    let cams = |st: &Value| -> BTreeMap<u64, (String, Option<String>)> {
        st["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["camera"].is_object())
            .filter_map(|i| {
                Some((
                    i["id"].as_u64()?,
                    (label(i), i["camera"]["error"].as_str().map(str::to_owned)),
                ))
            })
            .collect()
    };
    let (cb, cn) = (cams(before), cams(now));
    for (id, (name, err)) in &cn {
        let was = cb.get(id).and_then(|c| c.1.clone());
        match (err, was) {
            (Some(e), None) => out.push((
                "camera_unreachable",
                "warning",
                format!("{name}'s camera doesn't answer: {e}"),
                json!({"input": id, "label": name, "error": e}),
            )),
            (None, Some(_)) => out.push((
                "camera_unreachable",
                "ok",
                format!("{name}'s camera answers again"),
                json!({"input": id, "label": name, "over": true}),
            )),
            _ => {}
        }
    }
    // The desks' feeds (§S.26).
    let feeds = |st: &Value| -> BTreeMap<String, (String, bool)> {
        st["feeds"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                (
                    s(f, "output").to_owned(),
                    (s(f, "desk").to_owned(), f["linked"] != false),
                )
            })
            .collect()
    };
    let (fb, fnow) = (feeds(before), feeds(now));
    for (out_name, (desk, linked)) in &fnow {
        let was = fb.get(out_name).map(|f| f.1);
        match (linked, was) {
            (false, Some(true) | None) => out.push((
                "desk_link_lost",
                "warning",
                format!("{out_name}: the desk {desk} doesn't answer (its sound feed)"),
                json!({"output": out_name, "desk": desk}),
            )),
            (true, Some(false)) => out.push((
                "desk_link",
                "ok",
                format!("{out_name}: the desk {desk} answers again"),
                json!({"output": out_name, "desk": desk, "over": true}),
            )),
            _ => {}
        }
    }
    out
}

/// What is wrong now in a state (the inputs, cameras and feeds; the service adds the
/// streams', the engine's load and the sources gone).
pub fn problems(st: &Value) -> Vec<Value> {
    use idn_open_protocol::web::problems::problem;
    let mut out = Vec::new();
    for (id, i) in network_inputs(st) {
        let name = label(i);
        if let Some(why) = i["lost"].as_str() {
            out.push(problem(
                "lost",
                "warning",
                &format!("{name} lost: {why}"),
                json!({"input": id, "reason": why,
                       "cause": idn_open_protocol::web::problems::loss_cause(why),
                       "attempts": i["attempts"]}),
            ));
        }
        if let Some(f) = format_mismatch(i, st) {
            let mut f = f;
            f["input"] = json!(id);
            out.push(problem(
                "format_mismatch",
                if f["rate"] == true { "warning" } else { "info" },
                &format!("{name}: not the program's format"),
                f,
            ));
        }
    }
    for i in st["inputs"].as_array().into_iter().flatten() {
        if let Some(e) = i["camera"]["error"].as_str() {
            out.push(problem(
                "camera_unreachable",
                "warning",
                &format!("{}'s camera doesn't answer: {e}", label(i)),
                json!({"input": i["id"], "error": e}),
            ));
        }
    }
    for f in st["feeds"].as_array().into_iter().flatten() {
        if f["linked"] == false {
            out.push(problem(
                "desk_link_lost",
                "warning",
                &format!("the desk {} doesn't answer", s(f, "desk")),
                json!({"output": f["output"], "desk": f["desk"]}),
            ));
        }
    }
    out
}

/// What the watch keeps between rounds.
#[derive(Default)]
pub struct Watcher {
    pub before: Value,
    /// Network inputs whose source isn't listed: the source, since when, and whether said.
    unlisted: BTreeMap<u64, (String, Instant, bool)>,
    /// Each listed source's address as last seen, by name.
    addr_of: BTreeMap<String, String>,
    /// The engine's late frames at the last round, and the overload: since when, why,
    /// and when it last showed.
    late: Option<u64>,
    pub overloaded: Option<(Instant, &'static str)>,
    calm_since: Option<Instant>,
    /// Inputs gone or renamed now, for the problems: id → the problem.
    pub sources: BTreeMap<u64, Value>,
}

impl Watcher {
    /// Network inputs' sources gone from the network (not listed for [`GONE_AFTER`]) or
    /// renamed (another name at the same address); `listed`: every source with its
    /// address, `None` while the list isn't read yet.
    pub fn sources(
        &mut self,
        st: &Value,
        listed: Option<&[(String, String)]>,
        now: Instant,
    ) -> Vec<Ev> {
        use idn_open_protocol::web::problems::problem;
        let mut out = Vec::new();
        let Some(listed) = listed.filter(|l| !l.is_empty()) else {
            return out;
        };
        let names: BTreeSet<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        let inputs = network_inputs(st);
        self.unlisted.retain(|id, _| inputs.contains_key(id));
        self.sources.retain(|id, _| inputs.contains_key(id));
        for (id, i) in &inputs {
            let src = i["source"]["network"].as_str().unwrap_or("");
            // Pointed at another source since: what was watched for the old one goes (its
            // problem, its time, whether it was said).
            if self.unlisted.get(id).is_some_and(|(was, _, _)| was != src) {
                self.unlisted.remove(id);
                self.sources.remove(id);
            }
            // An address, or a routing source's way of naming: only names are watched.
            if src.is_empty() || (src.contains(':') && !src.contains('(')) {
                self.sources.remove(id);
                continue;
            }
            if names.contains(src) {
                if self.unlisted.remove(id).is_some_and(|(_, _, said)| said) {
                    out.push((
                        "gone",
                        "ok",
                        format!("{src} is on the network again"),
                        json!({"input": id, "source": src, "over": true}),
                    ));
                }
                self.sources.remove(id);
                continue;
            }
            let e = self
                .unlisted
                .entry(*id)
                .or_insert_with(|| (src.to_owned(), now, false));
            if e.2 || now.duration_since(e.1) < GONE_AFTER {
                continue;
            }
            e.2 = true;
            // Renamed: its last address is listed under another name.
            let renamed = self
                .addr_of
                .get(src)
                .filter(|a| !a.is_empty())
                .and_then(|a| listed.iter().find(|(n, x)| x == a && n != src))
                .map(|(n, _)| n.clone());
            let (ev, text, f) = match &renamed {
                Some(to) => (
                    "renamed",
                    format!("{src} is now named {to}: the input still asks for the old name"),
                    json!({"input": id, "source": src, "to": to}),
                ),
                None => (
                    "gone",
                    format!("{src} isn't on the network any more"),
                    json!({"input": id, "source": src}),
                ),
            };
            self.sources
                .insert(*id, problem(ev, "warning", &text, f.clone()));
            out.push((ev, "warning", text, f));
        }
        for (n, a) in listed {
            self.addr_of.insert(n.clone(), a.clone());
        }
        // Bounded: names not listed now let go past 4096.
        if self.addr_of.len() > 4096 {
            self.addr_of.retain(|n, _| names.contains(n.as_str()));
        }
        out
    }

    /// The engine's load (each round, about a second): `late_frames` growing by
    /// [`LATE_PER_ROUND`] or more, or `skipping` (an encoder skipping pictures) starts an
    /// overload; [`CALM_AFTER`] without either ends it.
    pub fn load(&mut self, st: &Value, skipping: bool, now: Instant) -> Vec<Ev> {
        let late = st["late_frames"].as_u64().unwrap_or(0);
        let grew = self
            .late
            .is_some_and(|l| late.saturating_sub(l) >= LATE_PER_ROUND);
        self.late = Some(late);
        let why = if grew {
            Some("compositor")
        } else if skipping {
            Some("encoder")
        } else {
            None
        };
        let mut out = Vec::new();
        match (why, self.overloaded) {
            (Some(part), None) => {
                self.overloaded = Some((now, part));
                self.calm_since = None;
                out.push((
                    "overloaded",
                    "warning",
                    match part {
                        "compositor" => format!(
                            "the switcher can't keep up: frames late (each takes {:.1} ms)",
                            st["frame_ms"].as_f64().unwrap_or(0.0)
                        ),
                        _ => "the encoder can't keep up: pictures skipped".to_owned(),
                    },
                    json!({"part": part, "frame_ms": st["frame_ms"], "late_frames": late}),
                ));
            }
            (Some(_), Some(_)) => self.calm_since = None,
            (None, Some((_, part))) => {
                let calm = *self.calm_since.get_or_insert(now);
                if now.duration_since(calm) >= CALM_AFTER {
                    self.overloaded = None;
                    self.calm_since = None;
                    out.push((
                        "overloaded",
                        "ok",
                        "the switcher keeps up again".to_owned(),
                        json!({"part": part, "over": true}),
                    ));
                }
            }
            (None, None) => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(lost: Option<&str>, attempts: u64, connected: bool, w: u64, fps: f64) -> Value {
        json!({
            "format": {"width": 1920, "height": 1080, "fps": [60, 1]},
            "inputs": [{"id": 3, "label": "Cam 1", "source": {"network": "BOOTH (Cam 1)"},
                        "lost": lost, "attempts": attempts, "connected": connected,
                        "width": w, "height": if w == 0 { 0 } else { 1080 }, "fps": fps}],
            "feeds": [],
        })
    }

    fn names(e: &[Ev]) -> Vec<&str> {
        e.iter().map(|x| x.0).collect()
    }

    /// A source lost: `lost` with its reason and cause, `reconnecting` at each attempt,
    /// `recovered` when it is back; another site's source says so.
    #[test]
    fn a_lost_source_reconnects_and_recovers() {
        let up = state(None, 0, true, 1920, 60.0);
        let lost = state(Some("connection timed out"), 0, false, 1920, 60.0);
        let e = transitions(&up, &lost);
        assert_eq!(names(&e), ["lost"]);
        assert_eq!(e[0].3["cause"], "unreachable");
        assert_eq!(e[0].3["input"], 3);
        let a1 = state(Some("connection timed out"), 1, false, 1920, 60.0);
        let a2 = state(Some("connection timed out"), 2, false, 1920, 60.0);
        assert_eq!(transitions(&lost, &a1)[0].3["attempt"], 1);
        assert_eq!(names(&transitions(&a1, &a2)), ["reconnecting"]);
        assert!(transitions(&a2, &a2).is_empty());
        let back = transitions(&a2, &up);
        assert_eq!(names(&back), ["recovered"]);
        let other = state(
            Some("BOOTH (Cam 1) is enrolled with another site authority"),
            0,
            false,
            0,
            0.0,
        );
        assert_eq!(names(&transitions(&up, &other)), ["lost", "other_site"]);
        assert_eq!(problems(&a2)[0]["problem"], "lost");
        assert!(problems(&up).is_empty());
    }

    /// A 30 frames a second camera on a 60 program, and a 720p one: format mismatches,
    /// once each.
    #[test]
    fn format_mismatch_once() {
        let ok = state(None, 0, true, 1920, 59.94);
        let slow = state(None, 0, true, 1920, 30.0);
        let e = transitions(&ok, &slow);
        assert_eq!(names(&e), ["format_mismatch"]);
        assert_eq!(e[0].1, "warning");
        assert!(transitions(&slow, &slow).is_empty());
        let small = state(None, 0, true, 1280, 60.0);
        assert_eq!(transitions(&ok, &small)[0].1, "info");
        assert!(transitions(&ok, &ok).is_empty());
    }

    /// Cameras not answering and desks' feeds lost, both ways.
    #[test]
    fn cameras_and_desk_links() {
        let cam = |e: Option<&str>| {
            json!({"inputs": [{"id": 1, "label": "PTZ", "source": {"ptz": {}},
            "camera": {"error": e}}], "feeds": [{"output": "P (Program)", "desk": "FOH (Mixer)", "linked": e.is_none()}]})
        };
        let e = transitions(&cam(None), &cam(Some("timed out")));
        assert_eq!(names(&e), ["camera_unreachable", "desk_link_lost"]);
        let e = transitions(&cam(Some("timed out")), &cam(None));
        assert_eq!(names(&e), ["camera_unreachable", "desk_link"]);
        assert_eq!(e[0].1, "ok");
        assert_eq!(problems(&cam(Some("x"))).len(), 2);
    }

    /// A stream refused by its platform is `auth_expired`; other errors aren't.
    #[test]
    fn a_streams_refusal_is_typed() {
        assert_eq!(
            stream_reason("rtmp: NetStream.Publish.BadName: invalid stream key"),
            Some("auth_expired")
        );
        assert_eq!(
            stream_reason("HTTP error 403 Forbidden"),
            Some("auth_expired")
        );
        assert_eq!(
            stream_reason("srt: connection rejected: BADSECRET"),
            Some("auth_expired")
        );
        assert_eq!(stream_reason("Connection timed out"), None);
        assert_eq!(stream_reason("Broken pipe"), None);
    }

    /// Late frames growing make the switcher overloaded, an encoder skipping too; ten
    /// quiet seconds end it.
    #[test]
    fn overloaded_starts_and_ends() {
        let t0 = Instant::now();
        let st = |late: u64| json!({"late_frames": late, "frame_ms": 18.0});
        let mut w = Watcher::default();
        assert!(w.load(&st(0), false, t0).is_empty());
        assert!(
            w.load(&st(1), false, t0 + Duration::from_secs(1))
                .is_empty()
        );
        let e = w.load(&st(9), false, t0 + Duration::from_secs(2));
        assert_eq!(
            (e[0].0, e[0].3["part"].as_str()),
            ("overloaded", Some("compositor"))
        );
        assert!(
            w.load(&st(9), false, t0 + Duration::from_secs(3))
                .is_empty()
        );
        assert!(
            w.load(&st(9), false, t0 + Duration::from_secs(8))
                .is_empty()
        );
        let e = w.load(&st(9), false, t0 + Duration::from_secs(14));
        assert_eq!((e[0].1, e[0].3["over"].as_bool()), ("ok", Some(true)));
        let e = w.load(&st(9), true, t0 + Duration::from_secs(15));
        assert_eq!(e[0].3["part"], "encoder");
    }

    /// A source not listed for 10 s is gone; one listed under another name at its address
    /// was renamed; listed again, it is back.
    #[test]
    fn sources_gone_and_renamed() {
        let t0 = Instant::now();
        let st = state(None, 0, true, 1920, 60.0);
        let mut w = Watcher::default();
        let listed = vec![("BOOTH (Cam 1)".to_owned(), "10.0.0.5:5961".to_owned())];
        assert!(w.sources(&st, Some(&listed), t0).is_empty());
        let renamed = vec![("BOOTH (Camera A)".to_owned(), "10.0.0.5:5961".to_owned())];
        assert!(
            w.sources(&st, Some(&renamed), t0 + Duration::from_secs(1))
                .is_empty()
        );
        let e = w.sources(&st, Some(&renamed), t0 + Duration::from_secs(12));
        assert_eq!(names(&e), ["renamed"]);
        assert_eq!(e[0].3["to"], "BOOTH (Camera A)");
        assert!(
            w.sources(&st, Some(&renamed), t0 + Duration::from_secs(13))
                .is_empty()
        );
        assert_eq!(w.sources.len(), 1);
        let e = w.sources(&st, Some(&listed), t0 + Duration::from_secs(14));
        assert_eq!((e[0].0, e[0].1), ("gone", "ok"));
        // Gone with nothing else at its address.
        let mut w = Watcher::default();
        let other = vec![("X (Y)".to_owned(), "10.0.0.9:5961".to_owned())];
        w.sources(&st, Some(&other), t0);
        assert_eq!(
            names(&w.sources(&st, Some(&other), t0 + Duration::from_secs(11))),
            ["gone"]
        );
        // No list yet: nothing.
        assert!(Watcher::default().sources(&st, None, t0).is_empty());
    }
}
