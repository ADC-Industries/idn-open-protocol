//! Failure states as typed events (idn-mixer.md §M.27, idn-node.md §N.41): each second the
//! desk is looked at ([`look`]) and what changed for the worse (or back) is an event on
//! the push channel (`{"type": "event", "event", …}`) and, rate-limited, a line in the Site
//! Manager's log through the Node; what is wrong now is the state's `problems`.
//!
//! - Routed sources: `lost` (`reason`, `cause`), `recovered`.
//! - A backup input taking over (`failover`), and back (`over`).
//! - Plugins: `plugin_failed` (the plugin host stopped and started again; a strip's
//!   plugins failing), `over` when a strip's error clears.
//! - The multitrack recorder: `recorder_failed`, `disk_low` (`over` with room again).
//! - The X32's control link: `x32_unreachable` (`over` once it answers).
//! - The desk's sound card (or network-audio device): `card_lost` (`over`).
//! - The DSP: `overloaded` (blocks skipped, or taking longer than their time; `over`).
//! - IEM phones: `iem_phone_dropped` (said by the sessions' sweep, `iem_link`).

use super::*;
use idn_open_protocol::web::problems::{loss_cause, problem};
use std::collections::BTreeSet;

/// A source heard before and silent this long is lost (the router's own "fresh").
pub const LOST_AFTER: Duration = Duration::from_secs(2);
/// A source routed this long and never heard is lost (the router's "no source").
pub const NEVER_HEARD_AFTER: Duration = Duration::from_secs(5);
/// An X32 that hasn't answered this long since it was set (or lost) is unreachable.
pub const X32_GRACE: Duration = Duration::from_secs(3);
/// A sound card's error lasting this long is a lost card (a network device waits for its
/// rings a moment at the start); errors counted by the card count at once.
pub const CARD_GRACE: Duration = Duration::from_secs(3);
/// A lost card is back after this long without an error.
pub const CARD_CALM: Duration = Duration::from_secs(5);
/// Blocks a round (about 1 s) over their time that make the DSP overloaded.
pub const SLOW_PER_ROUND: u64 = 10;
/// Overloaded ends after this long without a late or slow block.
pub const CALM_AFTER: Duration = Duration::from_secs(10);

/// One event: its name, level, text and fields.
pub type Ev = (&'static str, &'static str, String, Value);

/// A routed transmitter: the desk inputs it feeds (from 1), and how long ago its sound
/// last arrived (`None`: not yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceLook {
    pub inputs: Vec<usize>,
    pub heard_ago: Option<Duration>,
}

/// A channel on its backup input by failover.
#[derive(Debug, Clone, PartialEq)]
pub struct FailoverLook {
    pub name: String,
    pub input: usize,
    pub backup: usize,
}

/// The recording under way.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordLook {
    pub folder: String,
    pub path: String,
    pub error: Option<String>,
    /// Its disk's free space and size, bytes.
    pub free: Option<(u64, u64)>,
}

/// The X32 the settings name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct X32Look {
    pub address: String,
    pub connected: bool,
    pub error: Option<String>,
}

/// One side of the desk's sound card (`input`, `output`) or its network-audio device
/// (`network`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardLook {
    pub part: &'static str,
    pub device: String,
    pub error: Option<String>,
    /// Errors the card counted so far.
    pub errors: u64,
}

/// What one round looks at.
#[derive(Debug, Clone, Default)]
pub struct Look {
    pub sources: BTreeMap<String, SourceLook>,
    /// The sources on the network (`None`: not known, no Node).
    pub listed: Option<BTreeSet<String>>,
    /// By channel (from 1).
    pub failover: BTreeMap<usize, FailoverLook>,
    pub plugin_restarts: u64,
    /// Strip → error.
    pub plugin_errors: BTreeMap<String, String>,
    pub record: Option<RecordLook>,
    pub x32: Option<X32Look>,
    pub cards: Vec<CardLook>,
    pub late_blocks: u64,
    pub slow_blocks: u64,
    pub block_ms: f64,
    pub period_ms: f64,
}

/// A card side as the watch keeps it.
#[derive(Debug, Default)]
struct CardSeen {
    errors: Option<u64>,
    bad_since: Option<Instant>,
    last_bad: Option<Instant>,
    down: Option<Value>,
}

