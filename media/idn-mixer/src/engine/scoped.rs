//! What a caller may do and see on the desk (idn-node.md §N.31.5, spec §M.25): everything
//! (`control`, this machine, OSC, a surface, the hosting program), reading only (`read`),
//! one bus (`bus:…`): the channels' sends into it and its own master, with the state,
//! meters and control limits cut down to that bus; or one IEM mix (`iem:<mix id>`,
//! §M.26): that mix's changes alone, the state cut down to the IEM channels and that mix.
//!
//! A `bus:` scope names its bus as `main`, `aux:N`, `group:N`, `matrix:N` or `name:NAME`
//! (a bare `NAME` is the same: `bus:stream`). A name is found when it is checked, among
//! the main, aux, group and matrix buses' names, by its letters and digits alone (case
//! and the rest ignored: `bus:name:live-stream` is the bus named "Live Stream"); a name
//! no bus has, or more than one has, allows nothing.

use super::*;
use crate::desk::{AUX, GROUPS, MATRIX, bus_strip};
use idn_open_protocol::trust::token::Scope;
use idn_open_protocol::web::Who;

/// The bus a `bus:` scope names (the part after `bus:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusSel {
    Main,
    /// The stream bus.
    Stream,
    /// From 1.
    Aux(usize),
    Group(usize),
    Matrix(usize),
    /// By its name, as [`name_key`] gives it.
    Named(String),
}

