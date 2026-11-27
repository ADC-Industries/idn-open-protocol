//! What a client needs to draw the desk without knowing its layout (spec §M.5), added to
//! the state: the strips in order with where each is changed and metered (`strips`), the
//! desk's sizes (`desk_sizes`), a matrix mix's sources in order (`matrix_sources`), and
//! values worked out from the settings (`buttons_lit`, `any_solo`, `dsp_load_pct`,
//! `latency_ms`, `pending_restart`, `restart_for`).

use crate::desk::{
    AUX, BUS_STRIPS, BUTTONS, DCA, DUCKERS, EQ_BANDS, FX, GROUPS, MATRIX, MATRIX_SOURCES,
    MUTE_GROUPS, OUTPUTS, Settings, bus_out, bus_strip, matrix_source_names,
};
use serde_json::{Value, json};

/// Bus strip `k`'s kind (`main`, `aux`, `group`, `matrix`, `fx`, `headphones`) and its
/// number within the kind (from 1; 1 for the main bus and the headphones).
pub fn bus_kind(k: usize) -> (&'static str, usize) {
    match k {
        0 => ("main", 1),
        k if k <= AUX => ("aux", k),
        k if k <= AUX + GROUPS => ("group", k - AUX),
        k if k <= AUX + GROUPS + MATRIX => ("matrix", k - AUX - GROUPS),
        k if k <= AUX + GROUPS + MATRIX + FX => ("fx", k - AUX - GROUPS - MATRIX),
        crate::desk::STREAM_STRIP => ("stream", 1),
        _ => ("headphones", 1),
    }
}

/// Bus strip `k`'s address in commands (`{"bus": "main"}`, `{"bus": 3}`, `{"group": 2}`,
/// `{"matrix": 1}`, `{"fx": 4}`, `{"bus": "headphones"}`): its strip command's and its
/// plugin commands' (`insert`, `plugin_set`, …).
fn bus_address(k: usize) -> (&'static str, Value) {
    match bus_kind(k) {
        ("main", _) => ("bus", json!({"bus": "main"})),
        ("aux", i) => ("bus", json!({"bus": i})),
        ("headphones", _) => ("bus", json!({"bus": "headphones"})),
        ("stream", _) => ("bus", json!({"bus": "stream"})),
        (kind, i) => (kind, json!({ kind: i })),
    }
}

/// `{"cmd": cmd, …address}`.
fn command(cmd: &str, address: &Value) -> Value {
    let mut c = address.clone();
    c["cmd"] = json!(cmd);
    c
}

/// The desk outputs (from 1) whose patch is one of `sources`.
fn outputs_of(s: &Settings, sources: &[String]) -> Vec<usize> {
    s.outputs
        .iter()
        .enumerate()
        .filter(|(_, o)| sources.contains(o))
        .map(|(j, _)| j + 1)
        .collect()
}

/// The output patch's channel taps (`<tap>:<channel>`), in signal order.
pub const TAPS: [&str; 5] = ["input", "preeq", "posteq", "pre", "post"];

