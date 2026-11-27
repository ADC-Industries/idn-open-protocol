//! The values the commands take: routes, IEM sends, buses and strips by name or number,
//! inserts, transport, audio-follow-video and X32 patches, rates, snapshot scopes.

use super::*;

/// One channel's route: the receiver's channel `ch` from the transmitter `tx`'s channel `tx_ch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RouteChange {
    /// The receiver's channel, from 0.
    pub ch: u8,
    /// The transmitter; absent or empty: the route cleared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx: Option<String>,
    /// The transmitter's channel, from 0 (with `tx`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_ch: Option<u8>,
}

/// One channel's entry in an IEM mix (only the keys given change).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IemSendPatch {
    /// Its level, dB (−144: off, to +10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level_db: Option<f32>,
    /// −1 (left) to +1 (right); a linked pair's balance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pan: Option<f32>,
    /// Muted in this mix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    /// Soloed in this mix (while any is, only the soloed are heard).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solo: Option<bool>,
}

/// A send's bus: an aux bus, from 1, or the stream bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SendBus {
    /// An aux bus, from 1.
    Aux(usize),
    /// The stream bus.
    Named(SendBusName),
    /// An aux bus's number as text (`"3"`, as a form sends it).
    AuxText(NumText),
}

/// The send buses addressed by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendBusName {
    /// The stream bus.
    Stream,
}

/// A bus by name or number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum BusRef {
    /// An aux bus, from 1.
    Aux(usize),
    /// The main bus, the stream bus or the headphones.
    Named(BusName),
    /// An aux bus's number as text (`"5"`, as a form sends it).
    AuxText(NumText),
}

/// A whole number written as text (`"5"`): a form's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumText(pub usize);

impl Serialize for NumText {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for NumText {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let t = String::deserialize(d)?;
        t.trim()
            .parse()
            .map(NumText)
            .map_err(|_| serde::de::Error::custom(format!("{t:?}: not a number")))
    }
}

impl schemars::JsonSchema for NumText {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NumText".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^\\s*[0-9]+\\s*$"})
    }
}

/// The buses addressed by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BusName {
    /// The main bus.
    Main,
    /// The stream bus (the livestream's own mix).
    Stream,
    /// The headphones (monitor) bus.
    Headphones,
    /// The headphones bus, by its other name.
    Monitor,
}

/// A strip with a plugin chain: a channel or a bus (one key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Strip {
    /// An input channel, from 1 (a linked pair's plugins are on its first channel).
    Channel(usize),
    /// The main bus, an aux bus or the headphones.
    Bus(BusRef),
    /// A group bus, from 1.
    Group(usize),
    /// A matrix mix, from 1.
    Matrix(usize),
    /// An FX bus, from 1.
    Fx(usize),
}

/// A change to a strip's plugin chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum InsertOp {
    /// Adds a plugin.
    Add {
        /// The plugin, from the plugin list (`clap:PATH#ID`, `vst3:PATH#CLASS`).
        key: String,
        /// Its name; absent: the plugin list's.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Its place, from 0; absent: last.
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<usize>,
    },
    /// Removes a plugin.
    Remove {
        /// Its place, from 0.
        index: usize,
    },
    /// Moves a plugin.
    Move {
        /// Its place, from 0.
        index: usize,
        /// The new place, from 0.
        to: usize,
    },
    /// Bypasses a plugin or not.
    Bypass {
        /// Its place, from 0.
        index: usize,
        /// Bypassed.
        on: bool,
    },
}

/// The soundcheck playback's transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Transport {
    /// Plays.
    Play,
    /// Pauses.
    Pause,
    /// Goes to a time.
    Locate {
        /// Seconds from the start; absent: 0.
        #[serde(skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// Loops or not.
    Loop {
        /// Looping; absent: true.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
}

/// Audio follows video's settings (only the keys given change).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AfvPatch {
    /// On.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    /// The switcher's program source (`HOST (Switcher)`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switcher: Option<String>,
    /// A following channel's level while its input is off air, dB (−144: off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off: Option<f32>,
    /// The fade between, ms (0 to 5000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade: Option<f32>,
}

/// The desk's X32 setting (only the keys given change).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct X32Patch {
    /// `""`: none; `"auto"`: the first X32 found on the LAN; else its IP address or host name (`:port` if not 10023).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A sample rate: a number, or text ("48000"; "": the machine's).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Rate {
    /// Hz: 44100, 48000 or 96000.
    Hz(u32),
    /// As text; empty: the machine's.
    Text(String),
}

/// What a snapshot's recall takes (absent keys: all of it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SnapshotScope {
    /// The channels: a list (from 1), true (all) or false (none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<ScopeChannels>,
    /// The buses, DCAs, mute groups, talkback, user keys, automix, duckers and AFV.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buses: Option<bool>,
    /// The output patch and its delays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<bool>,
    /// What of each channel it takes; absent: all of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<crate::engine::RecallPart>>,
}

/// A snapshot recall's channels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ScopeChannels {
    /// True: all; false: none.
    All(bool),
    /// These channels, from 1.
    List(Vec<usize>),
}
