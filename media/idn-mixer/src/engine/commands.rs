//! The desk's commands (the panel's, OSC's, a surface's): channels, buses, mute groups, talkback, user buttons, solo, the output patch and delays, the desk's own settings, DCAs.

use super::*;
use crate::desk::PAIR_OWN;
use serde_json::Map;

impl Console {
    /// Does an API command (`POST /api/v1/<cmd>`'s body, spec §M.5) — the panel's, the
    /// OSC control's (§M.16) and an embedding program's one way in. The caller checks
    /// the lock (§M.11).
    pub fn command(&self, cmd: &str, v: &Value) -> Result<(), String> {
        // The typed commands are the contract (`GET /api/v1/schema`, refactor 4): what
        // doesn't parse as its typed form is refused before it runs, whichever way it
        // came (HTTP, the push channel, OSC, a host, a starter, a cue).
        typed::parse(cmd, v)?;
        let name = || v["name"].as_str().unwrap_or("").to_owned();
        // Everything but the addressing is the patch.
        let patch = |skip: &str| {
            let mut p = v.clone();
            if let Some(o) = p.as_object_mut() {
                o.remove(skip);
            }
            p
        };
        let num = |k: &str| v[k].as_u64().map(|n| n as usize);
        match cmd {
            // A starter (G6, §M.20): names, colours, filters and groups, by commands. (Not
            // while the desk is locked: it renames and regroups channels, deep bug run
            // 2026-11-09.)
            "starter" if self.locked() => Err("the desk is locked".into()),
            "starter" => self.starter(v["name"].as_str().unwrap_or("")),
            // Plugins looked for again (§M.6); the list is `plugin_list`.
            "plugins_scan" => self.plugin_list(true).map(|_| ()),
            // The Routing view's Network tab: through this machine's IDN Node.
            "route" => self.route(v),
            "receiver_latency" => self.receiver_latency(v),
            "lock" => {
                self.lock(v["on"].as_bool().unwrap_or(true));
                Ok(())
            }
            "group" | "matrix" | "fx" => match crate::desk::named_bus(v) {
                Ok(k) => self.bus_strip(k, &patch(cmd)),
                Err(e) => Err(e),
            },
            "mute_group" => match num("group") {
                Some(k) => self.mute_group(k, &patch("group")),
                None => Err(format!("group: the mute group, 1 to {MUTE_GROUPS}")),
            },
            "talkback" => self.talkback(v),
            "button" => match num("button") {
                Some(k) => self.button(k, &patch("button")),
                None => Err("button: 1 to 8".into()),
            },
            "press" => match num("button") {
                Some(k) => self.press(k),
                None => Err("button: 1 to 8".into()),
            },
            "clear_solo" => self.clear_solo(),
            "loudness_reset" => {
                self.reset_loudness();
                Ok(())
            }
            // Multitrack recording and virtual soundcheck (§M.13).
            "record_start" => {
                let channels = v["channels"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|n| n.as_u64())
                        .map(|n| n as usize)
                        .collect()
                });
                self.record_start(channels, v["name"].as_str().filter(|n| !n.is_empty()))
                    .map(|_| ())
            }
            "record_stop" => self.record_stop().map(|_| ()),
            "soundcheck" => self.soundcheck(v["name"].as_str()),
            "soundcheck_transport" => {
                use crate::record::Transport;
                match v["action"].as_str().unwrap_or("") {
                    "play" => self.soundcheck_transport(Transport::Play),
                    "pause" => self.soundcheck_transport(Transport::Pause),
                    "locate" => self.soundcheck_transport(Transport::Locate(
                        v["seconds"].as_f64().unwrap_or(0.0),
                    )),
                    "loop" => self
                        .soundcheck_transport(Transport::Loop(v["on"].as_bool().unwrap_or(true))),
                    _ => Err("action: play, pause, locate or loop".into()),
                }
            }
            "soundcheck_live" => match num("n") {
                Some(n) => self.soundcheck_live(n, v["live"].as_bool().unwrap_or(true)),
                None => Err("n: the channel, from 1".into()),
            },
            // Its source and/or its delay (ms, §M.15): what isn't given stays (an `output`
            // with only a delay patched the output to nothing, silencing it).
            "output" => match num("out") {
                Some(o) => {
                    if v.get("source").is_none() && v.get("delay").is_none() {
                        return Err("output: a source and/or a delay (ms)".into());
                    }
                    if let Some(d) = v.get("delay") {
                        self.output_delay(o, d.as_f64().ok_or("delay: ms")?)?;
                    }
                    match v.get("source") {
                        Some(src) => self.patch_output(o, src.as_str().ok_or("source: text")?),
                        None => Ok(()),
                    }
                }
                None => Err("out: the desk output, from 1".into()),
            },
            "desk" => self.desk(v),
            // An X32 preamp behind a channel (§M.23.1): not a setting of the desk's.
            "preamp" => match num("n") {
                Some(n) => {
                    let flag = |k: &str| match &v[k] {
                        Value::Null => Ok(None),
                        Value::Bool(b) => Ok(Some(*b)),
                        _ => Err(format!("{k}: true or false")),
                    };
                    let gain = match &v["gain_db"] {
                        Value::Null => None,
                        g => Some(g.as_f64().ok_or("gain_db: dB, -12 to 60")? as f32),
                    };
                    self.preamp(n, gain, flag("phantom")?, flag("invert")?)
                }
                None => Err("n: the channel, from 1".into()),
            },
            "preset_save" => match num("n") {
                Some(n) => self.preset_save(&name(), n),
                None => Err("n: the channel, from 1".into()),
            },
            "preset_load" => match num("n") {
                Some(n) => self.preset_load(&name(), n),
                None => Err("n: the channel, from 1".into()),
            },
            "preset_delete" => self.preset_delete(&name()),
            "channel_count" => match num("n") {
                Some(n) => self.channel_count(n),
                None => Err("n: the channels from the next start, 1 to 64".into()),
            },
            "restart" => self.restart(),
            "latency_test" => self.latency_test(v).map(|_| ()),
            "channel_reset" => match num("n") {
                Some(n) => self.channel_reset(n),
                None => Err("n: the channel, from 1".into()),
            },
            "copy_channel" => match (num("from"), num("n")) {
                (Some(from), Some(n)) => self.copy_channel(from, n),
                _ => Err("from and n: channels, from 1".into()),
            },
            "channel" => match v["n"].as_u64() {
                Some(n) => self.channel(n as usize, &patch("n")),
                None => Err("n: the channel, from 1".into()),
            },
            // The same patch on several channels at once, all or nothing.
            "channels" => match v["n"].as_array() {
                Some(list) if !list.is_empty() => {
                    let ns = list
                        .iter()
                        .map(|n| n.as_u64().map(|n| n as usize))
                        .collect::<Option<Vec<_>>>()
                        .ok_or("n: a list of channels, from 1")?;
                    self.channels(&ns, &patch("n"))
                }
                _ => Err("n: a list of channels, from 1".into()),
            },
            "bus" => match &v["bus"] {
                Value::String(b) => self.bus(b, &patch("bus")),
                Value::Number(b) => self.bus(&b.to_string(), &patch("bus")),
                _ => Err(format!(
                    "bus: \"main\", 1 to {} or \"headphones\"",
                    crate::desk::AUX
                )),
            },
            "dca" => match v["dca"].as_u64() {
                Some(k) => self.dca(k as usize, &patch("dca")),
                None => Err("dca: the group, 1 to 8".into()),
            },
            // With "include_iem": true, the IEM mixes and marks too (§M.26).
            "snapshot_save" => match &v["include_iem"] {
                Value::Null => self.snapshot_save(&name()),
                Value::Bool(iem) => self.snapshot_save_with(&name(), *iem),
                _ => Err("include_iem: true or false".into()),
            },
            // The IEM mixes (§M.26).
            "iem_mix_add" => self.iem_mix_add(&name(), v["id"].as_str()).map(|_| ()),
            "endpoint_add" => self.endpoint_add(v).map(|_| ()),
            "endpoint" => self.endpoint(v),
            "endpoint_remove" => self.endpoint_remove(v),
            "iem_mix" => self.iem_mix(v),
            "iem_mix_delete" => match v["id"].as_str() {
                Some(id) => self.iem_mix_delete(id),
                None => Err("id: the mix's id".into()),
            },
            // One channel's send into one aux bus.
            "send" => self.send(v),
            // With "scope": {"channels": [1, 2] | null, "buses": bool, "outputs": bool}
            // (§M.15); without, everything (but the recall-safe channels and buses).
            "snapshot_order" => match v["names"].as_array() {
                Some(l) => self.snapshot_order(
                    &l.iter()
                        .filter_map(|n| n.as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                ),
                None => Err("names: the snapshots, in order".into()),
            },
            "snapshot_recall" => {
                let sc = &v["scope"];
                // Channels: a list, true or absent (all), false (none); anything else is
                // refused ("channels": false recalled every channel).
                let channels = match &sc["channels"] {
                    Value::Null | Value::Bool(true) => None,
                    Value::Bool(false) => Some(Vec::new()),
                    Value::Array(a) => Some(
                        a.iter()
                            .map(|n| n.as_u64().filter(|&n| n >= 1).map(|n| n as usize))
                            .collect::<Option<Vec<_>>>()
                            .ok_or("scope channels: numbers from 1")?,
                    ),
                    _ => return Err("scope channels: a list, true or false".into()),
                };
                let flag = |k: &str| match &sc[k] {
                    Value::Null => Ok(true),
                    Value::Bool(b) => Ok(*b),
                    _ => Err(format!("scope {k}: true or false")),
                };
                let parts = match &sc["parts"] {
                    Value::Null => None,
                    p => Some(
                        serde_json::from_value::<Vec<crate::engine::RecallPart>>(p.clone())
                            .map_err(|_| {
                                "scope parts: a list of levels, sends, processing, inputs, names"
                            })?,
                    ),
                };
                let fade_s = match &v["fade_s"] {
                    Value::Null => None,
                    f => Some(f.as_f64().ok_or("fade_s: seconds, 0 to 10")? as f32),
                };
                let scope = SnapshotRecallScope {
                    channels,
                    buses: flag("buses")?,
                    outputs: flag("outputs")?,
                    parts,
                    fade_s,
                };
                self.snapshot_recall_scoped(&name(), &scope)
            }
            "snapshot_delete" => self.snapshot_delete(&name()),
            // Switchers' outputs as desk inputs (§M.17).
            "switcher_inputs" => {
                self.switcher_inputs(v["on"].as_bool().unwrap_or(true));
                Ok(())
            }
            "switcher_input" => self.switcher_input(&name(), v["on"].as_bool().unwrap_or(true)),
            // A switcher output's sound from a desk bus (§M.18); "bus": null for its own.
            "switcher_feed" => self.switcher_feed(&name(), v["bus"].as_str()),
            "insert" => self.insert(v),
            "plugin_set" => self.plugin_set(v),
            // A plugin's own window on the desk's machine (§M.10).
            "plugin_window" => self.plugin_editor(v).map(|_| ()),
            other => Err(format!("no command {other:?}")),
        }
    }

    /// Changes channel `n` (1-based) by a JSON merge patch of its settings
    /// (`{"fader": -6, "eq": …}`), all or nothing. `hpf_on` / `lpf_on` (true or false)
    /// switch a filter at its last frequency. The second channel of a linked pair keeps
    /// its own [`PAIR_OWN`]; the rest of its patch goes to the pair's first, whose
    /// settings the pair plays with (and the second is given). Only an odd-numbered
    /// channel starts a pair (`link`).
    pub fn channel(&self, n: usize, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| patch_channel(s, n, patch))
    }

    /// The same merge patch on channels `ns` (1-based), as [`Self::channel`], all or
    /// nothing.
    pub fn channels(&self, ns: &[usize], patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            ns.iter().try_for_each(|&n| patch_channel(s, n, patch))
        })
    }

    /// Changes a bus (`main`, `1`–`8` for the aux buses, `headphones`) by a merge patch.
    pub fn bus(&self, which: &str, patch: &Value) -> Result<(), String> {
        self.bus_strip(named_bus(&json!({ "bus": which }))?, patch)
    }

    /// Changes bus strip `k` (main, aux 1–8, groups, matrix mixes, headphones; as
    /// [`crate::desk::named_bus`] gives it) by a merge patch.
    pub fn bus_strip(&self, k: usize, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            let b = s.bus_mut(k).ok_or("no such bus")?;
            *b = patched::<Bus>(b, patch)?;
            Ok(())
        })
    }

    /// Changes mute group `k` (1–8) by a merge patch (`{"on": true}`, `{"name": …}`).
    pub fn mute_group(&self, k: usize, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            let g = s
                .mute_groups
                .get_mut(k.wrapping_sub(1))
                .ok_or(format!("mute groups are 1 to {MUTE_GROUPS}"))?;
            *g = patched::<MuteGroup>(g, patch)?;
            Ok(())
        })
    }

    /// Changes talkback by a merge patch (`{"on": true}`, `{"channel": 32}`, …).
    pub fn talkback(&self, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            s.talkback = patched::<Talkback>(&s.talkback, patch)?;
            Ok(())
        })
    }

    /// Changes user button `k` (1–8): its `label` and `action`.
    pub fn button(&self, k: usize, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            let b = s
                .buttons
                .get_mut(k.wrapping_sub(1))
                .ok_or(format!("buttons are 1 to {BUTTONS}"))?;
            *b = patched::<Button>(b, patch)?;
            Ok(())
        })
    }

    /// Presses user button `k` (1–8): does its action (a mute, a mute group or a DCA's
    /// mute turned over, a snapshot recalled, talkback turned over, solos cleared).
    pub fn press(&self, k: usize) -> Result<(), String> {
        let action = self
            .settings()
            .buttons
            .get(k.wrapping_sub(1))
            .map(|b| b.action.clone())
            .ok_or(format!("buttons are 1 to {BUTTONS}"))?;
        let (what, arg) = action.split_once(':').unwrap_or((action.as_str(), ""));
        let num = || {
            arg.trim()
                .parse::<usize>()
                .map_err(|_| format!("button {k}: {action:?} needs a number"))
        };
        match what {
            "mute" => {
                let n = num()?;
                // (A linked pair's second: the pair's mute, its first's.)
                let s = self.settings();
                let on = (1..=s.channels.len())
                    .contains(&n)
                    .then(|| s.channels[s.lead_of(n - 1)].mute);
                self.channel(n, &json!({ "mute": !on.ok_or("no such channel")? }))
            }
            "mute_group" => {
                let g = num()?;
                let on = self
                    .settings()
                    .mute_groups
                    .get(g.wrapping_sub(1))
                    .map(|m| m.on);
                self.mute_group(g, &json!({ "on": !on.ok_or("no such mute group")? }))
            }
            "dca_mute" => {
                let g = num()?;
                let on = self.settings().dca.get(g.wrapping_sub(1)).map(|d| d.mute);
                self.dca(g, &json!({ "mute": !on.ok_or("no such DCA")? }))
            }
            "snapshot" => self.snapshot_recall(arg.trim()),
            "talkback" => {
                let on = self.settings().talkback.on;
                self.talkback(&json!({ "on": !on }))
            }
            "clear_solo" => self.clear_solo(),
            // Every FX return muted or unmuted together.
            "fx_mute" => {
                let on = !self.settings().fx.iter().all(|f| f.mute);
                self.edit(false, |s| {
                    s.fx.iter_mut().for_each(|f| f.mute = on);
                    Ok(())
                })
            }
            "" => Ok(()),
            other => Err(format!("button {k}: no action {other:?}")),
        }
    }

    /// Clears every solo and listen.
    pub fn clear_solo(&self) -> Result<(), String> {
        self.edit(false, |s| {
            for c in &mut s.channels {
                c.solo = false;
            }
            for k in 0..BUS_STRIPS {
                if let Some(b) = s.bus_mut(k) {
                    b.listen = false;
                }
            }
            Ok(())
        })
    }

    /// Sets desk output `o` (from 1) to carry `source` (`bus:J`, `pre:N`, `post:N`, `in:N`
    /// or `""`; §M.11).
    pub fn patch_output(&self, o: usize, source: &str) -> Result<(), String> {
        let ok = source.is_empty()
            || source.split_once(':').is_some_and(|(k, n)| {
                matches!(
                    k,
                    "bus" | "pre" | "post" | "in" | "input" | "preeq" | "posteq"
                ) && n.parse::<usize>().is_ok()
            });
        if !ok {
            return Err(format!(
                "source {source:?}: bus:J, input:N, preeq:N, posteq:N, pre:N, post:N, in:N or empty"
            ));
        }
        self.edit(false, |s| {
            let count = s.outputs.len();
            let slot = s
                .outputs
                .get_mut(o.wrapping_sub(1))
                .ok_or(format!("outputs are 1 to {count}"))?;
            *slot = source.to_owned();
            Ok(())
        })
    }

    /// Desk output `o`'s delay, ms (§M.15; 0 to 500, as `desk {output_delay}`).
    pub fn output_delay(&self, o: usize, ms: f64) -> Result<(), String> {
        if !(0.0..=500.0).contains(&ms) {
            return Err("delay: 0 to 500 ms".into());
        }
        self.edit(false, |s| {
            let count = s.outputs.len();
            let d = s
                .output_delay
                .get_mut(o.wrapping_sub(1))
                .ok_or(format!("outputs are 1 to {count}"))?;
            *d = ms as f32;
            Ok(())
        })
    }

    /// Desk-wide settings: `solo_afl`, `auto_park`, `automix`, `duckers`, `afv`, `block`,
    /// `rate`, `output_delay`, `x32`, `clock`.
    pub fn desk(&self, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            if let Some(v) = patch.get("solo_afl") {
                s.solo_afl = v.as_bool().ok_or("solo_afl: true or false")?;
            }
            if let Some(v) = patch.get("auto_park") {
                let on = v.as_bool().ok_or("auto_park: true or false")?;
                // Automatic: every channel follows its routing again. Manual: each stays
                // where it is (placed by the next routing check).
                if on && !s.auto_park {
                    for c in &mut s.channels {
                        c.shown = None;
                    }
                }
                s.auto_park = on;
            }
            // The automix, duckers and audio follows video (§M.12).
            if let Some(v) = patch.get("automix") {
                s.automix = v.as_bool().ok_or("automix: true or false")?;
            }
            if let Some(v) = patch.get("duckers") {
                s.duckers = patched(&s.duckers, v)?;
            }
            if let Some(v) = patch.get("afv") {
                s.afv = patched(&s.afv, v)?;
            }
            // Samples a block from the desk's next start (32, 64, 128 or 256).
            if let Some(v) = patch.get("block") {
                let b = v
                    .as_u64()
                    .map(|b| b as usize)
                    .filter(|b| crate::desk::BLOCKS.contains(b))
                    .ok_or("block: 32, 64, 128 or 256 samples")?;
                s.block = b;
            }
            // The desk's own sample rate from its next start (44100, 48000, 96000; null
            // or "": the machine's; §N.40).
            if let Some(v) = patch.get("rate") {
                s.rate = match v {
                    Value::Null => None,
                    Value::String(t) if t.is_empty() => None,
                    Value::String(t) => Some(
                        idn_open_protocol::node::rate::parse(t)
                            .ok_or("rate: 44100, 48000 or 96000")?,
                    ),
                    _ => Some(
                        v.as_u64()
                            .and_then(|r| u32::try_from(r).ok())
                            .filter(|&r| idn_open_protocol::node::rate::valid(r))
                            .ok_or("rate: 44100, 48000 or 96000")?,
                    ),
                };
            }
            // The clock it follows: automatic, or chosen by hand.
            if let Some(v) = patch.get("clock") {
                let c = v.as_str().unwrap_or("");
                if !["auto", "card", "site", "own"].contains(&c) {
                    return Err("clock: \"auto\", \"card\", \"site\" or \"own\"".into());
                }
                s.clock = c.to_owned();
            }
            // Whether phones may use a `control` token on the IEM flow.
            if let Some(v) = patch.get("iem_control") {
                s.iem_control = v.as_bool().ok_or("iem_control: true or false")?;
            }
            // Each desk output's delay, ms (§M.15).
            if let Some(v) = patch.get("output_delay") {
                s.output_delay = patched(&s.output_delay, v)?;
            }
            // The X32 whose preamps the desk controls (§M.23.1): "", "auto", an address.
            if let Some(a) = patch.get("x32").and_then(|x| x.get("address")) {
                let a = a.as_str().ok_or("x32 address: text")?.trim();
                if a.chars().any(|c| c.is_whitespace() || c == '/') {
                    return Err("x32 address: \"\", \"auto\", an IP address or a host name".into());
                }
                s.x32.address = a.to_owned();
            }
            Ok(())
        })
    }

    /// Changes DCA group `k` (1–16) by a merge patch (`{"fader": -6}`, `{"mute": true}`).
    pub fn dca(&self, k: usize, patch: &Value) -> Result<(), String> {
        self.edit(false, |s| {
            let g = s
                .dca
                .get_mut(k.wrapping_sub(1))
                .ok_or(format!("DCA groups are 1 to {DCA}"))?;
            *g = patched::<Dca>(g, patch)?;
            Ok(())
        })
    }
}

