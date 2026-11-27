//! The X32's remote protocol as far as this module uses it: addresses, value scales and
//! the card routing, with no I/O.
//!
//! **Source** (public, community): Patrick-Gilles Maillot, "Unofficial X32/M32 OSC
//! Remote Protocol", version 4.09-02 (Nov 30, 2025), applying to console firmware 4.0
//! and later; it regroups Behringer's own published protocol notes (version 1.01, Oct
//! 2012) with community additions. Linked from
//! <https://sites.google.com/site/patrickmaillot/x32>. Page numbers below are that
//! document's own (as printed in its page headers).
//!
//! Nothing here comes from Behringer's applications or their traffic.

/// The X32's OSC port (p. 6: "OSC packets are received on UDP port 10023 and replies
/// are sent back to the requester's IP/port").
pub const PORT: u16 = 10023;

/// Headamps (preamps) the X32 addresses: `/headamp/[000…127]` (p. 43).
pub const HEADAMPS: usize = 128;

/// USB (card) channels: four routing blocks of eight (p. 23, `/config/routing/CARD`).
pub const USB_CHANNELS: usize = 32;

/// Input channels whose polarity and source the X32 reports (`/ch/[01…32]`, p. 25).
pub const CHANNELS: usize = 32;

// --- Headamp gain (p. 43, and the appendix "Headamp gain – 145 lin scale values",
// p. 162): `/headamp/[000…127]/gain`, linf [-12.000, 60.000, 0.500] dB. On the wire the
// float is 0.0…1.0 across that range (p. 11: "float parameters must be in range 0.0 –
// 1.0"; p. 14: "the data sent to or returned from the X32/M32 is always in the range
// [0.0 ... 1.0]"); the X32 rounds to the nearest known step (p. 13). ---

/// The lowest headamp gain, dB.
pub const GAIN_MIN_DB: f32 = -12.0;
/// The highest headamp gain, dB.
pub const GAIN_MAX_DB: f32 = 60.0;
/// The headamp gain step, dB (145 values).
pub const GAIN_STEP_DB: f32 = 0.5;

/// `db` on the X32's gain scale: clamped to [-12, +60] dB and rounded to its 0.5 dB
/// step. `None` for a NaN.
pub fn gain_step(db: f32) -> Option<f32> {
    if db.is_nan() {
        return None;
    }
    let db = db.clamp(GAIN_MIN_DB, GAIN_MAX_DB);
    Some((db / GAIN_STEP_DB).round() * GAIN_STEP_DB)
}

/// A gain in dB as the float the X32 takes (after [`gain_step`]).
pub fn gain_to_float(db: f32) -> Option<f32> {
    let db = gain_step(db)?;
    Some(((db - GAIN_MIN_DB) / (GAIN_MAX_DB - GAIN_MIN_DB)).clamp(0.0, 1.0))
}

/// The X32's gain float as dB, on its 0.5 dB step (the documented table rounds, e.g.
/// 0.0069 is -11.5 dB). Out-of-range or NaN floats are clamped.
pub fn float_to_gain(f: f32) -> f32 {
    let f = if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) };
    let db = GAIN_MIN_DB + f * (GAIN_MAX_DB - GAIN_MIN_DB);
    gain_step(db).unwrap_or(GAIN_MIN_DB)
}

// --- Addresses (all from the document's tables). ---

/// `/headamp/NNN/gain` (p. 43). `index` 0…127.
pub fn headamp_gain(index: u8) -> String {
    format!("/headamp/{index:03}/gain")
}

/// `/headamp/NNN/phantom` (p. 43): enum {OFF, ON}, int 0 or 1.
pub fn headamp_phantom(index: u8) -> String {
    format!("/headamp/{index:03}/phantom")
}

/// `/ch/NN/preamp/invert` (p. 25): enum {OFF, ON}, int 0 or 1. `channel` 1…32.
pub fn channel_invert(channel: u8) -> String {
    format!("/ch/{channel:02}/preamp/invert")
}

/// `/ch/NN/config/source` (p. 25): which input feeds the channel (FW 4.0+). Only
/// watched: a change makes the module ask `/-ha/NN/index` again.
pub fn channel_source(channel: u8) -> String {
    format!("/ch/{channel:02}/config/source")
}

