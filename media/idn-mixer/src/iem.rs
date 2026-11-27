//! In-ear monitoring (spec/extensions/idn-mixer.md §M.26 and
//! idn-iem.md): the desk marks channels `iem` and sends them all, after their processing
//! and before their faders, as one flow (an IDN audio output for wired listeners, and an
//! Opus copy for phones over Wi-Fi, [`idn_iem::wire`]). The desk mixes nothing for them:
//! each musician's phone mixes the flow by its mix, saved here ([`IemMix`]) and followed
//! over the push channel. A phone's push-to-talk comes back as a desk input (a mix's
//! talkback input).
//!
//! This module holds the saved mixes and the flow's channel list; the network side runs in
//! the engine (`engine::iem_link`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The most IEM mixes a desk keeps (each has its own talkback input).
pub const MAX_MIXES: usize = 16;
/// The talkback inputs after the routed desk inputs: one a mix ([`IemMix::talkback`]).
pub const TALKBACK_INPUTS: usize = MAX_MIXES;
/// Levels: dB, −144 (off) to +10.
pub const LEVEL_MIN: f32 = -144.0;
pub const LEVEL_MAX: f32 = 10.0;
/// The longest mix name.
pub const MAX_NAME: usize = 64;

/// One channel in a mix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IemSend {
    /// dB, −144 (off) to +10.
    pub level_db: f32,
    /// −1 (left) … +1 (right); a linked pair's balance.
    pub pan: f32,
    pub mute: bool,
    /// Soloed in this mix: while any channel of the mix is, only the soloed are heard.
    pub solo: bool,
}

impl Default for IemSend {
    fn default() -> Self {
        IemSend {
            level_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
        }
    }
}

/// A musician's mix of the IEM channels (§M.26): the phone mixes the flow by it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IemMix {
    /// Stable (a token's `iem:<id>` names it): letters, digits, `-`, `_`, `.`; up to 64.
    pub id: String,
    pub name: String,
    /// dB, −144 (off) to +10.
    pub master_db: f32,
    /// By channel id (the desk channel's number, from 1, as text): the channels in the
    /// mix. A linked pair is its first channel's entry (its `pan` the balance); a
    /// channel not listed is not in the mix.
    pub channels: BTreeMap<String, IemSend>,
    /// Its talkback input, 1 to [`TALKBACK_INPUTS`]: the desk input after the routed ones
    /// that the phone's push-to-talk arrives on.
    pub talkback: usize,
}

impl Default for IemMix {
    fn default() -> Self {
        IemMix {
            id: String::new(),
            name: String::new(),
            master_db: 0.0,
            channels: BTreeMap::new(),
            talkback: 0,
        }
    }
}

/// Whether `id` can name a mix (and so a token's scope).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A new mix's id from its name: its ASCII letters and digits in lower case, runs of
/// anything else one `-` ("Lead Vocal" → `lead-vocal`), `mix` when none; `-2`, `-3`… when
/// taken.
pub fn new_id(name: &str, taken: &[IemMix]) -> String {
    let mut base = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    let mut base = base.trim_end_matches('-').to_owned();
    base.truncate(56);
    if base.is_empty() {
        base = "mix".into();
    }
    let free = |id: &str| !taken.iter().any(|m| m.id == id);
    if free(&base) {
        return base;
    }
    (2..)
        .map(|k| format!("{base}-{k}"))
        .find(|id| free(id))
        .expect("a free id")
}

fn level(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(LEVEL_MIN, LEVEL_MAX)
    } else {
        LEVEL_MIN
    }
}