/// Patches channel `n` (from 1) of `s`: a linked pair's second keeps [`PAIR_OWN`], the
/// rest goes to its first. `link: true` on an even-numbered channel not in a pair is
/// refused: a pair starts on an odd-numbered channel.
fn patch_channel(s: &mut Settings, n: usize, patch: &Value) -> Result<(), String> {
    let count = s.channels.len();
    let i = n.wrapping_sub(1);
    if i >= count {
        return Err(format!("channels are 1 to {count}"));
    }
    let lead = s.lead_of(i);
    if i % 2 == 1 && lead == i && patch.get("link").and_then(Value::as_bool) == Some(true) {
        return Err(format!(
            "link: a stereo pair starts on an odd-numbered channel (link channel {i} to {n})"
        ));
    }
    let Some(all) = patch.as_object().filter(|_| lead != i) else {
        return patch_one(&mut s.channels[i], patch, patch);
    };
    let (own, rest): (Map<String, Value>, Map<String, Value>) = all
        .clone()
        .into_iter()
        .partition(|(k, _)| PAIR_OWN.contains(&k.as_str()));
    // An input patched to the second puts the pair on the desk (its first's `shown`).
    let mut rest = Value::Object(rest);
    if own
        .get("source")
        .and_then(Value::as_u64)
        .is_some_and(|k| k > 0)
        && rest.get("shown").is_none()
    {
        rest["shown"] = json!(true);
    }
    patch_one(&mut s.channels[i], &Value::Object(own), &Value::Null)?;
    patch_one(&mut s.channels[lead], &rest, &rest)
}