/// `/-ha/NN/index` (p. 43): read-only, "the actual headamp used as source for a given
/// input"; NN 00…31 are channels 1…32 (32…39 the aux inputs, not used here). -1 when
/// the channel's source has no headamp (e.g. the card).
pub fn channel_headamp(channel: u8) -> String {
    format!("/-ha/{:02}/index", channel - 1)
}

/// The card output routing blocks (p. 22–23): `/config/routing/CARD/1-8` … `25-32`.
pub const CARD_BLOCKS: [&str; 4] = [
    "/config/routing/CARD/1-8",
    "/config/routing/CARD/9-16",
    "/config/routing/CARD/17-24",
    "/config/routing/CARD/25-32",
];

/// `/config/userrout/out/NN` (p. 22, FW 4.0+): user output NN 1…48.
pub fn user_out(n: u8) -> String {
    format!("/config/userrout/out/{n:02}")
}

/// `/config/userrout/in/NN` (p. 22, FW 4.0+): user input NN 1…32.
pub fn user_in(n: u8) -> String {
    format!("/config/userrout/in/{n:02}")
}

/// `/outputs/main/NN/src` (p. 41): output NN 1…16's source.
pub fn output_src(n: u8) -> String {
    format!("/outputs/main/{n:02}/src")
}

/// `/outputs/p16/NN/src` (p. 41): P16 output NN 1…16's source.
pub fn p16_src(n: u8) -> String {
    format!("/outputs/p16/{n:02}/src")
}

/// The info request and its reply (p. 8, p. 10): `/info` answers `,ssss` server version,
/// server name, console model, console (firmware) version.
pub const INFO: &str = "/info";
/// `/xinfo` (p. 8, p. 10): network address, network name, console model, console
/// version.
pub const XINFO: &str = "/xinfo";
/// `/xremote` (p. 9): all parameter changes are sent to the client for 10 s; renew
/// before that. At most four clients at a time.
pub const XREMOTE: &str = "/xremote";
/// `/-stat/aes50/A` and `B` (p. 65): the box chain detected on each
/// AES50 port, as letters (see [`aes50_box`]).
pub const AES50_STAT: [&str; 2] = ["/-stat/aes50/A", "/-stat/aes50/B"];

/// The model names `/info` reports (p. 11) and their local mic preamps.
///
/// The preamp counts are from the models' published specifications, not from the
/// protocol document: Standard (X32, M32) 32; Compact, Producer, Rack (X32C, X32P,
/// X32RACK, M32C, M32R) 16; Core (X32CORE) none. Unknown models: 32 (the protocol's
/// range).
pub fn local_preamps(model: &str) -> u8 {
    match model.trim() {
        "X32" | "M32" => 32,
        "X32C" | "X32P" | "X32RACK" | "M32C" | "M32R" => 16,
        "X32CORE" => 0,
        _ => 32,
    }
}

/// A box on an AES50 port, from the letters `/-stat/aes50/[A,B]` reports (p. 65).
pub fn aes50_box(letter: char) -> Option<&'static str> {
    Some(match letter {
        'A' => "S16",
        'B' => "X32C",
        'C' => "X32",
        'D' => "DL251",
        'E' => "DL251HA",
        'F' => "S16B",
        'G' => "Z32",
        'H' => "T8",
        'I' => "X32P",
        'J' => "X32RACK",
        'K' => "X32CORE",
        'L' => "M32",
        'M' => "M32R",
        'N' => "DL16",
        'O' => "DL16B",
        'P' => "SD16",
        'Q' => "SD16B",
        'R' => "SD8",
        'S' => "SD8B",
        'T' => "DL15X",
        'U' => "DL15XHA",
        'V' => "DL231",
        'W' => "S32",
        'X' => "S32B",
        'Y' => "DL32",
        'Z' => "DL32B",
        'a' => "M32C",
        _ => return None,
    })
}

// --- What a signal is. ---