/// What the watch keeps between rounds; its conditions are the state's `problems`.
#[derive(Debug, Default)]
pub struct Watcher {
    routed_since: BTreeMap<String, Instant>,
    lost: BTreeMap<String, Value>,
    failover: BTreeMap<usize, Value>,
    restarts: Option<u64>,
    plugin_errors: BTreeMap<String, String>,
    record_error: Option<Value>,
    disk: &'static str,
    disk_problem: Option<Value>,
    x32_since: Option<Instant>,
    x32_problem: Option<Value>,
    cards: BTreeMap<&'static str, CardSeen>,
    late: Option<(u64, u64)>,
    overloaded: Option<(Instant, Value)>,
    calm_since: Option<Instant>,
}

impl Watcher {
    /// One round: the events since the last.
    pub fn round(&mut self, l: &Look, now: Instant) -> Vec<Ev> {
        let mut out = Vec::new();
        self.sources(l, now, &mut out);
        self.failovers(l, &mut out);
        self.plugins(l, &mut out);
        self.recorder(l, &mut out);
        self.x32(l, now, &mut out);
        self.cards(l, now, &mut out);
        self.load(l, now, &mut out);
        out
    }

    fn sources(&mut self, l: &Look, now: Instant, out: &mut Vec<Ev>) {
        self.routed_since.retain(|tx, _| l.sources.contains_key(tx));
        // A route taken away: not a recovery, only forgotten.
        self.lost.retain(|tx, _| l.sources.contains_key(tx));
        for (tx, s) in &l.sources {
            let since = *self.routed_since.entry(tx.clone()).or_insert(now);
            let down = match s.heard_ago {
                Some(a) => a >= LOST_AFTER,
                None => now.duration_since(since) >= NEVER_HEARD_AFTER,
            };
            let short = crate::switchers::short_name(tx);
            match (down, self.lost.contains_key(tx)) {
                (true, false) => {
                    // (Only a source by name is listed; one routed by address isn't.)
                    let unlisted =
                        tx.contains('(') && l.listed.as_ref().is_some_and(|n| !n.contains(tx));
                    let reason = if unlisted {
                        "not found on the network".to_owned()
                    } else {
                        match s.heard_ago {
                            Some(a) => format!("no answer for {} s", a.as_secs()),
                            None => "no answer since it was routed".to_owned(),
                        }
                    };
                    let cause = loss_cause(&reason);
                    let f = json!({"source": tx, "label": short, "inputs": s.inputs,
                                   "reason": reason, "cause": cause});
                    let text = format!("{short} lost: {reason}");
                    self.lost
                        .insert(tx.clone(), problem("lost", "warning", &text, f.clone()));
                    out.push(("lost", "warning", text, f));
                }
                (false, true) => {
                    self.lost.remove(tx);
                    out.push((
                        "recovered",
                        "ok",
                        format!("{short} is back"),
                        json!({"source": tx, "label": short, "inputs": s.inputs}),
                    ));
                }
                _ => {}
            }
        }
    }

    fn failovers(&mut self, l: &Look, out: &mut Vec<Ev>) {
        for (n, f) in &l.failover {
            if self.failover.contains_key(n) {
                continue;
            }
            let text = format!(
                "{}: its backup input {} took over (nothing on input {})",
                f.name, f.backup, f.input
            );
            let fields =
                json!({"channel": n, "name": f.name, "input": f.input, "backup": f.backup});
            self.failover
                .insert(*n, problem("failover", "warning", &text, fields.clone()));
            out.push(("failover", "warning", text, fields));
        }
        let back: Vec<usize> = self
            .failover
            .keys()
            .filter(|n| !l.failover.contains_key(n))
            .copied()
            .collect();
        for n in back {
            if let Some(p) = self.failover.remove(&n) {
                out.push((
                    "failover",
                    "ok",
                    format!("{}: back on input {}", s(&p, "name"), p["input"]),
                    json!({"channel": n, "name": p["name"], "input": p["input"],
                           "backup": p["backup"], "over": true}),
                ));
            }
        }
    }