fn pan(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// The mixes as a desk of `n` channels keeps them: at most [`MAX_MIXES`], ids valid and
/// unique, each talkback input its own, values in range, channel ids 1 to `n`.
pub fn normalise(mixes: &mut Vec<IemMix>, n: usize) {
    let mut seen = std::collections::BTreeSet::new();
    mixes.retain(|m| valid_id(&m.id) && seen.insert(m.id.clone()));
    mixes.truncate(MAX_MIXES);
    let mut slots = [false; TALKBACK_INPUTS + 1];
    for m in mixes.iter_mut() {
        m.master_db = level(m.master_db);
        if m.name.trim().is_empty() {
            m.name = m.id.clone();
        }
        cut(&mut m.name, MAX_NAME);
        m.channels.retain(|k, _| {
            k.parse::<usize>()
                .is_ok_and(|c| (1..=n).contains(&c) && *k == c.to_string())
        });
        for e in m.channels.values_mut() {
            e.level_db = level(e.level_db);
            e.pan = pan(e.pan);
        }
        if (1..=TALKBACK_INPUTS).contains(&m.talkback) && !slots[m.talkback] {
            slots[m.talkback] = true;
        } else {
            m.talkback = 0;
        }
    }
    // A mix without one (new, or two had the same): the first free.
    for m in mixes.iter_mut().filter(|m| m.talkback == 0) {
        if let Some(k) = (1..=TALKBACK_INPUTS).find(|&k| !slots[k]) {
            slots[k] = true;
            m.talkback = k;
        }
    }
}

fn cut(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Changes `mix` by a partial update (`iem_mix`'s body without `id`): `name`,
/// `master_db`, and `channels` `{"<id>": {level_db?, pan?, mute?, solo?} | null}` (each
/// entry's keys given change, a new entry starts at 0 dB centre; null takes the channel
/// out of the mix). Anything else is refused. `n`: the desk's channels.
pub fn patch(mix: &mut IemMix, v: &Value, n: usize) -> Result<(), String> {
    let o = v.as_object().ok_or("iem_mix: an object")?;
    for (k, val) in o {
        match k.as_str() {
            "id" => {}
            "name" => {
                let name = val.as_str().ok_or("name: text")?.trim();
                if name.is_empty() {
                    return Err("name: not empty".into());
                }
                mix.name = name.to_owned();
                cut(&mut mix.name, MAX_NAME);
            }
            "master_db" => {
                mix.master_db = level(val.as_f64().ok_or("master_db: dB")? as f32);
            }
            "channels" => {
                let ch = val
                    .as_object()
                    .ok_or("channels: {\"<channel id>\": {level_db, pan, mute, solo} or null}")?;
                if ch.len() > n.max(1) {
                    return Err(format!("channels: at most {n}"));
                }
                for (id, e) in ch {
                    let c = id
                        .parse::<usize>()
                        .ok()
                        .filter(|c| (1..=n).contains(c) && *id == c.to_string())
                        .ok_or(format!("channels: {id:?} is not a channel id (1 to {n})"))?;
                    let key = c.to_string();
                    if e.is_null() {
                        mix.channels.remove(&key);
                        continue;
                    }
                    let e = e.as_object().ok_or(format!("channels {key}: an object"))?;
                    let entry = mix.channels.entry(key.clone()).or_default();
                    for (f, x) in e {
                        match f.as_str() {
                            "level_db" => {
                                entry.level_db =
                                    level(x.as_f64().ok_or(format!("{key} level_db: dB"))? as f32)
                            }
                            "pan" => {
                                entry.pan =
                                    pan(x.as_f64().ok_or(format!("{key} pan: -1 to 1"))? as f32)
                            }
                            "mute" => {
                                entry.mute =
                                    x.as_bool().ok_or(format!("{key} mute: true or false"))?
                            }
                            "solo" => {
                                entry.solo =
                                    x.as_bool().ok_or(format!("{key} solo: true or false"))?
                            }
                            other => return Err(format!("channels {key}: no key {other:?}")),
                        }
                    }
                }
            }
            other => return Err(format!("iem_mix: no key {other:?}")),
        }
    }
    Ok(())
}

/// A channel of the flow, as `GET /api/v1/iem` and the state list it.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowChannel {
    /// The desk channel's number, from 1: what mixes and packets name it by.
    pub id: u16,
    pub name: String,
    pub color: String,
    /// A linked pair's other side, and this one's side (`L` or `R`).
    pub pair: Option<u16>,
    pub side: Option<&'static str>,
}

impl FlowChannel {
    pub fn json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "color": self.color,
               "pair": self.pair, "side": self.side})
    }

    /// Its label on the IDN output ("Kick (ch 3)").
    pub fn label(&self) -> String {
        format!("{} (ch {})", self.name, self.id)
    }
}

/// The channels marked on desk `s` (from 0), in order: a linked pair's two sides; then
/// the talkback microphone's when it goes to the phones (`talkback.to_iem`, §M.28) and
/// isn't marked itself.
pub fn marked(s: &crate::Settings) -> Vec<usize> {
    let mut l: Vec<usize> = (0..s.channels.len())
        .filter(|&i| s.channels[s.lead_of(i)].iem)
        .collect();
    if let Some(tb) = talkback_lane(s)
        && !l.contains(&tb)
    {
        l.push(tb);
    }
    l
}