/// A signal the X32 can put on a card (USB) channel, after the routing is followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "n")]
pub enum Source {
    /// Nothing.
    Off,
    /// A local XLR input 1…32 (headamp 0…31).
    Local(u8),
    /// AES50 port A input 1…48 (headamp 32…79).
    Aes50A(u8),
    /// AES50 port B input 1…48 (headamp 80…127).
    Aes50B(u8),
    /// The card's own input 1…32 (a loop back).
    CardIn(u8),
    /// Aux input 1…6.
    AuxIn(u8),
    /// Talkback, internal (1) or external (2).
    Talkback(u8),
    /// Aux output 1…6.
    AuxOut(u8),
    /// Monitor left (1) or right (2).
    Monitor(u8),
    /// Main left (1), right (2), mono/centre (3).
    Main(u8),
    /// Mix bus 1…16.
    Bus(u8),
    /// Matrix 1…6.
    Matrix(u8),
    /// An input channel's direct out, channel 1…32.
    DirectOut(u8),
    /// An aux input channel's direct out, 1…8.
    DirectOutAux(u8),
    /// An effect return's direct out, 1…8 (1L, 1R … 4R).
    DirectOutFx(u8),
    /// An output (1…16), a P16 output (17…32) or a user route whose patch was not read
    /// (yet, or the firmware does not have it): the hop it stopped at.
    Unresolved(Hop),
}

/// A routing step between the card and the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "n")]
pub enum Hop {
    /// Output 1…16 (its source: `/outputs/main/NN/src`).
    Output(u8),
    /// P16 output 1…16 (its source: `/outputs/p16/NN/src`).
    P16(u8),
    /// User output 1…48 (FW 4.0+, `/config/userrout/out/NN`).
    UserOut(u8),
    /// User input 1…32 (FW 4.0+, `/config/userrout/in/NN`).
    UserIn(u8),
}

impl std::fmt::Display for Source {
    /// In words, as the X32 names it: "Local 5", "AES50 A 3", "Direct out Ch 5".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Source::Off => write!(f, "Off"),
            Source::Local(n) => write!(f, "Local {n}"),
            Source::Aes50A(n) => write!(f, "AES50 A {n}"),
            Source::Aes50B(n) => write!(f, "AES50 B {n}"),
            Source::CardIn(n) => write!(f, "Card in {n}"),
            Source::AuxIn(n) => write!(f, "Aux in {n}"),
            Source::Talkback(1) => write!(f, "Talkback internal"),
            Source::Talkback(2) => write!(f, "Talkback external"),
            Source::Talkback(_) => write!(f, "Talkback"),
            Source::AuxOut(n) => write!(f, "Aux out {n}"),
            Source::Monitor(1) => write!(f, "Monitor L"),
            Source::Monitor(_) => write!(f, "Monitor R"),
            Source::Main(1) => write!(f, "Main L"),
            Source::Main(2) => write!(f, "Main R"),
            Source::Main(_) => write!(f, "Main M/C"),
            Source::Bus(n) => write!(f, "Mix bus {n}"),
            Source::Matrix(n) => write!(f, "Matrix {n}"),
            Source::DirectOut(n) => write!(f, "Direct out Ch {n}"),
            Source::DirectOutAux(n) => write!(f, "Direct out Aux {n}"),
            Source::DirectOutFx(n) => write!(f, "Direct out FX {n}"),
            Source::Unresolved(h) => match h {
                Hop::Output(n) => write!(f, "Output {n} (not read)"),
                Hop::P16(n) => write!(f, "P16 {n} (not read)"),
                Hop::UserOut(n) => write!(f, "User out {n} (not read)"),
                Hop::UserIn(n) => write!(f, "User in {n} (not read)"),
            },
        }
    }
}

impl Source {
    /// The headamp this source is, if it is one: local 0…31, AES50 A 32…79, AES50 B
    /// 80…127 (p. 43).
    pub fn headamp(self) -> Option<u8> {
        match self {
            Source::Local(n @ 1..=32) => Some(n - 1),
            Source::Aes50A(n @ 1..=48) => Some(31 + n),
            Source::Aes50B(n @ 1..=48) => Some(79 + n),
            _ => None,
        }
    }

    /// The source a headamp index is.
    pub fn of_headamp(index: u8) -> Option<Source> {
        match index {
            0..=31 => Some(Source::Local(index + 1)),
            32..=79 => Some(Source::Aes50A(index - 31)),
            80..=127 => Some(Source::Aes50B(index - 79)),
            _ => None,
        }
    }
}

