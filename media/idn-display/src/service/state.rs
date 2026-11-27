//! What the service tells (§D.8.3, §D.8.5): its state, each screen's, the figures, the
//! problems and the typed events a screen's status change makes.

use super::*;

impl Service {
    /// The state: each screen (listed, or saved and not there now) with its feed, status,
    /// figures and settings.
    pub fn state(&self) -> Value {
        let g = self.inner.lock_ok();
        let mut screens = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (k, s) in g.screens.iter().enumerate() {
            seen.insert(s.name.clone());
            screens.push(self.screen_json(&g, &s.name, Some((k + 1, s))));
        }
        for name in g.saved.keys().filter(|n| !seen.contains(*n)) {
            screens.push(self.screen_json(&g, name, None));
        }
        // What is wrong now (idn-node.md §N.41), from each screen's status.
        let problems: Vec<Value> = screens
            .iter()
            .flat_map(|s| {
                let st: Status = serde_json::from_value(json!({
                    "state": s["status"]["state"], "showing": s["status"]["showing"],
                    "reason": s["status"]["reason"],
                    "attempts": s["status"]["attempts"], "gave_up": s["status"]["gave_up"],
                }))
                .unwrap_or_default();
                let mut p = screen_problems(
                    s["id"].as_str().unwrap_or(""),
                    &st,
                    s["error"].as_str().unwrap_or(""),
                );
                p.extend(size_and_drop_problems(s));
                p
            })
            .collect();
        serde_json::to_value(DisplayState {
            service: "IDN Display".into(),
            version: g.version,
            build: idn_open_protocol::build_line(),
            machine: idn_open_protocol::discovery::hostname(),
            screens,
            error: g.list_error.clone(),
            problems,
            sound_cards: g.cards.clone(),
            lan: LanView {
                on: !g.lan_off,
                available: self.tokens.is_some(),
            },
        })
        .unwrap_or_default()
    }

    pub(super) fn screen_json(
        &self,
        g: &Inner,
        name: &str,
        listed: Option<(usize, &ScreenInfo)>,
    ) -> Value {
        let saved = g.saved.get(name).cloned().unwrap_or_default();
        let r = g.running.get(name);
        let status = r
            .and_then(|r| r.status.clone())
            .or_else(|| g.last.get(name).cloned())
            .filter(|_| !saved.feed.is_empty())
            .unwrap_or_default();
        let error = g
            .failed
            .get(name)
            .map(|(_, e)| e.clone())
            .unwrap_or_default();
        let dropped_last_min = r.map_or(0, |r| match (r.drops.front(), r.drops.back()) {
            (Some(a), Some(b)) => b.1.saturating_sub(a.1),
            _ => 0,
        });
        let v = ScreenView {
            id: name.to_owned(),
            name: name.to_owned(),
            label: saved.label,
            feed_by: saved.feed_by,
            feed_at: (saved.feed_at > 0).then_some(saved.feed_at),
            test_pattern: g.test.contains_key(name),
            dropped_last_min,
            connected: listed.is_some(),
            feed: saved.feed,
            routed_to: status.routed_to,
            running: r.is_some_and(|r| r.ready.is_some()),
            error,
            identify: g.identify.contains_key(name),
            status: StatusView {
                state: status.state,
                showing: status.showing,
                reason: status.reason,
                attempts: status.attempts,
                gave_up: status.gave_up,
                for_s: status.for_s,
            },
            stats: status.stats,
            settings: saved.settings,
            adapter: r.and_then(|r| r.ready.as_ref()).map(|(_, a)| a.clone()),
            listed: listed.map(|(n, s)| Listed {
                number: n,
                width: s.width,
                height: s.height,
                refresh_hz: s.refresh_hz,
                x: s.x,
                y: s.y,
                modes: s.modes.clone(),
            }),
        };
        serde_json::to_value(v).unwrap_or_default()
    }

    /// Each running screen's figures (the push channel's `stats` topic).
    pub fn stats(&self) -> Value {
        let g = self.inner.lock_ok();
        let screens: Vec<Value> = g
            .running
            .iter()
            .filter_map(|(k, r)| Some(json!({"id": k, "stats": r.status.as_ref()?.stats, "for_s": r.status.as_ref()?.for_s})))
            .collect();
        json!({"type": "stats", "screens": screens})
    }
}

