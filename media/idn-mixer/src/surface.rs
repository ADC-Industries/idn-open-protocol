//! MIDI control surfaces speaking Mackie Control (spec/extensions/idn-mixer.md §M.6):
//! eight channel faders, a master fader, mute, solo and select buttons, V-pots for pan,
//! bank and channel buttons, the scribble strips and the meters. Motorised faders
//! follow the desk (not while touched).
//!
//! [`Mcu`] is the protocol, with no I/O (tested on its own); [`Surface`] connects it to
//! MIDI ports through `midir` and to a [`Console`].

use crate::desk::Settings;
use crate::engine::Console;
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Strips on a surface.
pub const STRIPS: usize = 8;

// Note numbers (Mackie Control's).
const SOLO: u8 = 0x08;
const MUTE: u8 = 0x10;
const SELECT: u8 = 0x18;
const BANK_LEFT: u8 = 0x2E;
const BANK_RIGHT: u8 = 0x2F;
const CHANNEL_LEFT: u8 = 0x30;
const CHANNEL_RIGHT: u8 = 0x31;
const TOUCH: u8 = 0x68;
/// The master fader's touch sensor.
const TOUCH_MASTER: u8 = 0x70;

/// The panel's fader law (0…1 of travel ↔ dB): −∞ at the bottom, −60…−30 dB over the
/// lowest quarter, −30…+10 dB over the rest.
pub fn travel_to_db(t: f32) -> f32 {
    let p = t * 1000.0;
    if p <= 0.5 {
        -144.0
    } else if p >= 1000.0 {
        10.0
    } else if p < 250.0 {
        -60.0 + p / 250.0 * 30.0
    } else {
        -30.0 + (p - 250.0) / 750.0 * 40.0
    }
}

pub fn db_to_travel(d: f32) -> f32 {
    let p = if d <= -144.0 {
        0.0
    } else if d < -30.0 {
        ((d + 60.0) / 30.0 * 250.0).max(1.0)
    } else {
        (250.0 + (d + 30.0) / 40.0 * 750.0).min(1000.0)
    };
    p / 1000.0
}

/// A change the surface asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Channel (from 1) by a merge patch.
    Channel(usize, Value),
    /// The main bus by a merge patch.
    Main(Value),
    /// The surface shows other channels (nothing on the desk changes).
    Bank,
    /// A message to send back to the surface (its connection handshake).
    Reply(Vec<u8>),
}

/// What was last sent for a strip (so only changes go out).
#[derive(Debug, Clone, Default, PartialEq)]
struct Shown {
    fader: Option<u16>,
    mute: Option<bool>,
    solo: Option<bool>,
    select: Option<bool>,
    pan: Option<u8>,
    meter: Option<u8>,
}

/// The Mackie Control protocol state: which channels are on the strips, faders being
/// touched, what the surface shows.
#[derive(Debug, Clone)]
pub struct Mcu {
    /// The desk channel (from 0) on the first strip.
    pub offset: usize,
    /// The channel selected on the surface (from 0).
    pub selected: Option<usize>,
    touched: [bool; STRIPS + 1],
    shown: [Shown; STRIPS + 1],
    text: Vec<u8>,
    /// The surface's model byte in SysEx: 0x14 Mackie Control, 0x15 an extender.
    pub model: u8,
}

impl Default for Mcu {
    fn default() -> Self {
        Mcu {
            offset: 0,
            selected: None,
            touched: [false; STRIPS + 1],
            shown: Default::default(),
            text: Vec::new(),
            model: 0x14,
        }
    }
}

fn pan_ring(pan: f32) -> u8 {
    // Mode 0 (a single dot), positions 1–11, 6 the centre.
    (6.0 + pan.clamp(-1.0, 1.0) * 5.0).round() as u8
}

impl Mcu {
    /// Forgets what the surface shows, so that everything is sent again (a surface
    /// connected or restarted mid-service).
    pub fn refresh(&mut self) {
        self.shown = Default::default();
        self.text.clear();
    }