    fn plugins(&mut self, l: &Look, out: &mut Vec<Ev>) {
        if let Some(r) = self.restarts
            && l.plugin_restarts > r
        {
            out.push((
                "plugin_failed",
                "error",
                format!(
                    "the plugin host stopped: started again (restart {})",
                    l.plugin_restarts
                ),
                json!({"strip": "host", "restarts": l.plugin_restarts}),
            ));
        }
        self.restarts = Some(l.plugin_restarts);
        for (strip, e) in &l.plugin_errors {
            if self.plugin_errors.get(strip) != Some(e) {
                out.push((
                    "plugin_failed",
                    "warning",
                    format!("{strip}: {e}"),
                    json!({"strip": strip, "error": e}),
                ));
            }
        }
        for strip in self.plugin_errors.keys() {
            if !l.plugin_errors.contains_key(strip) {
                out.push((
                    "plugin_failed",
                    "ok",
                    format!("{strip}: its plugins run again"),
                    json!({"strip": strip, "over": true}),
                ));
            }
        }
        self.plugin_errors = l.plugin_errors.clone();
    }

    fn recorder(&mut self, l: &Look, out: &mut Vec<Ev>) {
        let Some(r) = &l.record else {
            // Stopped: what it had is over with it.
            self.record_error = None;
            self.disk = "";
            self.disk_problem = None;
            return;
        };
        match &r.error {
            Some(e) if self.record_error.as_ref().is_none_or(|p| p["error"] != *e) => {
                let text = format!("the recording {} stopped: {e}", r.folder);
                let f = json!({"folder": r.folder, "path": r.path, "error": e});
                self.record_error = Some(problem("recorder_failed", "error", &text, f.clone()));
                out.push(("recorder_failed", "error", text, f));
            }
            Some(_) => {}
            None => self.record_error = None,
        }
        // Its disk: 5 GB or 5% left, a warning; 1 GB, an error; 10 GB and 10%, fine again
        // (as the switcher's recordings).
        if let Some((free, total)) = r.free {
            let disk = if free < 1_000_000_000 {
                "error"
            } else if free < 5_000_000_000 || free < total / 20 {
                "warning"
            } else if free > 10_000_000_000 && free > total / 10 {
                ""
            } else {
                self.disk
            };
            if disk != self.disk {
                self.disk = disk;
                let gb = (free as f64 / 1e8).round() / 10.0;
                let f = json!({"folder": r.folder, "path": r.path, "free_gb": gb,
                               "free_bytes": free});
                if disk.is_empty() {
                    self.disk_problem = None;
                    let mut f = f;
                    f["over"] = json!(true);
                    out.push((
                        "disk_low",
                        "ok",
                        format!("the recording's disk has room again ({gb} GB left)"),
                        f,
                    ));
                } else {
                    let text = format!("the recording's disk is almost full ({gb} GB left)");
                    self.disk_problem = Some(problem("disk_low", disk, &text, f.clone()));
                    out.push(("disk_low", disk, text, f));
                }
            }
        }
    }

    fn x32(&mut self, l: &Look, now: Instant, out: &mut Vec<Ev>) {
        let Some(x) = &l.x32 else {
            // No X32 set: nothing to reach.
            self.x32_since = None;
            self.x32_problem = None;
            return;
        };
        if x.connected {
            self.x32_since = None;
            if self.x32_problem.take().is_some() {
                out.push((
                    "x32_unreachable",
                    "ok",
                    format!("the X32 at {} answers again", x.address),
                    json!({"address": x.address, "over": true}),
                ));
            }
            return;
        }
        let since = *self.x32_since.get_or_insert(now);
        if self.x32_problem.is_some()
            || (x.error.is_none() && now.duration_since(since) < X32_GRACE)
        {
            return;
        }
        let text = match &x.error {
            Some(e) => format!("the X32 doesn't answer: {e}"),
            None => format!("the X32 at {} doesn't answer", x.address),
        };
        let f = json!({"address": x.address, "error": x.error});
        self.x32_problem = Some(problem("x32_unreachable", "warning", &text, f.clone()));
        out.push(("x32_unreachable", "warning", text, f));
    }

