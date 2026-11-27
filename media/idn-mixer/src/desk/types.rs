//! The desk's settings as kept and sent (§M.5): the channels, buses, groups and their
//! parts; a snapshot is a whole [`Settings`]. (The processing state is in the parent.)

use super::*;

/// One EQ band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Band {
    pub on: bool,
    pub shape: Shape,
    pub freq: f32,
    /// dB, ±15.
    pub gain: f32,
    pub q: f32,
}

impl Band {
    pub(super) fn defaults() -> [Band; EQ_BANDS] {
        let b = |shape, freq| Band {
            on: true,
            shape,
            freq,
            gain: 0.0,
            q: 0.707,
        };
        [
            b(Shape::LowShelf, 100.0),
            b(Shape::Peak, 250.0),
            b(Shape::Peak, 500.0),
            b(Shape::Peak, 1000.0),
            b(Shape::Peak, 2500.0),
            b(Shape::Peak, 4000.0),
            b(Shape::Peak, 6000.0),
            b(Shape::HighShelf, 10_000.0),
        ]
    }
}

impl Default for Band {
    fn default() -> Self {
        Band {
            on: true,
            shape: Shape::Peak,
            freq: 1000.0,
            gain: 0.0,
            q: 0.707,
        }
    }
}

/// What a channel's gate does (§M.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    /// Below the threshold the sound goes down by `range`.
    #[default]
    Gate,
    /// Below the threshold the sound goes down `ratio` times as fast as the level, at
    /// most by `range`: gentler than a gate.
    Expander,
    /// Above the threshold (usually of another channel, its `key`) the sound goes down
    /// by `range`: music under a voice.
    Ducker,
}

/// A gate (off by default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    pub on: bool,
    pub threshold: f32,
    /// How far it closes, dB.
    pub range: f32,
    pub attack: f32,
    pub release: f32,
    /// How long it stays open after the level falls under the threshold (a ducker: stays
    /// down after it rises over), ms (0–2000).
    pub hold: f32,
    pub mode: GateMode,
    /// The expander's ratio (1–10).
    pub ratio: f32,
    /// The channel (from 1) whose sound opens and closes it; 0: its own.
    pub key: usize,
    /// The key's filters, Hz (0: off): what the gate listens to, not what it lets
    /// through (a kick's gate deaf to the cymbals).
    pub key_hpf: f32,
    pub key_lpf: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            on: false,
            threshold: -50.0,
            range: 40.0,
            attack: 1.0,
            release: 150.0,
            hold: 0.0,
            mode: GateMode::Gate,
            ratio: 2.0,
            key: 0,
            key_hpf: 0.0,
            key_lpf: 0.0,
        }
    }
}

impl Gate {
    /// None of §M.28's options in use: the plain gate (cheaper per sample).
    pub fn plain(&self) -> bool {
        self.mode == GateMode::Gate
            && self.key == 0
            && self.hold <= 0.0
            && self.key_hpf <= 0.0
            && self.key_lpf <= 0.0
    }
}

impl Comp {
    /// None of §M.28's detector options in use: the plain compressor (`mix` is applied
    /// either way).
    pub fn plain(&self) -> bool {
        self.detect == Detect::Peak && !self.auto_release && self.key == 0 && self.key_hpf <= 0.0
    }
}

/// How a compressor hears the level (§M.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detect {
    /// Each peak (fast, the usual).
    #[default]
    Peak,
    /// The average power over about 10 ms (smoother, as the ear hears loudness).
    Rms,
}

/// A compressor (off by default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Comp {
    pub on: bool,
    pub threshold: f32,
    pub ratio: f32,
    pub attack: f32,
    pub release: f32,
    pub knee: f32,
    pub makeup: f32,
    /// How much of the compressed sound is heard, % (0–100; under 100: parallel
    /// compression, the dry sound under it).
    pub mix: f32,
    pub detect: Detect,
    /// The release follows the sound: quick after a short peak, slower while it keeps
    /// compressing (`release` the longest).
    pub auto_release: bool,
    /// The channel (from 1) whose sound drives it; 0: its own.
    pub key: usize,
    /// A high-pass on what it hears, Hz (0: off): the bass doesn't pump it.
    pub key_hpf: f32,
}