/// An event's level and words for people (idn-node.md §N.41).
pub(super) fn event_words(event: &str, screen: &str, f: &Value) -> (&'static str, String) {
    let feed = f["feed"].as_str().unwrap_or("");
    let reason = f["reason"].as_str().unwrap_or("");
    match event {
        "lost" => ("warning", format!("{screen}: {feed} lost: {reason}")),
        "other_site" => ("error", format!("{screen}: {feed} belongs to another site")),
        "reconnecting" => (
            "info",
            format!(
                "{screen}: reconnecting to {feed} (attempt {})",
                f["attempt"]
            ),
        ),
        "fallback" => (
            "warning",
            format!(
                "{screen}: showing the fallback{}",
                if f["gave_up"] == true {
                    " (gave up reconnecting)"
                } else {
                    ""
                }
            ),
        ),
        "recovered" => ("ok", format!("{screen}: {feed} is back")),
        "format" => ("info", format!("{screen}: {feed} changed its format")),
        "decoder_trouble" => (
            "warning",
            format!(
                "{screen}: {feed}: {}",
                f["reason"].as_str().unwrap_or("decoder trouble")
            ),
        ),
        e => ("info", format!("{screen}: {e}")),
    }
}

/// What is wrong on a screen now (idn-node.md §N.41): its feed lost, the fallback up,
/// reconnecting given up, its process failing.
pub fn screen_problems(screen: &str, s: &Status, error: &str) -> Vec<Value> {
    use idn_open_protocol::web::problems::problem;
    let mut out = Vec::new();
    match s.state {
        State::Lost | State::Fallback => out.push(problem(
            if s.gave_up { "gave_up" } else { "lost" },
            "warning",
            &format!("{screen}: {}", s.reason),
            json!({"screen": screen, "reason": s.reason, "attempts": s.attempts,
                   "cause": idn_open_protocol::web::problems::loss_cause(&s.reason),
                   "fallback": s.state == State::Fallback}),
        )),
        _ => {}
    }
    if !error.is_empty() {
        out.push(problem(
            "screen_failed",
            "error",
            &format!("{screen}: {error}"),
            json!({"screen": screen, "error": error}),
        ));
    }
    out
}

/// A screen's problems from its figures (§D.8.5): a screen whose mode is smaller than
/// its feed (shown scaled down: a wall's screens excepted, each shows a part), and
/// pictures dropped over the last minute.
pub fn size_and_drop_problems(s: &Value) -> Vec<Value> {
    use idn_open_protocol::web::problems::problem;
    let id = s["id"].as_str().unwrap_or("");
    let mut out = Vec::new();
    let src = s["stats"]["source_size"]
        .as_array()
        .and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?)));
    // The surface drawn on (the mode in use), else the screen as listed.
    let surface = s["stats"]["screen_size"]
        .as_array()
        .and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?)))
        .filter(|&(w, h)| w > 0 && h > 0);
    let (mut sw, mut sh) = surface.unwrap_or((
        s["width"].as_u64().unwrap_or(0),
        s["height"].as_u64().unwrap_or(0),
    ));
    if matches!(s["settings"]["rotation"].as_u64(), Some(90 | 270)) {
        (sw, sh) = (sh, sw);
    }
    if let Some((w, h)) = src
        && s["status"]["state"] == "playing"
        && s["settings"]["wall"].is_null()
        && sw > 0
        && sh > 0
        && (w > sw || h > sh)
    {
        out.push(problem(
            "screen_smaller",
            "info",
            &format!("{id}: the feed is {w}×{h}, the screen {sw}×{sh}: shown smaller"),
            json!({"screen": id, "source": [w, h], "screen_size": [sw, sh]}),
        ));
    }
    let dropped = s["dropped_last_min"].as_u64().unwrap_or(0);
    if dropped >= DROPS_A_PROBLEM {
        out.push(problem(
            "dropping",
            "warning",
            &format!("{id}: {dropped} pictures dropped in the last minute"),
            json!({"screen": id, "dropped_last_min": dropped}),
        ));
    }
    out
}