    fn cards(&mut self, l: &Look, now: Instant, out: &mut Vec<Ev>) {
        let parts: BTreeSet<&str> = l.cards.iter().map(|c| c.part).collect();
        self.cards.retain(|p, _| parts.contains(p));
        for c in &l.cards {
            let seen = self.cards.entry(c.part).or_default();
            let counted = seen.errors.is_some_and(|e| c.errors > e);
            seen.errors = Some(c.errors);
            let erred = match &c.error {
                Some(_) => {
                    let since = *seen.bad_since.get_or_insert(now);
                    now.duration_since(since) >= CARD_GRACE
                }
                None => {
                    seen.bad_since = None;
                    false
                }
            };
            let bad = counted || c.error.is_some();
            if bad {
                seen.last_bad = Some(now);
            }
            if (counted || erred) && seen.down.is_none() {
                let why = c
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("{} errors", c.errors));
                let text = format!("the sound card's {} ({}): {why}", c.part, c.device);
                let f = json!({"part": c.part, "device": c.device, "error": why,
                               "errors": c.errors});
                seen.down = Some(problem("card_lost", "error", &text, f.clone()));
                out.push(("card_lost", "error", text, f));
            } else if !bad
                && seen.down.is_some()
                && seen
                    .last_bad
                    .is_none_or(|t| now.duration_since(t) >= CARD_CALM)
            {
                seen.down = None;
                out.push((
                    "card_lost",
                    "ok",
                    format!("the sound card's {} ({}) works again", c.part, c.device),
                    json!({"part": c.part, "device": c.device, "over": true}),
                ));
            }
        }
    }

    /// Blocks skipped (the clock fell behind) or [`SLOW_PER_ROUND`] taking longer than
    /// their time start an overload; [`CALM_AFTER`] without either ends it.
    fn load(&mut self, l: &Look, now: Instant, out: &mut Vec<Ev>) {
        let grew = self.late.is_some_and(|(late, slow)| {
            l.late_blocks > late || l.slow_blocks.saturating_sub(slow) >= SLOW_PER_ROUND
        });
        self.late = Some((l.late_blocks, l.slow_blocks));
        let load_pct = if l.period_ms > 0.0 {
            (l.block_ms / l.period_ms * 100.0).round()
        } else {
            0.0
        };
        let f = json!({"part": "dsp", "block_ms": l.block_ms, "period_ms": l.period_ms,
                       "dsp_load_pct": load_pct, "late_blocks": l.late_blocks,
                       "slow_blocks": l.slow_blocks});
        match (grew, self.overloaded.is_some()) {
            (true, false) => {
                let text = format!(
                    "the desk can't keep up: blocks late (one takes {:.2} ms of its {:.2})",
                    l.block_ms, l.period_ms
                );
                self.overloaded = Some((now, problem("overloaded", "warning", &text, f.clone())));
                self.calm_since = None;
                out.push(("overloaded", "warning", text, f));
            }
            (true, true) => self.calm_since = None,
            (false, true) => {
                let calm = *self.calm_since.get_or_insert(now);
                if now.duration_since(calm) >= CALM_AFTER {
                    self.overloaded = None;
                    self.calm_since = None;
                    out.push((
                        "overloaded",
                        "ok",
                        "the desk keeps up again".to_owned(),
                        json!({"part": "dsp", "over": true}),
                    ));
                }
            }
            (false, false) => {}
        }
    }

    /// What is wrong now.
    pub fn problems(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.lost.values().cloned().collect();
        out.extend(self.failover.values().cloned());
        out.extend(self.plugin_errors.iter().map(|(strip, e)| {
            problem(
                "plugin_failed",
                "warning",
                &format!("{strip}: {e}"),
                json!({"strip": strip, "error": e}),
            )
        }));
        out.extend(self.record_error.iter().cloned());
        out.extend(self.disk_problem.iter().cloned());
        out.extend(self.x32_problem.iter().cloned());
        out.extend(self.cards.values().filter_map(|c| c.down.clone()));
        if let Some((since, p)) = &self.overloaded {
            let mut p = p.clone();
            p["for_s"] = json!(since.elapsed().as_secs());
            out.push(p);
        }
        out
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The desk as one round looks at it.
pub(super) fn look(sh: &Shared) -> Look {
    let mut l = Look::default();
    // Routed transmitters, and when each was last heard.
    if let Some(rx) = sh.receiver.lock_ok().clone() {
        let heard = sh.heard.lock_ok();
        let routable = sh.iem.first_talkback - 1;
        for r in rx.routes() {
            if r.ch as usize >= routable {
                continue;
            }
            let e = l.sources.entry(r.tx.clone()).or_default();
            e.inputs.push(r.ch as usize + 1);
            e.heard_ago = heard.get(&r.tx).map(Instant::elapsed);
        }
    }
    l.listed = sh.listed.lock_ok().clone();
    let x32_address = {
        let d = sh.desk.lock_ok();
        let s = d.settings_arc();
        for (i, c) in s.channels.iter().enumerate() {
            if d.uses_b(i) && !c.use_b {
                l.failover.insert(
                    i + 1,
                    FailoverLook {
                        name: if c.name.is_empty() {
                            format!("Ch {}", i + 1)
                        } else {
                            c.name.clone()
                        },
                        input: c.source.unwrap_or(i + 1),
                        backup: c.input_b,
                    },
                );
            }
        }
        s.x32.address.trim().to_owned()
    };
    // (The X32's slot with the desk let go.)
    if !x32_address.is_empty() {
        let slot = sh.x32.lock_ok();
        l.x32 = Some(X32Look {
            address: x32_address,
            connected: slot.client.as_ref().is_some_and(|x| x.connected()),
            error: slot.error(),
        });
    }
    l.plugin_restarts = *sh.plugins.restarts.lock_ok();
    for e in sh.plugins.errors() {
        l.plugin_errors.insert(
            s(&e, "strip").to_owned(),
            e["error"].as_str().unwrap_or("").to_owned(),
        );
    }
    l.record = sh.recorder.lock_ok().as_ref().map(|r| {
        let st = r.status();
        RecordLook {
            folder: s(&st, "folder").to_owned(),
            path: r.dir.to_string_lossy().into_owned(),
            error: st["error"].as_str().map(str::to_owned),
            free: None,
        }
    });
    // (The disk asked with the recorder let go.)
    if let Some(r) = &mut l.record {
        r.free = idn_open_protocol::diag::free_space(std::path::Path::new(&r.path));
    }
    let cards = sh.cards_status.lock_ok().clone();
    if let Some(st) = cards.map(|c| c.lock_ok().clone()) {
        for (part, count) in [("input", "errors"), ("output", "device_errors")] {
            let c = &st[part];
            if c.is_object() {
                l.cards.push(CardLook {
                    part,
                    device: c["device"].as_str().unwrap_or("default").to_owned(),
                    error: c["error"].as_str().map(str::to_owned),
                    errors: c[count].as_u64().unwrap_or(0),
                });
            }
        }
    }
    if let (Some(cfg), Some(n)) = (&sh.cfg.network_audio, *sh.net_audio.lock_ok()) {
        let j = n.json(cfg);
        let state = s(&j, "state");
        l.cards.push(CardLook {
            part: "network",
            device: s(&j, "device").to_owned(),
            error: (state != "running").then(|| state.to_owned()),
            errors: 0,
        });
    }
    l.late_blocks = sh.late.load(Ordering::Relaxed);
    l.slow_blocks = sh.slow_blocks.load(Ordering::Relaxed);
    l.block_ms = sh.block_us.load(Ordering::Relaxed) as f64 / 1000.0;
    l.period_ms = sh.desk.lock_ok().block() as f64 * 1000.0 / f64::from(rate_hz());
    l
}

/// One round of the watch: the desk looked at, each change an event (to the push
/// channels and the site's log); the outputs' origin brought up to date.
pub(super) fn watch(sh: &Shared) {
    let l = look(sh);
    let evs = sh.watcher.lock_ok().round(&l, Instant::now());
    // (No `desk` field: it would be every event's subject in the log's deduplication;
    // the Node says which program.)
    for (name, level, text, f) in evs {
        sh.events.emit(name, level, &text, f);
    }
    tell_origin(sh);
    super::endpoints::reconcile(sh);
}

/// The desk's outputs' origin (idn-routing.md §A.16): a desk, its name, each output's
/// bus or channel; said again when a name changes.
pub(super) fn tell_origin(sh: &Shared) {
    let labels: Vec<String> = {
        let d = sh.desk.lock_ok();
        let s = d.settings_arc();
        (0..d.io().1).map(|o| s.output_label(o)).collect()
    };
    let mut sent = sh.origin.lock_ok();
    if *sent == labels {
        return;
    }
    sh.out
        .set_audio_origin(&idn_open_protocol::node::routing::AudioOrigin {
            kind: "desk".into(),
            device: sh.cfg.name.clone(),
            channels: labels.clone(),
        });
    *sent = labels;
}

impl Console {
    /// The desk's typed events (§M.27): for a host's own push channel or log.
    pub fn failure_events(&self) -> &Arc<idn_open_protocol::web::problems::Events> {
        &self.shared.events
    }

    /// What is wrong now (§M.27): the state's `problems`.
    pub fn problems(&self) -> Vec<Value> {
        self.shared.watcher.lock_ok().problems()
    }

    /// One round of the failure watch now (it runs each second by itself).
    pub fn watch_problems(&self) {
        watch(&self.shared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(name: &str, f: impl FnOnce(&mut ConsoleConfig)) -> Console {
        let mut cfg = ConsoleConfig {
            name: name.into(),
            channels: 4,
            inputs: 4,
            mdns: Some(false),
            iem: crate::iem::IemConfig {
                port: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        f(&mut cfg);
        Console::start(cfg).unwrap()
    }

    /// The events named `name` so far (watching now and then meanwhile), within `max`.
    fn wait_event(c: &Console, name: &str, level: &str, max: Duration) -> Value {
        let end = Instant::now() + max;
        loop {
            c.watch_problems();
            let (all, _) = c.failure_events().since(0);
            if let Some(e) = all
                .into_iter()
                .find(|e| e["event"] == name && e["level"] == level)
            {
                return e;
            }
            assert!(Instant::now() < end, "no {name} ({level}) event");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn has_problem(c: &Console, name: &str) -> bool {
        c.problems().iter().any(|p| p["problem"] == name)
    }

    /// A channel's backup input takes over when nothing arrives on its first one: a
    /// `failover` event and problem; back on its first, the same `over`.
    #[test]
    fn a_backup_input_taking_over_is_an_event() {
        let c = console("Failover test", |_| {});
        c.command(
            "channel",
            &json!({"n": 1, "name": "Pastor", "input_b": 2, "failover": true, "shown": true}),
        )
        .unwrap();
        let live = |a: bool| {
            let mut l = vec![true; 4 + crate::iem::TALKBACK_INPUTS];
            l[0] = a;
            c.shared.desk.lock_ok().set_live(l);
        };
        live(false);
        let e = wait_event(&c, "failover", "warning", Duration::from_secs(5));
        assert_eq!(
            (
                e["channel"].as_u64(),
                e["input"].as_u64(),
                e["backup"].as_u64()
            ),
            (Some(1), Some(1), Some(2)),
            "{e}"
        );
        assert_eq!(e["type"], "event");
        assert!(
            c.state()["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["problem"] == "failover"),
        );
        live(true);
        let e = wait_event(&c, "failover", "ok", Duration::from_secs(5));
        assert_eq!(e["over"], true);
        // (Only this one: a loaded test machine may be `overloaded` too.)
        assert!(!has_problem(&c, "failover"));
    }

    /// The X32 the setting names doesn't answer (its fake stopped): `x32_unreachable`;
    /// one that answers: the same `over`.
    #[test]
    fn an_x32_not_answering_is_an_event() {
        let gone = crate::x32::fake::Fake::start("X32", Duration::from_secs(10));
        let addr = gone.addr.to_string();
        drop(gone);
        let c = console("X32 events test", |cfg| cfg.x32 = Some(addr.clone()));
        let e = wait_event(&c, "x32_unreachable", "warning", Duration::from_secs(8));
        assert_eq!(e["address"], json!(addr), "{e}");
        assert!(
            c.problems()
                .iter()
                .any(|p| p["problem"] == "x32_unreachable")
        );
        let fake = crate::x32::fake::Fake::start("X32", Duration::from_secs(10));
        c.desk(&json!({"x32": {"address": fake.addr.to_string()}}))
            .unwrap();
        let e = wait_event(&c, "x32_unreachable", "ok", Duration::from_secs(8));
        assert_eq!(e["over"], true);
        assert!(!has_problem(&c, "x32_unreachable"));
    }

    /// The clock skipping blocks (forced: the count it keeps) makes the desk overloaded.
    #[test]
    fn blocks_late_make_the_desk_overloaded() {
        let c = console("Overload test", |_| {});
        c.watch_problems();
        c.shared.late.fetch_add(25, Ordering::Relaxed);
        let e = wait_event(&c, "overloaded", "warning", Duration::from_secs(3));
        assert_eq!(e["part"], "dsp");
        assert!(e["late_blocks"].as_u64().unwrap() >= 25);
        assert!(c.problems().iter().any(|p| p["problem"] == "overloaded"));
        // The newest first, as `GET /api/v1/events/recent` gives them.
        assert_eq!(c.failure_events().newest(1)[0]["event"], "overloaded");
    }

    /// The desk's receiver is a desk; its output says its sound is the desk's buses, and
    /// says it again when a bus is renamed.
    #[test]
    fn the_outputs_origin_follows_the_bus_names() {
        let c = console("Origin test", |_| {});
        let first = c.shared.origin.lock_ok().clone();
        assert!(!first.is_empty());
        c.command("bus", &json!({"bus": "main", "name": "Wedges"}))
            .unwrap();
        c.watch_problems();
        let now = c.shared.origin.lock_ok().clone();
        assert_ne!(first, now);
        assert!(now.iter().any(|l| l.contains("Wedges")), "{now:?}");
    }

    fn names(e: &[Ev]) -> Vec<&str> {
        e.iter().map(|x| x.0).collect()
    }

    fn src(inputs: &[usize], ago: Option<u64>) -> SourceLook {
        SourceLook {
            inputs: inputs.to_vec(),
            heard_ago: ago.map(Duration::from_millis),
        }
    }

    /// A routed source silent for 2 s is lost (not listed: `not_found`), back when heard;
    /// one never heard is lost after 5 s; a route taken away is forgotten.
    #[test]
    fn routed_sources_lost_and_back() {
        let t0 = Instant::now();
        let mut w = Watcher::default();
        let tx = "STAGE (Box)".to_owned();
        let mut l = Look::default();
        l.sources.insert(tx.clone(), src(&[1, 2], Some(10)));
        assert!(w.round(&l, t0).is_empty());
        l.sources.insert(tx.clone(), src(&[1, 2], Some(2500)));
        let e = w.round(&l, t0 + Duration::from_secs(1));
        assert_eq!(names(&e), ["lost"]);
        assert_eq!(
            (e[0].3["cause"].as_str(), e[0].3["inputs"].clone()),
            (Some("unreachable"), json!([1, 2]))
        );
        assert_eq!(w.problems()[0]["problem"], "lost");
        assert!(w.round(&l, t0 + Duration::from_secs(2)).is_empty());
        l.sources.insert(tx.clone(), src(&[1, 2], Some(5)));
        assert_eq!(
            names(&w.round(&l, t0 + Duration::from_secs(3))),
            ["recovered"]
        );
        assert!(w.problems().is_empty());
        // Not on the network: not found.
        l.listed = Some(BTreeSet::new());
        l.sources.insert(tx.clone(), src(&[1, 2], Some(3000)));
        let e = w.round(&l, t0 + Duration::from_secs(4));
        assert_eq!(e[0].3["cause"], "not_found");
        // Taken away: forgotten, nothing said.
        l.sources.clear();
        assert!(w.round(&l, t0 + Duration::from_secs(5)).is_empty());
        assert!(w.problems().is_empty());
        // Never heard: lost after 5 s.
        l.listed = None;
        l.sources.insert("B (C)".into(), src(&[3], None));
        assert!(w.round(&l, t0 + Duration::from_secs(6)).is_empty());
        assert_eq!(names(&w.round(&l, t0 + Duration::from_secs(12))), ["lost"]);
    }

    /// A backup input taking over, and back.
    #[test]
    fn failover_and_back() {
        let t0 = Instant::now();
        let mut w = Watcher::default();
        let mut l = Look::default();
        l.failover.insert(
            1,
            FailoverLook {
                name: "Pastor".into(),
                input: 1,
                backup: 2,
            },
        );
        let e = w.round(&l, t0);
        assert_eq!((e[0].0, e[0].1), ("failover", "warning"));
        assert_eq!(
            (e[0].3["channel"].as_u64(), e[0].3["backup"].as_u64()),
            (Some(1), Some(2))
        );
        assert!(w.round(&l, t0).is_empty());
        assert_eq!(w.problems().len(), 1);
        l.failover.clear();
        let e = w.round(&l, t0);
        assert_eq!(
            (e[0].0, e[0].1, e[0].3["over"].as_bool()),
            ("failover", "ok", Some(true))
        );
    }

    /// The plugin host's restart and a strip's error; the recorder's error and its disk;
    /// the X32 not answering; a card's errors; the DSP late.
    #[test]
    fn plugins_recorder_x32_cards_and_load() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watcher::default();
        let mut l = Look {
            period_ms: 2.0,
            ..Default::default()
        };
        assert!(w.round(&l, at(0)).is_empty());
        // Plugins.
        l.plugin_restarts = 1;
        l.plugin_errors.insert("channel 3".into(), "crashed".into());
        let e = w.round(&l, at(1));
        assert_eq!(names(&e), ["plugin_failed", "plugin_failed"]);
        assert_eq!(e[0].3["strip"], "host");
        l.plugin_errors.clear();
        assert_eq!(w.round(&l, at(2))[0].1, "ok");
        // The recorder.
        l.record = Some(RecordLook {
            folder: "Sunday".into(),
            path: "/rec/Sunday".into(),
            error: None,
            free: Some((50_000_000_000, 100_000_000_000)),
        });
        assert!(w.round(&l, at(3)).is_empty());
        l.record.as_mut().unwrap().free = Some((3_000_000_000, 100_000_000_000));
        let e = w.round(&l, at(4));
        assert_eq!((e[0].0, e[0].1), ("disk_low", "warning"));
        l.record.as_mut().unwrap().free = Some((500_000_000, 100_000_000_000));
        l.record.as_mut().unwrap().error = Some("No space left on device".into());
        let e = w.round(&l, at(5));
        assert_eq!(names(&e), ["recorder_failed", "disk_low"]);
        assert_eq!(e[1].1, "error");
        assert_eq!(w.problems().len(), 2);
        l.record = None;
        assert!(w.round(&l, at(6)).is_empty());
        assert!(w.problems().is_empty());
        // The X32: not answering for 3 s, then answering.
        l.x32 = Some(X32Look {
            address: "10.0.0.9".into(),
            ..Default::default()
        });
        assert!(w.round(&l, at(7)).is_empty());
        let e = w.round(&l, at(10));
        assert_eq!(names(&e), ["x32_unreachable"]);
        assert!(w.round(&l, at(11)).is_empty());
        l.x32.as_mut().unwrap().connected = true;
        assert_eq!(w.round(&l, at(12))[0].3["over"], true);
        // A card's errors: lost at once; back after 5 s without.
        l.cards.push(CardLook {
            part: "output",
            device: "USB".into(),
            error: None,
            errors: 0,
        });
        assert!(w.round(&l, at(13)).is_empty());
        l.cards[0].errors = 3;
        assert_eq!(names(&w.round(&l, at(14))), ["card_lost"]);
        assert!(w.round(&l, at(15)).is_empty());
        assert_eq!(w.round(&l, at(20))[0].1, "ok");
        // An error for 3 s.
        l.cards[0].error = Some("device gone".into());
        assert!(w.round(&l, at(21)).is_empty());
        assert_eq!(w.round(&l, at(24))[0].0, "card_lost");
        // The DSP: blocks skipped, then 10 s calm.
        l.cards.clear();
        l.late_blocks = 4;
        l.block_ms = 2.4;
        let e = w.round(&l, at(25));
        assert_eq!(
            (e[0].0, e[0].3["dsp_load_pct"].as_f64()),
            ("overloaded", Some(120.0))
        );
        assert!(w.round(&l, at(26)).is_empty());
        assert!(w.problems().iter().any(|p| p["problem"] == "overloaded"));
        assert_eq!(w.round(&l, at(37))[0].1, "ok");
        l.slow_blocks = SLOW_PER_ROUND;
        assert_eq!(names(&w.round(&l, at(38))), ["overloaded"]);
    }
}
