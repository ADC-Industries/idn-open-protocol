//! The values the commands take: inputs by id or label, lists, slideshows, shots and
//! cameras, timers, multiview tiles, the stream's sound and MQTT triggers.

use super::*;

/// An input: by its numeric id, or by its label (`"Camera 1"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum InputRef {
    /// The input's id.
    Id(u64),
    /// The input's label (the first input with it).
    Label(String),
}

impl From<u32> for InputRef {
    fn from(id: u32) -> Self {
        InputRef::Id(u64::from(id))
    }
}

impl From<u64> for InputRef {
    fn from(id: u64) -> Self {
        InputRef::Id(id)
    }
}

impl From<&str> for InputRef {
    fn from(label: &str) -> Self {
        InputRef::Label(label.to_owned())
    }
}

impl From<String> for InputRef {
    fn from(label: String) -> Self {
        InputRef::Label(label)
    }
}

/// One item of a list: a file, or a file with how long a picture shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ListItem {
    /// A file (a path, a name in the media folder, or `library:FOLDER/FILE`).
    Path(String),
    /// A file with its own time on screen.
    Timed {
        /// The file (a path, a name in the media folder, or `library:FOLDER/FILE`).
        path: String,
        /// Seconds a picture shows (0.1–3600; the list's `seconds` if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
}

/// A list input (§S.18): videos and pictures one after another.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListSpec {
    /// The list's files, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ListItem>>,
    /// A folder whose videos, sound files and pictures are added, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Files separated by `|`, or a folder (one name with no extension): as `items` or
    /// `folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Seconds each picture shows (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    /// Whether the list starts again at its end (default true).
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub looped: Option<bool>,
    /// Whether the items play in a random order (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
}

/// A slideshow input: pictures, each for a while.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SlideshowSpec {
    /// The picture files, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pictures: Option<Vec<String>>,
    /// A folder whose pictures are added, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Picture files separated by `|`, or a folder (one name that isn't a picture's): as
    /// `pictures` or `folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Seconds each picture shows (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<f64>,
    /// Seconds of the fade between pictures (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<f64>,
    /// Whether it starts again at the end (default true).
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub looped: Option<bool>,
    /// Whether the pictures come in a random order (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
}

/// A preset input: a camera's stored position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PtzShot {
    /// The camera's input id.
    pub camera: u32,
    /// The preset, 1 to 16.
    pub preset: u64,
}

/// A part of a picture: its centre and zoom.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct View {
    /// The centre across, 0–1 (default 0.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    /// The centre down, 0–1 (default 0.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// The zoom, 1 for the whole picture (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f64>,
}

/// A virtual shot input: a part of another input's picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VirtualShot {
    /// The input it is cut from (its id).
    pub input: u32,
    /// The part shown (the whole picture if missing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
}

/// How a camera is controlled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum CameraControl {
    /// `"network"`: over the IDN (NDI-Compatible) control channel.
    Network(String),
    /// VISCA to the camera's address.
    Visca {
        /// `ip`, `tcp` or `udp`.
        visca: String,
        /// The camera's host (and port).
        address: String,
    },
}

/// A camera move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PtzMove {
    /// Pan and tilt at `pan` and `tilt` speeds.
    Drive,
    /// Zoom at `speed`.
    Zoom,
    /// Stop pan, tilt and zoom.
    Stop,
    /// Go to the home position.
    Home,
    /// Focus automatically.
    Autofocus,
    /// Go to `preset` at `speed`.
    Recall,
    /// Store the position as `preset`.
    Store,
}

/// A list player's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListAction {
    /// The next item.
    Next,
    /// The item before.
    Previous,
    /// Play on.
    Play,
    /// Pause.
    Pause,
    /// From the first item again.
    Restart,
    /// To item `n` (from 0).
    Goto,
}

/// A media player's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MediaAction {
    /// Play on.
    Play,
    /// Pause.
    Pause,
    /// To `position` seconds.
    Seek,
    /// From the start, playing.
    Restart,
}

/// A slideshow's (or a title's data rows') move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SlideMove {
    /// The next one.
    Next,
    /// The one before.
    Previous,
    /// Move on by itself.
    Play,
    /// Stay.
    Pause,
    /// To `n` (from 0).
    Goto,
    /// Read the title's data again now (titles only).
    Read,
}

/// What a timer command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TimerOp {
    /// Make or change it (`mode`, `seconds`, `until`).
    Set,
    /// Start it.
    Start,
    /// Stop it.
    Stop,
    /// Back to its start.
    Reset,
    /// Remove it.
    Remove,
}

/// How a timer counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TimerMode {
    /// Down from `seconds`.
    Down,
    /// Up from 0.
    Up,
    /// Down to a time of day (`until`).
    Until,
}

/// What `slide` does to the slideshow or list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SlideStep {
    /// The next one.
    Next,
    /// The one before.
    Previous,
    /// Move on by itself.
    Play,
    /// Stay.
    Pause,
    /// To `n` (from 0).
    Goto,
    /// From the first item again (a list only).
    Restart,
}

/// A length of time: seconds, or text as `"m:ss"`, `"h:mm:ss"` or `"90"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TimeLength {
    /// Seconds.
    Seconds(f64),
    /// `"m:ss"`, `"h:mm:ss"`, or seconds as text.
    Text(String),
}

impl From<f64> for TimeLength {
    fn from(s: f64) -> Self {
        TimeLength::Seconds(s)
    }
}

/// A multiview tile (§S.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MultiviewTile {
    /// What it shows: `program`, `preview`, `input`, `scene` or `mix`.
    pub kind: String,
    /// The input's, scene's or mix's id (`input`, `scene`, `mix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    /// Where it goes: x, y, width, height as fractions of the multiview.
    pub rect: [f64; 4],
    /// Whether it has its label strip (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<bool>,
    /// Whether its border shows tally, red on program and green on preview (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tally: Option<bool>,
}

/// The program's sound source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AudioSourceSpec {
    /// `"none"`, `"program"` (follows the program) or `"routed"`.
    Named(String),
    /// A sound card's channels.
    Card {
        /// The card's device, or null for the default card.
        #[serde(deserialize_with = "nullable")]
        #[schemars(required)]
        card: Option<String>,
        /// Its first channel, from 1 (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first: Option<u64>,
    },
    /// An input's own sound.
    Input {
        /// The input's id.
        input: u32,
    },
}

/// An MQTT trigger: a message that runs a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MqttTriggerSpec {
    /// The topic, wildcards allowed.
    pub topic: String,
    /// Text the payload must contain (none: any payload).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// The command it runs.
    pub command: String,
    /// The command's body (an object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}