impl Default for Comp {
    fn default() -> Self {
        Comp {
            on: false,
            threshold: -20.0,
            ratio: 3.0,
            attack: 10.0,
            release: 150.0,
            knee: 6.0,
            makeup: 0.0,
            mix: 100.0,
            detect: Detect::Peak,
            auto_release: false,
            key: 0,
            key_hpf: 0.0,
        }
    }
}

/// A limiter (off by default): nothing much above the ceiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limiter {
    pub on: bool,
    /// dBFS.
    pub ceiling: f32,
    /// ms.
    pub release: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter {
            on: false,
            ceiling: -1.0,
            release: 50.0,
        }
    }
}

/// A de-esser (off by default, §M.11): sibilance above `freq` turned down 4:1, at most
/// `range` dB; split-band (only the band above `freq`) or the whole signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeEsser {
    pub on: bool,
    /// Hz.
    pub freq: f32,
    /// dBFS, of the band above `freq`.
    pub threshold: f32,
    /// dB, the most it turns down.
    pub range: f32,
    pub split: bool,
}

impl Default for DeEsser {
    fn default() -> Self {
        DeEsser {
            on: false,
            freq: 6000.0,
            threshold: -30.0,
            range: 10.0,
            split: true,
        }
    }
}

fn twelve() -> u8 {
    12
}

/// A channel's processing, as a preset holds it (§M.11): copied, pasted and saved by name;
/// not its name, input, fader, pan, sends or routing.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelPreset {
    pub polarity: bool,
    pub delay: f32,
    pub hpf: f32,
    pub lpf: f32,
    /// Their slopes (§M.28; a preset of before them: 12).
    #[serde(default = "twelve")]
    pub hpf_slope: u8,
    #[serde(default = "twelve")]
    pub lpf_slope: u8,
    pub gate: Gate,
    pub eq_on: bool,
    #[serde(deserialize_with = "eq_bands")]
    pub eq: [Band; EQ_BANDS],
    pub comp: Comp,
    pub deesser: DeEsser,
    pub limiter: Limiter,
    pub denoise: bool,
    /// (Presets of before §M.28: 100 and after.)
    #[serde(default = "hundred")]
    pub denoise_amount: f32,
    pub inserts: Vec<Insert>,
    #[serde(default)]
    pub insert_point: InsertPoint,
}

fn hundred() -> f32 {
    100.0
}

impl ChannelPreset {
    pub fn of(c: &Channel) -> ChannelPreset {
        ChannelPreset {
            polarity: c.polarity,
            delay: c.delay,
            hpf: c.hpf,
            lpf: c.lpf,
            hpf_slope: c.hpf_slope,
            lpf_slope: c.lpf_slope,
            gate: c.gate.clone(),
            eq_on: c.eq_on,
            eq: c.eq.clone(),
            comp: c.comp.clone(),
            deesser: c.deesser.clone(),
            limiter: c.limiter.clone(),
            denoise: c.denoise,
            denoise_amount: c.denoise_amount,
            inserts: c.inserts.clone(),
            insert_point: c.insert_point,
        }
    }

    pub fn apply(&self, c: &mut Channel) {
        let p = self.clone();
        (c.polarity, c.delay, c.hpf, c.lpf) = (p.polarity, p.delay, p.hpf, p.lpf);
        (c.hpf_slope, c.lpf_slope) = (p.hpf_slope, p.lpf_slope);
        (c.gate, c.eq_on, c.eq, c.comp) = (p.gate, p.eq_on, p.eq, p.comp);
        (c.deesser, c.limiter, c.denoise) = (p.deesser, p.limiter, p.denoise);
        (c.denoise_amount, c.insert_point) = (p.denoise_amount, p.insert_point);
        c.inserts = p.inserts;
    }
}

