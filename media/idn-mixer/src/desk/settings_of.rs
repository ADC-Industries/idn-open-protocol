//! A desk's settings: limits and defaults, what each channel's and bus's chain is made
//! of.

use super::*;

/// A high-pass switched on with no frequency used before (Hz).
pub const HPF_DEFAULT: f32 = 80.0;
/// A low-pass switched on with no frequency used before (Hz).
pub const LPF_DEFAULT: f32 = 12_000.0;

/// An EQ's bands from a saved desk or a patch: up to [`EQ_BANDS`]. A list with fewer (a
/// patch giving its first bands) gets flat peaks after its own (at 0 dB: nothing heard,
/// nothing run); one with more (a later build's) keeps its first [`EQ_BANDS`], the rest
/// left out (refused, the whole settings file was lost).
pub(super) fn eq_bands<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<[Band; EQ_BANDS], D::Error> {
    let mut v: Vec<Band> = Vec::deserialize(d)?;
    v.truncate(EQ_BANDS);
    // One for every band a short list can lack (an empty one: all of them).
    const PAD: [f32; EQ_BANDS] = [
        1000.0, 4000.0, 300.0, 3000.0, 600.0, 8000.0, 150.0, 12_000.0,
    ];
    for &freq in &PAD[..EQ_BANDS - v.len()] {
        v.push(Band {
            on: true,
            shape: Shape::Peak,
            freq,
            gain: 0.0,
            q: 0.707,
        });
    }
    Ok(v.try_into().unwrap_or_else(|_| Band::defaults()))
}

/// Levels within the desk's ranges (§M.2, the panel's controls): nothing else checked
/// them, and an API call or settings file with `"fader": 300` sent infinite samples (NaN
/// once mixed) down every output to the speakers.
pub(super) fn fader(v: f32) -> f32 {
    v.clamp(-144.0, 10.0)
}

pub(super) fn bound_eq(eq: &mut [Band; EQ_BANDS]) {
    for b in eq {
        (b.gain, b.q) = (b.gain.clamp(-15.0, 15.0), b.q.clamp(0.1, 10.0));
    }
}

/// A key filter's frequency: 0 (off) or 20 Hz to 20 kHz.
fn key_freq(f: f32) -> f32 {
    if f.is_finite() && f > 0.0 {
        f.clamp(20.0, 20_000.0)
    } else {
        0.0
    }
}

/// `#rrggbb`, or empty.
pub(super) fn colour(c: &mut String) {
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        c.clear();
    }
}

pub(super) fn bound_dynamics(c: &mut Comp, l: &mut Limiter) {
    (c.threshold, c.ratio) = (c.threshold.clamp(-60.0, 0.0), c.ratio.clamp(1.0, 20.0));
    c.mix = if c.mix.is_finite() {
        c.mix.clamp(0.0, 100.0)
    } else {
        100.0
    };
    (c.attack, c.release) = (c.attack.clamp(0.1, 200.0), c.release.clamp(5.0, 2000.0));
    (c.knee, c.makeup) = (c.knee.clamp(0.0, 20.0), c.makeup.clamp(0.0, 24.0));
    (l.ceiling, l.release) = (l.ceiling.clamp(-30.0, 0.0), l.release.clamp(5.0, 1000.0));
}

pub(super) fn bound_send(s: &mut SendTo) {
    s.level = fader(s.level);
    s.pan = s.pan.map(|p| p.clamp(-1.0, 1.0));
}

/// The chain number of channel `i`'s inserts (from 0), in the plugin host.
pub fn channel_chain(i: usize) -> u32 {
    i as u32 + 1
}

/// The bus strip of a bus chain.
pub fn chain_strip(chain: u32) -> Option<usize> {
    (0..BUS_STRIPS).find(|&k| bus_strip(k).0 == chain)
}

