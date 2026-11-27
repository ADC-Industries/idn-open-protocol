//! The IEM mixes and flow through the desk's API (§M.26): the mixes' commands, the flow's
//! description (`GET /api/v1/iem`), the state's `iem`, and `send` (one channel's send into
//! one aux bus).

use super::*;
use crate::iem::{self, IemMix, MAX_MIXES};

impl Console {
    /// The mixes' changes: the settings, without the plugins brought in line (a phone's
    /// fader moves many times a second, and touches no plugin).
    fn edit_iem(&self, f: impl FnOnce(&mut Settings) -> Result<(), String>) -> Result<(), String> {
        change_settings(&self.shared, false, f)?;
        self.changed();
        Ok(())
    }

    /// Adds an IEM mix named `name` (with id `id`, else one made from the name); its id.
    pub fn iem_mix_add(&self, name: &str, id: Option<&str>) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("iem_mix_add: a name".into());
        }
        let mut made = String::new();
        self.edit_iem(|s| {
            if s.iem_mixes.len() >= MAX_MIXES {
                return Err(format!("the desk keeps {MAX_MIXES} IEM mixes at most"));
            }
            let id = match id.map(str::trim).filter(|i| !i.is_empty()) {
                Some(i) if !iem::valid_id(i) => {
                    return Err(format!(
                        "id {i:?}: letters, digits, '-', '_' and '.', up to 64"
                    ));
                }
                Some(i) if s.iem_mixes.iter().any(|m| m.id == i) => {
                    return Err(format!("a mix {i:?} is there already"));
                }
                Some(i) => i.to_owned(),
                None => iem::new_id(name, &s.iem_mixes),
            };
            s.iem_mixes.push(IemMix {
                id: id.clone(),
                name: name.to_owned(),
                ..Default::default()
            });
            made = id;
            Ok(())
        })?;
        Ok(made)
    }

    /// Changes IEM mix `v["id"]` by a partial update ([`iem::patch`]).
    pub fn iem_mix(&self, v: &Value) -> Result<(), String> {
        let id = v["id"].as_str().ok_or("id: the mix's id")?;
        self.edit_iem(|s| {
            let n = s.channels.len();
            let m = s
                .iem_mixes
                .iter_mut()
                .find(|m| m.id == id)
                .ok_or(format!("no IEM mix {id:?}"))?;
            iem::patch(m, v, n)
        })
    }

    /// Deletes IEM mix `id`.
    pub fn iem_mix_delete(&self, id: &str) -> Result<(), String> {
        self.edit_iem(|s| {
            let before = s.iem_mixes.len();
            s.iem_mixes.retain(|m| m.id != id);
            if s.iem_mixes.len() == before {
                return Err(format!("no IEM mix {id:?}"));
            }
            Ok(())
        })
    }

    /// One channel's send into one aux bus (`{n, bus, level?, on?, pan?, pre?}`): the
    /// `channel` command's `sends` without the other buses' nulls.
    pub fn send(&self, v: &Value) -> Result<(), String> {
        let (n, bus) = send_target(v)?;
        self.channel(n, &send_patch(bus, v)?)
    }

    /// The flow's channels now (§M.26) and their version.
    pub fn iem_channels(&self) -> (Vec<iem::FlowChannel>, u16) {
        let chans = iem::flow_channels(&self.settings());
        let layout = self.shared.iem.layout_of(&chans);
        (chans, layout)
    }

    /// The flow as a phone needs it (`GET /api/v1/iem`, idn-iem.md §I.2): its channels,
    /// where it is (multicast, unicast), its coding and latency.
    pub fn iem_description(&self) -> Value {
        let hub = &self.shared.iem;
        let (chans, layout) = self.iem_channels();
        let rate = rate_hz();
        let block = self.shared.desk.lock_ok().block();
        let block_ms = block as f64 * 1000.0 / f64::from(rate);
        // The resampler to 48 kHz: half its 128-tap filter and a block.
        let resample_ms = if rate == idn_iem::RATE {
            0.0
        } else {
            (64.0 + block as f64) * 1000.0 / f64::from(rate)
        };
        let frame_ms = idn_iem::FRAME_SAMPLES as f64 * 1000.0 / f64::from(idn_iem::RATE);
        let look_ahead_ms = 2.5;
        let r1 = |v: f64| (v * 10.0).round() / 10.0;
        let codec = json!({
            "name": "opus", "rate": idn_iem::RATE, "channels": 1,
            "frame_samples": idn_iem::FRAME_SAMPLES, "frame_ms": frame_ms,
            "bitrate": idn_iem::BITRATE, "vbr": "constrained",
            "application": "restricted_lowdelay", "fec": false,
        });
        json!({
            "wire_version": idn_iem::wire::VERSION,
            "desk": self.shared.cfg.name,
            "flow": hub.flow,
            "layout": layout,
            "channels": chans.iter().map(iem::FlowChannel::json).collect::<Vec<_>>(),
            // The uncompressed flow for wired listeners: an IDN source.
            "idn_source": hub.source(),
            "multicast": hub.group.map(|g| json!({"group": g.ip().to_string(), "port": g.port(), "ttl": 1})),
            "unicast": hub.port().map(|p| json!({"port": p, "max_phones": hub.cfg.max_phones})),
            "session": {
                "keepalive_ms": idn_iem::KEEPALIVE_MS,
                "timeout_ms": hub.cfg.timeout.as_millis() as u64,
                "token": "a site token for the desk, iem:<mix>, read or control (none from this machine)",
            },
            "codec": codec,
            "talkback": {
                "codec": "opus", "rate": idn_iem::RATE, "channels": 1,
                "max_frame_samples": idn_iem::MAX_TALK_SAMPLES,
            },
            // The desk's part of the latency (§I.6): its block, the conversion to 48 kHz,
            // a frame's wait and the coder's look-ahead; the phone adds the network, its
            // buffer and its sound output.
            "latency": {
                "block_ms": r1(block_ms),
                "resample_ms": r1(resample_ms),
                "frame_ms": frame_ms,
                "look_ahead_ms": look_ahead_ms,
                "desk_ms": r1(block_ms + resample_ms + frame_ms + look_ahead_ms),
                "phone_buffer_ms": 15,
            },
        })
    }

    /// The state's `iem` (§M.26): the flow's channels, the mixes (each with its talkback
    /// input), the phones and the flow's place on the network.
    pub(super) fn iem_state(&self, s: &Settings) -> Value {
        let hub = &self.shared.iem;
        let chans = iem::flow_channels(s);
        let layout = hub.layout_of(&chans);
        let talking = hub.talking_slots();
        let mixes: Vec<Value> = s
            .iem_mixes
            .iter()
            .map(|m| self.mix_json(m, &talking))
            .collect();
        let mut v = hub.status();
        v["channels"] = json!(chans.iter().map(iem::FlowChannel::json).collect::<Vec<_>>());
        v["layout"] = json!(layout);
        v["mixes"] = json!(mixes);
        v["talkback_inputs"] = json!([
            hub.first_talkback,
            hub.first_talkback + iem::TALKBACK_INPUTS - 1
        ]);
        v
    }

    /// A mix as the state shows it: its settings, its talkback input's desk input number
    /// and whether a phone talks into it now.
    fn mix_json(&self, m: &IemMix, talking: &[bool]) -> Value {
        let mut v = serde_json::to_value(m).unwrap_or_default();
        v["talkback_input"] = json!(self.shared.iem.first_talkback + m.talkback.max(1) - 1);
        v["talking"] = json!(
            talking
                .get(m.talkback.wrapping_sub(1))
                .copied()
                .unwrap_or(false)
        );
        // Who is on it now (§M.28): the phones listening to this mix.
        v["who"] = json!(self.shared.iem.phones_on(&m.id));
        v
    }

    /// The state for an `iem:<mix>` token (§M.25, §M.26): the desk's name, version and
    /// lock, the flow's channels and that mix alone (`error` when it isn't on the desk).
    pub fn iem_token_state(&self, scope: &str, mix: &str) -> Value {
        let s = self.settings();
        let (chans, layout) = self.iem_channels();
        let talking = self.shared.iem.talking_slots();
        let m = s.iem_mixes.iter().find(|m| m.id == mix);
        json!({
            "version": self.version(),
            "name": self.shared.cfg.name,
            "locked": self.locked(),
            "scope": scope,
            "channels": chans.iter().map(iem::FlowChannel::json).collect::<Vec<_>>(),
            "layout": layout,
            "mix": m.map(|m| self.mix_json(m, &talking)),
            "error": m.is_none().then(|| format!("no IEM mix {mix:?} on this desk")),
        })
    }
}