/// A channel's send to an aux bus (or a matrix's from a bus).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendTo {
    pub on: bool,
    /// dB.
    pub level: f32,
    /// Before the channel fader (`true`) or after; `None`: as the bus says (its `pre`).
    pub pre: Option<bool>,
    /// Where it sits in a stereo aux, −1…+1; `None`: as the channel's pan.
    pub pan: Option<f32>,
    /// Where on the strip it is taken (§M.28); `None`: as `pre` says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tap: Option<SendTap>,
}

/// Where on a channel's strip a send is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendTap {
    /// The input as it came, before its trim (and everything after).
    Input,
    /// After the filters and gate, before the EQ.
    PreEq,
    /// After the EQ, before the compressor.
    PostEq,
    /// After the processing, before the fader (`pre: true`).
    PreFader,
    /// After the fader (`pre: false`).
    PostFader,
}

impl Default for SendTo {
    fn default() -> Self {
        SendTo {
            on: false,
            level: 0.0,
            pre: None,
            pan: None,
            tap: None,
        }
    }
}

/// A mute group (§M.11): when on, its member channels are muted.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MuteGroup {
    pub name: String,
    pub on: bool,
    /// Its colour in panels (`#rrggbb`; empty: none).
    pub color: String,
}

/// Talkback (§M.11): while `on`, a channel's signal (after its processing, before its
/// fader and mute) goes to the chosen aux buses and the main bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Talkback {
    /// The talkback microphone's channel, from 1; 0: none.
    pub channel: usize,
    pub on: bool,
    /// dB.
    pub level: f32,
    pub aux: Vec<bool>,
    pub main: bool,
    /// The main bus turned down this much while talkback is on, dB (0: not; −40–0): the
    /// PA doesn't feed back the engineer's voice.
    pub dim_main: f32,
    /// The talkback microphone in the IEM flow (§M.28): the musicians' phones hear the
    /// engineer while talkback is on.
    pub to_iem: bool,
}

impl Default for Talkback {
    fn default() -> Self {
        Talkback {
            channel: 0,
            on: false,
            level: 0.0,
            aux: vec![true; AUX],
            main: false,
            dim_main: 0.0,
            to_iem: false,
        }
    }
}

/// A user-assignable button (§M.11): a label and what it does (`mute:N`, `mute_group:N`,
/// `dca_mute:N`, `snapshot:NAME`, `talkback`, `clear_solo`, `fx_mute`; empty: nothing).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Button {
    pub label: String,
    pub action: String,
}

/// A DCA group (§M.6): a fader and mute over its member channels, carrying no sound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dca {
    pub name: String,
    /// dB, added to each member's fader.
    pub fader: f32,
    pub mute: bool,
    /// Its colour in panels (`#rrggbb`; empty: none).
    pub color: String,
}

impl Default for Dca {
    fn default() -> Self {
        Dca {
            name: String::new(),
            fader: 0.0,
            mute: false,
            color: String::new(),
        }
    }
}

/// Where a channel's inserts sit (§M.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertPoint {
    /// After the filters, gate, EQ, compressor, de-esser and limiter (as always).
    #[default]
    Post,
    /// Before them, after the trim, delay and noise suppression.
    Pre,
}

/// A plugin in a strip's insert chain (§M.6).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Insert {
    /// The plugin, as the plugin list gives it (`clap:PATH#ID`, `vst3:PATH#CLASS`).
    pub key: String,
    pub name: String,
    /// Kept loaded (its reverb tail and settings kept), the sound passing round it.
    pub bypass: bool,
    /// Its saved state, base64: kept up to date about once a second while it changes.
    pub state: String,
}

/// What a linked pair's second channel keeps for itself (its name and its inputs); a
/// patch's other keys go to the pair's first, whose settings it is given.
pub const PAIR_OWN: [&str; 5] = ["name", "source", "input_b", "use_b", "failover"];