/// One channel patched (`hpf_on` / `lpf_on` taken out first); `source_of` the patch whose
/// `source` puts it on the desk.
fn patch_one(c: &mut Channel, patch: &Value, source_of: &Value) -> Result<(), String> {
    let mut patch = patch.clone();
    if let Some(o) = patch.as_object_mut() {
        for (key, f, f_last, last) in [
            ("hpf_on", "hpf", "hpf_last", c.hpf_last),
            ("lpf_on", "lpf", "lpf_last", c.lpf_last),
        ] {
            let Some(on) = o.remove(key) else { continue };
            let on = on.as_bool().ok_or(format!("{key}: true or false"))?;
            let now = if f == "hpf" { c.hpf } else { c.lpf };
            // A frequency given with it: a number (Hz; 0 or less: none given).
            let given = match o.get(f) {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_f64().ok_or(format!("{f}: a frequency in Hz"))?),
            }
            .filter(|v| *v > 0.0 && v.is_finite());
            if !on {
                // Off: a frequency given is the one it comes back on at.
                if let Some(g) = given {
                    o.insert(f_last.into(), json!(g));
                }
                o.insert(f.into(), json!(0.0));
            } else if given.is_none() {
                // On at the frequency it has, else its last (not off with `"hpf": 0`).
                o.insert(f.into(), json!(if now > 0.0 { now } else { last }));
            }
        }
    }
    *c = patched::<Channel>(c, &patch)?;
    // An input patched to it puts it on the desk, to stay until parked, routed or not
    // (§M.17), unless the patch says otherwise.
    if source_of["source"].as_u64().is_some_and(|k| k > 0) && source_of.get("shown").is_none() {
        c.shown = Some(true);
    }
    Ok(())
}

/// The starters built in (G6, §M.20): each a name, what it brings, and the desk commands.
const STARTERS: &str = include_str!("../starters.json");

/// The starters' names and what each brings.
pub(super) fn starters_list() -> Value {
    let all: Value = serde_json::from_str(STARTERS).unwrap_or_default();
    Value::Array(
        all.as_array()
            .into_iter()
            .flatten()
            .map(|s| json!({"name": s["name"], "about": s["about"]}))
            .collect(),
    )
}

impl Console {
    /// Applies starter `name`: its commands over the desk as it is (the patch, levels and
    /// snapshots stay). The first refused stops it, saying which.
    pub fn starter(&self, name: &str) -> Result<(), String> {
        let all: Value = serde_json::from_str(STARTERS).map_err(|e| e.to_string())?;
        let s = all
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == name))
            .ok_or(format!("no starter {name:?}"))?;
        for (k, cmd) in s["commands"].as_array().into_iter().flatten().enumerate() {
            let what = cmd[0].as_str().unwrap_or("");
            self.command(what, &cmd[1])
                .map_err(|e| format!("starter {name:?}, step {} ({what}): {e}", k + 1))?;
        }
        Ok(())
    }
}