/// What a card routing block's value names for slot `k` (0…7) of its eight (p. 23,
/// the CARD blocks' enum [0…35]): `{AN1-8, AN9-16, AN17-24, AN25-32, A1-8 … A41-48,
/// B1-8 … B41-48, CARD1-8 … CARD25-32, OUT1-8, OUT9-16, P16 1-8, P16 9-16,
/// AUX1-6/Mon, AuxIN1-6/TB, UOUT1-8 … UOUT41-48, UIN1-8 … UIN25-32}`.
///
/// `AUX1-6/Mon` is read as aux outs 1–6 then monitor L, R; `AuxIN1-6/TB` as aux ins
/// 1–6 then talkback internal, external (the order the names give; to check on a
/// desk).
pub enum Slot {
    /// A signal directly.
    Signal(Source),
    /// A hop to follow.
    Hop(Hop),
}

/// Card block value `v`, slot `k` (0…7).
pub fn card_slot(v: i32, k: u8) -> Slot {
    let k = k.min(7);
    let n = |base: i32| (((v - base) * 8) as u8) + k + 1;
    match v {
        0..=3 => Slot::Signal(Source::Local(n(0))),
        4..=9 => Slot::Signal(Source::Aes50A(n(4))),
        10..=15 => Slot::Signal(Source::Aes50B(n(10))),
        16..=19 => Slot::Signal(Source::CardIn(n(16))),
        20..=21 => Slot::Hop(Hop::Output(n(20))),
        22..=23 => Slot::Hop(Hop::P16(n(22))),
        24 => Slot::Signal(match k {
            0..=5 => Source::AuxOut(k + 1),
            _ => Source::Monitor(k - 5),
        }),
        25 => Slot::Signal(match k {
            0..=5 => Source::AuxIn(k + 1),
            _ => Source::Talkback(k - 5),
        }),
        26..=31 => Slot::Hop(Hop::UserOut(n(26))),
        32..=35 => Slot::Hop(Hop::UserIn(n(32))),
        _ => Slot::Signal(Source::Off),
    }
}

/// A user route's value (p. 22, `/config/userrout/out` [0…208]; `in` is its first
/// 169): 0 OFF, 1…32 Local In, 33…80 AES50-A, 81…128 AES50-B, 129…160 Card In,
/// 161…166 Aux In, 167 TB internal, 168 TB external, 169…184 Outputs 1…16, 185…200 P16
/// 1…16, 201…206 AUX 1…6, 207 Monitor L, 208 Monitor R.
pub fn user_route(v: i32) -> Slot {
    let n = |base: i32| (v - base) as u8;
    match v {
        1..=32 => Slot::Signal(Source::Local(n(0))),
        33..=80 => Slot::Signal(Source::Aes50A(n(32))),
        81..=128 => Slot::Signal(Source::Aes50B(n(80))),
        129..=160 => Slot::Signal(Source::CardIn(n(128))),
        161..=166 => Slot::Signal(Source::AuxIn(n(160))),
        167..=168 => Slot::Signal(Source::Talkback(n(166))),
        169..=184 => Slot::Hop(Hop::Output(n(168))),
        185..=200 => Slot::Hop(Hop::P16(n(184))),
        201..=206 => Slot::Signal(Source::AuxOut(n(200))),
        207..=208 => Slot::Signal(Source::Monitor(n(206))),
        _ => Slot::Signal(Source::Off),
    }
}

/// An output's source (p. 41, `/outputs/*/NN/src` [0…76]): OFF, Main L, Main R, M/C,
/// MixBus 01…16, Matrix 1…6, DirectOut Ch 01…32, DirectOut Aux 1…8, DirectOut FX
/// 1L…4R, Monitor L, Monitor R, Talkback.
pub fn output_source(v: i32) -> Source {
    let n = |base: i32| (v - base) as u8;
    match v {
        1..=3 => Source::Main(n(0)),
        4..=19 => Source::Bus(n(3)),
        20..=25 => Source::Matrix(n(19)),
        26..=57 => Source::DirectOut(n(25)),
        58..=65 => Source::DirectOutAux(n(57)),
        66..=73 => Source::DirectOutFx(n(65)),
        74..=75 => Source::Monitor(n(73)),
        // "Talkback" here has no internal/external split.
        76 => Source::Talkback(0),
        _ => Source::Off,
    }
}

