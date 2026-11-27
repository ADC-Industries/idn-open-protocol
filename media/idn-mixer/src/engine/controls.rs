//! The desk's control limits (`GET /api/v1/controls`, spec §M.5): every setting a panel
//! has a knob, slider or list for, with the range the desk keeps it in (its own clamps,
//! `desk/settings_of.rs`, `fx.rs`), its step, default (the settings' `Default`s), unit and
//! scale, and the choices of the settings that are lists (EQ shapes, effects, taps, the
//! output patch's and the user buttons' grammars, rates, blocks).
//!
//! A level of −144 dB (or below) is off (−∞): faders, sends, a matrix's sources,
//! talkback, the audio-follows-video off-air level.

use crate::desk::{
    Afv, BLOCKS, Comp, DCA, DeEsser, Ducker, Gate, HPF_DEFAULT, LPF_DEFAULT, Limiter, MUTE_GROUPS,
    Settings,
};
use crate::fx::{Effect, GEQ_BANDS};
use serde_json::{Value, json};

/// The level that is off (−∞), dB.
pub const OFF_DB: f32 = -144.0;

/// One control: its range, step, default, unit and scale (`lin`, `log` or `db`: a level
/// whose bottom, −144, is off).
fn ctl(min: f64, max: f64, step: f64, default: f64, unit: &str, scale: &str) -> Value {
    json!({"min": min, "max": max, "step": step, "default": default, "unit": unit, "scale": scale})
}

/// A level in dB (−144 = off … `max`), with a fader's taper: travel 0–1 against dB.
fn level(default: f64, max: f64) -> Value {
    let mut v = ctl(f64::from(OFF_DB), max, 0.5, default, "dB", "db");
    v["off"] = json!(OFF_DB);
    // The panel's fader: the bottom off, −60…−30 dB over the first quarter, −30…+10
    // over the rest.
    v["taper"] = json!([[0.0, OFF_DB], [0.001, -60.0], [0.25, -30.0], [1.0, 10.0]]);
    v
}

fn options(list: &[(Value, &str)]) -> Value {
    Value::Array(
        list.iter()
            .map(|(v, l)| json!({"value": v, "label": l}))
            .collect(),
    )
}

fn gate() -> Value {
    let d = Gate::default();
    json!({
        "threshold": ctl(-80.0, 0.0, 0.5, d.threshold.into(), "dB", "lin"),
        "range": ctl(0.0, 80.0, 1.0, d.range.into(), "dB", "lin"),
        "attack": ctl(0.1, 50.0, 0.1, d.attack.into(), "ms", "log"),
        "release": ctl(5.0, 2000.0, 1.0, d.release.into(), "ms", "log"),
        "hold": ctl(0.0, 2000.0, 1.0, d.hold.into(), "ms", "lin"),
        "mode": {"options": options(&[(json!("gate"), "Gate"), (json!("expander"), "Expander"), (json!("ducker"), "Ducker")]), "default": "gate"},
        "ratio": ctl(1.0, 10.0, 0.1, d.ratio.into(), ":1", "log"),
        "key": key(),
        "key_hpf": key_filter(),
        "key_lpf": key_filter(),
    })
}

/// A dynamics stage's key: a channel from 1 (0: its own).
fn key() -> Value {
    json!({"min": 0, "max": null, "max_from": "channels", "default": 0, "about": "0: its own sound"})
}

/// A key filter's frequency (0: off).
fn key_filter() -> Value {
    let mut v = ctl(20.0, 20_000.0, 1.0, 0.0, "Hz", "log");
    v["off"] = json!(0.0);
    v
}

fn comp() -> Value {
    let d = Comp::default();
    json!({
        "threshold": ctl(-60.0, 0.0, 0.5, d.threshold.into(), "dB", "lin"),
        "ratio": ctl(1.0, 20.0, 0.1, d.ratio.into(), ":1", "log"),
        "attack": ctl(0.1, 200.0, 0.1, d.attack.into(), "ms", "log"),
        "release": ctl(5.0, 2000.0, 1.0, d.release.into(), "ms", "log"),
        "knee": ctl(0.0, 20.0, 0.5, d.knee.into(), "dB", "lin"),
        "makeup": ctl(0.0, 24.0, 0.5, d.makeup.into(), "dB", "lin"),
        "mix": ctl(0.0, 100.0, 1.0, d.mix.into(), "%", "lin"),
        "detect": {"options": options(&[(json!("peak"), "Peak"), (json!("rms"), "RMS")]), "default": "peak"},
        "auto_release": {"options": options(&[(json!(false), "Off"), (json!(true), "On")]), "default": false},
        "key": key(),
        "key_hpf": key_filter(),
    })
}