    /// One incoming MIDI message: what to change, if anything. `n` is the desk's channel
    /// count, `s` its settings now.
    pub fn incoming(&mut self, msg: &[u8], s: &Settings) -> Option<Action> {
        let n = s.channels.len();
        let (&status, data) = msg.split_first()?;
        let ch = (status & 0x0F) as usize;
        // The surface's connection query (serial number, a challenge): the reply the
        // Logic Control documentation gives, then everything shown afresh.
        if status == 0xF0 {
            if msg.len() >= 18 && msg[1..4] == [0x00, 0x00, 0x66] && msg[5] == 0x01 {
                let (serial, l) = (&msg[6..13], &msg[13..17]);
                let (l1, l2, l3, l4) = (l[0] as i32, l[1] as i32, l[2] as i32, l[3] as i32);
                let r = [
                    0x7F & (l1 + (l2 ^ 0xA) - l4),
                    0x7F & ((l3 >> 4) ^ (l1 + l4)),
                    0x7F & ((l4 - (l3 << 2)) ^ (l1 | l2)),
                    0x7F & (l2 - l3 + (0xF0 ^ (l4 << 4))),
                ];
                let mut reply = vec![0xF0, 0x00, 0x00, 0x66, msg[4], 0x02];
                reply.extend_from_slice(serial);
                reply.extend(r.iter().map(|&b| b as u8));
                reply.push(0xF7);
                self.model = msg[4];
                self.refresh();
                return Some(Action::Reply(reply));
            }
            return None;
        }
        match status & 0xF0 {
            // A fader: 14 bits on its MIDI channel (strips on 0–7, the master on 8).
            0xE0 if data.len() >= 2 => {
                let v = (data[0] as u16 & 0x7F) | ((data[1] as u16 & 0x7F) << 7);
                let db = (travel_to_db(v as f32 / 16383.0) * 2.0).round() / 2.0;
                // The fader is where it was put: not moved back by the reply.
                if ch < STRIPS {
                    self.shown[ch].fader = Some(v);
                    let c = self.offset + ch;
                    (c < n).then(|| Action::Channel(c + 1, json!({"fader": db})))
                } else if ch == STRIPS {
                    self.shown[STRIPS].fader = Some(v);
                    Some(Action::Main(json!({"fader": db})))
                } else {
                    None
                }
            }
            // Buttons: pressed (velocity > 0) acts; released does nothing but a touch.
            0x90 if data.len() >= 2 => {
                let (note, down) = (data[0], data[1] > 0);
                if (TOUCH..TOUCH + STRIPS as u8).contains(&note) || note == TOUCH_MASTER {
                    self.touched[(note - TOUCH) as usize] = down;
                    return None;
                }
                if !down {
                    return None;
                }
                let strip = |base: u8| {
                    (base..base + STRIPS as u8)
                        .contains(&note)
                        .then(|| (note - base) as usize)
                };
                let chan = |k: usize| Some(self.offset + k).filter(|&c| c < n);
                if let Some(c) = strip(MUTE).and_then(chan) {
                    return Some(Action::Channel(c + 1, json!({"mute": !s.channels[c].mute})));
                }
                if let Some(c) = strip(SOLO).and_then(chan) {
                    return Some(Action::Channel(c + 1, json!({"solo": !s.channels[c].solo})));
                }
                if let Some(c) = strip(SELECT).and_then(chan) {
                    self.selected = if self.selected == Some(c) {
                        None
                    } else {
                        Some(c)
                    };
                    return Some(Action::Bank);
                }
                let last = n.saturating_sub(STRIPS);
                let offset = match note {
                    BANK_LEFT => self.offset.saturating_sub(STRIPS),
                    BANK_RIGHT => (self.offset + STRIPS).min(last),
                    CHANNEL_LEFT => self.offset.saturating_sub(1),
                    CHANNEL_RIGHT => (self.offset + 1).min(last),
                    _ => return None,
                };
                if offset == self.offset {
                    return None;
                }
                self.offset = offset;
                // Everything on the strips is shown afresh.
                self.shown = Default::default();
                self.text.clear();
                Some(Action::Bank)
            }
            // V-pots: relative, bit 6 the direction, the rest the speed. They pan.
            0xB0 if data.len() >= 2 && (0x10..0x18).contains(&data[0]) => {
                let k = (data[0] - 0x10) as usize;
                let c = self.offset + k;
                if c >= n {
                    return None;
                }
                let steps = (data[1] & 0x3F) as f64 * if data[1] & 0x40 != 0 { -1.0 } else { 1.0 };
                let pan = (s.channels[c].pan as f64 + steps * 0.02).clamp(-1.0, 1.0);
                Some(Action::Channel(
                    c + 1,
                    json!({"pan": (pan * 100.0).round() / 100.0}),
                ))
            }
            _ => None,
        }
    }