/// A matrix mix's sources in words, in their order: `main`, `aux 1`…, `group 1`…,
/// `fx 1`….
pub fn matrix_source_names() -> Vec<String> {
    std::iter::once("main".to_owned())
        .chain((1..=AUX).map(|k| format!("aux {k}")))
        .chain((1..=GROUPS).map(|k| format!("group {k}")))
        .chain((1..=FX).map(|k| format!("fx {k}")))
        .collect()
}

/// Bus strip `k` in words: `main`, `aux 3`, `group 2`, `matrix 1`, `headphones`.
pub fn bus_strip_name(k: usize) -> String {
    match k {
        0 => "main".into(),
        k if k <= AUX => format!("aux {k}"),
        k if k <= AUX + GROUPS => format!("group {}", k - AUX),
        k if k <= AUX + GROUPS + MATRIX => format!("matrix {}", k - AUX - GROUPS),
        k if k <= AUX + GROUPS + MATRIX + FX => format!("fx {}", k - AUX - GROUPS - MATRIX),
        STREAM_STRIP => "stream".into(),
        _ => "headphones".into(),
    }
}

/// The bus strip a command names: `{"bus": "main" | 1–8 | "headphones"}`, `{"group": 1–8}`
/// or `{"matrix": 1–4}`.
pub fn named_bus(v: &serde_json::Value) -> Result<usize, String> {
    let num = |x: &serde_json::Value| {
        x.as_u64()
            .map(|n| n as usize)
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    };
    if let Some(g) = v.get("group").filter(|g| !g.is_null()) {
        return num(g)
            .filter(|g| (1..=GROUPS).contains(g))
            .map(|g| AUX + g)
            .ok_or(format!("group: 1 to {GROUPS}"));
    }
    if let Some(f) = v.get("fx").filter(|f| !f.is_null()) {
        return num(f)
            .filter(|f| (1..=FX).contains(f))
            .map(|f| AUX + GROUPS + MATRIX + f)
            .ok_or(format!("fx: 1 to {FX}"));
    }
    if let Some(m) = v.get("matrix").filter(|m| !m.is_null()) {
        return num(m)
            .filter(|m| (1..=MATRIX).contains(m))
            .map(|m| AUX + GROUPS + m)
            .ok_or(format!("matrix: 1 to {MATRIX}"));
    }
    match &v["bus"] {
        serde_json::Value::String(b) if b == "main" => Ok(0),
        serde_json::Value::String(b) if b == "stream" => Ok(STREAM_STRIP),
        serde_json::Value::String(b) if b == "headphones" || b == "monitor" => Ok(BUS_STRIPS - 1),
        b => num(b).filter(|k| (1..=AUX).contains(k)).ok_or(format!(
            "bus: \"main\", 1 to {AUX}, \"stream\" or \"headphones\" (or group, matrix, fx)"
        )),
    }
}

impl Settings {
    /// The channel whose settings channel `i` (from 0) plays with: a linked pair's first
    /// for its second, else itself.
    pub fn lead_of(&self, i: usize) -> usize {
        if i % 2 == 1 && self.channels.get(i - 1).is_some_and(|c| c.link) {
            i - 1
        } else {
            i
        }
    }