/// An input channel's settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Channel {
    pub name: String,
    /// The strip's colour in panels (`#rrggbb`; empty: none).
    pub color: String,
    /// The desk input it takes (the input patch, from 1; 0: none); `None`: its own number.
    pub source: Option<usize>,
    /// A second (backup) input (from 1; 0: none): taken while `use_b`, or with `failover`
    /// while nothing arrives on the first and something does on it.
    pub input_b: usize,
    pub use_b: bool,
    pub failover: bool,
    /// On the desk (§M.11): `Some(true)`, on it (processed while its input is routed);
    /// `Some(false)`, parked (hidden): nothing of it in any mix; `None`, as routing says,
    /// with automatic parking ([`Settings::auto_park`]) on. With it off (the default) the
    /// desk sets `None` to on or parked once, by whether the input is routed then.
    pub shown: Option<bool>,
    /// dB, −20…+40.
    pub trim: f32,
    pub polarity: bool,
    /// Input delay, ms (0–250).
    pub delay: f32,
    /// High-pass frequency, Hz; 0 is off.
    pub hpf: f32,
    /// Low-pass frequency, Hz; 0 is off.
    pub lpf: f32,
    /// The high-pass and low-pass frequencies last used (kept while the filter is off):
    /// a patch's `hpf_on: true` / `lpf_on: true` turns the filter on at them
    /// ([`HPF_DEFAULT`] and [`LPF_DEFAULT`] until one is set).
    pub hpf_last: f32,
    pub lpf_last: f32,
    /// The high-pass's and low-pass's slopes, dB an octave: 6, 12 (default), 18, 24, 36
    /// or 48 (§M.28).
    pub hpf_slope: u8,
    pub lpf_slope: u8,
    pub gate: Gate,
    pub eq_on: bool,
    #[serde(deserialize_with = "eq_bands")]
    pub eq: [Band; EQ_BANDS],
    pub comp: Comp,
    pub deesser: DeEsser,
    pub limiter: Limiter,
    /// Speech noise suppression right after the input delay (an RNNoise-class network,
    /// `idn_dsp::denoise`): steady noise out, speech kept. It adds 20 ms, so while any
    /// channel has it on the others are delayed alike and everything stays in line.
    pub denoise: bool,
    /// How much of the suppressed sound is heard, % (0–100, default 100; §M.28): the rest
    /// the sound as it came, delayed alike, so a little natural room stays.
    pub denoise_amount: f32,
    pub mute: bool,
    /// dB; −144 and below is off.
    pub fader: f32,
    /// −1 (left) … +1 (right); for a linked pair, the balance.
    pub pan: f32,
    /// This (odd-numbered) channel and the next are a stereo pair, with this one's
    /// settings: the next keeps its own [`PAIR_OWN`] (its name and inputs) and its own
    /// plugins, and is given the rest. On an even-numbered channel: it is a pair's
    /// second (its first's, as [`Settings::normalised`] makes it).
    pub link: bool,
    pub solo: bool,
    pub sends: Vec<SendTo>,
    /// Its sends to the FX buses (after the fader unless set otherwise).
    pub fx_sends: Vec<SendTo>,
    /// Its send to the stream bus (after the fader unless set otherwise; on at 0 dB on a
    /// new desk, so the stream starts as the main mix).
    pub stream: SendTo,
    /// Plugins after the compressor, before the fader, in order (no fixed number).
    pub inserts: Vec<Insert>,
    /// Where its inserts sit (§M.28): after its processing (`post`, the default) or
    /// before it, straight after the trim and delay (`pre`).
    pub insert_point: InsertPoint,
    /// The DCA groups it belongs to (one flag per group).
    pub dca: Vec<bool>,
    /// It goes to the main bus (after its fader and pan).
    pub main: bool,
    /// The group buses it goes to (after its fader and pan).
    pub groups: Vec<bool>,
    /// The mute groups it belongs to.
    pub mute_groups: Vec<bool>,
    /// In the automix (§M.12): its level shared with the other automixed channels by how
    /// loud each is (gain sharing), after its fader's own level.
    pub automix: bool,
    /// Its weight in the automix, dB (−12…+12): a higher one wins more of the share.
    pub automix_weight: f32,
    /// Audio follows video (§M.12): the switcher input (by its label) this channel
    /// follows; empty: none.
    pub follows: String,
    /// Recall safe (§M.15): a snapshot's recall leaves this channel as it is. Kept with the
    /// desk, not taken from snapshots.
    pub safe: bool,
    /// In the IEM flow (§M.26, idn-iem.md): sent to the musicians' phones after its
    /// processing, before its fader and mute. A linked pair is marked as one (both sides).
    /// Taken from a snapshot only with its IEM mixes (`include_iem`).
    pub iem: bool,
}