/// Every strip a desk panel shows, in order: the channels, the bus strips (main, aux,
/// groups, matrix mixes, FX returns, headphones: `bus_names`' order), the DCAs.
pub fn strips(s: &Settings, on_b: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, c) in s.channels.iter().enumerate() {
        let n = i + 1;
        let lead = s.lead_of(i);
        // (Only an odd-numbered channel leads a pair.)
        let pair = lead.is_multiple_of(2) && s.channels[lead].link && lead + 1 < s.channels.len();
        let on_b = on_b[i].as_bool().unwrap_or(false);
        let input = if on_b {
            c.input_b
        } else {
            c.source.unwrap_or(n)
        };
        let taps: Vec<String> = TAPS.iter().map(|t| format!("{t}:{n}")).collect();
        out.push(json!({
            "kind": "channel",
            "index": n,
            "label": c.name,
            "number": format!("Ch {n}"),
            "short": format!("{n:02}"),
            "command": {"cmd": "channel", "n": n},
            // The channel whose settings it plays with (a linked pair's first), where
            // its plugins are.
            "lead": lead + 1,
            "linked": pair,
            "plugins": {"channel": lead + 1},
            // The desk input it plays now (0: none), and whether that is its backup.
            "input": input,
            "on_b": on_b,
            "meter": {"ch": i},
            "bus_ext": null,
            "outputs": outputs_of(s, &taps),
        }));
    }
    for k in 0..BUS_STRIPS {
        let (kind, i) = bus_kind(k);
        let (cmd, address) = bus_address(k);
        let first = bus_strip(k).1;
        let ext = [bus_out(first), bus_out(first + 1)];
        let name = s.bus(k).map(|b| b.name.clone()).unwrap_or_default();
        let (number, short) = match kind {
            "main" => ("Main".to_owned(), "LR".to_owned()),
            "headphones" => ("Headphones".to_owned(), "PH".to_owned()),
            "stream" => ("Stream".to_owned(), "ST".to_owned()),
            "aux" => (format!("Aux {i}"), i.to_string()),
            "group" => (format!("Group {i}"), i.to_string()),
            "matrix" => (format!("Matrix {i}"), i.to_string()),
            _ => (format!("FX {i}"), i.to_string()),
        };
        out.push(json!({
            "kind": kind,
            "index": i,
            "bus_strip": k,
            "label": name,
            "number": number,
            "short": short,
            "command": command(cmd, &address),
            "plugins": address,
            // Its levels: `out[j]` of GET /meters for its two internal bus channels,
            // `bus_gr[k]` its compressor's and limiter's reduction.
            "meter": {"out": [first, first + 1], "bus_gr": k},
            // Its channels in the output patch (`bus:J`) and the outputs carrying them.
            "bus_ext": ext,
            "outputs": outputs_of(s, &ext.map(|j| format!("bus:{j}"))),
        }));
    }
    for (k, d) in s.dca.iter().enumerate() {
        let n = k + 1;
        out.push(json!({
            "kind": "dca",
            "index": n,
            "label": d.name,
            "number": format!("DCA {n}"),
            "short": n.to_string(),
            "command": {"cmd": "dca", "dca": n},
            "plugins": null,
            "meter": null,
            "bus_ext": null,
            // Its member channels, from 1.
            "members": s.channels.iter().enumerate()
                .filter(|(_, c)| c.dca.get(k).copied().unwrap_or(false))
                .map(|(i, _)| i + 1).collect::<Vec<_>>(),
        }));
    }
    out
}

/// The desk's sizes, from its constants.
pub fn desk_sizes(channels: usize, inputs: &Value, outputs: usize) -> Value {
    json!({
        "channels": channels,
        "inputs": inputs,
        "outputs": outputs,
        "aux": AUX,
        "groups": GROUPS,
        "matrices": MATRIX,
        "fx": FX,
        "stream": 1,
        "dca": DCA,
        "mute_groups": MUTE_GROUPS,
        "buttons": BUTTONS,
        "eq_bands": EQ_BANDS,
        "geq_bands": crate::fx::GEQ_BANDS.len(),
        "duckers": DUCKERS,
        "bus_strips": BUS_STRIPS,
        "bus_channels": OUTPUTS,
        "matrix_sources": MATRIX_SOURCES,
    })
}

/// A matrix mix's sources in the order of its `sources`: each one's name (as
/// `source_names`), its bus's label and bus strip.
pub fn matrix_sources(s: &Settings) -> Vec<Value> {
    matrix_source_names()
        .into_iter()
        .enumerate()
        .map(|(j, name)| {
            let k = match j {
                0 => 0,
                j if j <= AUX + GROUPS => j,
                j => j + MATRIX,
            };
            json!({
                "name": name,
                "label": s.bus(k).map(|b| b.name.clone()).unwrap_or_default(),
                "bus_strip": k,
            })
        })
        .collect()
}