    /// The messages that bring the surface up to date with the desk: faders (not those
    /// being touched), buttons' lights, pan rings, the scribble strips.
    pub fn outgoing(&mut self, s: &Settings) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for k in 0..STRIPS {
            let c = self.offset + k;
            let ch = s.channels.get(c);
            let fader = ch.map_or(0, |c| (db_to_travel(c.fader) * 16383.0).round() as u16);
            if !self.touched[k] && self.shown[k].fader != Some(fader) {
                out.push(vec![
                    0xE0 | k as u8,
                    (fader & 0x7F) as u8,
                    (fader >> 7) as u8,
                ]);
                self.shown[k].fader = Some(fader);
            }
            let led = |on: bool| if on { 0x7F } else { 0x00 };
            let mute = ch.is_some_and(|c| c.mute);
            if self.shown[k].mute != Some(mute) {
                out.push(vec![0x90, MUTE + k as u8, led(mute)]);
                self.shown[k].mute = Some(mute);
            }
            let solo = ch.is_some_and(|c| c.solo);
            if self.shown[k].solo != Some(solo) {
                out.push(vec![0x90, SOLO + k as u8, led(solo)]);
                self.shown[k].solo = Some(solo);
            }
            let select = self.selected == Some(c);
            if self.shown[k].select != Some(select) {
                out.push(vec![0x90, SELECT + k as u8, led(select)]);
                self.shown[k].select = Some(select);
            }
            let pan = ch.map_or(0, |c| pan_ring(c.pan));
            if self.shown[k].pan != Some(pan) {
                out.push(vec![0xB0, 0x30 + k as u8, pan]);
                self.shown[k].pan = Some(pan);
            }
        }
        let master = (db_to_travel(s.main.fader) * 16383.0).round() as u16;
        if !self.touched[STRIPS] && self.shown[STRIPS].fader != Some(master) {
            out.push(vec![0xE8, (master & 0x7F) as u8, (master >> 7) as u8]);
            self.shown[STRIPS].fader = Some(master);
        }
        // The scribble strips: 2 rows of 56 characters, 7 a strip: the name, and the
        // fader's level.
        let cell = |t: &str| {
            let mut v: Vec<u8> = t
                .chars()
                .map(|c| {
                    if c.is_ascii() && !c.is_ascii_control() {
                        c as u8
                    } else {
                        b'?'
                    }
                })
                .take(6)
                .collect();
            v.resize(7, b' ');
            v
        };
        let mut text = Vec::with_capacity(112);
        for k in 0..STRIPS {
            text.extend(cell(
                s.channels
                    .get(self.offset + k)
                    .map_or("", |c| c.name.as_str()),
            ));
        }
        for k in 0..STRIPS {
            let t = match s.channels.get(self.offset + k) {
                Some(c) if c.fader <= -144.0 => "-inf".to_owned(),
                Some(c) => format!("{:+.1}", c.fader),
                None => String::new(),
            };
            text.extend(cell(&t));
        }
        if text != self.text {
            let mut m = vec![0xF0, 0x00, 0x00, 0x66, self.model, 0x12, 0x00];
            m.extend_from_slice(&text);
            m.push(0xF7);
            out.push(m);
            self.text = text;
        }
        out
    }

    /// The meters: each strip's level (peak dBFS), as channel pressure (the surface lets
    /// it fall by itself).
    pub fn meters(&mut self, peaks_db: &[f32]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for k in 0..STRIPS {
            let d = peaks_db.get(self.offset + k).copied().unwrap_or(-144.0);
            let level = ((d + 60.0) / 60.0 * 12.0).clamp(0.0, 12.0) as u8;
            // Sent while there is something to show, and once as it falls to nothing.
            if level > 0 || self.shown[k].meter != Some(0) {
                out.push(vec![0xD0, ((k as u8) << 4) | level]);
                self.shown[k].meter = Some(level);
            }
        }
        out
    }
}

/// MIDI ports by name: (inputs, outputs).
pub fn ports() -> Result<(Vec<String>, Vec<String>), String> {
    let i = midir::MidiInput::new("IDN Mixer").map_err(|e| e.to_string())?;
    let o = midir::MidiOutput::new("IDN Mixer").map_err(|e| e.to_string())?;
    Ok((
        i.ports()
            .iter()
            .filter_map(|p| i.port_name(p).ok())
            .collect(),
        o.ports()
            .iter()
            .filter_map(|p| o.port_name(p).ok())
            .collect(),
    ))
}