/// The talkback microphone's channel (from 0) when it goes into the IEM flow
/// (`talkback.to_iem`) and isn't marked itself: heard there only while talkback is on.
pub fn talkback_lane(s: &crate::Settings) -> Option<usize> {
    let t = &s.talkback;
    let i = t.channel.checked_sub(1)?;
    (t.to_iem && i < s.channels.len() && !s.channels[s.lead_of(i)].iem).then_some(i)
}

/// The flow's channels on desk `s`, in order ([`marked`]).
pub fn flow_channels(s: &crate::Settings) -> Vec<FlowChannel> {
    describe(s, &marked(s))
}

/// Channels `ids` (from 0) of desk `s` as the flow lists them.
pub fn describe(s: &crate::Settings, ids: &[usize]) -> Vec<FlowChannel> {
    let n = s.channels.len();
    ids.iter()
        .filter(|&&i| i < n)
        .map(|&i| {
            let lead = s.lead_of(i);
            let paired = s.channels[lead].link && lead + 1 < n;
            let c = &s.channels[i];
            FlowChannel {
                id: (i + 1) as u16,
                name: if c.name.trim().is_empty() {
                    format!("Ch {}", i + 1)
                } else {
                    c.name.clone()
                },
                color: c.color.clone(),
                pair: paired.then(|| (if i == lead { i + 2 } else { lead + 1 }) as u16),
                side: paired.then_some(if i == lead { "L" } else { "R" }),
            }
        })
        .collect()
}

/// How the desk serves the IEM flow on the network.
#[derive(Debug, Clone)]
pub struct IemConfig {
    /// The UDP port phones subscribe and talk on, and unicast audio comes from (default
    /// 29224, the network desk's 29225); taken, another free one (said in the log and in
    /// `GET /api/v1/iem`). 0: any free one. `None`: no Opus flow at all (the IDN output
    /// for wired listeners stays).
    pub port: Option<u16>,
    /// The Opus copy also as multicast (UDP 29253 on a group of its own), sent while a
    /// phone says it listens there. Default on.
    pub multicast: bool,
    /// The most phones taking the unicast copy at once (default 32).
    pub max_phones: usize,
    /// A session not renewed for this long ends (default 5 s).
    pub timeout: std::time::Duration,
    /// Requests from this machine need no token (default true; tests turn it off to try
    /// the network's rules).
    pub trust_loopback: bool,
    /// The local IPv4 address (its interface) the multicast copy leaves by (idn-iem.md
    /// §I.5; `--iem-interface`). `None`: the Node's chosen interface, else the one
    /// towards the Site Manager, else the default route's ([`multicast_interface`]), so a
    /// machine on a LAN and an overlay (wt0) sends to the phones on the LAN.
    pub interface: Option<std::net::Ipv4Addr>,
}

impl Default for IemConfig {
    fn default() -> Self {
        IemConfig {
            port: Some(idn_common::ports::IEM),
            multicast: true,
            max_phones: 32,
            timeout: std::time::Duration::from_millis(u64::from(idn_iem::TIMEOUT_MS)),
            trust_loopback: true,
            interface: None,
        }
    }
}

/// The local address a packet to `to` leaves from (the system's route; nothing is sent).
pub fn route_from(to: std::net::SocketAddr) -> Option<std::net::Ipv4Addr> {
    let u = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    u.connect(to).ok()?;
    match u.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(a) if !a.is_unspecified() => Some(a),
        _ => None,
    }
}