impl Default for Channel {
    fn default() -> Self {
        Channel {
            name: String::new(),
            color: String::new(),
            source: None,
            input_b: 0,
            use_b: false,
            failover: false,
            shown: None,
            trim: 0.0,
            polarity: false,
            delay: 0.0,
            hpf: 0.0,
            lpf: 0.0,
            hpf_last: HPF_DEFAULT,
            lpf_last: LPF_DEFAULT,
            hpf_slope: 12,
            lpf_slope: 12,
            gate: Gate::default(),
            eq_on: true,
            eq: Band::defaults(),
            comp: Comp::default(),
            deesser: DeEsser::default(),
            limiter: Limiter::default(),
            denoise: false,
            denoise_amount: 100.0,
            mute: false,
            fader: 0.0,
            pan: 0.0,
            link: false,
            solo: false,
            sends: vec![SendTo::default(); AUX],
            fx_sends: vec![SendTo::default(); FX],
            stream: SendTo {
                on: true,
                ..SendTo::default()
            },
            inserts: Vec::new(),
            insert_point: InsertPoint::Post,
            dca: vec![false; DCA],
            main: true,
            groups: vec![false; GROUPS],
            mute_groups: vec![false; MUTE_GROUPS],
            automix: false,
            automix_weight: 0.0,
            follows: String::new(),
            safe: false,
            iem: false,
        }
    }
}

/// A ducker (§M.12): while its key channels (speech) are over the threshold, its target
/// channels (music, playback) go down by `depth`, after their faders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ducker {
    pub name: String,
    pub on: bool,
    /// The channels whose sound ducks the targets.
    pub keys: Vec<bool>,
    /// The channels ducked.
    pub targets: Vec<bool>,
    /// The level the keys must pass, dBFS (−80…0).
    pub threshold: f32,
    /// How far the targets go down, dB (−60…0).
    pub depth: f32,
    /// Going down, ms (1–1000); staying down after the keys fall quiet, ms (0–5000);
    /// coming back up, ms (10–10000).
    pub attack: f32,
    pub hold: f32,
    pub release: f32,
}

impl Default for Ducker {
    fn default() -> Self {
        Ducker {
            name: String::new(),
            on: false,
            keys: Vec::new(),
            targets: Vec::new(),
            threshold: -40.0,
            depth: -15.0,
            attack: 20.0,
            hold: 300.0,
            release: 800.0,
        }
    }
}

/// Audio follows video (§M.12): channels follow the switcher inputs they are set to,
/// coming up when their input is on air and going down (or off) when not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Afv {
    pub on: bool,
    /// The switcher's program source (`HOST (Switcher)`): what is on air comes from it.
    pub switcher: String,
    /// A following channel's level while its input is off air, dB (−144: off).
    pub off: f32,
    /// The fade between, ms (0–5000).
    pub fade: f32,
}

impl Default for Afv {
    fn default() -> Self {
        Afv {
            on: false,
            switcher: String::new(),
            off: -144.0,
            fade: 500.0,
        }
    }
}