/// Whether user button action `action` is "on" now: a muted channel (a linked pair's
/// mute), a mute group or DCA on, talkback on, every FX return muted, any solo (for
/// `clear_solo`), the snapshot recalled and unchanged.
pub fn button_lit(s: &Settings, action: &str, snapshot: &Value, any_solo: bool) -> bool {
    let (what, arg) = action.split_once(':').unwrap_or((action, ""));
    let num = arg
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1));
    match what {
        "mute" => num
            .filter(|&i| i < s.channels.len())
            .is_some_and(|i| s.channels[s.lead_of(i)].mute),
        "mute_group" => num.and_then(|g| s.mute_groups.get(g)).is_some_and(|g| g.on),
        "dca_mute" => num.and_then(|g| s.dca.get(g)).is_some_and(|d| d.mute),
        "talkback" => s.talkback.on,
        "fx_mute" => !s.fx.is_empty() && s.fx.iter().all(|f| f.mute),
        "clear_solo" => any_solo,
        "snapshot" => snapshot["name"] == arg.trim() && snapshot["changed"] == false,
        _ => false,
    }
}

/// Any channel soloed (a linked pair by its first) or any bus listened to.
pub fn any_solo(s: &Settings) -> bool {
    (0..s.channels.len()).any(|i| s.lead_of(i) == i && s.channels[i].solo)
        || (0..BUS_STRIPS - 1).any(|k| s.bus(k).is_some_and(|b| b.listen))
}