/// A surface connected to the desk; disconnected when dropped.
pub struct Surface {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    _input: midir::MidiInputConnection<()>,
    /// Still reaching the surface (it can't be sent to: unplugged, restarted).
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl Surface {
    /// Connects the first MIDI input and output whose names contain `name` (any case) to
    /// `console`.
    /// Whether it still reaches the surface (false once a send failed: open it again).
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn start(console: Arc<Console>, name: &str) -> Result<Surface, String> {
        let want = name.to_lowercase();
        let midi_in = midir::MidiInput::new("IDN Mixer").map_err(|e| e.to_string())?;
        let midi_out = midir::MidiOutput::new("IDN Mixer").map_err(|e| e.to_string())?;
        let in_port = midi_in
            .ports()
            .into_iter()
            .find(|p| {
                midi_in
                    .port_name(p)
                    .is_ok_and(|n| n.to_lowercase().contains(&want))
            })
            .ok_or_else(|| format!("no MIDI input called {name:?}"))?;
        let out_port = midi_out
            .ports()
            .into_iter()
            .find(|p| {
                midi_out
                    .port_name(p)
                    .is_ok_and(|n| n.to_lowercase().contains(&want))
            })
            .ok_or_else(|| format!("no MIDI output called {name:?}"))?;
        let out = Arc::new(Mutex::new(
            midi_out
                .connect(&out_port, "IDN Mixer")
                .map_err(|e| e.to_string())?,
        ));
        let mcu = Arc::new(Mutex::new(Mcu::default()));
        let input = {
            let (console, mcu, out) = (console.clone(), mcu.clone(), out.clone());
            midi_in
                .connect(
                    &in_port,
                    "IDN Mixer",
                    move |_, msg, _| {
                        let s = console.settings();
                        let action = mcu.lock_ok().incoming(msg, &s);
                        // Written in the audit as the surface's (§M.25).
                        let audited = |action: &str, mut detail: Value, r: Result<(), String>| {
                            console.audit().record(
                                &idn_open_protocol::web::Who::surface(),
                                action,
                                &detail.take(),
                                r.as_ref().err().map(String::as_str),
                            );
                            r
                        };
                        let r = match action {
                            Some(Action::Channel(c, p)) => {
                                let r = console.channel(c, &p);
                                let mut d = p;
                                d["n"] = json!(c);
                                audited("channel", d, r)
                            }
                            Some(Action::Main(p)) => {
                                let r = console.bus("main", &p);
                                let mut d = p;
                                d["bus"] = json!("main");
                                audited("bus", d, r)
                            }
                            Some(Action::Reply(m)) => {
                                out.lock_ok().send(&m).map_err(|e| e.to_string())
                            }
                            // Shown within a tenth of a second.
                            Some(Action::Bank) | None => Ok(()),
                        };
                        if let Err(e) = r {
                            idn_open_protocol::log::log(&format!("mixer: control surface: {e}"));
                        }
                    },
                    (),
                )
                .map_err(|e| e.to_string())?
        };
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive2 = alive.clone();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("idn-mixer-surface".into())
                .spawn(move || {
                    let alive = alive2;
                    let mut version = 0;
                    let mut meters_at = Instant::now();
                    // Everything sent again every few seconds: a surface plugged in (or
                    // restarted) mid-service shows the desk without waiting for a change;
                    // only what isn't touched moves.
                    let mut refresh_at = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        // A change on the desk, a bank change, or the meters' turn (10 a
                        // second).
                        version = console.wait_change(version, Duration::from_millis(100));
                        let s = console.settings();
                        if refresh_at.elapsed() >= Duration::from_secs(5) {
                            refresh_at = Instant::now();
                            mcu.lock_ok().refresh();
                        }
                        let mut msgs = mcu.lock_ok().outgoing(&s);
                        if meters_at.elapsed() >= Duration::from_millis(100) {
                            meters_at = Instant::now();
                            let m = console.meters();
                            let peaks: Vec<f32> = m["ch"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|c| c[2].as_f64().unwrap_or(-144.0) as f32)
                                .collect();
                            msgs.extend(mcu.lock_ok().meters(&peaks));
                        }
                        let mut o = out.lock_ok();
                        for m in msgs {
                            if o.send(&m).is_err() {
                                // Gone (unplugged, restarted): said, for opening it again.
                                alive.store(false, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        Ok(Surface {
            alive,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
            _input: input,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fader_law_round_trips() {
        for d in [-50.0f32, -30.0, -10.0, 0.0, 6.0, 10.0] {
            assert!((travel_to_db(db_to_travel(d)) - d).abs() < 0.01, "{d}");
        }
        assert_eq!(travel_to_db(0.0), -144.0);
        assert_eq!(db_to_travel(-144.0), 0.0);
    }

    #[test]
    fn buttons_faders_and_pots_become_changes() {
        let mut s = Settings::new(20);
        let mut m = Mcu::default();
        // Strip 3's fader at 0 dB.
        let v = (db_to_travel(0.0) * 16383.0).round() as u16;
        let a = m.incoming(&[0xE2, (v & 0x7F) as u8, (v >> 7) as u8], &s);
        assert_eq!(a, Some(Action::Channel(3, json!({"fader": 0.0}))));
        // Mute on strip 1 toggles; a release does nothing.
        assert_eq!(
            m.incoming(&[0x90, MUTE, 0x7F], &s),
            Some(Action::Channel(1, json!({"mute": true})))
        );
        assert_eq!(m.incoming(&[0x90, MUTE, 0x00], &s), None);
        // Bank right: strips show 9–16; then 13–20 (no further than the last).
        assert_eq!(
            m.incoming(&[0x90, BANK_RIGHT, 0x7F], &s),
            Some(Action::Bank)
        );
        assert_eq!(m.offset, 8);
        m.incoming(&[0x90, BANK_RIGHT, 0x7F], &s);
        assert_eq!(m.offset, 12);
        assert_eq!(m.incoming(&[0x90, BANK_RIGHT, 0x7F], &s), None);
        // A V-pot turned left 5 steps on strip 1 (channel 13).
        s.channels[12].pan = 0.5;
        assert_eq!(
            m.incoming(&[0xB0, 0x10, 0x45], &s),
            Some(Action::Channel(13, json!({"pan": 0.4})))
        );
        // The master fader.
        assert!(matches!(
            m.incoming(&[0xE8, 0x00, 0x00], &s),
            Some(Action::Main(_))
        ));
    }

    #[test]
    fn the_surface_follows_the_desk_except_a_touched_fader() {
        let mut s = Settings::new(8);
        let mut m = Mcu::default();
        let first = m.outgoing(&s);
        assert!(first.iter().any(|x| x[0] == 0xF0), "the scribble strips");
        assert!(m.outgoing(&s).is_empty(), "nothing changed, nothing sent");
        s.channels[1].mute = true;
        s.channels[1].fader = -10.0;
        let msgs = m.outgoing(&s);
        assert!(msgs.contains(&vec![0x90, MUTE + 1, 0x7F]));
        assert!(msgs.iter().any(|x| x[0] == 0xE1), "the fader moves");
        // Touched: the desk doesn't move it.
        m.incoming(&[0x90, TOUCH + 1, 0x7F], &s);
        s.channels[1].fader = -20.0;
        assert!(!m.outgoing(&s).iter().any(|x| x[0] == 0xE1));
        m.incoming(&[0x90, TOUCH + 1, 0x00], &s);
        assert!(
            m.outgoing(&s).iter().any(|x| x[0] == 0xE1),
            "let go: it follows again"
        );
        // Meters: strip 1 at −6 dBFS is level 10.
        assert!(m.meters(&[-6.0]).contains(&vec![0xD0, 10]));
    }

    #[test]
    fn a_surface_that_connects_gets_everything() {
        let s = Settings::new(8);
        let mut m = Mcu::default();
        let _ = m.outgoing(&s);
        assert!(m.outgoing(&s).is_empty());
        // Its connection query: a reply with its serial number, and a full refresh.
        let mut q = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x01];
        q.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        q.extend_from_slice(&[0x10, 0x20, 0x30, 0x40]);
        q.push(0xF7);
        match m.incoming(&q, &s) {
            Some(Action::Reply(r)) => {
                assert_eq!(&r[..6], &[0xF0, 0x00, 0x00, 0x66, 0x14, 0x02]);
                assert_eq!(&r[6..13], &[1, 2, 3, 4, 5, 6, 7]);
                assert!(r[13..17].iter().all(|&b| b < 0x80) && r[17] == 0xF7);
            }
            other => panic!("{other:?}"),
        }
        let all = m.outgoing(&s);
        assert!(
            all.iter().any(|x| x[0] == 0xF0)
                && all.iter().filter(|x| x[0] & 0xF0 == 0xE0).count() == 9
        );
    }
}