fn limiter() -> Value {
    let d = Limiter::default();
    json!({
        "ceiling": ctl(-30.0, 0.0, 0.5, d.ceiling.into(), "dBFS", "lin"),
        "release": ctl(5.0, 1000.0, 1.0, d.release.into(), "ms", "log"),
    })
}

/// The parametric EQ's bands (each band's default as a new desk has it).
fn eq() -> Value {
    let bands = Settings::new(1).channels[0].eq.clone();
    json!({
        // (Not held by the settings: the filters keep to 10 Hz … 0.45 × the rate.)
        "freq": ctl(20.0, 20_000.0, 1.0, 1000.0, "Hz", "log"),
        "gain": ctl(-15.0, 15.0, 0.5, 0.0, "dB", "lin"),
        "q": ctl(0.1, 10.0, 0.01, 0.707, "", "log"),
        "shape": {"options": eq_shapes()},
        "bands": bands,
    })
}

/// The EQ shapes, and which have a gain (the others ignore it).
pub fn eq_shapes() -> Value {
    Value::Array(
        [
            ("low_shelf", "Low shelf", true),
            ("peak", "Peak", true),
            ("veq", "Vintage peak", true),
            ("high_shelf", "High shelf", true),
            ("high_pass", "High-pass", false),
            ("low_pass", "Low-pass", false),
            ("notch", "Notch", false),
        ]
        .iter()
        .map(|(v, l, g)| json!({"value": v, "label": l, "has_gain": g}))
        .collect(),
    )
}

/// A send's (aux or FX) level, place and tap.
fn send() -> Value {
    json!({
        "level": level(0.0, 10.0),
        "pan": ctl(-1.0, 1.0, 0.05, 0.0, "", "lin"),
        "pre": {"options": options(&[(Value::Null, "As the bus says"), (json!(true), "Before the fader"), (json!(false), "After the fader")]), "default": null},
        "tap": {"options": options(&[(Value::Null, "As pre says"), (json!("input"), "Input"), (json!("pre_eq"), "Before the EQ"), (json!("post_eq"), "After the EQ"), (json!("pre_fader"), "Before the fader"), (json!("post_fader"), "After the fader")]), "default": null},
    })
}

/// The built-in effects (§M.14) and each kind's settings.
fn effect() -> Value {
    let d = Effect::default();
    let f = |v: f32| f64::from(v);
    json!({
        "kind": {"options": options(&[(json!("none"), "No built-in effect"), (json!("reverb"), "Reverb"), (json!("delay"), "Delay"), (json!("chorus"), "Chorus")]), "default": "none"},
        "reverb": {
            "size": ctl(0.0, 1.0, 0.01, f(d.size), "", "lin"),
            "damping": ctl(0.0, 1.0, 0.01, f(d.damping), "", "lin"),
            "predelay": ctl(0.0, 200.0, 1.0, f(d.predelay), "ms", "lin"),
            "width": ctl(0.0, 1.0, 0.01, f(d.width), "", "lin"),
        },
        "delay": {
            "time": ctl(1.0, 2000.0, 1.0, f(d.time), "ms", "log"),
            "feedback": ctl(0.0, 0.95, 0.01, f(d.feedback), "", "lin"),
            "tone": ctl(1000.0, 20_000.0, 10.0, f(d.tone), "Hz", "log"),
            "pingpong": {"options": [false, true], "default": d.pingpong},
        },
        "chorus": {
            "rate": ctl(0.05, 5.0, 0.01, f(d.rate), "Hz", "log"),
            "depth": ctl(0.1, 10.0, 0.1, f(d.depth), "ms", "lin"),
        },
    })
}

/// The graphic EQ's bands' labels, in [`GEQ_BANDS`]' order.
const GEQ_LABELS: [&str; GEQ_BANDS.len()] = [
    "20", "25", "31", "40", "50", "63", "80", "100", "125", "160", "200", "250", "315", "400",
    "500", "630", "800", "1k", "1.2k", "1.6k", "2k", "2.5k", "3.1k", "4k", "5k", "6.3k", "8k",
    "10k", "12k", "16k", "20k",
];