/// Adds the strips, sizes and derived values to state `v` (made from settings `s`).
pub(super) fn describe(v: &mut Value, s: &Settings) {
    let outputs = v["outputs"].as_array().map_or(s.outputs.len(), Vec::len);
    v["strips"] = json!(strips(s, &v["on_b"]));
    // A channel strip's X32 preamp (§M.23.1), as the state's `preamp` has it.
    let preamps = v["preamp"].clone();
    for (i, strip) in v["strips"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .take(s.channels.len())
        .enumerate()
    {
        strip["preamp"] = preamps.get(i).cloned().unwrap_or(Value::Null);
    }
    v["desk_sizes"] = desk_sizes(s.channels.len(), &v["inputs"], outputs);
    v["matrix_sources"] = json!(matrix_sources(s));
    let solo = any_solo(s);
    v["any_solo"] = json!(solo);
    v["buttons_lit"] = json!(
        s.buttons
            .iter()
            .map(|b| button_lit(s, &b.action, &v["snapshot"], solo))
            .collect::<Vec<_>>()
    );
    // The DSP's load: a block's processing time against the block's own time; the
    // latency through the desk: a block, its plugins' and, while a channel on the desk
    // has noise suppression on, the suppression's (every channel delayed alike).
    let rate = v["rate"].as_f64().filter(|r| *r > 0.0).unwrap_or(48_000.0);
    let block = v["block"].as_f64().filter(|b| *b > 0.0).unwrap_or(128.0);
    let block_ms = block * 1000.0 / rate;
    let load = v["block_ms"].as_f64().unwrap_or(0.0) / block_ms * 100.0;
    v["dsp_load_pct"] = json!(load.clamp(0.0, 100.0).round() as u32);
    let denoise = (0..s.channels.len())
        .any(|i| v["active"][i].as_bool().unwrap_or(false) && s.channels[s.lead_of(i)].denoise);
    let denoise_ms = if denoise {
        idn_dsp::denoise::latency(rate as u32) as f64 * 1000.0 / rate
    } else {
        0.0
    };
    let latency = block_ms + v["plugins"]["latency_ms"].as_f64().unwrap_or(0.0) + denoise_ms;
    v["latency_ms"] = json!((latency * 100.0).round() / 100.0);
    // Settings that take effect from the desk's next start, differing from how it runs.
    let mut restart = Vec::new();
    let fixed = v["rate_fixed"].as_bool().unwrap_or(false);
    if !fixed && s.rate.is_some_and(|r| f64::from(r) != rate) {
        restart.push("rate");
    }
    if s.block as f64 != block {
        restart.push("block");
    }
    v["pending_restart"] = json!(!restart.is_empty());
    v["restart_for"] = json!(restart);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Console, ConsoleConfig};

    fn console(name: &str) -> Console {
        Console::start(ConsoleConfig {
            name: name.into(),
            channels: 8,
            mdns: Some(false),
            ..Default::default()
        })
        .unwrap()
    }

    /// The strips: channels, then the 42 bus strips in the desk's order, then the DCAs,
    /// each with its command, meter place and outside bus numbers.
    #[test]
    fn the_state_lists_the_strips_in_order() {
        let c = console("Strips test");
        c.channel(1, &json!({"link": true})).unwrap();
        let st = c.state();
        let strips = st["strips"].as_array().unwrap();
        assert_eq!(strips.len(), 8 + BUS_STRIPS + DCA);
        let ch2 = &strips[1];
        assert_eq!(ch2["kind"], "channel");
        assert_eq!(ch2["command"], json!({"cmd": "channel", "n": 2}));
        assert_eq!(
            (ch2["lead"].as_u64(), ch2["linked"].as_bool()),
            (Some(1), Some(true))
        );
        assert_eq!(ch2["plugins"], json!({"channel": 1}));
        assert_eq!(ch2["meter"], json!({"ch": 1}));
        assert_eq!(ch2["short"], "02");
        let bus = |k: usize| &strips[8 + k];
        assert_eq!(bus(0)["kind"], "main");
        assert_eq!(bus(0)["command"], json!({"cmd": "bus", "bus": "main"}));
        assert_eq!(bus(0)["outputs"], json!([1, 2]));
        assert_eq!(bus(3)["command"], json!({"cmd": "bus", "bus": 3}));
        assert_eq!(bus(3)["number"], "Aux 3");
        // Aux 9 (added 2026-11-08): its outside numbers past the 52 there were, no
        // output patched on a new desk.
        assert_eq!(bus(9)["bus_ext"], json!([52, 53]));
        assert_eq!(bus(9)["meter"]["out"], json!([18, 19]));
        assert_eq!(bus(9)["outputs"], json!([]));
        let g2 = bus(AUX + 2);
        assert_eq!(
            (g2["kind"].as_str(), g2["command"].clone()),
            (Some("group"), json!({"cmd": "group", "group": 2}))
        );
        assert_eq!(
            bus(AUX + GROUPS + 1)["command"],
            json!({"cmd": "matrix", "matrix": 1})
        );
        let fx = bus(AUX + GROUPS + MATRIX + 1);
        assert_eq!(
            (fx["kind"].as_str(), fx["label"].as_str()),
            (Some("fx"), Some("Reverb"))
        );
        let hp = bus(BUS_STRIPS - 1);
        assert_eq!(hp["command"], json!({"cmd": "bus", "bus": "headphones"}));
        assert_eq!(
            hp["meter"]["out"],
            json!([crate::desk::MONITOR, crate::desk::MONITOR + 1])
        );
        assert_eq!(
            strips[8 + BUS_STRIPS]["command"],
            json!({"cmd": "dca", "dca": 1})
        );
        // Every bus strip's command is taken by the desk.
        for k in 0..BUS_STRIPS {
            let mut cmd = bus(k)["command"].clone();
            let name = cmd["cmd"].as_str().unwrap().to_owned();
            cmd.as_object_mut().unwrap().remove("cmd");
            cmd["mute"] = json!(false);
            c.command(&name, &cmd).unwrap();
        }
        // The sizes and the matrix sources.
        let z = &st["desk_sizes"];
        assert_eq!(
            (z["aux"].as_u64(), z["eq_bands"].as_u64()),
            (Some(16), Some(8))
        );
        assert_eq!(
            (z["geq_bands"].as_u64(), z["mute_groups"].as_u64()),
            (Some(31), Some(8))
        );
        let ms = st["matrix_sources"].as_array().unwrap();
        assert_eq!(ms.len(), MATRIX_SOURCES);
        assert_eq!(ms[AUX + 1]["name"], "group 1");
        let last = &ms[MATRIX_SOURCES - 1];
        assert_eq!(
            (last["name"].as_str(), last["bus_strip"].as_u64()),
            (Some("fx 8"), Some((BUS_STRIPS - 3) as u64))
        );
        assert_eq!(last["label"], st["settings"]["fx"][7]["name"]);
    }

    /// Lit user keys, any solo, the load and latency, a restart pending.
    #[test]
    fn the_state_derives_what_panels_worked_out() {
        let c = console("Derived test");
        for (k, action) in [
            "mute:2",
            "mute_group:1",
            "dca_mute:3",
            "talkback",
            "fx_mute",
            "clear_solo",
            "snapshot:A",
            "",
        ]
        .iter()
        .enumerate()
        {
            c.button(k + 1, &json!({"action": action})).unwrap();
        }
        let lit = |c: &Console| c.state()["buttons_lit"].clone();
        assert_eq!(lit(&c), json!(vec![false; 8]));
        c.channel(1, &json!({"link": true, "mute": true})).unwrap();
        c.mute_group(1, &json!({"on": true})).unwrap();
        c.dca(3, &json!({"mute": true})).unwrap();
        c.talkback(&json!({"on": true})).unwrap();
        c.press(5).unwrap();
        c.bus("2", &json!({"listen": true})).unwrap();
        c.snapshot_save("A").unwrap();
        assert_eq!(
            lit(&c),
            json!([true, true, true, true, true, true, true, false])
        );
        assert_eq!(c.state()["any_solo"], true);
        c.clear_solo().unwrap();
        let st = c.state();
        assert_eq!(st["any_solo"], false);
        // (The snapshot changed since it was saved.)
        assert_eq!(st["buttons_lit"][5], false);
        assert_eq!(st["buttons_lit"][6], false);
        // Load and latency: a 128-sample block at 48 kHz is 2.67 ms.
        assert!(st["dsp_load_pct"].as_u64().unwrap() <= 100);
        assert!(
            (st["latency_ms"].as_f64().unwrap() - 2.67).abs() < 0.01,
            "{}",
            st["latency_ms"]
        );
        assert_eq!(
            (st["pending_restart"].as_bool(), st["restart_for"].clone()),
            (Some(false), json!([]))
        );
        c.desk(&json!({"block": 64, "rate": 96000})).unwrap();
        let st = c.state();
        assert_eq!(st["pending_restart"], true);
        assert_eq!(st["restart_for"], json!(["rate", "block"]));
    }

    /// Only an odd-numbered channel leads a pair: `link` on an even-numbered one (a
    /// settings file's) doesn't make it or the next a pair (bug run O M6).
    #[test]
    fn an_even_channels_link_is_no_pair() {
        let mut s = Settings::new(4);
        s.channels[1].link = true;
        let st = strips(&s, &json!([]));
        assert_eq!(
            (st[1]["linked"].as_bool(), st[2]["linked"].as_bool()),
            (Some(false), Some(false))
        );
        assert_eq!(st[2]["lead"], 3);
        // Made what the desk makes it: no link there.
        assert!(!s.normalised(4, OUTPUTS).channels[1].link);
    }

    /// The latency through the desk counts noise suppression's while a channel on the
    /// desk has it on (bug run O L1).
    #[test]
    fn the_latency_counts_noise_suppression() {
        let c = console("Latency test");
        let latency = |c: &Console| c.state()["latency_ms"].as_f64().unwrap();
        let block_ms = latency(&c);
        c.channel(2, &json!({"denoise": true})).unwrap();
        let rate = crate::desk::rate_hz();
        let nr = idn_dsp::denoise::latency(rate) as f64 * 1000.0 / f64::from(rate);
        // (Channel 2 on the desk once placed: every input counts as routed with no Node.)
        idn_common::testing::wait_for(
            "the latency with noise suppression",
            std::time::Duration::from_secs(3),
            || (latency(&c) - block_ms - nr).abs() <= 0.02,
        );
        c.channel(2, &json!({"shown": false})).unwrap();
        assert!((latency(&c) - block_ms).abs() < 0.02, "parked: none");
    }

    #[test]
    fn button_lit_reads_the_action() {
        let mut s = Settings::new(4);
        s.channels[0].link = true;
        s.channels[0].mute = true;
        let none = json!(null);
        // A linked pair's second: the pair's mute.
        assert!(button_lit(&s, "mute:2", &none, false));
        assert!(!button_lit(&s, "mute:3", &none, false));
        assert!(!button_lit(&s, "mute:9", &none, false));
        assert!(!button_lit(&s, "mute:x", &none, false));
        let sc = json!({"name": "Sermon", "changed": false});
        assert!(button_lit(&s, "snapshot:Sermon", &sc, false));
        assert!(!button_lit(&s, "snapshot:Other", &sc, false));
        assert!(button_lit(&s, "clear_solo", &none, true));
    }
}