/// A bus's settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bus {
    pub name: String,
    /// Its colour in panels (`#rrggbb`; empty: none).
    pub color: String,
    pub eq_on: bool,
    #[serde(deserialize_with = "eq_bands")]
    pub eq: [Band; EQ_BANDS],
    pub comp: Comp,
    pub limiter: Limiter,
    pub mute: bool,
    pub fader: f32,
    /// Aux buses: sends to it are taken before the channel faders (monitor mixes).
    pub pre: bool,
    /// Aux buses: stereo (groups and matrix mixes always are).
    pub stereo: bool,
    /// Main and aux buses: plugins after the limiter, before the fader.
    pub inserts: Vec<Insert>,
    /// Groups: into the main bus after their fader.
    pub to_main: bool,
    /// Matrix mixes: their sources (main, aux 1–16, groups 1–8, FX 1–4) and levels.
    pub sources: Vec<SendTo>,
    /// Matrix mixes: the sources' names, in the same order, as saved (`main`, `aux 3`,
    /// `group 2`, `fx 1`): a desk saved with another layout is put in place by name, not
    /// by place (bug run 5, 3-7b: the places moved when the aux went from 8 to 16).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_names: Vec<String>,
    /// Heard on the headphones (the monitor bus) while on (§M.11).
    pub listen: bool,
    /// The main bus: summed to mono (both sides the same).
    pub mono: bool,
    /// The main bus: down by `dim_db`.
    pub dim: bool,
    /// How far `dim` turns it down, dB (−60 to 0; default −20).
    pub dim_db: f32,
    /// FX buses: the built-in effect its sends feed (§M.14); its return is all effect.
    pub effect: crate::fx::Effect,
    /// Main, aux, matrix and headphone buses: a 31-band graphic EQ (§M.14).
    pub geq: crate::fx::Geq,
    /// Recall safe (§M.15): a snapshot's recall leaves this bus as it is.
    pub safe: bool,
}

impl Default for Bus {
    fn default() -> Self {
        Bus {
            name: String::new(),
            color: String::new(),
            eq_on: true,
            eq: Band::defaults(),
            comp: Comp::default(),
            limiter: Limiter::default(),
            mute: false,
            fader: 0.0,
            pre: true,
            stereo: false,
            inserts: Vec::new(),
            to_main: false,
            sources: Vec::new(),
            source_names: Vec::new(),
            listen: false,
            mono: false,
            dim: false,
            dim_db: -20.0,
            effect: crate::fx::Effect::default(),
            geq: crate::fx::Geq::default(),
            safe: false,
        }
    }
}