/// A name as compared: its letters and digits in lower case, every run of anything else
/// one `-` ("Live Stream", "live_stream" and "LIVE-STREAM" are all `live-stream`).
pub fn name_key(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

impl BusSel {
    /// `main`, `stream`, `aux:N` (1–16), `group:N` (1–8), `matrix:N` (1–8), `name:NAME` or
    /// `NAME`.
    pub fn parse(s: &str) -> Result<BusSel, String> {
        let num = |n: &str, max: usize, kind: &str| {
            n.parse::<usize>()
                .ok()
                .filter(|n| (1..=max).contains(n))
                .ok_or(format!("bus:{kind}:N takes N from 1 to {max}"))
        };
        let named = |n: &str| {
            let k = name_key(n);
            if k.is_empty() {
                Err(format!("bus {s:?}: no name"))
            } else {
                Ok(BusSel::Named(k))
            }
        };
        match s.split_once(':') {
            None if s == "main" => Ok(BusSel::Main),
            None if s == "stream" => Ok(BusSel::Stream),
            None => named(s),
            Some(("aux", n)) => num(n, AUX, "aux").map(BusSel::Aux),
            Some(("group", n)) => num(n, GROUPS, "group").map(BusSel::Group),
            Some(("matrix", n)) => num(n, MATRIX, "matrix").map(BusSel::Matrix),
            Some(("name", n)) => named(n),
            _ => Err(format!(
                "bus {s:?}: main, stream, aux:N, group:N, matrix:N or name:NAME"
            )),
        }
    }

    /// The bus it is on desk `s` now: a name found on exactly one of the main, aux, group
    /// and matrix buses.
    pub fn resolve(&self, s: &Settings) -> Result<BusTarget, String> {
        let Self::Named(key) = self else {
            return Ok(match *self {
                Self::Main => BusTarget::Main,
                Self::Stream => BusTarget::Stream,
                Self::Aux(n) => BusTarget::Aux(n),
                Self::Group(n) => BusTarget::Group(n),
                Self::Matrix(n) => BusTarget::Matrix(n),
                Self::Named(_) => unreachable!(),
            });
        };
        let all = [BusTarget::Main, BusTarget::Stream]
            .into_iter()
            .chain((1..=AUX).map(BusTarget::Aux))
            .chain((1..=GROUPS).map(BusTarget::Group))
            .chain((1..=MATRIX).map(BusTarget::Matrix));
        let found: Vec<BusTarget> = all
            .filter(|t| s.bus(t.strip()).is_some_and(|b| name_key(&b.name) == *key))
            .collect();
        match found.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!("no bus named {key:?} on this desk")),
            more => Err(format!(
                "{key:?} names more than one bus ({})",
                more.iter()
                    .map(|t| t.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// A bus a token is limited to, as found on the desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusTarget {
    Main,
    Stream,
    /// From 1.
    Aux(usize),
    Group(usize),
    Matrix(usize),
}

/// What a bus token may change on its bus's own strip: its master and processing.
const BUS_KEYS: [&str; 6] = ["fader", "mute", "eq_on", "eq", "comp", "limiter"];

/// What it may change in a channel's send into an aux bus (`pan` only into a stereo one).
const SEND_KEYS: [&str; 3] = ["on", "level", "pre"];

impl BusTarget {
    /// Its bus strip (as [`crate::desk::named_bus`] gives it).
    pub fn strip(self) -> usize {
        match self {
            Self::Main => 0,
            Self::Stream => crate::desk::STREAM_STRIP,
            Self::Aux(n) => n,
            Self::Group(n) => AUX + n,
            Self::Matrix(n) => AUX + GROUPS + n,
        }
    }

    /// Its kind and number (`aux`, 5; `main`, 1).
    pub fn kind(self) -> (&'static str, usize) {
        match self {
            Self::Main => ("main", 1),
            Self::Stream => ("stream", 1),
            Self::Aux(n) => ("aux", n),
            Self::Group(n) => ("group", n),
            Self::Matrix(n) => ("matrix", n),
        }
    }

    /// `main`, `aux 5`, `group 2`, `matrix 1`.
    pub fn label(self) -> String {
        crate::desk::bus_strip_name(self.strip())
    }

    /// Its command (`{"cmd": "bus", "bus": 5}`, `{"cmd": "matrix", "matrix": 1}`).
    pub fn command(self) -> Value {
        match self.kind() {
            ("main", _) => json!({"cmd": "bus", "bus": "main"}),
            ("stream", _) => json!({"cmd": "bus", "bus": "stream"}),
            ("aux", n) => json!({"cmd": "bus", "bus": n}),
            (kind, n) => json!({"cmd": kind, kind: n}),
        }
    }

    /// It has a graphic EQ (the main, aux and matrix buses).
    fn has_geq(self) -> bool {
        !matches!(self, Self::Group(_))
    }

    /// It is stereo (an aux bus as set; the others always).
    fn stereo(self, s: &Settings) -> bool {
        match self {
            Self::Aux(n) => s.aux.get(n - 1).is_some_and(|b| b.stereo),
            _ => true,
        }
    }

    /// Whether command `cmd` with body `v` changes this bus alone: a channel's send into
    /// it (`channel`, `channels`, `send` into an aux bus), its own strip's master and
    /// processing (`bus`, `group`, `matrix`). Everything else is refused, saying why.
    pub fn permits(self, s: &Settings, cmd: &str, v: &Value) -> Result<(), String> {
        let outside = |what: &str| -> Result<(), String> {
            Err(format!(
                "outside this token's bus ({}): {what}",
                self.label()
            ))
        };
        let without = |key: &str| -> Value {
            let mut p = v.clone();
            if let Some(o) = p.as_object_mut() {
                o.remove(key);
            }
            p
        };
        match cmd {
            "channel" | "channels" => match self.channel_patch(s, &without("n")) {
                Ok(()) => Ok(()),
                Err(e) => outside(&e),
            },
            // One channel's send (§M.26's convenience): as the channel patch it makes.
            "send" => match super::iem_api::send_target(v)
                .and_then(|(_, bus)| super::iem_api::send_patch(bus, v))
            {
                Ok(p) => match self.channel_patch(s, &p) {
                    Ok(()) => Ok(()),
                    Err(e) => outside(&e),
                },
                Err(e) => outside(&e),
            },
            "bus" | "group" | "matrix" => {
                let k = if cmd == "bus" {
                    crate::desk::named_bus(&json!({ "bus": v["bus"] }))
                } else {
                    crate::desk::named_bus(v)
                };
                match k {
                    Ok(k) if k == self.strip() => match self.bus_patch(&without(cmd)) {
                        Ok(()) => Ok(()),
                        Err(e) => outside(&e),
                    },
                    Ok(k) => outside(&format!("{} is another bus", bus_strip_name(k))),
                    Err(e) => outside(&e),
                }
            }
            other => outside(&format!("{other} is not a change of this bus")),
        }
    }

    /// A channel merge patch that changes only the channel's send into this bus: `sends`
    /// with only this aux bus's element (`on`, `level`, `pre`, and `pan` into a stereo
    /// aux), `groups` with only this group's, `main` for the main bus.
    fn channel_patch(self, s: &Settings, p: &Value) -> Result<(), String> {
        let o = p.as_object().ok_or("a channel's settings")?;
        // One element of a list, the others null (left as they are).
        let only = |list: &Value, i: usize, what: &str| -> Result<Option<Value>, String> {
            let a = list
                .as_array()
                .ok_or(format!("{what}: a list, one entry per bus"))?;
            for (j, e) in a.iter().enumerate() {
                if j != i && !e.is_null() {
                    return Err(format!("{what} {}: another bus", j + 1));
                }
            }
            Ok(a.get(i).filter(|e| !e.is_null()).cloned())
        };
        for (key, val) in o {
            match (self, key.as_str()) {
                (Self::Aux(n), "sends") => {
                    let Some(send) = only(val, n - 1, "sends")? else {
                        continue;
                    };
                    let send = send.as_object().ok_or("a send's settings")?;
                    let stereo = self.stereo(s);
                    for k in send.keys() {
                        if !(SEND_KEYS.contains(&k.as_str()) || (k == "pan" && stereo)) {
                            return Err(if k == "pan" {
                                "pan: this aux bus is mono".into()
                            } else {
                                format!("a send's {k}")
                            });
                        }
                    }
                }
                (Self::Group(n), "groups") => {
                    if let Some(on) = only(val, n - 1, "groups")?
                        && !on.is_boolean()
                    {
                        return Err("groups: true or false".into());
                    }
                }
                (Self::Main, "main") => {}
                (Self::Stream, "stream") => {
                    let send = val.as_object().ok_or("stream: a send's settings")?;
                    for k in send.keys() {
                        if !(SEND_KEYS.contains(&k.as_str()) || k == "pan") {
                            return Err(format!("a send's {k}"));
                        }
                    }
                }
                (Self::Matrix(_), _) => {
                    return Err("a matrix mix takes buses, not channels".into());
                }
                (_, k) => return Err(format!("a channel's {k}")),
            }
        }
        Ok(())
    }

    /// A bus merge patch that changes only this bus's master and processing: fader, mute,
    /// EQ, compressor, limiter, graphic EQ (main, aux, matrix), sources (a matrix mix).
    fn bus_patch(self, p: &Value) -> Result<(), String> {
        let o = p.as_object().ok_or("the bus's settings")?;
        for k in o.keys() {
            let ok = BUS_KEYS.contains(&k.as_str())
                || (k == "geq" && self.has_geq())
                || (k == "sources" && matches!(self, Self::Matrix(_)));
            if !ok {
                return Err(format!("the bus's {k}"));
            }
        }
        Ok(())
    }

    /// The bus's own settings a bus token sees and changes, as the full state has them.
    fn settings_of(self, b: &Bus) -> Value {
        let all = serde_json::to_value(b).unwrap_or_default();
        let mut out = json!({});
        for k in BUS_KEYS
            .iter()
            .chain(self.has_geq().then_some(&"geq"))
            .chain(matches!(self, Self::Matrix(_)).then_some(&"sources"))
        {
            out[*k] = all[*k].clone();
        }
        out
    }

    /// The control limits for this bus (`GET /api/v1/controls` cut down): its strip's,
    /// and a send's into an aux bus.
    pub fn controls(self, s: &Settings) -> Value {
        let all = super::controls();
        let mut bus = json!({});
        for k in BUS_KEYS
            .iter()
            .chain(self.has_geq().then_some(&"geq"))
            .chain(matches!(self, Self::Matrix(_)).then_some(&"sources"))
        {
            if !all["bus"][*k].is_null() {
                bus[*k] = all["bus"][*k].clone();
            }
        }
        let mut v = json!({"off_db": all["off_db"], "bus": bus});
        if self == Self::Stream {
            v["channel"] = json!({ "stream": all["channel"]["sends"].clone() });
        }
        if let Self::Aux(_) = self {
            let mut send = all["channel"]["sends"].clone();
            if !self.stereo(s) {
                send.as_object_mut().map(|o| o.remove("pan"));
            }
            v["channel"] = json!({ "sends": send });
        }
        v
    }
}

/// What a caller may do on the desk now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Everything.
    All,
    /// Reading only (the whole state), and why changes are refused: `read`, a channel
    /// still waiting for its token.
    ReadOnly(String),
    /// One bus: the scope as written, and the bus as found (or why none).
    Bus {
        scope: String,
        target: Result<BusTarget, String>,
    },
    /// One IEM mix (§M.26): the scope as written and the mix's id.
    Iem { scope: String, mix: String },
}

impl Access {
    /// `who`'s on desk `s`.
    pub fn of(who: &Who, s: &Settings) -> Access {
        match &who.scope {
            Scope::Bus(b) if who.ready() => Access::Bus {
                scope: who.scope.as_string(),
                target: BusSel::parse(b).and_then(|sel| sel.resolve(s)),
            },
            Scope::Iem(m) if who.ready() => Access::Iem {
                scope: who.scope.as_string(),
                mix: m.clone(),
            },
            _ => match who.may_change() {
                Ok(()) => Access::All,
                Err(e) => Access::ReadOnly(e.to_owned()),
            },
        }
    }

    /// Whether it sees the whole desk (not one bus or IEM mix).
    pub fn whole_desk(&self) -> bool {
        !matches!(self, Access::Bus { .. } | Access::Iem { .. })
    }
}

/// A refusal of something an IEM mix's token may not do or read.
pub fn outside_mix(mix: &str, what: &str) -> String {
    format!("outside this token's IEM mix ({mix}): {what}")
}

/// Whether `who` is limited to one IEM mix (§M.26; its reads too).
pub fn limited_to_mix(who: &Who) -> bool {
    who.ready() && matches!(who.scope, Scope::Iem(_))
}

/// Whether `who` sees only part of the desk (one bus or one IEM mix): no audit, a state of
/// its own.
pub fn limited(who: &Who) -> bool {
    limited_to_bus(who) || limited_to_mix(who)
}

/// Whether an `iem:<mix>` token may run `cmd` with body `v`: `iem_mix` on its own mix
/// alone (§M.26).
fn mix_permits(mix: &str, cmd: &str, v: &Value) -> Result<(), String> {
    match cmd {
        "iem_mix" if v["id"].as_str() == Some(mix) => Ok(()),
        "iem_mix" => Err(outside_mix(mix, "another mix")),
        other => Err(outside_mix(
            mix,
            &format!("{other} is not a change of this mix"),
        )),
    }
}

/// A refusal of something a bus token may not read: `outside this token's bus: WHAT`.
pub fn outside_bus(what: &str) -> String {
    format!("outside this token's bus: {what}")
}

/// Whether `who` is limited to one bus (its reads too).
pub fn limited_to_bus(who: &Who) -> bool {
    who.ready() && matches!(who.scope, Scope::Bus(_))
}

impl Console {
    /// What `who` may do on the desk now (a bus by name found now).
    pub fn access(&self, who: &Who) -> Access {
        if let (true, Scope::Iem(m)) = (limited_to_mix(who), &who.scope) {
            return Access::Iem {
                scope: who.scope.as_string(),
                mix: m.clone(),
            };
        }
        if !limited_to_bus(who) {
            // (The settings copied only to find a bus: not on every fader move.)
            return match who.may_change() {
                Ok(()) => Access::All,
                Err(e) => Access::ReadOnly(e.to_owned()),
            };
        }
        Access::of(who, &self.settings())
    }

    /// Whether `who` may run command `cmd` with body `v` (idn-node.md §N.31.5): `Err`
    /// with why not ("read-only", "outside this token's bus (aux 5): …"). A bus token may
    /// change the channels' sends into its bus and the bus's master; nothing else.
    pub fn permits(&self, who: &Who, cmd: &str, v: &Value) -> Result<(), String> {
        if let (true, Scope::Iem(m)) = (limited_to_mix(who), &who.scope) {
            return mix_permits(m, cmd, v);
        }
        if !limited_to_bus(who) {
            return who.may_change().map_err(str::to_owned);
        }
        let s = self.settings();
        match Access::of(who, &s) {
            Access::All => Ok(()),
            Access::ReadOnly(e) => Err(e),
            Access::Iem { mix, .. } => mix_permits(&mix, cmd, v),
            Access::Bus { target, .. } => {
                let t = target.map_err(|e| format!("outside this token's bus ({e})"))?;
                t.permits(&s, cmd, v)
            }
        }
    }

    /// The state as `who` may see it: the whole of it ([`Console::state`]), or for a bus
    /// token [`Console::bus_state`].
    pub fn state_for(&self, who: &Who) -> Value {
        match self.access(who) {
            Access::Bus { scope, target } => self.bus_state(&scope, &target),
            Access::Iem { scope, mix } => self.iem_token_state(&scope, &mix),
            _ => self.state(),
        }
    }

    /// The meters as `who` may see them: all ([`Console::meters`]), or for a bus token
    /// the channels' (`ch`), the bus's two channels (`bus`: [peak, RMS] each), its
    /// compressor's and limiter's reduction (`bus_gr`) and, for the main bus, `lufs`.
    pub fn meters_for(&self, who: &Who) -> Value {
        let all = self.meters();
        match self.access(who) {
            Access::Bus { target, .. } => {
                let mut v = json!({"ch": all["ch"], "bus": null, "bus_gr": null});
                if let Ok(t) = target {
                    let first = bus_strip(t.strip()).1;
                    v["bus"] = json!([all["out"][first], all["out"][first + 1]]);
                    v["bus_gr"] = all["bus_gr"][t.strip()].clone();
                    if t == BusTarget::Main {
                        v["lufs"] = all["lufs"].clone();
                    }
                    if t == BusTarget::Stream {
                        v["lufs"] = all["stream_lufs"].clone();
                    }
                }
                v
            }
            // The IEM channels' rows, in the flow's order (§M.26).
            Access::Iem { .. } => {
                let ids = crate::iem::marked(&self.settings());
                json!({"ch": ids.iter().map(|&i| all["ch"][i].clone()).collect::<Vec<_>>()})
            }
            _ => all,
        }
    }

    /// The control limits as `who` may use them: all ([`controls`]), or a bus token's
    /// ([`BusTarget::controls`]).
    pub fn controls_for(&self, who: &Who) -> Value {
        if limited_to_mix(who) {
            return json!({"off_db": super::controls::OFF_DB, "iem_mix": controls()["iem_mix"]});
        }
        if !limited_to_bus(who) {
            return controls();
        }
        let s = self.settings();
        match Access::of(who, &s) {
            Access::Bus { target: Ok(t), .. } => t.controls(&s),
            Access::Bus { .. } => json!({"off_db": super::controls::OFF_DB}),
            _ => controls(),
        }
    }

    /// The state for a bus token (§M.25): the desk's name, version and lock; the bus
    /// (`kind`, `index`, `number`, `label`, `color`, `stereo`, `pre`, its `command`, and
    /// `settings`: fader, mute, EQ, dynamics, graphic EQ, a matrix mix's sources); each
    /// channel's name, colour, pair and its `send` into the bus (an aux bus's
    /// `{on, level, pre, pan}`; a group's or the main bus's `{on}`; none into a matrix
    /// mix); a matrix mix's sources' names. Nothing of the other buses. `error` when the
    /// scope's bus isn't on the desk (by name).
    pub fn bus_state(&self, scope: &str, target: &Result<BusTarget, String>) -> Value {
        let s = self.settings();
        let mut v = json!({
            "version": self.version(),
            "name": self.shared.cfg.name,
            "locked": self.locked(),
            "scope": scope,
            "bus": null,
            "channels": [],
            "error": null,
        });
        let t = match target {
            Ok(t) => *t,
            Err(e) => {
                v["error"] = json!(e);
                return v;
            }
        };
        let Some(b) = s.bus(t.strip()) else {
            v["error"] = json!("no such bus");
            return v;
        };
        let (kind, index) = t.kind();
        let mut bus = json!({
            "kind": kind,
            "index": index,
            "number": match kind {
                "main" => "Main".to_owned(),
                "stream" => "Stream".to_owned(),
                "aux" => format!("Aux {index}"),
                "group" => format!("Group {index}"),
                _ => format!("Matrix {index}"),
            },
            "label": b.name,
            "color": b.color,
            "stereo": t.stereo(&s),
            "command": t.command(),
            "settings": t.settings_of(b),
        });
        if let BusTarget::Aux(_) = t {
            bus["pre"] = json!(b.pre);
        }
        if let BusTarget::Matrix(_) = t {
            // The sources' names (main, aux 3, …) and their buses' names, in `sources`' order.
            bus["sources"] = json!(
                super::describe::matrix_sources(&s)
                    .into_iter()
                    .map(|m| json!({"name": m["name"], "label": m["label"]}))
                    .collect::<Vec<_>>()
            );
        }
        v["bus"] = bus;
        let active: Vec<bool> = {
            let d = self.shared.desk.lock_ok();
            (0..s.channels.len()).map(|i| d.active(i)).collect()
        };
        v["channels"] = json!(
            s.channels
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let lead = s.lead_of(i);
                    let plays = &s.channels[lead];
                    let send = match t {
                        BusTarget::Aux(n) => {
                            serde_json::to_value(plays.sends.get(n - 1)).unwrap_or_default()
                        }
                        BusTarget::Group(n) => {
                            json!({"on": plays.groups.get(n - 1).copied().unwrap_or(false)})
                        }
                        BusTarget::Main => json!({ "on": plays.main }),
                        BusTarget::Stream => {
                            serde_json::to_value(&plays.stream).unwrap_or_default()
                        }
                        BusTarget::Matrix(_) => Value::Null,
                    };
                    json!({
                        "n": i + 1,
                        "name": c.name,
                        "color": c.color,
                        // The channel whose settings it plays with (a linked pair's first).
                        "lead": lead + 1,
                        "linked": lead.is_multiple_of(2)
                            && s.channels[lead].link
                            && lead + 1 < s.channels.len(),
                        "active": active.get(i).copied().unwrap_or(false),
                        "send": send,
                    })
                })
                .collect::<Vec<_>>()
        );
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(name: &str) -> Console {
        Console::start(ConsoleConfig {
            name: name.into(),
            channels: 8,
            mdns: Some(false),
            ..Default::default()
        })
        .unwrap()
    }

    /// A caller holding a token of `scope`.
    fn holder(scope: &str) -> Who {
        let mut w = Who::host("carol");
        w.scope = Scope::parse(scope).unwrap();
        w
    }

    #[test]
    fn the_scope_names_a_bus() {
        let ok = |s: &str| BusSel::parse(s).unwrap();
        assert_eq!(ok("main"), BusSel::Main);
        assert_eq!(ok("aux:16"), BusSel::Aux(16));
        assert_eq!(ok("group:8"), BusSel::Group(8));
        assert_eq!(ok("matrix:1"), BusSel::Matrix(1));
        assert_eq!(ok("name:Live_Stream"), BusSel::Named("live-stream".into()));
        assert_eq!(ok("stream"), BusSel::Stream);
        assert_eq!(ok("name:stream"), BusSel::Named("stream".into()));
        for bad in [
            "aux:0",
            "aux:17",
            "matrix:9",
            "group:x",
            "fx:1",
            "name:--",
            "headphones:1",
        ] {
            assert!(BusSel::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(name_key(" Live  Stream! "), "live-stream");
    }

    /// A `bus:aux:N` token changes the sends into aux N and its master, nothing else; a
    /// read token nothing; a control token anything.
    #[test]
    fn a_bus_token_changes_its_bus_alone() {
        let c = console("Bus scope test");
        let who = holder("bus:aux:5");
        let run = |cmd: &str, v: Value| c.command_as(&who, cmd, &v);
        let mut send5 = vec![Value::Null; 4];
        send5.push(json!({"on": true, "level": -6.0, "pre": false}));
        run("channel", json!({"n": 2, "sends": send5})).unwrap();
        run("channels", json!({"n": [3, 4], "sends": send5})).unwrap();
        run(
            "bus",
            json!({"bus": 5, "fader": -3.0, "mute": true, "eq": [{"gain": 2.0}]}),
        )
        .unwrap();
        run(
            "bus",
            json!({"bus": "5", "geq": {"on": true}, "comp": {"ratio": 3.0}}),
        )
        .unwrap();
        let s = c.settings();
        assert_eq!(s.channels[1].sends[4].level, -6.0);
        assert!(s.channels[3].sends[4].on);
        assert_eq!((s.aux[4].fader, s.aux[4].mute), (-3.0, true));
        // Pan only into a stereo aux.
        let pan = {
            let mut p = vec![Value::Null; 4];
            p.push(json!({"pan": -0.5}));
            json!({"n": 1, "sends": p})
        };
        assert!(run("channel", pan.clone()).unwrap_err().contains("mono"));
        Console::bus(&c, "5", &json!({"stereo": true})).unwrap();
        run("channel", pan).unwrap();
        // Everything else: refused, saying so.
        let mut send6 = vec![Value::Null; 5];
        send6.push(json!({"level": 0.0}));
        for (cmd, v) in [
            ("channel", json!({"n": 1, "sends": send6})),
            ("channel", json!({"n": 1, "fader": -10.0})),
            ("channel", json!({"n": 1, "name": "X"})),
            ("channel", json!({"n": 1, "fx_sends": [{"level": 0.0}]})),
            ("bus", json!({"bus": 6, "fader": 0.0})),
            ("bus", json!({"bus": "main", "fader": 0.0})),
            ("bus", json!({"bus": 5, "name": "Mine"})),
            ("bus", json!({"bus": 5, "inserts": []})),
            ("bus", json!({"bus": 5, "listen": true})),
            ("matrix", json!({"matrix": 1, "fader": 0.0})),
            ("group", json!({"group": 1, "mute": true})),
            ("snapshot_recall", json!({"name": "A"})),
            ("snapshot_save", json!({"name": "A"})),
            ("lock", json!({"on": true})),
            ("desk", json!({"automix": true})),
            ("output", json!({"out": 1, "source": ""})),
            ("dca", json!({"dca": 1, "mute": true})),
            ("record_start", json!({})),
            ("plugins_scan", json!({})),
            ("insert", json!({"bus": 5, "op": "remove", "index": 0})),
            ("press", json!({"button": 1})),
        ] {
            let e = run(cmd, v.clone()).unwrap_err();
            assert!(
                e.contains("outside this token's bus (aux 5)"),
                "{cmd} {v}: {e}"
            );
        }
        let s = c.settings();
        assert_eq!(s.channels[0].fader, 0.0);
        assert!(!s.channels[0].sends[5].on && s.aux[5].fader == 0.0 && !s.dca[0].mute);
        assert!(!c.locked());
        // Read and control: as before.
        assert_eq!(
            c.permits(&holder("read"), "channel", &json!({"n": 1, "fader": 0.0})),
            Err("read-only".into())
        );
        assert!(c.permits(&holder("control"), "lock", &json!({})).is_ok());
        assert!(
            c.permits(&holder("iem:band"), "channel", &json!({}))
                .unwrap_err()
                .contains("outside this token's IEM mix (band)")
        );
        // The audit: carol's changes, done and refused.
        let log = c.audit().newest(100);
        assert!(log.iter().any(|l| l["user"] == "carol" && l["ok"] == true));
        assert!(log.iter().any(|l| l["user"] == "carol" && l["ok"] == false));
    }

    /// `send` (one channel's send into one aux bus): a control token any, a bus token
    /// only into its own aux bus (pan only into a stereo one), nothing into a group.
    #[test]
    fn the_send_command_and_bus_tokens() {
        let c = console("Send test");
        c.command(
            "send",
            &json!({"n": 2, "bus": 3, "level": -12.0, "on": true}),
        )
        .unwrap();
        let s = c.settings();
        assert_eq!(
            (s.channels[1].sends[2].level, s.channels[1].sends[2].on),
            (-12.0, true)
        );
        // Only the keys given change; null `pre` is the bus's again.
        c.command("send", &json!({"n": 2, "bus": "3", "pre": true}))
            .unwrap();
        c.command("send", &json!({"n": 2, "bus": 3, "pre": null}))
            .unwrap();
        let s = c.settings();
        assert_eq!(s.channels[1].sends[2].level, -12.0);
        assert_eq!(s.channels[1].sends[2].pre, None);
        for bad in [
            json!({"n": 2, "bus": 17, "level": 0.0}),
            json!({"n": 2, "bus": 3}),
            json!({"n": 0, "bus": 3, "on": true}),
            json!({"n": 2, "bus": 3, "on": "yes"}),
            json!({"n": 2, "bus": 3, "fader": 0.0}),
        ] {
            assert!(c.command("send", &bad).is_err(), "{bad}");
        }
        let who = holder("bus:aux:5");
        c.command_as(
            &who,
            "send",
            &json!({"n": 1, "bus": 5, "level": -6.0, "on": true}),
        )
        .unwrap();
        assert_eq!(c.settings().channels[0].sends[4].level, -6.0);
        let e = c
            .command_as(&who, "send", &json!({"n": 1, "bus": 6, "level": 0.0}))
            .unwrap_err();
        assert!(e.contains("outside this token's bus (aux 5)"), "{e}");
        let e = c
            .command_as(&who, "send", &json!({"n": 1, "bus": 5, "pan": 0.5}))
            .unwrap_err();
        assert!(e.contains("mono"), "{e}");
        assert!(
            c.command_as(
                &holder("bus:group:1"),
                "send",
                &json!({"n": 1, "bus": 1, "on": true})
            )
            .is_err()
        );
        assert!(
            c.command_as(
                &holder("read"),
                "send",
                &json!({"n": 1, "bus": 1, "on": true})
            )
            .is_err()
        );
    }

    /// A group's or the main bus's token: its channels' assignment and its master.
    #[test]
    fn group_main_and_matrix_tokens() {
        let c = console("Bus kinds test");
        let g = holder("bus:group:2");
        c.command_as(&g, "channel", &json!({"n": 1, "groups": [null, true]}))
            .unwrap();
        c.command_as(&g, "group", &json!({"group": 2, "fader": -4.0}))
            .unwrap();
        assert!(
            c.command_as(&g, "channel", &json!({"n": 1, "groups": [true]}))
                .is_err()
        );
        assert!(
            c.command_as(&g, "group", &json!({"group": 2, "to_main": true}))
                .is_err()
        );
        assert!(
            c.command_as(&g, "group", &json!({"group": 2, "geq": {}}))
                .is_err()
        );
        let m = holder("bus:main");
        c.command_as(&m, "channel", &json!({"n": 1, "main": false}))
            .unwrap();
        c.command_as(&m, "bus", &json!({"bus": "main", "fader": -2.0}))
            .unwrap();
        assert!(
            c.command_as(&m, "bus", &json!({"bus": "main", "dim": true}))
                .is_err()
        );
        assert!(
            c.command_as(&m, "channel", &json!({"n": 1, "fader": -2.0}))
                .is_err()
        );
        let x = holder("bus:matrix:3");
        c.command_as(
            &x,
            "matrix",
            &json!({"matrix": 3, "sources": [{"level": -5.0}], "mute": true}),
        )
        .unwrap();
        // (A stray address in the body names another bus: refused.)
        assert!(
            c.command_as(
                &x,
                "matrix",
                &json!({"matrix": 3, "group": 1, "mute": true})
            )
            .is_err()
        );
        assert!(
            c.command_as(&x, "channel", &json!({"n": 1, "main": true}))
                .is_err()
        );
        let s = c.settings();
        assert!(s.channels[0].groups[1] && !s.channels[0].main);
        assert_eq!((s.groups[1].fader, s.main.fader), (-4.0, -2.0));
        assert_eq!(
            (s.matrix[2].sources[0].level, s.matrix[2].mute),
            (-5.0, true)
        );
    }

    /// A bus token's state, meters and control limits: its bus alone.
    #[test]
    fn a_bus_tokens_state_is_its_bus_alone() {
        let c = console("Bus state test");
        c.channel(
            1,
            &json!({"name": "Pastor", "color": "#ff0000", "link": true}),
        )
        .unwrap();
        c.channel(
            3,
            &json!({"sends": [null, null, {"on": true, "level": -12.0}]}),
        )
        .unwrap();
        Console::bus(&c, "4", &json!({"fader": -7.0})).unwrap();
        let st = c.state_for(&holder("bus:aux:3"));
        let keys: Vec<&str> = st.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys.iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "bus", "channels", "error", "locked", "name", "scope", "version"
            ]
            .into()
        );
        assert_eq!(st["scope"], "bus:aux:3");
        let b = &st["bus"];
        assert_eq!(
            (b["kind"].as_str(), b["index"].as_u64()),
            (Some("aux"), Some(3))
        );
        assert_eq!(b["number"], "Aux 3");
        assert_eq!(b["command"], json!({"cmd": "bus", "bus": 3}));
        assert!(b["settings"]["fader"].is_number() && b["settings"]["geq"].is_object());
        assert!(b["settings"]["inserts"].is_null() && b["settings"]["name"].is_null());
        let ch = st["channels"].as_array().unwrap();
        assert_eq!(ch.len(), 8);
        assert_eq!(
            (ch[0]["name"].as_str(), ch[0]["color"].as_str()),
            (Some("Pastor"), Some("#ff0000"))
        );
        assert_eq!(
            (ch[1]["linked"].as_bool(), ch[1]["lead"].as_u64()),
            (Some(true), Some(1))
        );
        assert_eq!(ch[2]["send"]["level"], -12.0);
        assert_eq!(ch[2]["send"]["on"], true);
        // Nothing of the other buses (aux 4's fader of −7 is nowhere).
        let text = st.to_string();
        assert!(
            !text.contains("-7.0") && !text.contains("\"settings\":{\"channels\""),
            "{text}"
        );
        assert!(st["channels"][0].get("fader").is_none());
        // Meters: the channels', the bus's two channels and its reduction.
        let m = c.meters_for(&holder("bus:aux:3"));
        assert_eq!(m["bus"].as_array().map(Vec::len), Some(2));
        assert!(m["ch"].is_array() && m["bus_gr"].is_array());
        assert!(m.get("out").is_none() && m.get("lufs").is_none());
        assert!(c.meters_for(&holder("bus:main")).get("lufs").is_some());
        // Controls: the bus's strip and a send's.
        let t = c.controls_for(&holder("bus:aux:3"));
        assert!(t["bus"]["fader"].is_object() && t["channel"]["sends"]["level"].is_object());
        assert!(
            t["channel"]["sends"].get("pan").is_none(),
            "a mono aux: no pan"
        );
        assert!(t.get("dca").is_none() && t["bus"].get("effect").is_none());
        assert!(c.controls_for(&holder("bus:group:1"))["channel"].is_null());
        // A matrix mix: its sources named, no channel sends.
        let mx = c.state_for(&holder("bus:matrix:2"));
        assert_eq!(mx["bus"]["sources"][0]["name"], "main");
        assert!(mx["bus"]["settings"]["sources"].is_array());
        assert!(mx["channels"][0]["send"].is_null());
        // Read and control tokens: the whole state.
        assert!(c.state_for(&holder("read"))["settings"]["aux"].is_array());
        assert!(c.state_for(&holder("control"))["settings"]["aux"].is_array());
    }

    /// `bus:name:NAME` (and a bare `bus:NAME`): the bus by that name now, found when
    /// checked; none or two: nothing allowed.
    #[test]
    fn a_bus_is_found_by_name() {
        let c = console("Bus name test");
        let stream = holder("bus:foh");
        let e = c
            .permits(&stream, "bus", &json!({"bus": 4, "fader": 0.0}))
            .unwrap_err();
        assert!(
            e.contains("outside this token's bus") && e.contains("no bus named"),
            "{e}"
        );
        assert!(
            c.state_for(&stream)["error"]
                .as_str()
                .unwrap()
                .contains("no bus named")
        );
        Console::bus(&c, "4", &json!({"name": "FOH"})).unwrap();
        c.command_as(&stream, "bus", &json!({"bus": 4, "fader": -1.0}))
            .unwrap();
        assert!(
            c.permits(&stream, "bus", &json!({"bus": 3, "fader": 0.0}))
                .is_err()
        );
        assert_eq!(c.state_for(&stream)["bus"]["index"], 4);
        let named = holder("bus:name:live-stream");
        c.command("matrix", &json!({"matrix": 2, "name": "Live Stream"}))
            .unwrap();
        assert_eq!(c.state_for(&named)["bus"]["kind"], "matrix");
        // Renamed away: it follows the name, not the bus.
        Console::bus(&c, "4", &json!({"name": "Lobby"})).unwrap();
        assert!(
            c.permits(&stream, "bus", &json!({"bus": 4, "fader": 0.0}))
                .is_err()
        );
        // `bus:stream`: the dedicated stream bus, whatever the others are named.
        assert_eq!(c.state_for(&holder("bus:stream"))["bus"]["kind"], "stream");
        // Two buses by one name: neither.
        c.command("group", &json!({"group": 1, "name": "live stream"}))
            .unwrap();
        let e = c.state_for(&named)["error"].as_str().unwrap().to_owned();
        assert!(e.contains("more than one bus"), "{e}");
        assert!(
            c.permits(&named, "matrix", &json!({"matrix": 2, "mute": true}))
                .is_err()
        );
    }
}