/// `send`'s channel and aux bus.
pub(super) fn send_target(v: &Value) -> Result<(usize, usize), String> {
    let n = v["n"]
        .as_u64()
        .filter(|&n| n >= 1)
        .ok_or("n: the channel, from 1")? as usize;
    // (0: the stream bus.)
    if v["bus"] == "stream" {
        return Ok((n, 0));
    }
    let bus = match &v["bus"] {
        Value::Number(b) => b.as_u64(),
        Value::String(b) => b.trim_start_matches("aux:").parse().ok(),
        _ => None,
    }
    .map(|b| b as usize)
    .filter(|b| (1..=crate::desk::AUX).contains(b))
    .ok_or(format!(
        "bus: the aux bus, 1 to {}, or \"stream\"",
        crate::desk::AUX
    ))?;
    Ok((n, bus))
}

/// `send`'s body as a channel merge patch: `sends` with only that bus's element.
pub(super) fn send_patch(bus: usize, v: &Value) -> Result<Value, String> {
    let mut one = serde_json::Map::new();
    for k in ["level", "on", "pan", "pre"] {
        match v.get(k) {
            None => {}
            Some(x) => {
                let ok = match k {
                    "on" => x.is_boolean(),
                    // (null: as the bus says, and as the channel's pan)
                    "pre" => x.is_boolean() || x.is_null(),
                    "pan" => x.is_number() || x.is_null(),
                    _ => x.is_number(),
                };
                if !ok {
                    return Err(format!("send {k}: {x}"));
                }
                one.insert(k.to_owned(), x.clone());
            }
        }
    }
    for k in v.as_object().into_iter().flat_map(|o| o.keys()) {
        if !matches!(
            k.as_str(),
            "n" | "bus" | "level" | "on" | "pan" | "pre" | "cmd"
        ) {
            return Err(format!("send: no key {k:?}"));
        }
    }
    if one.is_empty() {
        return Err("send: level, on, pan and/or pre".into());
    }
    if bus == 0 {
        return Ok(json!({ "stream": Value::Object(one) }));
    }
    let mut sends = vec![Value::Null; bus];
    sends[bus - 1] = Value::Object(one);
    Ok(json!({ "sends": sends }))
}