/// The whole desk's settings: what a snapshot holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub channels: Vec<Channel>,
    pub main: Bus,
    pub aux: Vec<Bus>,
    pub dca: Vec<Dca>,
    pub groups: Vec<Bus>,
    pub matrix: Vec<Bus>,
    pub fx: Vec<Bus>,
    pub mute_groups: Vec<MuteGroup>,
    pub talkback: Talkback,
    pub buttons: Vec<Button>,
    /// The headphone (monitor) bus: what is soloed or listened to, else the main mix.
    pub monitor: Bus,
    /// The stream bus: the livestream's own mix, beside main.
    pub stream: Bus,
    /// Soloed channels are heard after their fader and pan (AFL) instead of before (PFL).
    pub solo_afl: bool,
    /// Channels go on the desk and park by themselves as their inputs are routed and
    /// unrouted (off by default; manual then). Kept with the
    /// desk, not taken from snapshots.
    pub auto_park: bool,
    /// Samples a block the desk runs (one of [`BLOCKS`]): kept
    /// with the desk, not taken from snapshots; from the desk's next start (its buffers,
    /// plugin chains and clock are made for it).
    pub block: usize,
    /// The desk's own sample rate (44100, 48000 or 96000; §N.40), from its next start;
    /// `None`: the machine's. Kept with the desk, not taken from snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
    /// The output patch (§M.11): what each desk output carries: `bus:J` (internal bus
    /// channel J, as [`Settings::output_labels`] names them), `pre:N` / `post:N`
    /// (channel N before or after its fader), `input:N`, `preeq:N`, `posteq:N` (channel N
    /// as its input came, before its EQ, after its EQ), `in:N` (desk input N as it came),
    /// or `""`.
    pub outputs: Vec<String>,
    /// The automix is on (§M.12): the channels set to it share their level.
    pub automix: bool,
    /// Duckers (§M.12).
    pub duckers: Vec<Ducker>,
    /// Audio follows video (§M.12).
    pub afv: Afv,
    /// Each desk output's delay, ms (0–500; §M.15): a delay speaker, a screen running late.
    pub output_delay: Vec<f32>,
    /// A Behringer X32 whose preamps the desk controls over Ethernet (§M.23.1). Kept with
    /// the desk, not taken from snapshots.
    pub x32: X32Setting,
    /// The clock the desk follows (automatic by default, an engineer may
    /// choose): `"auto"` (R.26: its card, else site time, else its own), `"card"`,
    /// `"site"` or `"own"`; one not available falls back to the automatic choice. Kept
    /// with the desk, not taken from snapshots.
    pub clock: String,
    /// Named output endpoints, each its own IDN output. Kept with the
    /// desk, not taken from snapshots.
    pub endpoints: Vec<Endpoint>,
    /// The IEM mixes (§M.26): each musician's levels of the IEM channels, which the phones
    /// mix by; kept with the desk, in a snapshot only with `include_iem`.
    pub iem_mixes: Vec<crate::iem::IemMix>,
    /// Phones may use a `control` token on the IEM flow (idn-iem.md §I.4; an
    /// option, off by default): a subscription sends its token over plain
    /// UDP, and a captured `control` token works on the desk's API for its life. Off: only
    /// `iem:<mix>` and `read` tokens there (and this machine's own requests). Kept with the
    /// desk, not taken from snapshots.
    pub iem_control: bool,
    /// In a snapshot: it holds the IEM mixes and marks, recalled with it (§M.26). Always
    /// false on the desk itself.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub includes_iem: bool,
}

/// A named output endpoint: some desk outputs published as their own
/// IDN output (`"<desk> <name>"`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Endpoint {
    /// Stable (`e1`, `e2`, …).
    pub id: String,
    pub name: String,
    /// The desk outputs it carries, from 1, in order.
    pub outputs: Vec<usize>,
}

/// The X32 the desk controls over Ethernet (§M.23.1, [`crate::x32`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct X32Setting {
    /// `""`: none; `"auto"`: the first found on the LAN; else its IP address or host name,
    /// with `:port` if not 10023.
    pub address: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::new(32)
    }
}

/// Meters and gain reduction, for the panel.
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Per channel: input (after trim) and output (after the fader).
    pub channel_in: Vec<Meter>,
    pub channel_out: Vec<Meter>,
    /// Per channel: gate, compressor, limiter and de-esser gain reduction, dB.
    pub channel_gr: Vec<(f32, f32, f32, f32)>,
    /// Per output channel.
    pub outputs: Vec<Meter>,
    /// Blocks whose plugins didn't finish in time (passed on dry): per channel (on a
    /// linked pair's first), and per bus (main, then the aux buses).
    pub channel_late: Vec<u64>,
    pub bus_late: Vec<u64>,
    /// Per bus strip (main, aux, groups, matrix, FX, headphones): compressor and limiter
    /// gain reduction, dB.
    pub bus_gr: Vec<(f32, f32)>,
    /// The main bus's loudness, LUFS: momentary (400 ms) and short-term (3 s).
    pub lufs_m: f32,
    pub lufs_s: f32,
    /// And integrated, since the last reset (§M.15).
    pub lufs_i: f32,
    /// The stream bus's loudness: momentary, short-term and integrated.
    pub stream_lufs: (f32, f32, f32),
    /// Per channel: what the automix, the duckers and audio-follows-video do to it
    /// together, dB (0: nothing).
    pub channel_auto: Vec<f32>,
}
