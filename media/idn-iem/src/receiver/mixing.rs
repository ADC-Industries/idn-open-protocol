//! The mix (idn-iem.md §I.8.3): the flow's channels, the musician's levels, pans, mutes,
//! solos and master, and the arithmetic that makes stereo of them, as the desk's own
//! (an equal-power pan, −3 dB each side at the centre; a linked pair's sides hard left
//! and right with `pan` their balance).

use super::jitter::{Frame, Pop};
use crate::codec::Decoder;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Levels: dB, −144 (off) to +10, as the desk keeps them.
pub const LEVEL_MIN: f32 = -144.0;
pub const LEVEL_MAX: f32 = 10.0;

/// A linked pair's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    L,
    R,
}

/// A channel of the flow (`GET /api/v1/iem`'s `channels`).
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// The desk channel's number (from 1): what mixes and packets name it by.
    pub id: u16,
    pub name: String,
    /// `#rrggbb` or empty.
    pub color: String,
    /// A linked pair's other side, and this one's side.
    pub pair: Option<u16>,
    pub side: Option<Side>,
}

impl Channel {
    pub fn from_json(v: &Value) -> Option<Channel> {
        Some(Channel {
            id: u16::try_from(v["id"].as_u64()?).ok()?,
            name: v["name"].as_str().unwrap_or_default().to_owned(),
            color: v["color"].as_str().unwrap_or_default().to_owned(),
            pair: v["pair"].as_u64().and_then(|p| u16::try_from(p).ok()),
            side: match v["side"].as_str() {
                Some("L") => Some(Side::L),
                Some("R") => Some(Side::R),
                _ => None,
            },
        })
    }

    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "color": self.color, "pair": self.pair,
               "side": self.side.map(|s| if s == Side::L { "L" } else { "R" })})
    }

    /// The channel whose mix entry it uses: a pair's left side.
    pub fn lead(&self) -> u16 {
        match (self.side, self.pair) {
            (Some(Side::R), Some(p)) => p,
            _ => self.id,
        }
    }
}

/// The flow's channels from a JSON list.
pub(crate) fn channels_of(v: &Value) -> Vec<Channel> {
    v.as_array()
        .map(|l| l.iter().filter_map(Channel::from_json).collect())
        .unwrap_or_default()
}

/// One channel in a mix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelMix {
    /// dB, −144 (off) to +10.
    pub level_db: f32,
    /// −1 (left) … +1 (right); a linked pair's balance.
    pub pan: f32,
    pub mute: bool,
    /// While any channel of the mix is soloed, only the soloed are heard.
    pub solo: bool,
}

impl Default for ChannelMix {
    fn default() -> Self {
        ChannelMix {
            level_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
        }
    }
}

/// A musician's mix, as saved on the desk (idn-mixer.md §M.26).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mix {
    pub id: String,
    pub name: String,
    pub master_db: f32,
    /// By channel id: the channels in the mix (a pair by its left side's id); a channel
    /// not listed is not heard.
    pub channels: BTreeMap<u16, ChannelMix>,
    /// The desk input the musician's push-to-talk arrives on.
    pub talkback_input: Option<u32>,
    /// A phone talks into this mix now.
    pub talking: bool,
}

impl Mix {
    pub fn from_json(v: &Value) -> Option<Mix> {
        let mut channels = BTreeMap::new();
        for (k, e) in v["channels"].as_object()? {
            let Ok(id) = k.parse::<u16>() else { continue };
            let d = ChannelMix::default();
            channels.insert(
                id,
                ChannelMix {
                    level_db: e["level_db"].as_f64().map_or(d.level_db, |x| x as f32),
                    pan: e["pan"].as_f64().map_or(d.pan, |x| x as f32),
                    mute: e["mute"].as_bool().unwrap_or(false),
                    solo: e["solo"].as_bool().unwrap_or(false),
                },
            );
        }
        Some(Mix {
            id: v["id"].as_str()?.to_owned(),
            name: v["name"].as_str().unwrap_or_default().to_owned(),
            master_db: v["master_db"].as_f64().unwrap_or(0.0) as f32,
            channels,
            talkback_input: v["talkback_input"].as_u64().map(|x| x as u32),
            talking: v["talking"].as_bool().unwrap_or(false),
        })
    }

    pub fn to_json(&self) -> Value {
        let ch: serde_json::Map<String, Value> = self
            .channels
            .iter()
            .map(|(id, e)| {
                (
                    id.to_string(),
                    json!({"level_db": e.level_db, "pan": e.pan, "mute": e.mute, "solo": e.solo}),
                )
            })
            .collect();
        json!({"id": self.id, "name": self.name, "master_db": self.master_db, "channels": ch,
               "talkback_input": self.talkback_input, "talking": self.talking})
    }
}