/// The graphic EQ's bands: centre (Hz) and label.
fn geq_bands() -> Value {
    Value::Array(
        GEQ_BANDS
            .iter()
            .zip(GEQ_LABELS)
            .map(|(hz, label)| json!({"hz": hz, "label": label}))
            .collect(),
    )
}

/// The control limits table (`GET /api/v1/controls`).
pub fn controls() -> Value {
    let de = DeEsser::default();
    let du = Ducker::default();
    let afv = Afv::default();
    let f = |v: f32| f64::from(v);
    let mut v = json!({
        "off_db": OFF_DB,
        "channel": {
            "trim": ctl(-20.0, 40.0, 0.5, 0.0, "dB", "lin"),
            // 0 is off; `hpf_on` / `lpf_on` switch at the last frequency (`hpf_last`,
            // `lpf_last`). Not held by the settings: the filter keeps to 10 Hz … 0.45 ×
            // the rate.
            "hpf": {"min": 20.0, "max": 400.0, "step": 1.0, "default": 0.0, "off": 0.0, "on_default": HPF_DEFAULT, "unit": "Hz", "scale": "log"},
            "lpf": {"min": 1000.0, "max": 20_000.0, "step": 10.0, "default": 0.0, "off": 0.0, "on_default": LPF_DEFAULT, "unit": "Hz", "scale": "log"},
            "denoise_amount": ctl(0.0, 100.0, 1.0, 100.0, "%", "lin"),
            "insert_point": {"options": options(&[(json!("post"), "After the processing"), (json!("pre"), "Before the processing")]), "default": "post"},
            "hpf_slope": {"options": options(&[(json!(6), "6 dB/oct"), (json!(12), "12 dB/oct"), (json!(18), "18 dB/oct"), (json!(24), "24 dB/oct"), (json!(36), "36 dB/oct"), (json!(48), "48 dB/oct")]), "default": 12, "unit": "dB/oct"},
            "lpf_slope": {"options": options(&[(json!(6), "6 dB/oct"), (json!(12), "12 dB/oct"), (json!(18), "18 dB/oct"), (json!(24), "24 dB/oct"), (json!(36), "36 dB/oct"), (json!(48), "48 dB/oct")]), "default": 12, "unit": "dB/oct"},
            "delay": ctl(0.0, 250.0, 0.1, 0.0, "ms", "lin"),
            "fader": level(0.0, 10.0),
            "pan": ctl(-1.0, 1.0, 0.02, 0.0, "", "lin"),
            "automix_weight": ctl(-12.0, 12.0, 0.5, 0.0, "dB", "lin"),
            "gate": gate(),
            "comp": comp(),
            "limiter": limiter(),
            "deesser": {
                "freq": ctl(2000.0, 16_000.0, 10.0, f(de.freq), "Hz", "log"),
                "threshold": ctl(-60.0, 0.0, 0.5, f(de.threshold), "dB", "lin"),
                "range": ctl(0.0, 24.0, 0.5, f(de.range), "dB", "lin"),
                "split": {"options": [false, true], "default": de.split},
            },
            "eq": eq(),
            "sends": send(),
            "fx_sends": send(),
            // A desk input from 1 (to the state's `inputs`); 0: none.
            "source": {"min": 0, "max": null, "max_from": "inputs", "default": null, "about": "null: its own number"},
            "shown": {"options": options(&[(Value::Null, "As routing says"), (json!(true), "On the desk"), (json!(false), "Parked")]), "default": null},
        },
        "bus": {
            "fader": level(0.0, 10.0),
            "dim_db": ctl(-60.0, 0.0, 1.0, -20.0, "dB", "lin"),
            "eq": eq(),
            "comp": comp(),
            "limiter": limiter(),
            // A matrix mix's sources (the state's `matrix_sources` order).
            "sources": {"level": level(0.0, 10.0)},
            "geq": {"gain": ctl(-15.0, 15.0, 0.5, 0.0, "dB", "lin"), "bands": geq_bands(), "on": "main, aux, matrix and headphones"},
            "effect": effect(),
        },
        "dca": {"fader": level(0.0, 10.0)},
        "talkback": {"level": level(0.0, 10.0), "dim_main": ctl(-40.0, 0.0, 1.0, 0.0, "dB", "lin")},
        "ducker": {
            "threshold": ctl(-80.0, 0.0, 1.0, f(du.threshold), "dB", "lin"),
            "depth": ctl(-60.0, 0.0, 1.0, f(du.depth), "dB", "lin"),
            "attack": ctl(1.0, 1000.0, 1.0, f(du.attack), "ms", "log"),
            "hold": ctl(0.0, 5000.0, 10.0, f(du.hold), "ms", "lin"),
            "release": ctl(10.0, 10_000.0, 10.0, f(du.release), "ms", "log"),
        },
        "afv": {
            "off": {"min": OFF_DB, "max": 0.0, "step": 1.0, "default": f(afv.off), "off": OFF_DB, "unit": "dB", "scale": "db",
                    "options": options(&[(json!(OFF_DB), "Off"), (json!(-40.0), "−40 dB"), (json!(-30.0), "−30 dB"), (json!(-20.0), "−20 dB"), (json!(-10.0), "−10 dB")])},
            "fade": ctl(0.0, 5000.0, 50.0, f(afv.fade), "ms", "lin"),
        },
        "output": {
            "delay": ctl(0.0, 500.0, 0.1, 0.0, "ms", "lin"),
            "source": {
                "grammar": ["<tap>:<channel>", "bus:<J>", "in:<K>", ""],
                "about": "<channel> from 1; <J> a bus channel's outside number (a strip's `bus_ext`, the state's `buses`); <K> a desk input from 1; \"\": nothing",
                "taps": options(&[(json!("input"), "Input"), (json!("preeq"), "Pre EQ"), (json!("posteq"), "Post EQ"), (json!("pre"), "Pre fader"), (json!("post"), "Post fader")]),
            },
        },
        "button": {
            "action": {
                "grammar": ["mute:<N>", "mute_group:<N>", "dca_mute:<N>", "talkback", "clear_solo", "fx_mute", "snapshot:<NAME>", ""],
                "about": format!("mute:<N> a channel (from 1); mute_group:<N> 1–{MUTE_GROUPS}; dca_mute:<N> 1–{DCA}; fx_mute every FX return; snapshot:<NAME> recalls it; \"\": nothing"),
            },
        },
        // The X32 preamp behind a channel (§M.23.1, the `preamp` command): only where the
        // state's `preamp` has one.
        "preamp": {
            "gain_db": ctl(
                f64::from(crate::x32::proto::GAIN_MIN_DB),
                f64::from(crate::x32::proto::GAIN_MAX_DB),
                f64::from(crate::x32::proto::GAIN_STEP_DB),
                0.0,
                "dB",
                "lin",
            ),
            "phantom": {"options": [false, true], "label": "48 V", "about": "48 V phantom power on the X32 input; only ever changed when asked"},
            "invert": {"options": [false, true], "label": "X32 polarity", "about": "inverts the X32's own channel strip, not what the desk receives over USB; the desk channel's own polarity is what changes the desk's sound"},
        },
        "desk": {
            "x32": {"address": {"about": "\"\": none; \"auto\": the first X32 found on the LAN; else its IP address or host name (:port if not 10023)", "default": ""}},
            "rate": {"options": idn_open_protocol::node::rate::RATES, "default": null, "about": "null: the machine's; from the desk's next start"},
            "block": {"options": BLOCKS, "default": crate::desk::BLOCK, "unit": "samples", "about": "from the desk's next start"},
        },
    });
    // (Set apart: the json! above is at the macro's recursion limit.)
    // In the IEM flow (§M.26), and an IEM mix's levels (`iem_mix`).
    v["channel"]["iem"] = json!({"options": [false, true], "default": false, "label": "IEM"});
    v["iem_mix"] = json!({
        "master_db": level(0.0, f64::from(crate::iem::LEVEL_MAX)),
        "level_db": level(0.0, f64::from(crate::iem::LEVEL_MAX)),
        "pan": ctl(-1.0, 1.0, 0.02, 0.0, "", "lin"),
        "max_mixes": crate::iem::MAX_MIXES,
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Console, ConsoleConfig};

    /// `{"a": [null, {"b": leaf}]}` from `["a", "1", "b"]`: a merge patch of one setting.
    fn nest(parts: &[&str], leaf: Value) -> Value {
        match parts.split_first() {
            None => leaf,
            Some((p, rest)) => {
                let inner = nest(rest, leaf);
                match p.parse::<usize>() {
                    Ok(i) => {
                        let mut a = vec![Value::Null; i + 1];
                        a[i] = inner;
                        Value::Array(a)
                    }
                    Err(_) => json!({ *p: inner }),
                }
            }
        }
    }

    /// The table's ranges are the desk's own: a value past either end comes back held at
    /// it; the defaults are a new desk's; every EQ shape and effect kind is one it takes.
    #[test]
    fn the_controls_match_what_the_desk_keeps() {
        let t = controls();
        let c = Console::start(ConsoleConfig {
            name: "Controls test".into(),
            channels: 4,
            mdns: Some(false),
            ..Default::default()
        })
        .unwrap();
        // (control path in the table, path in a channel's settings)
        let ch = &t["channel"];
        for (ctl, key) in [
            (&ch["trim"], "/trim"),
            (&ch["delay"], "/delay"),
            (&ch["fader"], "/fader"),
            (&ch["pan"], "/pan"),
            (&ch["automix_weight"], "/automix_weight"),
            (&ch["gate"]["threshold"], "/gate/threshold"),
            (&ch["gate"]["release"], "/gate/release"),
            (&ch["comp"]["ratio"], "/comp/ratio"),
            (&ch["comp"]["makeup"], "/comp/makeup"),
            (&ch["limiter"]["ceiling"], "/limiter/ceiling"),
            (&ch["deesser"]["freq"], "/deesser/freq"),
            (&ch["deesser"]["range"], "/deesser/range"),
            (&ch["sends"]["level"], "/sends/0/level"),
        ] {
            let (min, max) = (ctl["min"].as_f64().unwrap(), ctl["max"].as_f64().unwrap());
            let set = c.settings();
            let now = serde_json::to_value(&set.channels[0]).unwrap();
            assert_eq!(
                now.pointer(key).unwrap().as_f64().unwrap(),
                ctl["default"].as_f64().unwrap(),
                "{key}'s default"
            );
            for (v, want) in [(min - 1000.0, min), (max + 1000.0, max)] {
                let parts: Vec<&str> = key.trim_start_matches('/').split('/').collect();
                let patch = nest(&parts, json!(v));
                c.channel(1, &patch).unwrap();
                let got = serde_json::to_value(&c.settings().channels[0]).unwrap();
                let got = got.pointer(key).unwrap().as_f64().unwrap();
                assert!(
                    (got - want).abs() < 1e-3,
                    "{key}: {v} became {got}, want {want}"
                );
            }
        }
        // Every EQ shape and effect kind is taken.
        for s in t["channel"]["eq"]["shape"]["options"].as_array().unwrap() {
            c.channel(1, &json!({"eq": [{"shape": s["value"]}]}))
                .unwrap();
        }
        for k in t["bus"]["effect"]["kind"]["options"].as_array().unwrap() {
            c.command("fx", &json!({"fx": 1, "effect": {"kind": k["value"]}}))
                .unwrap();
        }
        assert!(c.channel(1, &json!({"eq": [{"shape": "nope"}]})).is_err());
        // The GEQ's labels, the blocks, the rates.
        let bands = t["bus"]["geq"]["bands"].as_array().unwrap();
        assert_eq!(bands.len(), 31);
        let labels: Vec<&str> = bands.iter().map(|b| b["label"].as_str().unwrap()).collect();
        assert_eq!(&labels[..3], ["20", "25", "31"]);
        assert_eq!(&labels[17..20], ["1k", "1.2k", "1.6k"]);
        assert_eq!(&labels[28..], ["12k", "16k", "20k"]);
        assert_eq!(t["desk"]["block"]["options"], json!([32, 64, 128, 256]));
        assert_eq!(t["desk"]["rate"]["options"], json!([44100, 48000, 96000]));
        // Every output tap is one the output patch takes.
        for tap in t["output"]["source"]["taps"].as_array().unwrap() {
            let src = format!("{}:1", tap["value"].as_str().unwrap());
            c.patch_output(1, &src).unwrap();
        }
    }
}