/// The typed events a status change makes (§D.8.5): `lost` (with its `cause`),
/// `other_site` (the feed belongs to another site), `reconnecting` (each new attempt),
/// `fallback`, `recovered`, `playing` (a new feed's first picture), `format` (the feed's
/// size or frame rate changed), `routed`, `decoder_trouble` (pictures failed to decode, or
/// decoding fell back from the GPU to the CPU, while it plays).
pub fn transitions(before: &Status, now: &Status, feed: &str) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    let was = before.state;
    match now.state {
        State::Lost if was != State::Lost => {
            let cause = idn_open_protocol::web::problems::loss_cause(&now.reason);
            out.push((
                "lost",
                json!({"feed": feed, "reason": now.reason, "cause": cause}),
            ));
            if cause == "other_site" {
                out.push(("other_site", json!({"feed": feed, "reason": now.reason})));
            }
        }
        State::Fallback if was != State::Fallback => out.push((
            "fallback",
            json!({"feed": feed, "show": now.showing, "reason": now.reason, "gave_up": now.gave_up}),
        )),
        State::Playing if matches!(was, State::Lost | State::Fallback) => {
            out.push(("recovered", json!({"feed": feed, "after_s": before.for_s})));
        }
        State::Playing if was != State::Playing => out.push(("playing", json!({"feed": feed}))),
        _ => {}
    }
    if now.attempts > before.attempts
        && matches!(now.state, State::Lost | State::Fallback | State::Connecting)
    {
        out.push((
            "reconnecting",
            json!({"feed": feed, "attempt": now.attempts, "reason": now.reason}),
        ));
    }
    let fmt = |s: &Status| (s.stats.source_size, s.stats.frame_rate);
    if now.state == State::Playing
        && before.stats.source_size.is_some()
        && now.stats.source_size.is_some()
        && fmt(before) != fmt(now)
    {
        out.push((
            "format",
            json!({"feed": feed,
                   "from": {"size": before.stats.source_size, "frame_rate": before.stats.frame_rate},
                   "to": {"size": now.stats.source_size, "frame_rate": now.stats.frame_rate}}),
        ));
    }
    // Decoder trouble (§D.8.5): pictures that failed to decode since the last look, or the
    // GPU's decoding given up for the CPU's, the feed playing on.
    if now.state == State::Playing {
        let failed = now
            .stats
            .decode_errors
            .saturating_sub(before.stats.decode_errors);
        if failed > 0 {
            out.push((
                "decoder_trouble",
                json!({"feed": feed, "failed": failed, "failed_total": now.stats.decode_errors,
                       "decode": now.stats.decode,
                       "reason": format!("{failed} pictures failed to decode")}),
            ));
        }
        if before.stats.decode == "gpu" && now.stats.decode == "cpu" {
            out.push((
                "decoder_trouble",
                json!({"feed": feed, "failed": 0, "failed_total": now.stats.decode_errors,
                       "decode": "cpu",
                       "reason": "decoding fell back from the GPU to the CPU"}),
            ));
        }
    }
    if now.routed_to.is_some() && now.routed_to != before.routed_to {
        out.push(("routed", json!({"feed": feed, "to": now.routed_to})));
    }
    out
}

/// The display service's state (`GET /api/v1/state`, §D.8.3), typed: its schema is
/// `GET /api/v1/state_schema` (refactor 4).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DisplayState {
    /// `IDN Display`.
    pub service: String,
    /// Moves with every change worth sending (not the figures).
    pub version: u64,
    pub build: String,
    /// This computer's name.
    pub machine: String,
    /// Each screen: listed now, or saved and not there now.
    pub screens: Vec<Value>,
    /// Why the screens couldn't be listed, if so.
    pub error: Option<String>,
    /// What is wrong now (idn-node.md §N.41).
    pub problems: Vec<Value>,
    /// The sound cards a screen's sound can go to (`settings.audio.device` takes a
    /// name); null: not listed yet.
    pub sound_cards: Option<Vec<Value>>,
    /// The API on the network.
    pub lan: LanView,
}

/// The API on the network: turned on or off (`lan`), and whether it can be (the
/// machine in a site).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LanView {
    pub on: bool,
    pub available: bool,
}

/// A screen in the state (each of `DisplayState::screens`).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ScreenView {
    /// The screen's name as the desktop gives it (`DP-2`, `\\.\DISPLAY1`).
    pub id: String,
    pub name: String,
    /// Its name for people (`label`).
    pub label: String,
    /// Who set the feed (as the audit says them), and when.
    pub feed_by: String,
    pub feed_at: Option<u64>,
    pub test_pattern: bool,
    /// Pictures dropped over the last minute.
    pub dropped_last_min: u64,
    /// Plugged in now.
    pub connected: bool,
    pub feed: String,
    /// Where a routing source sends it now.
    pub routed_to: Option<String>,
    /// Its process is up.
    pub running: bool,
    pub error: String,
    pub identify: bool,
    pub status: StatusView,
    pub stats: crate::proto::Stats,
    pub settings: Settings,
    /// The GPU drawing it.
    pub adapter: Option<String>,
    /// Listed now: its number (from 1), size, refresh, place and modes.
    #[serde(flatten)]
    pub listed: Option<Listed>,
}

/// A screen's status.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct StatusView {
    pub state: State,
    pub showing: crate::proto::Showing,
    pub reason: String,
    pub attempts: u64,
    pub gave_up: bool,
    pub for_s: f64,
}

/// A listed screen's place.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Listed {
    pub number: usize,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub x: i32,
    pub y: i32,
    pub modes: Vec<crate::proto::ScreenMode>,
}

/// [`DisplayState`]'s JSON schema, with a screen's ([`ScreenView`]).
pub fn state_schema() -> Value {
    let mut s = schemars::schema_for!(DisplayState).to_value();
    s["screen"] = schemars::schema_for!(ScreenView).to_value();
    s
}