/// What the module has read of the X32's routing, by the values it reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Tables {
    /// The four card blocks' values.
    pub card: [Option<i32>; 4],
    /// User outputs 1…48.
    pub user_out: [Option<i32>; 48],
    /// User inputs 1…32.
    pub user_in: [Option<i32>; 32],
    /// Outputs 1…16's sources.
    pub outputs: [Option<i32>; 16],
    /// P16 outputs 1…16's sources.
    pub p16: [Option<i32>; 16],
    /// Each channel's headamp (`/-ha/NN/index`), channels 1…32.
    pub channel_headamp: [Option<i32>; CHANNELS],
}

impl Default for Tables {
    fn default() -> Tables {
        Tables {
            card: [None; 4],
            user_out: [None; 48],
            user_in: [None; 32],
            outputs: [None; 16],
            p16: [None; 16],
            channel_headamp: [None; CHANNELS],
        }
    }
}

/// One USB (card) channel: what the X32 sends on it, and the preamp behind it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsbChannel {
    /// The USB channel, 1…32.
    pub usb: u8,
    /// What it carries (as far as the routing was read).
    pub source: Source,
    /// The hops followed to get there (a user route, an output), in order.
    pub via: Vec<Hop>,
    /// The headamp (preamp) behind it, 0…127, if there is one.
    pub headamp: Option<u8>,
    /// The X32 input channel 1…32 whose polarity applies: a direct out's channel, or
    /// the lowest channel taking this headamp as its source (`/-ha/NN/index`).
    pub channel: Option<u8>,
}

impl Tables {
    /// Follows a hop one step (`None`: not read yet).
    fn follow(&self, hop: Hop) -> Option<Slot> {
        let at = |t: &[Option<i32>], n: u8| t.get(usize::from(n).checked_sub(1)?).copied()?;
        Some(match hop {
            Hop::Output(n) => Slot::Signal(output_source(at(&self.outputs, n)?)),
            Hop::P16(n) => Slot::Signal(output_source(at(&self.p16, n)?)),
            Hop::UserOut(n) => user_route(at(&self.user_out, n)?),
            Hop::UserIn(n) => user_route(at(&self.user_in, n)?),
        })
    }

    /// The lowest X32 channel whose source is headamp `ha`.
    pub fn channel_of(&self, ha: u8) -> Option<u8> {
        self.channel_headamp
            .iter()
            .position(|v| *v == Some(i32::from(ha)))
            .map(|i| i as u8 + 1)
    }

    /// USB channel `usb` (1…32) as far as it can be followed. `None` when its card block
    /// has not been read.
    pub fn usb(&self, usb: u8) -> Option<UsbChannel> {
        let i = usize::from(usb.checked_sub(1)?);
        let v = (*self.card.get(i / 8)?)?;
        let mut slot = card_slot(v, (i % 8) as u8);
        let mut via = Vec::new();
        // At most a user route, then an output: three hops is plenty and stops loops.
        let source = loop {
            match slot {
                Slot::Signal(s) => break s,
                Slot::Hop(h) => {
                    via.push(h);
                    match (via.len() <= 3).then(|| self.follow(h)).flatten() {
                        Some(next) => slot = next,
                        None => break Source::Unresolved(h),
                    }
                }
            }
        };
        let (headamp, channel) = match source {
            Source::DirectOut(ch) => {
                let ha = self
                    .channel_headamp
                    .get(usize::from(ch).wrapping_sub(1))
                    .copied()
                    .flatten()
                    .and_then(|v| u8::try_from(v).ok())
                    .filter(|&v| usize::from(v) < HEADAMPS);
                (ha, Some(ch))
            }
            s => {
                let ha = s.headamp();
                (ha, ha.and_then(|ha| self.channel_of(ha)))
            }
        };
        Some(UsbChannel {
            usb,
            source,
            via,
            headamp,
            channel,
        })
    }

    /// The addresses whose values are needed to follow every USB channel read so far
    /// (user routes and output sources the card blocks point at).
    pub fn needs(&self) -> Vec<String> {
        let mut out = Vec::new();
        for usb in 1..=USB_CHANNELS as u8 {
            let Some(u) = self.usb(usb) else { continue };
            if let Source::Unresolved(h) = u.source {
                out.push(match h {
                    Hop::Output(n) => output_src(n),
                    Hop::P16(n) => p16_src(n),
                    Hop::UserOut(n) => user_out(n),
                    Hop::UserIn(n) => user_in(n),
                });
            }
        }
        out
    }
}