    /// `n` channels named 1…n, the main bus, aux 1–8 (mono, pre-fader).
    pub fn new(n: usize) -> Settings {
        Settings {
            channels: (1..=n)
                .map(|i| Channel {
                    name: format!("Ch {i}"),
                    ..Default::default()
                })
                .collect(),
            main: Bus {
                name: "Main".into(),
                pre: false,
                stereo: true,
                ..Default::default()
            },
            aux: (1..=AUX)
                .map(|i| Bus {
                    name: format!("Aux {i}"),
                    ..Default::default()
                })
                .collect(),
            dca: (1..=DCA)
                .map(|i| Dca {
                    name: format!("DCA {i}"),
                    ..Default::default()
                })
                .collect(),
            groups: (1..=GROUPS)
                .map(|i| Bus {
                    name: format!("Group {i}"),
                    pre: false,
                    stereo: true,
                    to_main: true,
                    ..Default::default()
                })
                .collect(),
            matrix: (1..=MATRIX)
                .map(|i| Bus {
                    name: format!("Matrix {i}"),
                    pre: false,
                    stereo: true,
                    sources: vec![SendTo::default(); MATRIX_SOURCES],
                    ..Default::default()
                })
                .collect(),
            // A new desk's FX buses come with the built-in effects (§M.14): a reverb, a
            // delay, a chorus, and one free.
            fx: (1..=FX)
                .map(|i| {
                    let kind = match i {
                        1 => crate::fx::EffectKind::Reverb,
                        2 => crate::fx::EffectKind::Delay,
                        3 => crate::fx::EffectKind::Chorus,
                        _ => crate::fx::EffectKind::None,
                    };
                    let name = match kind {
                        crate::fx::EffectKind::Reverb => "Reverb".to_owned(),
                        crate::fx::EffectKind::Delay => "Delay".to_owned(),
                        crate::fx::EffectKind::Chorus => "Chorus".to_owned(),
                        crate::fx::EffectKind::None => format!("FX {i}"),
                    };
                    Bus {
                        name,
                        pre: false,
                        stereo: true,
                        to_main: true,
                        effect: crate::fx::Effect {
                            kind,
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
                .collect(),
            mute_groups: (1..=MUTE_GROUPS)
                .map(|i| MuteGroup {
                    name: format!("Mute {i}"),
                    on: false,
                    color: String::new(),
                })
                .collect(),
            talkback: Talkback::default(),
            buttons: vec![Button::default(); BUTTONS],
            monitor: Bus {
                name: "Headphones".into(),
                pre: false,
                stereo: true,
                ..Default::default()
            },
            stream: Bus {
                name: "Stream".into(),
                pre: false,
                stereo: true,
                ..Default::default()
            },
            endpoints: Vec::new(),
            clock: "auto".into(),
            solo_afl: false,
            auto_park: false,
            block: default_block(),
            rate: None,
            x32: Default::default(),
            iem_mixes: Vec::new(),
            iem_control: false,
            includes_iem: false,
            // The buses of before 2026-11-08 on the first outputs as they were; those
            // added since (aux 9–16, matrix 5–8, FX 5–8) on none: outputs left free for a
            // switcher's feed or a patch (bug run 5: a new desk had none free).
            outputs: (0..OUTPUTS).map(default_patch).collect(),
            automix: false,
            duckers: (1..=DUCKERS)
                .map(|i| Ducker {
                    name: format!("Ducker {i}"),
                    keys: vec![false; n],
                    targets: vec![false; n],
                    ..Default::default()
                })
                .collect(),
            afv: Afv::default(),
            output_delay: vec![0.0; OUTPUTS],
        }
    }

    /// Fills in what a partial or older settings file lacks (lengths of lists), for `n`
    /// channels and `outputs` desk outputs (the first carrying the buses in order).
    pub fn normalised(mut self, n: usize, outputs: usize) -> Settings {
        let fresh = Settings::new(n);
        self.channels.truncate(n);
        while self.channels.len() < n {
            let i = self.channels.len();
            self.channels.push(fresh.channels[i].clone());
        }
        for c in &mut self.channels {
            c.sends.resize(AUX, SendTo::default());
            c.dca.resize(DCA, false);
            c.groups.resize(GROUPS, false);
            c.fx_sends.resize(FX, SendTo::default());
            c.mute_groups.resize(MUTE_GROUPS, false);
            c.delay = c.delay.clamp(0.0, 250.0);
            c.denoise_amount = if c.denoise_amount.is_finite() {
                c.denoise_amount.clamp(0.0, 100.0)
            } else {
                100.0
            };
            c.trim = c.trim.clamp(-20.0, 40.0);
            // (The sound was held to the edges already; the setting kept what it was
            // sent, and the panel showed 50: lab 477(a).)
            c.pan = if c.pan.is_finite() {
                c.pan.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            c.fader = fader(c.fader);
            bound_eq(&mut c.eq);
            bound_dynamics(&mut c.comp, &mut c.limiter);
            let g = &mut c.gate;
            (g.threshold, g.range) = (g.threshold.clamp(-80.0, 0.0), g.range.clamp(0.0, 80.0));
            (g.attack, g.release) = (g.attack.clamp(0.1, 50.0), g.release.clamp(5.0, 2000.0));
            (g.hold, g.ratio) = (g.hold.clamp(0.0, 2000.0), g.ratio.clamp(1.0, 10.0));
            // A slope it takes (§M.28): the nearest.
            for sl in [&mut c.hpf_slope, &mut c.lpf_slope] {
                *sl = *crate::dsp::SLOPES
                    .iter()
                    .min_by_key(|&&x| x.abs_diff(*sl))
                    .unwrap_or(&12);
            }
            (g.key_hpf, g.key_lpf) = (key_freq(g.key_hpf), key_freq(g.key_lpf));
            c.comp.key_hpf = key_freq(c.comp.key_hpf);
            // A key that isn't a channel of this desk: its own.
            if g.key > n {
                g.key = 0;
            }
            if c.comp.key > n {
                c.comp.key = 0;
            }
            let d = &mut c.deesser;
            d.freq = d.freq.clamp(2000.0, 16_000.0);
            (d.threshold, d.range) = (d.threshold.clamp(-60.0, 0.0), d.range.clamp(0.0, 24.0));
            c.sends
                .iter_mut()
                .chain(&mut c.fx_sends)
                .for_each(bound_send);
            c.automix_weight = c.automix_weight.clamp(-12.0, 12.0);
            // The filters' last frequencies: the one in use, else as kept.
            for (f, last, d) in [
                (c.hpf, &mut c.hpf_last, HPF_DEFAULT),
                (c.lpf, &mut c.lpf_last, LPF_DEFAULT),
            ] {
                if f > 0.0 && f.is_finite() {
                    *last = f;
                } else if !(last.is_finite() && *last > 0.0) {
                    *last = d;
                }
            }
        }
        self.mirror_pairs();
        fn fill<T: Clone>(list: &mut Vec<T>, fresh: &[T]) {
            list.truncate(fresh.len());
            let have = list.len();
            list.extend_from_slice(&fresh[have..]);
        }
        fill(&mut self.dca, &fresh.dca);
        fill(&mut self.aux, &fresh.aux);
        fill(&mut self.groups, &fresh.groups);
        fill(&mut self.matrix, &fresh.matrix);
        fill(&mut self.fx, &fresh.fx);
        fill(&mut self.mute_groups, &fresh.mute_groups);
        fill(&mut self.buttons, &fresh.buttons);
        fill(&mut self.duckers, &fresh.duckers);
        for d in &mut self.duckers {
            d.keys.resize(n, false);
            d.targets.resize(n, false);
            d.threshold = d.threshold.clamp(-80.0, 0.0);
            d.depth = d.depth.clamp(-60.0, 0.0);
            d.attack = d.attack.clamp(1.0, 1000.0);
            d.hold = d.hold.clamp(0.0, 5000.0);
            d.release = d.release.clamp(10.0, 10_000.0);
        }
        self.afv.off = fader(self.afv.off).min(0.0);
        self.output_delay.resize(outputs, 0.0);
        for d in &mut self.output_delay {
            *d = if d.is_finite() {
                d.clamp(0.0, 500.0)
            } else {
                0.0
            };
        }
        self.afv.fade = self.afv.fade.clamp(0.0, 5000.0);
        let names = matrix_source_names();
        for m in &mut self.matrix {
            // Saved with its sources' names (from 2026-11-09): put in place by name.
            if !m.source_names.is_empty() && m.source_names != names {
                let old = std::mem::take(&mut m.sources);
                let mut s = vec![SendTo::default(); MATRIX_SOURCES];
                for (n, v) in m.source_names.iter().zip(old) {
                    if let Some(j) = names.iter().position(|x| x == n) {
                        s[j] = v;
                    }
                }
                m.sources = s;
            }
            m.sources.resize(MATRIX_SOURCES, SendTo::default());
            m.source_names = names.clone();
        }
        for b in [&mut self.main, &mut self.monitor, &mut self.stream]
            .into_iter()
            .chain(&mut self.aux)
            .chain(&mut self.groups)
            .chain(&mut self.matrix)
            .chain(&mut self.fx)
        {
            b.fader = fader(b.fader);
            b.dim_db = if b.dim_db.is_finite() {
                b.dim_db.clamp(-60.0, 0.0)
            } else {
                -20.0
            };
            bound_eq(&mut b.eq);
            bound_dynamics(&mut b.comp, &mut b.limiter);
            b.sources.iter_mut().for_each(bound_send);
            b.effect = std::mem::take(&mut b.effect).bounded();
            b.geq = std::mem::take(&mut b.geq).bounded();
        }
        if !["auto", "card", "site", "own"].contains(&self.clock.as_str()) {
            self.clock = "auto".into();
        }
        for d in &mut self.dca {
            d.fader = fader(d.fader);
            colour(&mut d.color);
        }
        for g in &mut self.mute_groups {
            colour(&mut g.color);
        }
        self.talkback.level = fader(self.talkback.level);
        self.talkback.dim_main = if self.talkback.dim_main.is_finite() {
            self.talkback.dim_main.clamp(-40.0, 0.0)
        } else {
            0.0
        };
        // The new auxes (9–16 on a desk of before 2026-11-08) on only if every one was.
        let all = self.talkback.aux.iter().all(|&a| a);
        self.talkback.aux.resize(AUX, all);
        if self
            .rate
            .is_some_and(|r| !idn_open_protocol::node::rate::valid(r))
        {
            self.rate = None;
        }
        if !BLOCKS.contains(&self.block) {
            self.block = default_block();
        }
        self.outputs.truncate(outputs);
        let have = self.outputs.len();
        self.outputs.extend((have..outputs).map(default_patch));
        crate::iem::normalise(&mut self.iem_mixes, n);
        self
    }

    /// Each linked pair's second channel (even-numbered) given its first's settings but
    /// for its own ([`PAIR_OWN`]: name, inputs) and its plugins (changed only on the
    /// first, which the pair's chain runs on), so that whatever reads the second (a
    /// surface, OSC, a preset, a recall safe, DCA members) reads what the pair plays
    /// with, and an unlinked second keeps the pair's settings (bug run O M1–M4). An
    /// even-numbered channel's `link` is its first's: it says it is a pair's second.
    fn mirror_pairs(&mut self) {
        for i in (1..self.channels.len()).step_by(2) {
            let (firsts, rest) = self.channels.split_at_mut(i);
            let (lead, second) = (&mut firsts[i - 1], &mut rest[0]);
            if !lead.link {
                second.link = false;
                continue;
            }
            // (Plugin states can be hundreds of kB: not cloned only to be dropped.)
            let lead_inserts = std::mem::take(&mut lead.inserts);
            let mut c = lead.clone();
            lead.inserts = lead_inserts;
            c.name = std::mem::take(&mut second.name);
            c.source = second.source;
            c.input_b = second.input_b;
            c.use_b = second.use_b;
            c.failover = second.failover;
            c.inserts = std::mem::take(&mut second.inserts);
            *second = c;
        }
    }

    /// Bus strip `k` ([`bus_strip`]: main, aux 1–8, groups, matrix mixes, headphones).
    pub fn bus(&self, k: usize) -> Option<&Bus> {
        match k {
            0 => Some(&self.main),
            k if k <= AUX => self.aux.get(k - 1),
            k if k <= AUX + GROUPS => self.groups.get(k - 1 - AUX),
            k if k <= AUX + GROUPS + MATRIX => self.matrix.get(k - 1 - AUX - GROUPS),
            k if k <= AUX + GROUPS + MATRIX + FX => self.fx.get(k - 1 - AUX - GROUPS - MATRIX),
            STREAM_STRIP => Some(&self.stream),
            k if k == BUS_STRIPS - 1 => Some(&self.monitor),
            _ => None,
        }
    }

    pub fn bus_mut(&mut self, k: usize) -> Option<&mut Bus> {
        match k {
            0 => Some(&mut self.main),
            k if k <= AUX => self.aux.get_mut(k - 1),
            k if k <= AUX + GROUPS => self.groups.get_mut(k - 1 - AUX),
            k if k <= AUX + GROUPS + MATRIX => self.matrix.get_mut(k - 1 - AUX - GROUPS),
            k if k <= AUX + GROUPS + MATRIX + FX => self.fx.get_mut(k - 1 - AUX - GROUPS - MATRIX),
            STREAM_STRIP => Some(&mut self.stream),
            k if k == BUS_STRIPS - 1 => Some(&mut self.monitor),
            _ => None,
        }
    }

    /// What desk output `o` carries, in words (its label on the network).
    pub fn output_label(&self, o: usize) -> String {
        let buses = self.output_labels();
        let ch = |n: &str| {
            n.parse::<usize>()
                .ok()
                .and_then(|n| self.channels.get(n.wrapping_sub(1)))
                .map(|c| c.name.clone())
        };
        let p = self.outputs.get(o).map(String::as_str).unwrap_or("");
        let named = match p.split_once(':') {
            Some(("bus", j)) => j.parse::<usize>().ok().and_then(|j| buses.get(j).cloned()),
            Some(("pre", n)) => ch(n).map(|c| format!("{c} (pre)")),
            Some(("post", n)) => ch(n).map(|c| format!("{c} (direct)")),
            Some(("input", n)) => ch(n).map(|c| format!("{c} (input)")),
            Some(("preeq", n)) => ch(n).map(|c| format!("{c} (pre-EQ)")),
            Some(("posteq", n)) => ch(n).map(|c| format!("{c} (post-EQ)")),
            Some(("in", n)) => Some(format!("In {n}")),
            _ => None,
        };
        named.unwrap_or_else(|| format!("Out {}", o + 1))
    }

    /// The output channels' labels, by the outside numbering (`bus:J`, [`bus_out`]).
    pub fn output_labels(&self) -> Vec<String> {
        let inside = self.bus_labels();
        let mut l = vec![String::new(); inside.len()];
        for (i, x) in inside.into_iter().enumerate() {
            if let Some(slot) = l.get_mut(bus_out(i)) {
                *slot = x;
            }
        }
        l
    }

    /// The internal bus channels' labels, in the processing's order.
    fn bus_labels(&self) -> Vec<String> {
        let mut l = vec![
            format!("{} L", self.main.name),
            format!("{} R", self.main.name),
        ];
        for b in &self.aux {
            if b.stereo {
                l.push(format!("{} L", b.name));
                l.push(format!("{} R", b.name));
            } else {
                l.push(b.name.clone());
                l.push(format!("{} (copy)", b.name));
            }
        }
        l.push(format!("{} L", self.monitor.name));
        l.push(format!("{} R", self.monitor.name));
        for b in self.groups.iter().chain(&self.matrix).chain(&self.fx) {
            l.push(format!("{} L", b.name));
            l.push(format!("{} R", b.name));
        }
        l.push(format!("{} L", self.stream.name));
        l.push(format!("{} R", self.stream.name));
        l
    }
}

/// Output `j`'s patch on a new desk: the bus of that outside number up to the buses of
/// before 2026-11-08, then nothing.
fn default_patch(j: usize) -> String {
    if j < ADDED_AT {
        format!("bus:{j}")
    } else {
        String::new()
    }
}