/// dB as a gain (−144 and below: off).
pub(crate) fn gain(db: f32) -> f32 {
    if !db.is_finite() || db <= LEVEL_MIN {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// The equal-power pan law: (left, right), −3 dB each at the centre.
pub(crate) fn pan_law(p: f32) -> (f32, f32) {
    let p = if p.is_finite() {
        p.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let a = (p + 1.0) * std::f32::consts::FRAC_PI_4;
    (a.cos(), a.sin())
}

/// The mix as gains: each channel to decode, its left and right gain.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Compiled {
    pub chans: Vec<(u16, f32, f32)>,
}

/// `mix` over `channels` (in the flow's order). Every channel in the mix is decoded, a
/// muted one too (unmuted, it comes back without a click).
pub(crate) fn compile(mix: Option<&Mix>, channels: &[Channel]) -> Compiled {
    let Some(mix) = mix else {
        return Compiled::default();
    };
    let any_solo = mix.channels.values().any(|e| e.solo);
    let master = gain(mix.master_db);
    let chans = channels
        .iter()
        .filter_map(|c| {
            let e = mix.channels.get(&c.lead())?;
            let mut g = gain(e.level_db) * master;
            if e.mute || (any_solo && !e.solo) {
                g = 0.0;
            }
            let p = if e.pan.is_finite() {
                e.pan.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let (l, r) = match (c.side, c.pair) {
                (Some(Side::L), Some(_)) => ((1.0 - p).min(1.0), 0.0),
                (Some(Side::R), Some(_)) => (0.0, (1.0 + p).min(1.0)),
                _ => pan_law(p),
            };
            Some((c.id, g * l, g * r))
        })
        .collect();
    Compiled { chans }
}

/// The render side: each channel's decoder, the gains as last played (each frame glides
/// from them to the mix's: no zipper noise), the frame being played out.
pub(crate) struct Engine {
    decoders: HashMap<u16, Decoder>,
    now: HashMap<u16, (f32, f32)>,
    pub compiled: Arc<Compiled>,
    pub version: u64,
    /// The frame being played (stereo, interleaved) and how far.
    out: Vec<f32>,
    pos: usize,
    spare: Vec<f32>,
    /// The last frame played: its desk time, flags and when.
    pub last: Option<(u64, u8, std::time::Instant)>,
    /// Samples of the last frame (for concealing at the same length).
    samples: usize,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            decoders: HashMap::new(),
            now: HashMap::new(),
            compiled: Arc::default(),
            version: u64::MAX,
            out: Vec::new(),
            pos: 0,
            spare: Vec::new(),
            last: None,
            samples: crate::FRAME_SAMPLES,
        }
    }

    /// Fills `out` (stereo, interleaved, 48 kHz), taking frames from `next` as needed.
    pub fn render(&mut self, out: &mut [f32], mut next: impl FnMut(&mut Engine) -> Pop) {
        let mut i = 0;
        while i < out.len() {
            if self.pos >= self.out.len() {
                let pop = next(self);
                self.play(pop);
                self.pos = 0;
            }
            let n = (out.len() - i).min(self.out.len() - self.pos);
            out[i..i + n].copy_from_slice(&self.out[self.pos..self.pos + n]);
            self.pos += n;
            i += n;
        }
    }

    /// The next frame into `self.out`.
    fn play(&mut self, pop: Pop) {
        // Decoders only for the channels in the mix.
        // (Looked up in place: nothing allocated on the audio thread.)
        let chans = &self.compiled.chans;
        if self.decoders.len() > chans.len() {
            self.decoders
                .retain(|id, _| chans.iter().any(|c| c.0 == *id));
        }
        if self.now.len() > chans.len() {
            self.now.retain(|id, _| chans.iter().any(|c| c.0 == *id));
        }
        match pop {
            Pop::Silence => {
                self.out.clear();
                self.out.resize(self.samples * 2, 0.0);
                // (Gains start from nothing again: the next sound fades in.)
                self.now.clear();
            }
            Pop::Play(f) => {
                self.samples = f.samples;
                self.last = Some((f.time, f.flags, std::time::Instant::now()));
                let mut out = std::mem::take(&mut self.out);
                self.mix(Some(&f), &mut out, true);
                self.out = out;
            }
            Pop::Lost | Pop::Stall => {
                let mut out = std::mem::take(&mut self.out);
                self.mix(None, &mut out, true);
                self.out = out;
            }
            Pop::Splice(a, b) => {
                self.samples = b.samples;
                self.last = Some((b.time, b.flags, std::time::Instant::now()));
                let (mut out, mut spare) = (
                    std::mem::take(&mut self.out),
                    std::mem::take(&mut self.spare),
                );
                self.mix(Some(&a), &mut out, false);
                self.mix(Some(&b), &mut spare, true);
                let n = out.len().min(spare.len());
                let frames = (n / 2).max(1);
                for k in 0..n {
                    let w = (k / 2) as f32 / frames as f32;
                    out[k] = out[k] * (1.0 - w) + spare[k] * w;
                }
                out.truncate(n);
                self.out = out;
                self.spare = spare;
            }
        }
    }

    /// One frame of every channel in the mix (its entry decoded, or concealed when it
    /// isn't there), mixed into `out`; the gains glide over the frame to the mix's
    /// (and are kept as reached when `keep`).
    fn mix(&mut self, f: Option<&Frame>, out: &mut Vec<f32>, keep: bool) {
        let samples = f.map_or(self.samples, |f| f.samples);
        out.clear();
        out.resize(samples * 2, 0.0);
        let compiled = self.compiled.clone();
        for &(id, tl, tr) in &compiled.chans {
            let dec = match self.decoders.entry(id) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => match Decoder::new() {
                    Ok(d) => v.insert(d),
                    Err(_) => continue,
                },
            };
            let data = f.and_then(|f| f.entries.iter().find(|e| e.0 == id).map(|e| &e.1[..]));
            let Ok(pcm) = dec.decode(data, samples) else {
                continue;
            };
            let n = pcm.len().min(samples);
            if n == 0 {
                continue;
            }
            let (l0, r0) = self.now.get(&id).copied().unwrap_or((0.0, 0.0));
            let (dl, dr) = ((tl - l0) / n as f32, (tr - r0) / n as f32);
            for (k, &x) in pcm[..n].iter().enumerate() {
                let x = if x.is_finite() { x } else { 0.0 };
                let s = (k + 1) as f32;
                out[2 * k] += x * (l0 + dl * s);
                out[2 * k + 1] += x * (r0 + dr * s);
            }
            if keep {
                self.now.insert(id, (tl, tr));
            }
        }
        for v in out.iter_mut() {
            *v = v.clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: u16, pair: Option<u16>, side: Option<Side>) -> Channel {
        Channel {
            id,
            name: format!("Ch {id}"),
            color: String::new(),
            pair,
            side,
        }
    }

    #[test]
    fn gains_as_the_desk_makes_them() {
        let chans = [
            ch(1, None, None),
            ch(2, None, None),
            ch(3, Some(4), Some(Side::L)),
            ch(4, Some(3), Some(Side::R)),
            ch(5, None, None),
        ];
        let mut m = Mix {
            id: "a".into(),
            master_db: -6.0,
            ..Default::default()
        };
        m.channels.insert(
            1,
            ChannelMix {
                pan: -1.0,
                ..Default::default()
            },
        );
        m.channels.insert(
            2,
            ChannelMix {
                level_db: -144.0,
                ..Default::default()
            },
        );
        m.channels.insert(
            3,
            ChannelMix {
                pan: 0.5,
                ..Default::default()
            },
        );
        let c = compile(Some(&m), &chans);
        let half = gain(-6.0);
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert_eq!(c.chans.len(), 4, "channel 5 isn't in the mix: {c:?}");
        assert!(near(c.chans[0].1, half) && near(c.chans[0].2, 0.0));
        assert!(near(c.chans[1].1, 0.0), "−144 is off");
        // The pair: balance 0.5 takes half of the left side, the right side stays whole.
        assert!(near(c.chans[2].1, half * 0.5) && near(c.chans[2].2, 0.0));
        assert!(near(c.chans[3].1, 0.0) && near(c.chans[3].2, half));
        // A centre pan is −3 dB each side.
        let (l, r) = pan_law(0.0);
        assert!(near(l, std::f32::consts::FRAC_1_SQRT_2) && near(r, l));
        // Solo: only the soloed; a muted soloed one stays muted.
        m.channels.get_mut(&1).unwrap().solo = true;
        let c = compile(Some(&m), &chans);
        assert!(c.chans[0].1 > 0.0);
        assert!(c.chans[2..].iter().all(|x| x.1 == 0.0 && x.2 == 0.0));
        m.channels.get_mut(&1).unwrap().mute = true;
        assert_eq!(compile(Some(&m), &chans).chans[0].1, 0.0);
        assert!(compile(None, &chans).chans.is_empty());
    }

    #[test]
    fn mixes_go_to_and_from_json() {
        let v = json!({"id": "drums", "name": "Drums", "master_db": -2.0,
            "channels": {"2": {"level_db": -6.0, "pan": 0.25, "mute": true, "solo": false}},
            "talkback": 1, "talkback_input": 65, "talking": false});
        let m = Mix::from_json(&v).unwrap();
        assert_eq!(m.channels[&2].level_db, -6.0);
        assert!(m.channels[&2].mute);
        assert_eq!(m.talkback_input, Some(65));
        assert_eq!(Mix::from_json(&m.to_json()).unwrap(), m);
        let c = Channel::from_json(
            &json!({"id": 3, "name": "Keys L", "color": "", "pair": 4, "side": "L"}),
        )
        .unwrap();
        assert_eq!((c.lead(), c.side), (3, Some(Side::L)));
        assert_eq!(Channel::from_json(&c.to_json()).unwrap(), c);
    }
}