/// The interface the IEM multicast copy leaves by: `asked` (the config's); else the
/// Node's chosen interface (`node_interface`, its status's `interface`); else the one
/// towards the Site Manager (`server`, `HOST[:PORT]`); else the default route's. `None`:
/// none found (the system's choice).
pub fn multicast_interface(
    asked: Option<std::net::Ipv4Addr>,
    node_interface: Option<&str>,
    server: Option<&str>,
) -> Option<std::net::Ipv4Addr> {
    use std::net::ToSocketAddrs as _;
    if let Some(a) = asked.filter(|a| !a.is_unspecified()) {
        return Some(a);
    }
    if let Some(a) = node_interface
        .and_then(|i| i.trim().parse::<std::net::Ipv4Addr>().ok())
        .filter(|a| !a.is_unspecified())
    {
        return Some(a);
    }
    let towards_site = server
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            let with_port = if s.contains(':') {
                s.to_owned()
            } else {
                format!("{s}:{}", idn_common::ports::SITE_MANAGER)
            };
            with_port.to_socket_addrs().ok()?.find(|a| a.is_ipv4())
        })
        .and_then(route_from)
        .filter(|a| !a.is_loopback());
    // (A documentation address: only the route is asked, nothing goes there.)
    towards_site.or_else(|| route_from(([192, 0, 2, 1], 9).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The multicast interface: asked for, else the Node's, else towards the site, else the
    /// default route's; never 0.0.0.0.
    #[test]
    fn the_multicast_interface_is_the_lans() {
        use std::net::Ipv4Addr;
        let asked = Ipv4Addr::new(192, 168, 1, 20);
        assert_eq!(
            multicast_interface(Some(asked), Some("10.0.0.5"), None),
            Some(asked)
        );
        assert_eq!(
            multicast_interface(None, Some(" 10.0.0.5 "), Some("127.0.0.1:29202")),
            Some(Ipv4Addr::new(10, 0, 0, 5))
        );
        assert_eq!(
            multicast_interface(Some(Ipv4Addr::UNSPECIFIED), Some("0.0.0.0"), None),
            route_from(([192, 0, 2, 1], 9).into())
        );
        // A Site Manager on this machine: not the loopback, the default route's.
        assert_eq!(
            multicast_interface(None, Some(""), Some("127.0.0.1")),
            route_from(([192, 0, 2, 1], 9).into())
        );
        assert_eq!(
            route_from(([127, 0, 0, 1], 9).into()),
            Some(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn ids_come_from_names() {
        let mut taken = Vec::new();
        for (name, want) in [
            ("Lead Vocal", "lead-vocal"),
            ("Lead vocal!", "lead-vocal-2"),
            ("", "mix"),
            ("Ünï", "n"),
        ] {
            let id = new_id(name, &taken);
            assert_eq!(id, want);
            assert!(valid_id(&id));
            taken.push(IemMix {
                id,
                ..Default::default()
            });
        }
    }

    #[test]
    fn mixes_are_normalised() {
        let m = |id: &str, tb: usize| IemMix {
            id: id.into(),
            talkback: tb,
            master_db: 99.0,
            channels: [
                ("3".to_owned(), IemSend::default()),
                ("03".to_owned(), IemSend::default()),
                ("40".to_owned(), IemSend::default()),
            ]
            .into(),
            ..Default::default()
        };
        let mut list = vec![m("a", 2), m("a", 3), m("bad id", 1), m("b", 2), m("c", 0)];
        normalise(&mut list, 32);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        let slots: Vec<usize> = list.iter().map(|m| m.talkback).collect();
        assert_eq!(slots, [2, 1, 3]);
        assert_eq!(list[0].master_db, LEVEL_MAX);
        assert_eq!(list[0].channels.keys().collect::<Vec<_>>(), ["3"]);
        assert_eq!(list[0].name, "a");
    }

    #[test]
    fn a_mix_takes_partial_updates() {
        let mut m = IemMix {
            id: "drums".into(),
            ..Default::default()
        };
        patch(
            &mut m,
            &json!({"master_db": -3.0, "channels": {"2": {"level_db": -6.0}, "5": {"pan": 0.5, "mute": true}}}),
            8,
        )
        .unwrap();
        patch(
            &mut m,
            &json!({"channels": {"2": {"pan": -1.0}, "5": null}}),
            8,
        )
        .unwrap();
        assert_eq!(m.master_db, -3.0);
        assert_eq!(
            m.channels["2"],
            IemSend {
                level_db: -6.0,
                pan: -1.0,
                ..Default::default()
            }
        );
        assert!(!m.channels.contains_key("5"));
        for bad in [
            json!({"channels": {"9": {}}}),
            json!({"channels": {"x": {}}}),
            json!({"channels": {"2": {"gain": 1}}}),
            json!({"fader": 0}),
            json!({"name": ""}),
            json!({"master_db": "loud"}),
        ] {
            assert!(patch(&mut m.clone(), &bad, 8).is_err(), "{bad}");
        }
    }
}
